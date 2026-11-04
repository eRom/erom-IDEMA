//! Les vignettes de la galerie : chaque carte montre l'image d'aperçu de son artifact. Un cache mémoire borné (`ThumbCache`,
//! pur, testé sans réseau ni fenêtre) et l'entité `Thumbnails` qui le remplit.
//!
//! Le téléchargement suit celui des favicons (`browser/webkit/favicon.rs`) : `NSURLSession` éphémère, sans cookie ni
//! cache disque (l'adresse est signée, elle se charge seule), délai de 10 s, corps borné. Le décodage se fait par AppKit
//! (PNG, JPEG, WebP), hors du fil d'interface, et réduit l'image à `MAX_WIDTH` pixels : une vignette de 1200 x 630 pèse
//! 3 Mo décodée, 96 d'entre elles en feraient près de 290.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use block2::DynBlock;
use futures::channel::oneshot;
use gpui::{Context, Image, ImageFormat, RenderImage};
use objc2::rc::Retained;
use objc2::{AnyThread, ClassType, DefinedClass, define_class, msg_send};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSCalibratedRGBColorSpace, NSCompositingOperation,
    NSGraphicsContext, NSImage, NSImageInterpolation,
};
use objc2_foundation::{
    NSData, NSDictionary, NSError, NSHTTPURLResponse, NSObject, NSOperationQueue, NSPoint, NSRect,
    NSSize, NSString, NSURL, NSURLResponse, NSURLSession, NSURLSessionConfiguration,
    NSURLSessionDataTask, NSURLSessionResponseDisposition, NSURLSessionTask,
};

use url::Url;

use crate::hud;

/// Combien d'images décodées on garde : une page de la galerie en montre 48, la borne laisse le double.
const CAPACITY: usize = 96;
/// Délai d'une requête de vignette, en secondes (inactivité et durée totale).
const REQUEST_TIMEOUT: f64 = 10.;
/// Au-delà, l'image n'est pas lue : une vignette pèse quelques dizaines de ko, la borne coupe court à une image piégée.
const MAX_BYTES: usize = 4 * 1024 * 1024;
/// Côté maximal, en pixels, d'une image acceptée (avant tout décodage complet).
const MAX_SOURCE_SIDE: isize = 4096;
/// Largeur à laquelle on réduit : une carte est plus étroite que cela, même sur un écran 2x.
const MAX_WIDTH: isize = 600;

/// Où en est la vignette d'une adresse. `I` est l'image décodée (`Arc<RenderImage>` en vrai, un entier dans les tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbState<I = Arc<RenderImage>> {
    Loading,
    Ready(I),
    Failed,
}

enum Slot<I> {
    Loading,
    Ready(I),
    Failed,
}

pub struct ThumbCache<I> {
    slots: HashMap<String, Slot<I>>,
    /// Les adresses dont l'image est prête, la plus ancienne en tête.
    ready_order: VecDeque<String>,
}

impl<I> Default for ThumbCache<I> {
    fn default() -> Self {
        Self {
            slots: HashMap::new(),
            ready_order: VecDeque::new(),
        }
    }
}

impl<I: Clone> ThumbCache<I> {
    /// Ce que le cache sait de cette adresse ; `None` : jamais demandée (ou image sortie du cache).
    pub fn state(&self, url: &str) -> Option<ThumbState<I>> {
        self.slots.get(url).map(|slot| match slot {
            Slot::Loading => ThumbState::Loading,
            Slot::Ready(image) => ThumbState::Ready(image.clone()),
            Slot::Failed => ThumbState::Failed,
        })
    }

    /// Vrai si l'adresse est inconnue : elle passe en chargement et l'appelant lance le téléchargement. Une adresse en
    /// cours, prête ou en échec rend faux : un échec n'est jamais relancé.
    pub fn begin_load(&mut self, url: &str) -> bool {
        if self.slots.contains_key(url) {
            return false;
        }
        self.slots.insert(url.to_owned(), Slot::Loading);
        true
    }

    /// Mémorise l'échec : l'adresse n'est plus demandée tant que le cache vit. Une image déjà prête n'est pas écrasée.
    pub fn mark_failed(&mut self, url: &str) {
        if !matches!(self.slots.get(url), Some(Slot::Ready(_))) {
            self.slots.insert(url.to_owned(), Slot::Failed);
        }
    }

