//! Le favicon d'un onglet : une fois la navigation finie, un script d'une seule passe lit les `<link rel="icon">` de la
//! page (monde de contenu du client, isolé des scripts de la page ; rien n'est injecté à demeure), puis l'image se
//! télécharge par `NSURLSession` et se décode par AppKit. Le résultat part en `PageEvent::Favicon` (PNG), l'onglet le
//! peint. Tout échec est muet : l'onglet garde son globe.

use std::cell::{Cell, RefCell};

use block2::{DynBlock, RcBlock};
use futures::channel::mpsc::UnboundedSender;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, ClassType, DefinedClass, MainThreadMarker, define_class, msg_send};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage};
use objc2_foundation::{
    NSData, NSDictionary, NSError, NSHTTPURLResponse, NSObject, NSOperationQueue, NSString, NSURL,
    NSURLResponse, NSURLSession, NSURLSessionConfiguration, NSURLSessionDataTask,
    NSURLSessionResponseDisposition, NSURLSessionTask,
};
use objc2_web_kit::{WKContentWorld, WKWebView};
use url::Url;

use super::{PageEvent, png_of};
use crate::hud;

/// Au-delà, l'image n'est pas lue : un favicon pèse quelques ko, et la borne coupe court à une image piégée.
const MAX_ICON_BYTES: usize = 256 * 1024;
/// Côté maximal, en pixels, d'une image acceptée (avant tout décodage complet).
const MAX_ICON_SIDE: isize = 512;
/// Délai d'une requête d'icône, en secondes.
const REQUEST_TIMEOUT: f64 = 10.;
/// Au plus ce nombre d'icônes déclarées par la page sont essayées, avant `/favicon.ico`.
const MAX_DECLARED_ATTEMPTS: usize = 2;
/// Le côté que l'on vise : l'icône s'affiche à 15 points, soit 30 pixels sur un écran 2x.
const TARGET_SIDE: u32 = 32;
/// En dessous, une icône déclarée est trop petite pour un onglet : elle passe après les autres.
const MIN_SIDE: u32 = 16;

/// Lu une fois la navigation finie : les icônes déclarées, `[adresse absolue, sizes, type]` chacune, en JSON.
const ICON_LINKS_SCRIPT: &str = r#"JSON.stringify(Array.from(document.querySelectorAll('link[rel~="icon" i]')).filter(link => link.href).map(link => [link.href, link.getAttribute('sizes') || '', link.type || '']))"#;

/// Une icône déclarée par la page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconLink {
    pub href: String,
    pub sizes: String,
}

/// L'origine (`schéma://hôte[:port]`) d'une adresse web, ou rien : une adresse de fichier, `about:` ou sans hôte n'a pas
/// de favicon à chercher. C'est la clé du cache des icônes.
pub fn page_origin(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed.host_str()?;
    Some(parsed.origin().ascii_serialization())
}

/// Le JSON que rend le script, en icônes. Un JSON qui n'a pas la forme attendue ne donne aucune icône.
pub fn parse_links(json: &str) -> Vec<IconLink> {
    let Ok(rows) = serde_json::from_str::<Vec<(String, String, String)>>(json) else {
        return Vec::new();
    };
    rows.into_iter()
        .map(|(href, sizes, _)| IconLink { href, sizes })
        .collect()
}

/// Le plus grand côté que déclare un attribut `sizes` (« 16x16 32x32 »). « any » (vectoriel) et l'absence de
/// déclaration valent le côté visé : on ne les écarte pas, on ne les préfère pas non plus.
fn declared_side(sizes: &str) -> u32 {
    sizes
        .split_whitespace()
        .filter_map(|token| {
            let lower = token.to_ascii_lowercase();
            let (width, _) = lower.split_once('x')?;
            width.parse::<u32>().ok()
        })
        .max()
        .unwrap_or(TARGET_SIDE)
}

/// Le rang d'une icône (le plus petit passe en premier) : celles d'au moins 32 de côté d'abord, la plus proche du côté
/// visé en tête ; puis celles de 16 à 31, la plus grande d'abord ; en dernier les trop petites.
fn rank(side: u32) -> (u8, u32) {
    if side >= TARGET_SIDE {
        (0, side - TARGET_SIDE)
    } else if side >= MIN_SIDE {
        (1, TARGET_SIDE - side)
    } else {
        (2, MIN_SIDE - side)
    }
}