    /// Garde l'image de `url`. Rend les images qui sortent du cache (l'ancienne de cette adresse, ou la plus ancienne
    /// d'un cache plein) : l'appelant les rend à GPUI. Seules les images prêtes comptent dans la borne.
    pub fn insert(&mut self, url: &str, image: I) -> Vec<I> {
        let mut gone: Vec<I> = self.forget_image(url).into_iter().collect();
        self.slots.insert(url.to_owned(), Slot::Ready(image));
        self.ready_order.push_back(url.to_owned());
        while self.ready_order.len() > CAPACITY {
            let Some(oldest) = self.ready_order.pop_front() else {
                break;
            };
            if let Some(Slot::Ready(old)) = self.slots.remove(&oldest) {
                gone.push(old);
            }
        }
        gone
    }

    /// Retire l'image prête de `url`, s'il y en a une, de la table et de l'ordre.
    fn forget_image(&mut self, url: &str) -> Option<I> {
        if !matches!(self.slots.get(url), Some(Slot::Ready(_))) {
            return None;
        }
        self.ready_order.retain(|known| known != url);
        match self.slots.remove(url) {
            Some(Slot::Ready(image)) => Some(image),
            _ => None,
        }
    }
}

/// Les vignettes de la galerie. La vue demande chaque carte par `get` à chaque rendu : la première demande lance le
/// chargement, les suivantes lisent le cache. Un échec (réseau, statut, taille, image illisible) est mémorisé et jamais
/// relancé pendant la vie de l'entité ; la carte garde alors son repli.
#[derive(Default)]
pub struct Thumbnails {
    cache: ThumbCache<Arc<RenderImage>>,
}

impl Thumbnails {
    pub fn get(&mut self, url: &str, cx: &mut Context<Self>) -> ThumbState {
        if self.cache.begin_load(url) {
            self.start_load(url, cx);
            return ThumbState::Loading;
        }
        self.cache.state(url).unwrap_or(ThumbState::Failed)
    }

    fn start_load(&self, url: &str, cx: &mut Context<Self>) {
        let (done, received) = oneshot::channel();
        fetch(url, done);
        let renderer = cx.svg_renderer();
        let url = url.to_owned();
        cx.spawn(async move |this, cx| {
            let body = received.await.ok().flatten();
            // Le décodage et la réduction se font hors du fil d'interface.
            let image = match body {
                Some(body) => {
                    cx.background_executor()
                        .spawn(async move {
                            let png = scaled_png(&NSData::with_bytes(&body))?;
                            Image::from_bytes(ImageFormat::Png, png)
                                .to_image_data(renderer)
                                .ok()
                        })
                        .await
                }
                None => None,
            };
            let _ = this.update(cx, |thumbnails, cx| thumbnails.finished(&url, image, cx));
        })
        .detach();
    }

    fn finished(&mut self, url: &str, image: Option<Arc<RenderImage>>, cx: &mut Context<Self>) {
        match image {
            Some(image) => {
                for old in self.cache.insert(url, image) {
                    cx.drop_image(old, None);
                }
                hud::log_debug("artifacts thumbnail ready");
            }
            None => {
                self.cache.mark_failed(url);
                hud::log_debug("artifacts thumbnail failed");
            }
        }
        cx.notify();
    }
}

/// Le domaine qui sert les vignettes signées de claude.ai.
const THUMBNAIL_HOST_SUFFIX: &str = ".frame.claudeusercontent.com";

/// Vrai pour une adresse `https` dont l'hôte est un sous-domaine de `THUMBNAIL_HOST_SUFFIX`. Le modèle ne fait que
/// relayer ce que l'API rend : tout autre hôte n'est jamais contacté.
fn is_thumbnail_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed
            .host_str()
            .is_some_and(|host| host.to_ascii_lowercase().ends_with(THUMBNAIL_HOST_SUFFIX))
}

/// Télécharge `url` et rend le corps par `done` (`None` : échec). Rappelé depuis le fil principal. L'adresse est signée
/// et sert hors du site : seules les adresses de `is_thumbnail_url` sont suivies, et jamais avec des cookies (session
/// éphémère).
fn fetch(url: &str, done: oneshot::Sender<Option<Vec<u8>>>) {
    let target = if is_thumbnail_url(url) {
        NSURL::URLWithString(&NSString::from_str(url))
    } else {
        None
    };
    let Some(target) = target else {
        hud::log_debug("artifacts thumbnail refused, not a thumbnail address");
        let _ = done.send(None);
        return;
    };
    // Le délai de ressource borne la durée totale (celui de requête n'est qu'un délai d'inactivité : un flux sans fin ne
    // l'atteint jamais). Les rappels du délégué arrivent sur la file principale.
    let configuration = NSURLSessionConfiguration::ephemeralSessionConfiguration();
    configuration.setTimeoutIntervalForRequest(REQUEST_TIMEOUT);
    configuration.setTimeoutIntervalForResource(REQUEST_TIMEOUT);
    let delegate = ThumbFetch::new(done);
    // SAFETY: `delegate` est un objet NSObject qui répond aux sélecteurs `NSURLSessionDataDelegate` qu'il déclare (la
    // session teste `respondsToSelector:`) ; la file principale est celle où ils s'exécutent. La session garde le
    // délégué jusqu'à son invalidation.
    let session: Retained<NSURLSession> = unsafe {
        msg_send![
            NSURLSession::class(),
            sessionWithConfiguration: &*configuration,
            delegate: &*delegate,
            delegateQueue: &*NSOperationQueue::mainQueue(),
        ]
    };
    let task = session.dataTaskWithURL(&target);
    task.resume();
    session.finishTasksAndInvalidate();
}

/// L'état d'un téléchargement de vignette : le corps reçu, et où le rendre.
struct ThumbFetchIvars {
    done: RefCell<Option<oneshot::Sender<Option<Vec<u8>>>>>,
    body: RefCell<Vec<u8>>,
    /// La réponse est refusée (statut, taille annoncée ou reçue au-delà de la borne) : le téléchargement est annulé.
    rejected: Cell<bool>,
}

define_class!(
    /// Le délégué d'un téléchargement de vignette. Il reçoit le corps par morceaux et coupe le téléchargement dès qu'il
    /// passe `MAX_BYTES` : un serveur qui diffuse sans fin ne remplit ni la mémoire ni le réseau.
    #[unsafe(super(NSObject))]
    #[name = "IdemaThumbFetch"]
    #[ivars = ThumbFetchIvars]
    struct ThumbFetch;

    // SAFETY: signatures des sélecteurs de `NSURLSessionDataDelegate` et `NSURLSessionTaskDelegate` ; objc2 les vérifie à
    // l'enregistrement en débogage. Les rappels viennent de la file principale (`delegateQueue`).
    impl ThumbFetch {
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
                .is_ok_and(|length| length > MAX_BYTES);
            if announced_too_big || !is_success(response) {
                self.ivars().rejected.set(true);
                hud::log_debug("artifacts thumbnail refused");
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
            if body.len() > MAX_BYTES {
                // La mémoire reste bornée : au plus la borne et un morceau.
                body.clear();
                ivars.rejected.set(true);
                task.cancel();
                hud::log_event("artifacts thumbnail aborted over the size cap");
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
            let body = if error.is_some() || ivars.rejected.get() {
                None
            } else {
                Some(ivars.body.take())
            };
            if let Some(done) = ivars.done.take() {
                let _ = done.send(body);
            }
        }
    }
);

impl ThumbFetch {
    fn new(done: oneshot::Sender<Option<Vec<u8>>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ThumbFetchIvars {
            done: RefCell::new(Some(done)),
            body: RefCell::new(Vec::new()),
            rejected: Cell::new(false),
        });
        // SAFETY: initialiseur désigné de `NSObject`, appelé une fois sur un objet alloué.
        unsafe { msg_send![super(this), init] }
    }
}

/// Une réponse HTTP 2xx.
fn is_success(response: &NSURLResponse) -> bool {
    response
        .downcast_ref::<NSHTTPURLResponse>()
        .is_none_or(|http| (200..300).contains(&http.statusCode()))
}

/// L'image en PNG, réduite à `MAX_WIDTH` de large si elle l'excède (jamais agrandie). Une image qui n'est pas un bitmap
/// lisible, ou dont un côté dépasse `MAX_SOURCE_SIDE`, ne donne rien.
fn scaled_png(data: &NSData) -> Option<Vec<u8>> {
    let representations = NSBitmapImageRep::imageRepsWithData(data);
    let bitmap = representations
        .iter()
        .filter_map(|rep| rep.downcast::<NSBitmapImageRep>().ok())
        .max_by_key(|rep| rep.pixelsWide())?;
    let (width, height) = (bitmap.pixelsWide(), bitmap.pixelsHigh());
    if !(1..=MAX_SOURCE_SIDE).contains(&width) || !(1..=MAX_SOURCE_SIDE).contains(&height) {
        return None;
    }
    if width <= MAX_WIDTH {
        return png_of_rep(&bitmap);
    }
    let target_height = ((height * MAX_WIDTH + width / 2) / width).max(1);
    let source = NSImage::initWithData(NSImage::alloc(), data)?;
    // Le bitmap d'arrivée a ses dimensions en pixels fixées ici : le résultat ne dépend d'aucun écran (une `NSImage`
    // dynamique prendrait l'échelle de l'écran courant).
    let target = new_bitmap(MAX_WIDTH, target_height)?;
    let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&target)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    context.setImageInterpolation(NSImageInterpolation::High);
    // `NSZeroRect` pour la source = l'image entière.
    source.drawInRect_fromRect_operation_fraction(
        NSRect::new(
            NSPoint::new(0., 0.),
            NSSize::new(MAX_WIDTH as f64, target_height as f64),
        ),
        NSRect::ZERO,
        NSCompositingOperation::Copy,
        1.,
    );
    context.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();
    png_of_rep(&target)
}