/// Les adresses à essayer, dans l'ordre : les icônes déclarées par la page (http, https ou `data:`, mieux rangées
/// d'abord, à égalité l'ordre du document), puis `<origine>/favicon.ico`.
pub fn candidate_urls(links: &[IconLink], origin: &str) -> Vec<String> {
    let mut declared: Vec<&IconLink> = links
        .iter()
        .filter(|link| {
            let lower = link.href.to_ascii_lowercase();
            lower.starts_with("http://")
                || lower.starts_with("https://")
                || lower.starts_with("data:image/")
        })
        .collect();
    declared.sort_by_key(|link| rank(declared_side(&link.sizes)));
    let fallback = format!("{origin}/favicon.ico");
    let mut urls: Vec<String> = Vec::new();
    for link in declared {
        if urls.len() == MAX_DECLARED_ATTEMPTS {
            break;
        }
        if !urls.contains(&link.href) && link.href != fallback {
            urls.push(link.href.clone());
        }
    }
    urls.push(fallback);
    urls
}

/// Lance la recherche du favicon de la page : le script, puis les téléchargements un à un jusqu'à une image lisible.
/// `origin` est celle de la page au moment de la demande ; l'événement la rapporte, le volet écarte la réponse d'une
/// page qui a changé d'origine entre-temps.
pub(super) fn request(
    mtm: MainThreadMarker,
    webview: &WKWebView,
    origin: String,
    events: UnboundedSender<PageEvent>,
) {
    let script = NSString::from_str(ICON_LINKS_SCRIPT);
    // SAFETY: fil principal (`mtm`) ; le monde de contenu du client est un singleton de WebKit.
    let world = unsafe { WKContentWorld::defaultClientWorld(mtm) };
    let block = RcBlock::new(move |result: *mut AnyObject, _error: *mut NSError| {
        // SAFETY: WebKit passe nil ou un objet valide pour la durée de l'appel ; le script ne rend qu'une chaîne.
        let json = unsafe { result.as_ref() }
            .and_then(|value| value.downcast_ref::<NSString>())
            .map(ToString::to_string);
        let links = json.as_deref().map(parse_links).unwrap_or_default();
        fetch_first(
            candidate_urls(&links, &origin),
            origin.clone(),
            events.clone(),
        );
    });
    // SAFETY: fil principal ; frame `None` = le cadre principal ; le bloc est copié par WebKit.
    unsafe {
        webview.evaluateJavaScript_inFrame_inContentWorld_completionHandler(
            &script,
            None,
            &world,
            Some(&block),
        );
    }
}

/// Télécharge la première adresse de `urls` ; image illisible ou échec, la suivante. Rappelé sur le fil principal.
fn fetch_first(mut urls: Vec<String>, origin: String, events: UnboundedSender<PageEvent>) {
    if urls.is_empty() {
        return;
    }
    let next = urls.remove(0);
    let Some(url) = NSURL::URLWithString(&NSString::from_str(&next)) else {
        fetch_first(urls, origin, events);
        return;
    };
    // Session éphémère : ni cookies ni cache disque pour une icône. Le délai de ressource borne la durée totale (celui de
    // requête n'est qu'un délai d'inactivité : un flux sans fin ne l'atteint jamais). Les rappels du délégué arrivent sur
    // la file principale, où AppKit décode.
    let configuration = NSURLSessionConfiguration::ephemeralSessionConfiguration();
    configuration.setTimeoutIntervalForRequest(REQUEST_TIMEOUT);
    configuration.setTimeoutIntervalForResource(REQUEST_TIMEOUT);
    let delegate = FaviconFetch::new(origin, urls, events);
    // SAFETY: `delegate` est un objet NSObject qui répond aux sélecteurs `NSURLSessionDataDelegate` qu'il déclare (la
    // session teste `respondsToSelector:`) ; la file principale est celle où ils décodent. La session garde le délégué
    // jusqu'à son invalidation.
    let session: Retained<NSURLSession> = unsafe {
        msg_send![
            NSURLSession::class(),
            sessionWithConfiguration: &*configuration,
            delegate: &*delegate,
            delegateQueue: &*NSOperationQueue::mainQueue(),
        ]
    };
    let task = session.dataTaskWithURL(&url);
    task.resume();
    session.finishTasksAndInvalidate();
}

/// L'état d'un téléchargement d'icône : le corps reçu, et ce qu'il faut tenter ensuite s'il échoue.
struct FaviconFetchIvars {
    origin: String,
    /// Les adresses qui restent à essayer.
    rest: Vec<String>,
    events: UnboundedSender<PageEvent>,
    body: RefCell<Vec<u8>>,
    /// La réponse est refusée (statut, taille annoncée ou reçue au-delà de la borne) : le téléchargement est annulé.
    rejected: Cell<bool>,
}

define_class!(
    /// Le délégué d'un téléchargement d'icône. Il reçoit le corps par morceaux et coupe le téléchargement dès qu'il passe
    /// `MAX_ICON_BYTES` : un serveur qui diffuse sans fin ne remplit ni la mémoire ni le réseau.
    #[unsafe(super(NSObject))]
    #[name = "IdemaFaviconFetch"]
    #[ivars = FaviconFetchIvars]
    struct FaviconFetch;

    // SAFETY: signatures des sélecteurs de `NSURLSessionDataDelegate` et `NSURLSessionTaskDelegate` ; objc2 les vérifie à
    // l'enregistrement en débogage. Les rappels viennent de la file principale (`delegateQueue`).
    impl FaviconFetch {
        #[unsafe(method(URLSession:dataTask:didReceiveResponse:completionHandler:))]
        fn did_receive_response(
            &self,
            _session: &NSURLSession,
            _task: &NSURLSessionDataTask,
            response: &NSURLResponse,
            completion_handler: &DynBlock<dyn Fn(NSURLSessionResponseDisposition)>,
        ) {
            // Une longueur annoncée trop grande coupe tout de suite ; inconnue (négative), elle se juge à la réception.
            let announced_too_big = usize::try_from(response.expectedContentLength())
                .is_ok_and(|length| length > MAX_ICON_BYTES);
            if announced_too_big || !is_success(response) {
                self.ivars().rejected.set(true);
                hud::log_debug("browser favicon refused");
                completion_handler.call((NSURLSessionResponseDisposition::Cancel,));
            } else {
                completion_handler.call((NSURLSessionResponseDisposition::Allow,));
            }
        }

        #[unsafe(method(URLSession:dataTask:didReceiveData:))]
        fn did_receive_data(
            &self,
            _session: &NSURLSession,
            task: &NSURLSessionDataTask,
            data: &NSData,
        ) {
            let ivars = self.ivars();
            if ivars.rejected.get() {
                return;
            }
            let mut body = ivars.body.borrow_mut();
            body.extend_from_slice(&data.to_vec());
            if body.len() > MAX_ICON_BYTES {
                // La mémoire reste bornée : au plus la borne et un morceau.
                body.clear();
                ivars.rejected.set(true);
                task.cancel();
                hud::log_event("browser favicon aborted over the size cap");
            }
        }

        #[unsafe(method(URLSession:task:didCompleteWithError:))]
        fn did_complete(
            &self,
            _session: &NSURLSession,
            _task: &NSURLSessionTask,
            error: Option<&NSError>,
        ) {
            let ivars = self.ivars();
            let png = if error.is_some() || ivars.rejected.get() {
                None
            } else {
                let body = ivars.body.borrow();
                icon_png(&NSData::with_bytes(&body))
            };
            match png {
                Some(png) => {
                    hud::log_debug("browser favicon ready");
                    let _ = ivars.events.unbounded_send(PageEvent::Favicon {
                        origin: ivars.origin.clone(),
                        png,
                    });
                }
                None => fetch_first(
                    ivars.rest.clone(),
                    ivars.origin.clone(),
                    ivars.events.clone(),
                ),
            }
        }
    }
);

impl FaviconFetch {
    fn new(
        origin: String,
        rest: Vec<String>,
        events: UnboundedSender<PageEvent>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(FaviconFetchIvars {
            origin,
            rest,
            events,
            body: RefCell::new(Vec::new()),
            rejected: Cell::new(false),
        });
        // SAFETY: initialiseur désigné de `NSObject`, appelé une fois sur un objet alloué.
        unsafe { msg_send![super(this), init] }
    }
}

/// Une réponse HTTP 2xx, ou une réponse qui n'est pas HTTP (`data:`).
fn is_success(response: &NSURLResponse) -> bool {
    response
        .downcast_ref::<NSHTTPURLResponse>()
        .is_none_or(|http| (200..300).contains(&http.statusCode()))
}