/// Un bitmap RGBA 8 bits de `width` x `height` pixels, dont AppKit alloue le tampon.
fn new_bitmap(width: isize, height: isize) -> Option<Retained<NSBitmapImageRep>> {
    // SAFETY: plans nuls = AppKit alloue le tampon ; paramètres d'un bitmap RGBA 8 bits entrelacé.
    unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            width,
            height,
            8,
            4,
            true,
            false,
            NSCalibratedRGBColorSpace,
            0,
            0,
        )
    }
}

fn png_of_rep(rep: &NSBitmapImageRep) -> Option<Vec<u8>> {
    // SAFETY: dictionnaire de propriétés vide, type PNG supporté.
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(png.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_thumbnail_starts_loading_once() {
        let mut cache: ThumbCache<i32> = ThumbCache::default();
        assert_eq!(cache.state("https://a.test/1"), None);
        assert!(cache.begin_load("https://a.test/1"));
        assert_eq!(cache.state("https://a.test/1"), Some(ThumbState::Loading));
        assert!(!cache.begin_load("https://a.test/1"));
    }

    #[test]
    fn failed_thumbnail_is_not_retried() {
        let mut cache: ThumbCache<i32> = ThumbCache::default();
        assert!(cache.begin_load("https://a.test/1"));
        cache.mark_failed("https://a.test/1");
        assert_eq!(cache.state("https://a.test/1"), Some(ThumbState::Failed));
        assert!(!cache.begin_load("https://a.test/1"));
        assert_eq!(cache.state("https://a.test/1"), Some(ThumbState::Failed));
    }

    #[test]
    fn a_ready_thumbnail_is_served_without_a_new_load() {
        let mut cache = ThumbCache::default();
        assert!(cache.begin_load("https://a.test/1"));
        assert!(cache.insert("https://a.test/1", 7).is_empty());
        assert_eq!(cache.state("https://a.test/1"), Some(ThumbState::Ready(7)));
        assert!(!cache.begin_load("https://a.test/1"));
    }

    #[test]
    fn cache_evicts_oldest_beyond_capacity() {
        let mut cache = ThumbCache::default();
        for n in 0..CAPACITY as i32 {
            assert!(cache.insert(&format!("https://a.test/{n}"), n).is_empty());
        }
        // Le 97e entré fait sortir le plus ancien, et lui seul.
        assert_eq!(cache.insert("https://a.test/new", 1000), vec![0]);
        assert_eq!(cache.state("https://a.test/0"), None);
        assert_eq!(cache.state("https://a.test/1"), Some(ThumbState::Ready(1)));
        assert_eq!(
            cache.state("https://a.test/new"),
            Some(ThumbState::Ready(1000))
        );
    }

    #[test]
    fn an_evicted_thumbnail_can_be_loaded_again() {
        let mut cache = ThumbCache::default();
        for n in 0..=CAPACITY as i32 {
            cache.insert(&format!("https://a.test/{n}"), n);
        }
        assert!(cache.begin_load("https://a.test/0"));
    }

    #[test]
    fn failures_and_loads_in_flight_do_not_count_against_the_capacity() {
        let mut cache = ThumbCache::default();
        for n in 0..CAPACITY as i32 {
            cache.insert(&format!("https://a.test/{n}"), n);
        }
        for n in 0..10 {
            cache.begin_load(&format!("https://a.test/loading-{n}"));
            cache.mark_failed(&format!("https://a.test/failed-{n}"));
        }
        assert!(cache.insert("https://a.test/last", 500).len() == 1);
        assert_eq!(
            cache.state("https://a.test/failed-3"),
            Some(ThumbState::Failed)
        );
    }

    #[test]
    fn a_new_image_for_a_known_address_returns_the_old_one_to_release() {
        let mut cache = ThumbCache::default();
        cache.insert("https://a.test/1", 1);
        assert_eq!(cache.insert("https://a.test/1", 2), vec![1]);
        assert_eq!(cache.state("https://a.test/1"), Some(ThumbState::Ready(2)));
    }

    /// Un PNG de `width` x `height` pixels, entièrement blanc et opaque, fabriqué par AppKit.
    fn white_png(width: isize, height: isize) -> Vec<u8> {
        let rep = new_bitmap(width, height).expect("bitmap");
        // SAFETY: le tampon alloué par AppKit fait `bytesPerRow * height` octets ; on le remplit en entier.
        unsafe {
            std::ptr::write_bytes(
                rep.bitmapData(),
                0xFF,
                (rep.bytesPerRow() * height) as usize,
            );
        }
        png_of_rep(&rep).expect("png")
    }

    fn pixel_size(png: &[u8]) -> (isize, isize) {
        let rep = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(png)).expect("decodable");
        (rep.pixelsWide(), rep.pixelsHigh())
    }

    #[test]
    fn a_wide_thumbnail_is_scaled_down_keeping_its_ratio() {
        let source = white_png(1200, 630);
        let scaled = scaled_png(&NSData::with_bytes(&source)).expect("scaled");
        assert_eq!(pixel_size(&scaled), (600, 315));
    }

    #[test]
    fn the_scaled_width_is_exactly_the_cap_whatever_the_source_ratio() {
        for (width, height) in [(1201, 631), (4096, 100), (601, 4096)] {
            let source = white_png(width, height);
            let scaled = scaled_png(&NSData::with_bytes(&source)).expect("scaled");
            assert_eq!(pixel_size(&scaled).0, 600, "{width}x{height}");
        }
    }

    #[test]
    fn scaling_keeps_the_pixels_of_the_source() {
        let source = white_png(1200, 630);
        let scaled = scaled_png(&NSData::with_bytes(&source)).expect("scaled");
        let rep = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(&scaled)).expect("png");
        assert!(rep.samplesPerPixel() >= 3);
        // SAFETY: le tampon d'un bitmap décodé compte au moins un pixel de `samplesPerPixel` octets.
        let first = unsafe { std::slice::from_raw_parts(rep.bitmapData(), 3) };
        assert_eq!(first, [0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn a_small_thumbnail_is_never_enlarged() {
        let source = white_png(300, 200);
        let scaled = scaled_png(&NSData::with_bytes(&source)).expect("kept");
        assert_eq!(pixel_size(&scaled), (300, 200));
    }

    #[test]
    fn bytes_that_are_not_an_image_give_nothing() {
        assert!(scaled_png(&NSData::with_bytes(b"<html>not an image</html>")).is_none());
        assert!(scaled_png(&NSData::with_bytes(&[])).is_none());
    }

    #[test]
    fn an_oversized_source_is_refused_before_scaling() {
        let source = white_png(MAX_SOURCE_SIDE + 1, 8);
        assert!(scaled_png(&NSData::with_bytes(&source)).is_none());
    }

    #[test]
    fn only_https_addresses_of_the_thumbnail_domain_are_fetched() {
        for accepted in [
            "https://abc123.frame.claudeusercontent.com/x/_thumb.img?__frame_t=tok",
            "HTTPS://A.Frame.ClaudeUserContent.com/x_thumb.img",
        ] {
            assert!(is_thumbnail_url(accepted), "{accepted}");
        }
        for refused in [
            "http://abc.frame.claudeusercontent.com/t.img",
            "https://frame.claudeusercontent.com/t.img",
            "https://evil.test/t.img",
            "https://abc.frame.claudeusercontent.com.evil.test/t.img",
            "https://evil.test/?u=.frame.claudeusercontent.com",
            "https://abc.frame.claudeusercontent.com@evil.test/t.img",
            "https://abcframe.claudeusercontent.com/t.img",
            "file:///etc/hosts",
            "data:image/png;base64,AAAA",
            "pas une adresse",
            "",
        ] {
            assert!(!is_thumbnail_url(refused), "{refused}");
        }
    }

    #[test]
    fn a_refused_address_fails_without_a_request() {
        for refused in [
            "http://abc.frame.claudeusercontent.com/t.img",
            "https://evil.test/t.img",
            "pas une adresse",
        ] {
            let (done, mut received) = oneshot::channel();
            fetch(refused, done);
            assert_eq!(received.try_recv(), Ok(Some(None)), "{refused}");
        }
    }

    #[test]
    fn the_fetch_delegate_answers_the_selectors_that_cap_the_download() {
        let class = ThumbFetch::class();
        for selector in [
            objc2::sel!(URLSession:dataTask:didReceiveResponse:completionHandler:),
            objc2::sel!(URLSession:dataTask:didReceiveData:),
            objc2::sel!(URLSession:task:didCompleteWithError:),
        ] {
            assert!(class.responds_to(selector), "{selector:?}");
        }
    }
}