/// L'image en PNG : le plus grand bitmap du fichier (un `.ico` en porte plusieurs) dans la borne de taille, sinon le
/// rendu d'un vectoriel (SVG). Une image trop grande, ou qui n'est pas une image, ne donne rien.
fn icon_png(data: &NSData) -> Option<Vec<u8>> {
    let representations = NSBitmapImageRep::imageRepsWithData(data);
    if !representations.is_empty() {
        let best = representations
            .iter()
            .filter_map(|rep| rep.downcast::<NSBitmapImageRep>().ok())
            .filter(|rep| (1..=MAX_ICON_SIDE).contains(&rep.pixelsWide()))
            .filter(|rep| (1..=MAX_ICON_SIDE).contains(&rep.pixelsHigh()))
            .max_by_key(|rep| rep.pixelsWide())?;
        // SAFETY: dictionnaire de propriétés vide, type PNG supporté.
        let png = unsafe {
            best.representationUsingType_properties(
                NSBitmapImageFileType::PNG,
                &NSDictionary::new(),
            )
        }?;
        return Some(png.to_vec());
    }
    use objc2::AnyThread;
    let image: Retained<NSImage> = NSImage::initWithData(NSImage::alloc(), data)?;
    let size = image.size();
    if size.width < 1.
        || size.height < 1.
        || size.width > MAX_ICON_SIDE as f64
        || size.height > MAX_ICON_SIDE as f64
    {
        return None;
    }
    png_of(&image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fetch_delegate_answers_the_selectors_that_cap_the_download() {
        let class = FaviconFetch::class();
        for selector in [
            objc2::sel!(URLSession:dataTask:didReceiveResponse:completionHandler:),
            objc2::sel!(URLSession:dataTask:didReceiveData:),
            objc2::sel!(URLSession:task:didCompleteWithError:),
        ] {
            assert!(class.responds_to(selector), "{selector:?}");
        }
    }

    fn link(href: &str, sizes: &str) -> IconLink {
        IconLink {
            href: href.to_owned(),
            sizes: sizes.to_owned(),
        }
    }

    #[test]
    fn the_origin_keys_the_icon_cache_for_web_pages_only() {
        assert_eq!(
            page_origin("https://example.com/a?b=c#d").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            page_origin("http://localhost:8811/page2").as_deref(),
            Some("http://localhost:8811")
        );
        assert_eq!(
            page_origin("HTTP://Example.COM:80/").as_deref(),
            Some("http://example.com")
        );
        for none in [
            "file:///tmp/a.html",
            "about:blank",
            "data:text/html,x",
            "mailto:a@b.c",
            "",
            "pas une adresse",
        ] {
            assert_eq!(page_origin(none), None, "{none}");
        }
    }

    #[test]
    fn the_declared_icons_are_read_from_the_script_json() {
        let json =
            r#"[["https://a.test/i.png","32x32","image/png"],["https://a.test/j.ico","",""]]"#;
        assert_eq!(
            parse_links(json),
            vec![
                link("https://a.test/i.png", "32x32"),
                link("https://a.test/j.ico", "")
            ]
        );
        assert!(parse_links("pas du json").is_empty());
        assert!(parse_links(r#"{"a":1}"#).is_empty());
        assert!(parse_links("[]").is_empty());
    }

    #[test]
    fn a_page_without_declared_icon_falls_back_to_favicon_ico() {
        assert_eq!(
            candidate_urls(&[], "http://127.0.0.1:8811"),
            vec!["http://127.0.0.1:8811/favicon.ico"]
        );
    }

    #[test]
    fn declared_icons_are_ranked_by_size_then_document_order_and_end_with_the_fallback() {
        let links = [
            link("https://a.test/16.png", "16x16"),
            link("https://a.test/512.png", "512x512"),
            link("https://a.test/32.png", "32x32"),
            link("https://a.test/8.png", "8x8"),
        ];
        // 32 (le plus proche du côté visé), puis 512, puis le repli ; les deux autres ne sont pas essayées.
        assert_eq!(
            candidate_urls(&links, "https://a.test"),
            vec![
                "https://a.test/32.png",
                "https://a.test/512.png",
                "https://a.test/favicon.ico"
            ]
        );
        // Sans taille déclarée, l'ordre du document.
        let plain = [
            link("https://a.test/x.ico", ""),
            link("https://a.test/y.png", ""),
        ];
        assert_eq!(
            candidate_urls(&plain, "https://a.test"),
            vec![
                "https://a.test/x.ico",
                "https://a.test/y.png",
                "https://a.test/favicon.ico"
            ]
        );
        // Une icône trop petite passe après une icône d'au moins 16.
        let small = [
            link("https://a.test/8.png", "8x8"),
            link("https://a.test/16.png", "16x16 24x24"),
        ];
        assert_eq!(
            candidate_urls(&small, "https://a.test")[0],
            "https://a.test/16.png"
        );
    }

    #[test]
    fn only_web_and_data_addresses_are_fetched_and_duplicates_collapse() {
        let links = [
            link("javascript:alert(1)", ""),
            link("file:///etc/hosts", ""),
            link("ftp://a.test/i.ico", ""),
            link("DATA:image/png;base64,AAAA", ""),
            link("https://a.test/favicon.ico", ""),
            link("https://a.test/favicon.ico", ""),
        ];
        assert_eq!(
            candidate_urls(&links, "https://a.test"),
            vec!["DATA:image/png;base64,AAAA", "https://a.test/favicon.ico"]
        );
    }
}
