//! La couche WebKit du navigateur (P4) : le seul code qui parle à WebKit. Une `WebPage` par onglet web, sous-vue de la
//! `GPUIView` (E-P4-a, confirmé par la sonde), avec ses délégués et son observateur KVO ; tout ce qui s'y passe
//! revient à l'onglet en `PageEvent` sur un canal. Les pages des onglets n'injectent aucun script utilisateur et n'ont
//! aucun pont JS : leur seule exécution de script est la lecture d'une passe des `<link rel="icon">` après une
//! navigation, dans le monde de contenu du client (`favicon`), isolé de ceux de la page. La seule exception est la page
//! cachée de la galerie Artifacts (`new_with_script`, `call_async`), qui injecte un script au début du document et exécute
//! du JS dans le monde de la page. Rien ici ne change l'agent utilisateur ni n'active l'application.

mod classes;
mod downloads;
mod favicon;
mod popup;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use block2::RcBlock;
use futures::channel::mpsc::UnboundedSender;
use gpui::{Bounds, Pixels, Size, Window, px};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, MainThreadMarker, msg_send, sel};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSEventModifierFlags, NSImage, NSView,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSHTTPCookie, NSNumber, NSPoint, NSRect, NSSize, NSString,
    NSURL, NSURLRequest, NSUUID,
};
use objc2_web_kit::{
    WKContentWorld, WKFindConfiguration, WKFindResult, WKUserScript, WKUserScriptInjectionTime,
    WKWebView, WKWebViewConfiguration, WKWebsiteDataStore,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use self::classes::{IdemaWebView, PageDelegate, PageObserver};
pub use self::favicon::page_origin;
use crate::hud;

/// Identifiant du profil persistant : un seul magasin (cookies, stockage) pour tous les onglets web, d'un lancement
/// à l'autre. Une constante, jamais un identifiant par onglet.
pub const PROFILE_ID: &str = "8D2B6A4E-3C1F-4E7A-9B5D-1F0E2C3A4B5C";

/// L'identifiant du paquet (`packaging/Info.plist`) : le nom du dossier de cache d'IdemA.
const BUNDLE_ID: &str = "com.erom.idema";

/// `NSURLErrorBadURL` : le code d'un `Failed` pour une adresse qu'AppKit ne sait pas lire.
const INVALID_ADDRESS_CODE: isize = -1000;

/// Ce qu'une page rapporte à son onglet. Consommé sur le fil principal, hors de tout `update` GPUI ; la fin d'un
/// téléchargement (`DownloadFinished`, `DownloadFailed`) est émise depuis un fil de travail, le canal la remet au fil
/// principal.
#[derive(Debug, Clone, PartialEq)]
pub enum PageEvent {
    Title(Option<String>),
    Url(Option<String>),
    Loading {
        loading: bool,
        progress: f64,
    },
    History {
        back: bool,
        forward: bool,
    },
    /// Une navigation a commencé à recevoir du contenu (adresse brute, avec requête et fragment).
    Committed(String),
    Finished(String),
    Failed {
        code: isize,
        host: Option<String>,
        /// L'adresse qui a échoué (brute), quand l'erreur la porte : le `KVO` de `URL` a pu laisser l'onglet sur la
        /// page d'avant.
        url: Option<String>,
        description: String,
    },
    /// Le processus WebContent s'est arrêté.
    Terminated,
    /// Un lien `target=_blank` ou un `window.open` sans taille : un nouvel onglet.
    OpenTab(String),
    PopupOpened(String),
    PopupClosed,
    DownloadFinished(PathBuf),
    DownloadFailed(String),
    /// Le favicon de la page d'`origin` (PNG), lu une fois la navigation finie ; l'onglet le peint à la place du globe.
    Favicon {
        origin: String,
        png: Vec<u8>,
    },
    /// `⌘←` ou `⌘→` que la page n'a pas pris (aucun champ éditable ne l'a gardé) : l'onglet recule ou avance.
    HistoryKey(HistoryKey),
    /// Un clic est tombé dans la page : le clavier est à elle, le focus GPUI doit revenir au volet.
    FocusedByClick,
}

/// Les seules commandes d'édition qu'IdemA envoie lui-même à la page. Copier, couper, coller et tout sélectionner
/// lui parviennent par le menu « Édition » (sélecteurs sans cible, rapport de sonde §3) ; annuler et rétablir n'ont
/// pas d'entrée qui parte à la page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditCommand {
    Undo,
    Redo,
}

/// Les boîtes de dialogue qu'une page peut demander.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    Alert,
    Confirm,
    Prompt,
    FilePicker,
}

impl DialogKind {
    /// Le mot du journal.
    pub fn label(self) -> &'static str {
        match self {
            Self::Alert => "alert",
            Self::Confirm => "confirm",
            Self::Prompt => "prompt",
            Self::FilePicker => "file",
        }
    }
}

/// La réponse automatique à une boîte que personne ne peut voir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoAnswer {
    /// OK : seule `alert()` n'a pas de refus à donner.
    Accept,
    /// Annuler : `confirm()` rend faux, `prompt()` rend `null`, le sélecteur de fichiers rend rien. Une page cachée ne
    /// reçoit jamais un « oui » silencieux.
    Decline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogAction {
    /// La feuille s'affiche sur la fenêtre.
    Sheet,
    /// La page est répondue sur-le-champ, sans feuille.
    Answer(AutoAnswer),
}

/// Une page qui n'est pas à l'écran (volet fermé, ou onglet qui n'est pas l'actif) n'ouvre aucune feuille : personne ne la
/// verrait, et la fenêtre resterait bloquée sur un dialogue invisible.
pub fn dialog_action(kind: DialogKind, on_screen: bool) -> DialogAction {
    match (on_screen, kind) {
        (true, _) => DialogAction::Sheet,
        (false, DialogKind::Alert) => DialogAction::Answer(AutoAnswer::Accept),
        (false, _) => DialogAction::Answer(AutoAnswer::Decline),
    }
}

/// `⌘←` et `⌘→` : reculer et avancer dans l'historique de l'onglet, quand la page ne les a pas prises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryKey {
    Back,
    Forward,
}

/// `⌘` est-il le seul des quatre modificateurs de texte (`⌘`, `⇧`, `⌥`, `⌃`) ? Les flèches portent aussi `Fonction` et
/// `PavéNumérique`, et `Verr. Maj` s'ajoute quand il est allumé : tous dans le masque des modificateurs indépendants du
/// clavier, donc comparer ce masque entier à `⌘` seul ne reconnaîtrait jamais une vraie flèche.
pub fn is_command_only(flags: NSEventModifierFlags) -> bool {
    let text_modifiers = NSEventModifierFlags::Command
        | NSEventModifierFlags::Shift
        | NSEventModifierFlags::Option
        | NSEventModifierFlags::Control;
    flags.intersection(text_modifiers) == NSEventModifierFlags::Command
}

/// La touche d'historique que désigne un appui : `⌘←` ou `⌘→` seuls (`command_only` : aucun autre modificateur, `⇧`, `⌥`
/// et `⌃` appartiennent à l'édition de texte). Les codes sont ceux, physiques, des flèches gauche et droite.
pub fn history_key(key_code: u16, command_only: bool) -> Option<HistoryKey> {
    match (key_code, command_only) {
        (123, true) => Some(HistoryKey::Back),
        (124, true) => Some(HistoryKey::Forward),
        _ => None,
    }
}

/// Reconnaît le renvoi d'un appui de touche que la page n'a pas pris. WebKit juge une frappe dans son processus de
/// contenu, puis, si elle n'est pas prise, la renvoie à l'application (`[NSApp sendEvent:]`), et `keyDown:` revoit alors le
/// même événement : le même code et la même date. Une frappe vue deux fois de suite est donc celle que la page a laissée ;
/// vue une seule fois, la page l'a prise (le curseur d'un champ éditable bouge) ou elle est en vol.
#[derive(Debug, Default)]
pub struct ResendGuard {
    seen: Option<(u16, u64)>,
}

impl ResendGuard {
    /// `true` quand `(key_code, timestamp)` est le renvoi de la dernière frappe vue.
    pub fn is_resend(&mut self, key_code: u16, timestamp: f64) -> bool {
        let key = (key_code, timestamp.to_bits());
        if self.seen == Some(key) {
            self.seen = None;
            true
        } else {
            self.seen = Some(key);
            false
        }
    }
}

/// Le filet d'un point que macOS trace au bord de la fenêtre : une vue native posée sur ce bord le recouvre, une page
/// claire le fait disparaître sur les deux côtés qu'elle touche (droite et bas).
const WINDOW_EDGE: f32 = 1.;

/// `bounds` sans le filet de la fenêtre : un côté qui touche le bord de la fenêtre (`viewport`) recule d'un point, pour
/// que la page ne recouvre pas le contour.
pub fn inset_from_window_edge(bounds: Bounds<Pixels>, viewport: Size<Pixels>) -> Bounds<Pixels> {
    let mut inset = bounds;
    let right = f32::from(bounds.origin.x) + f32::from(bounds.size.width);
    let bottom = f32::from(bounds.origin.y) + f32::from(bounds.size.height);
    if right >= f32::from(viewport.width) - 0.5 {
        inset.size.width = px((f32::from(bounds.size.width) - WINDOW_EDGE).max(0.));
    }
    if bottom >= f32::from(viewport.height) - 0.5 {
        inset.size.height = px((f32::from(bounds.size.height) - WINDOW_EDGE).max(0.));
    }
    inset
}

/// Le cadre AppKit `(x, y, largeur, hauteur)` d'un élément GPUI. La `GPUIView` n'est pas retournée : l'axe des y
/// part du bas, donc `y_appkit = hauteur_parent - y - hauteur`.
pub fn appkit_frame(bounds: Bounds<Pixels>, parent_height: Pixels) -> (f64, f64, f64, f64) {
    let x = f64::from(f32::from(bounds.origin.x));
    let y = f64::from(f32::from(bounds.origin.y));
    let width = f64::from(f32::from(bounds.size.width));
    let height = f64::from(f32::from(bounds.size.height));
    (
        x,
        f64::from(f32::from(parent_height)) - y - height,
        width,
        height,
    )
}

// ---------------------------------------------------------------------------------------------------------------------
// La partie pure : décisions de téléchargement, erreurs à taire, tailles de popup, résumé des cookies.

/// Un document que la page ne sait pas montrer, ou que le serveur donne en pièce jointe, devient un téléchargement.
pub(super) fn should_download(can_show_mime_type: bool, content_disposition: Option<&str>) -> bool {
    let attachment = content_disposition.is_some_and(|value| {
        value
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("attachment")
    });
    attachment || !can_show_mime_type
}

/// Ce que devient la réponse d'une navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResponseAction {
    Allow,
    Download,
    /// La réponse d'un sous-cadre qui se téléchargerait : un `iframe` caché n'écrit pas dans `~/Downloads`.
    Cancel,
}

/// Seule la réponse du cadre principal devient un téléchargement ; celle d'un sous-cadre qui en serait un est annulée.
pub(super) fn response_action(
    is_main_frame: bool,
    can_show_mime_type: bool,
    content_disposition: Option<&str>,
) -> ResponseAction {
    match (
        should_download(can_show_mime_type, content_disposition),
        is_main_frame,
    ) {
        (false, _) => ResponseAction::Allow,
        (true, true) => ResponseAction::Download,
        (true, false) => ResponseAction::Cancel,
    }
}

/// Les échecs qui ne sont pas des échecs : une navigation annulée par la suivante (`NSURLErrorCancelled`) et un
/// chargement interrompu par une décision de politique (un téléchargement qui prend le relais).
pub(super) fn is_ignored_load_error(domain: &str, code: isize) -> bool {
    matches!(
        (domain, code),
        ("NSURLErrorDomain", -999) | ("WebKitErrorDomain", 102)
    )
}

/// Le schéma d'une adresse (en minuscules), `None` quand elle ne se lit pas.
pub(super) fn scheme_of_url(url: &str) -> Option<String> {
    url::Url::parse(url).ok().map(|url| url.scheme().to_owned())
}

/// Une page peut demander un onglet (`target=_blank`, `window.open`) pour une adresse web seulement : un `file:` reste
/// réservé aux gestes d'IdemA (⌘-clic, barre d'adresse, favori), sans quoi une page lirait un fichier local par
/// `load_file`, et tout autre schéma (`mailto:`, `javascript:`) ne laisserait qu'un onglet mort.
pub(super) fn page_may_open_tab(url: &str) -> bool {
    matches!(scheme_of_url(url).as_deref(), Some("http" | "https"))
}

/// Une popup (`window.open` avec une taille) suit la même règle que l'onglet, plus deux cas que WebKit rend sans
/// adresse à charger : l'adresse vide et `about:blank` (la page écrit ensuite dans la fenêtre). Un `file:` n'est pas
/// refusé par WebKit pour une popup : le délégué est appelé quand même.
pub(super) fn page_may_open_popup(url: &str) -> bool {
    url.is_empty() || url == "about:blank" || page_may_open_tab(url)
}

/// L'hôte d'une adresse, `None` pour ce qui n'en a pas (`file:`, `about:`, texte).
pub(super) fn host_of_url(url: &str) -> Option<String> {
    url::Url::parse(url).ok()?.host_str().map(str::to_owned)
}

// Bornes de notre choix, sans source dans la spec ni dans un artefact observé. En dessous de 200 points une fenêtre
// n'est plus utilisable ; 1600 empêche une page de demander une fenêtre démesurée tout en laissant tenir, sur un
// écran de MacBook Pro 16" (1728 points de large), une popup de paiement ou de connexion très large.
const POPUP_MIN_SIDE: f64 = 200.;
const POPUP_MAX_SIDE: f64 = 1600.;

/// La taille d'une popup : celle que la page demande, ramenée à une fenêtre utilisable (ni minuscule, ni plus
/// grande qu'un écran).
pub(super) fn popup_size(width: f64, height: f64) -> (f64, f64) {
    let side = |value: f64, fallback: f64| {
        if value.is_finite() {
            value.clamp(POPUP_MIN_SIDE, POPUP_MAX_SIDE)
        } else {
            fallback
        }
    };
    (side(width, 520.), side(height, 680.))
}

/// Le zoom de la page va de 50 % à 300 %, par pas de 10 %.
pub const ZOOM_MIN: f64 = 0.5;
pub const ZOOM_MAX: f64 = 3.0;

/// Un geste de zoom : `⌘=`, `⌘-`, `⌘0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomStep {
    In,
    Out,
    Reset,
}

/// Le zoom après un geste. Le calcul se fait en dixièmes entiers : dix pas ne dérivent pas (0,1 n'est pas exact en
/// virgule flottante), et un zoom hors de la grille y retombe au premier pas.
pub fn stepped_zoom(current: f64, step: ZoomStep) -> f64 {
    let tenths = match step {
        ZoomStep::Reset => 10,
        ZoomStep::In => (current * 10.).round() as i64 + 1,
        ZoomStep::Out => (current * 10.).round() as i64 - 1,
    };
    let bounds = (
        (ZOOM_MIN * 10.).round() as i64,
        (ZOOM_MAX * 10.).round() as i64,
    );
    tenths.clamp(bounds.0, bounds.1) as f64 / 10.
}

/// Un nom de fichier sûr à partir de celui que propose le serveur : jamais un chemin, jamais vide.
pub(super) fn safe_file_name(suggested: &str) -> String {
    let name = suggested
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    if name.is_empty() || name == "." || name == ".." {
        "download".to_owned()
    } else {
        name.to_owned()
    }
}

/// Où poser un téléchargement : `<dossier>/<nom>`, puis `<nom> (2).<ext>`, `(3)`… tant que le fichier existe ou
/// qu'un téléchargement en cours (`in_flight`, pas encore sur le disque) l'a déjà pris.
pub(super) fn unique_destination(
    directory: &Path,
    suggested: &str,
    in_flight: &[PathBuf],
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    let taken = |path: &Path| exists(path) || in_flight.iter().any(|known| known == path);
    let name = safe_file_name(suggested);
    let first = directory.join(&name);
    if !taken(&first) {
        return first;
    }
    let path = Path::new(&name);
    let stem = path
        .file_stem()
        .map_or_else(|| name.clone(), |stem| stem.to_string_lossy().into_owned());
    let extension = path
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| directory.join(format!("{stem} ({n}){extension}")))
        .find(|candidate| !taken(candidate))
        .expect("une suite infinie finit par trouver un nom libre")
}

/// `~/Downloads`, ou rien quand `HOME` n'est pas posé.
pub(super) fn downloads_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads"))
}

/// `browser cookies total=<n> <domaine>=<n> …`, domaines triés, jamais une valeur ni un nom de cookie.
fn cookie_summary(domains: impl IntoIterator<Item = String>) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0;
    for domain in domains {
        total += 1;
        *counts.entry(domain).or_default() += 1;
    }
    let mut line = format!("browser cookies total={total}");
    for (domain, count) in counts {
        line.push_str(&format!(" {domain}={count}"));
    }
    line
}

/// Allume `developerExtrasEnabled` (clé privée de `WKPreferences`, par KVC) : sans elle `setInspectable` ne suffit pas
/// et `_inspector show` n'ouvre aucune fenêtre (Wry/Tauri posent la même clé avant d'ouvrir l'inspecteur). À poser sur
/// la configuration avant qu'une vue n'en naisse ; une popup reçoit celle de WebKit, qui la dérive de la page d'origine.
fn enable_developer_extras(configuration: &WKWebViewConfiguration) {
    let yes = NSNumber::numberWithBool(true);
    let key = NSString::from_str("developerExtrasEnabled");
    // SAFETY: `setValue:forKey:` est le KVC public de `NSObject` ; une clé que WebKit ne connaîtrait plus lève
    // `NSUndefinedKeyException`, d'où la vérification préalable de la clé.
    unsafe {
        let preferences = configuration.preferences();
        // KVC cherche `setKey:` puis `_setKey:` : WebKit expose celui-ci sous sa forme privée.
        let class = preferences.class();
        if class.responds_to(sel!(_setDeveloperExtrasEnabled:))
            || class.responds_to(sel!(setDeveloperExtrasEnabled:))
        {
            let _: () = msg_send![&*preferences, setValue: &*yes, forKey: &*key];
        } else {
            hud::log_debug("browser developer extras key unavailable");
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Le profil.

fn profile_store(mtm: MainThreadMarker) -> Option<Retained<WKWebsiteDataStore>> {
    let uuid = NSUUID::initWithUUIDString(NSUUID::alloc(), &NSString::from_str(PROFILE_ID))?;
    // SAFETY: fil principal (`mtm`), identifiant UUID valide ; WebKit rend le magasin persistant de cet identifiant.
    Some(unsafe { WKWebsiteDataStore::dataStoreForIdentifier(&uuid, mtm) })
}

/// À la première ouverture du volet navigateur, une fois par lancement : retire du dossier de dépôt les téléchargements
/// qu'un IdemA arrêté en route y a laissés. Sur un fil à part, le fil principal n'attend jamais le disque.
pub fn sweep_orphaned_downloads() {
    static SWEPT: AtomicBool = AtomicBool::new(false);
    if SWEPT.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(root) = downloads::staging_root() else {
        return;
    };
    std::thread::spawn(move || downloads::sweep_orphaned_staging(&root, downloads::ORPHAN_MIN_AGE));
}

/// Journalise une fois par lancement les cookies du profil, par domaine (`log_debug`, jamais une valeur).
pub fn log_profile_cookies() {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !hud::debug_enabled() {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(store) = profile_store(mtm) else {
        return;
    };
    let block = RcBlock::new(move |cookies: NonNull<NSArray<NSHTTPCookie>>| {
        // SAFETY: WebKit passe un tableau valide pour la durée de l'appel.
        let cookies = unsafe { cookies.as_ref() };
        let domains = cookies.iter().map(|cookie| cookie.domain().to_string());
        hud::log_debug(&cookie_summary(domains));
    });
    // SAFETY: fil principal ; le bloc est copié par WebKit et ne capture rien.
    unsafe { store.httpCookieStore().getAllCookies(&block) };
}

// ---------------------------------------------------------------------------------------------------------------------
// La page.

/// Une page web : une `WKWebView` sous-vue de la `GPUIView`, ses délégués et son observateur. Fil principal seulement
/// (elle n'est pas `Send`).
pub struct WebPage {
    // Ordre des champs = ordre de libération après `Drop::drop` : l'observateur d'abord (il défait le KVO tant que la
    // vue vit), puis les délégués, puis la vue.
    _observer: Retained<PageObserver>,
    delegate: Retained<PageDelegate>,
    webview: Retained<IdemaWebView>,
    parent: Retained<NSView>,
    /// Numéro de la dernière demande d'instantané : un instantané d'un numéro passé n'est jamais livré.
    snapshot_generation: Rc<Cell<u64>>,
    /// Le zoom de la page, gardé le temps de l'onglet : WebKit ne le rend pas, et il n'est pas sauvegardé.
    zoom: Cell<f64>,
    events: UnboundedSender<PageEvent>,
}

impl WebPage {
    /// Crée la vue, cachée et de cadre nul, sous la `GPUIView` de `window`. `None` sans poignée AppKit (hors macOS,
    /// ou fenêtre sans vue) ou hors du fil principal. La première création coûte un demi-seconde (WebKit froid).
    pub fn new(window: &Window, events: UnboundedSender<PageEvent>) -> Option<WebPage> {
        Self::build(window, events, None)
    }

    /// Comme `new`, avec `document_start_script` injecté au début de chaque document du cadre principal, dans le monde
    /// de la page (il voit et peut envelopper les API de la page, `fetch` compris). Les sous-cadres n'en reçoivent pas.
    pub fn new_with_script(
        window: &Window,
        events: UnboundedSender<PageEvent>,
        document_start_script: &str,
    ) -> Option<WebPage> {
        Self::build(window, events, Some(document_start_script))
    }

    fn build(
        window: &Window,
        events: UnboundedSender<PageEvent>,
        document_start_script: Option<&str>,
    ) -> Option<WebPage> {
        let mtm = MainThreadMarker::new()?;
        let RawWindowHandle::AppKit(handle) = HasWindowHandle::window_handle(window).ok()?.as_raw()
        else {
            return None;
        };
        // SAFETY: GPUI garantit que `ns_view` est la `GPUIView` vivante de la fenêtre ; on en prend une référence forte.
        let parent: Retained<NSView> =
            unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }?;

        // SAFETY: fil principal (`mtm`) ; configuration neuve, magasin persistant du profil.
        let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
        let store = profile_store(mtm)?;
        // SAFETY: la configuration n'est pas encore utilisée par une vue ; la préférence ouvre `window.open` sans geste.
        unsafe {
            configuration.setWebsiteDataStore(&store);
            configuration
                .preferences()
                .setJavaScriptCanOpenWindowsAutomatically(true);
        }
        enable_developer_extras(&configuration);
        if let Some(source) = document_start_script {
            // SAFETY: fil principal (`mtm`) ; le contrôleur appartient à la configuration, encore inutilisée. Ce
            // constructeur de `WKUserScript` vise le monde de la page.
            unsafe {
                let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                    mtm.alloc::<WKUserScript>(),
                    &NSString::from_str(source),
                    WKUserScriptInjectionTime::AtDocumentStart,
                    true,
                );
                configuration.userContentController().addUserScript(&script);
            }
        }

        let hidden_frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.));
        let webview = IdemaWebView::new(mtm, hidden_frame, &configuration, events.clone());
        let delegate = PageDelegate::new(mtm, events.clone());
        // SAFETY: les délégués sont faibles côté WebKit ; `delegate` est gardé par la page, donc vit autant que la vue.
        unsafe {
            webview.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            webview.setUIDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            webview.setInspectable(true);
        }
        webview.setHidden(true);
        parent.addSubview(&webview);

        let plain: Retained<WKWebView> = Retained::into_super(webview.clone());
        let observer = PageObserver::new(mtm, &plain, events.clone());
        Some(Self {
            _observer: observer,
            delegate,
            webview,
            parent,
            snapshot_generation: Rc::new(Cell::new(0)),
            zoom: Cell::new(1.),
            events,
        })
    }

    /// Charge une adresse. Une adresse qu'AppKit refuse (`NSURL` nul) n'est pas avalée en silence : l'onglet reçoit un
    /// `Failed`, sans hôte, comme pour tout autre échec de chargement.
    pub fn load(&self, url: &str) {
        let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) else {
            let _ = self.events.unbounded_send(PageEvent::Failed {
                code: INVALID_ADDRESS_CODE,
                host: None,
                url: None,
                description: "adresse invalide".to_owned(),
            });
            return;
        };
        // SAFETY: fil principal ; la requête est neuve.
        unsafe {
            self.webview
                .loadRequest(&NSURLRequest::requestWithURL(&url))
        };
    }

    /// Charge un fichier local (HTML, PDF), avec accès en lecture à son dossier (images et feuilles de style voisines).
    pub fn load_file(&self, path: &Path) {
        let file = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let directory = path.parent().unwrap_or(path);
        let access = NSURL::fileURLWithPath_isDirectory(
            &NSString::from_str(&directory.to_string_lossy()),
            true,
        );
        // SAFETY: fil principal ; deux URL de fichier valides.
        unsafe {
            self.webview
                .loadFileURL_allowingReadAccessToURL(&file, &access)
        };
    }

    pub fn go_back(&self) {
        // SAFETY: fil principal ; sans entrée précédente WebKit rend nil, sans effet.
        unsafe { self.webview.goBack() };
    }

    pub fn go_forward(&self) {
        // SAFETY: comme `go_back`.
        unsafe { self.webview.goForward() };
    }

    pub fn reload(&self) {
        // SAFETY: fil principal.
        unsafe { self.webview.reload() };
    }

    /// Recharge sans passer par le cache (« Recharger sans le cache » du menu).
    pub fn reload_from_origin(&self) {
        // SAFETY: fil principal.
        unsafe { self.webview.reloadFromOrigin() };
    }

    pub fn stop(&self) {
        // SAFETY: fil principal.
        unsafe { self.webview.stopLoading() };
    }

    /// Pose le cadre de la vue sur celui de l'élément GPUI (`parent_height` : hauteur de la `GPUIView`).
    pub fn set_frame(&self, bounds: Bounds<Pixels>, parent_height: Pixels) {
        let (x, y, width, height) = appkit_frame(bounds, parent_height);
        let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
        if self.webview.frame() != frame {
            self.webview.setFrame(frame);
        }
    }

    /// Montre ou cache la vue native. À cacher, la page rend d'abord le clavier à GPUI : une page invisible qui
    /// garderait le premier répondant avalerait les frappes.
    pub fn set_visible(&self, visible: bool) {
        if !visible && self.has_keyboard() {
            self.release_focus();
        }
        if self.webview.isHidden() == visible {
            self.webview.setHidden(!visible);
        }
    }

    /// La vue native est-elle à l'écran ? Une vue cachée ou pas encore posée ne se photographie pas.
    pub fn is_visible(&self) -> bool {
        !self.webview.isHidden()
    }

    /// La page devient le premier répondant : le clavier est à elle.
    pub fn focus(&self) {
        if let Some(window) = self.parent.window() {
            let _ = window.makeFirstResponder(Some(&self.webview));
        }
    }

    /// La `GPUIView` redevient le premier répondant. Obligatoire quand un champ GPUI prend le focus : sans cela la
    /// frappe part dans le champ éditable de la page qui avait le focus DOM (rapport de sonde §4, cas 2).
    pub fn release_focus(&self) {
        if let Some(window) = self.parent.window() {
            let _ = window.makeFirstResponder(Some(&self.parent));
        }
    }

    /// Le premier répondant est-il la page, ou une de ses sous-vues ?
    pub fn has_keyboard(&self) -> bool {
        self.parent
            .window()
            .and_then(|window| window.firstResponder())
            .and_then(|responder| responder.downcast::<NSView>().ok())
            .is_some_and(|view| view.isDescendantOf(&self.webview))
    }

    /// Annuler et rétablir passent par le `undoManager` de la page : `undo:` n'est pas un sélecteur de `WKWebView`
    /// (rapport de sonde §3). Rien n'est envoyé quand la pile est vide : l'appeler à vide lèverait une exception.
    pub fn send_edit(&self, command: EditCommand) {
        // SAFETY: `undoManager` est un accesseur de `NSResponder` ; il rend un `NSUndoManager` ou nil.
        let manager: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*self.webview, undoManager] };
        let Some(manager) = manager else {
            return;
        };
        // SAFETY: `canUndo`, `canRedo`, `undo` et `redo` sont des méthodes publiques de `NSUndoManager`.
        unsafe {
            match command {
                EditCommand::Undo => {
                    let can: bool = msg_send![&*manager, canUndo];
                    if can {
                        let _: () = msg_send![&*manager, undo];
                    }
                }
                EditCommand::Redo => {
                    let can: bool = msg_send![&*manager, canRedo];
                    if can {
                        let _: () = msg_send![&*manager, redo];
                    }
                }
            }
        }
    }

    /// Demande un instantané PNG de la page. `done(None)` sur toute erreur, pour que l'appelant cache la page au lieu
    /// d'attendre une image. Rend le numéro de la demande : une demande plus récente, ou `cancel_snapshots`, rend
    /// toutes les précédentes muettes (leur `done` n'est jamais appelé).
    pub fn snapshot(&self, done: impl FnOnce(Option<Vec<u8>>) + 'static) -> u64 {
        let request = self.snapshot_generation.get() + 1;
        self.snapshot_generation.set(request);
        let generation = Rc::clone(&self.snapshot_generation);
        let done = Cell::new(Some(done));
        let block = RcBlock::new(move |image: *mut NSImage, _error: *mut NSError| {
            let Some(done) = done.take() else {
                return;
            };
            if generation.get() != request {
                return;
            }
            // SAFETY: WebKit passe une image valide ou nil pour la durée de l'appel.
            done(unsafe { image.as_ref() }.and_then(png_of));
        });
        // SAFETY: fil principal ; sans configuration, WebKit prend toute la vue ; le bloc est copié par WebKit.
        unsafe {
            self.webview
                .takeSnapshotWithConfiguration_completionHandler(None, &block);
        }
        request
    }

    /// Rend muettes les demandes d'instantané en vol (menu refermé avant que l'image arrive).
    pub fn cancel_snapshots(&self) {
        self.snapshot_generation
            .set(self.snapshot_generation.get() + 1);
    }

    /// Cherche `text` dans la page ; `done(true)` s'il y a une occurrence (sélectionnée et montrée par WebKit).
    pub fn find(&self, text: &str, backwards: bool, done: impl FnOnce(bool) + 'static) {
        let Some(mtm) = MainThreadMarker::new() else {
            done(false);
            return;
        };
        // SAFETY: fil principal (`mtm`) ; configuration neuve.
        let configuration = unsafe { WKFindConfiguration::new(mtm) };
        // SAFETY: réglages d'une configuration neuve, pas encore partagée.
        unsafe {
            configuration.setBackwards(backwards);
            configuration.setWraps(true);
        }
        let done = Cell::new(Some(done));
        let block = RcBlock::new(move |result: NonNull<WKFindResult>| {
            if let Some(done) = done.take() {
                // SAFETY: WebKit passe un résultat valide pour la durée de l'appel.
                done(unsafe { result.as_ref().matchFound() });
            }
        });
        // SAFETY: fil principal ; le bloc est copié par WebKit.
        unsafe {
            self.webview.findString_withConfiguration_completionHandler(
                &NSString::from_str(text),
                Some(&configuration),
                &block,
            );
        }
    }

    /// Exécute `body` comme corps d'une fonction asynchrone (`await` permis) dans le monde de la page et rend la
    /// chaîne qu'il retourne. `arguments` devient des variables locales du corps (noms et valeurs en chaînes). Une
    /// exception JS donne `Err(message)`. `done` est rappelé sur le fil principal, jamais deux fois ; si WebKit
    /// abandonne l'appel (page détruite), `done` n'est pas rappelé.
    pub fn call_async(
        &self,
        body: &str,
        arguments: Vec<(String, String)>,
        done: impl FnOnce(Result<String, String>) + 'static,
    ) {
        let Some(mtm) = MainThreadMarker::new() else {
            done(Err("not on the main thread".to_owned()));
            return;
        };
        let keys: Vec<Retained<NSString>> = arguments
            .iter()
            .map(|(name, _)| NSString::from_str(name))
            .collect();
        let values: Vec<Retained<AnyObject>> = arguments
            .iter()
            // SAFETY: un `NSString` est un objet Objective-C ; le dictionnaire n'est lu que par WebKit.
            .map(|(_, value)| unsafe { Retained::cast_unchecked(NSString::from_str(value)) })
            .collect();
        let key_refs: Vec<&NSString> = keys.iter().map(|key| &**key).collect();
        let dictionary = NSDictionary::from_retained_objects(&key_refs, &values);
        // SAFETY: fil principal (`mtm`) ; le monde de la page est un singleton de WebKit.
        let world = unsafe { WKContentWorld::pageWorld(mtm) };
        let done = Cell::new(Some(done));
        let block = RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
            let Some(done) = done.take() else {
                return;
            };
            // SAFETY: WebKit passe nil ou un objet valide pour la durée de l'appel.
            if let Some(error) = unsafe { error.as_ref() } {
                done(Err(script_error_message(error)));
                return;
            }
            // SAFETY: comme ci-dessus ; le corps ne rend qu'une chaîne.
            let text = unsafe { result.as_ref() }
                .and_then(|value| value.downcast_ref::<NSString>())
                .map(ToString::to_string);
            done(text.ok_or_else(|| "the script returned no string".to_owned()));
        });
        // SAFETY: fil principal ; cadre `None` = le cadre principal ; les valeurs du dictionnaire sont des `NSString` ;
        // le bloc est copié par WebKit.
        unsafe {
            self.webview
                .callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
                    &NSString::from_str(body),
                    Some(&dictionary),
                    None,
                    &world,
                    Some(&block),
                );
        }
    }

    /// Le zoom courant de la page (1 = 100 %).
    pub fn zoom(&self) -> f64 {
        self.zoom.get()
    }

    pub fn set_zoom(&self, zoom: f64) {
        self.zoom.set(zoom);
        // SAFETY: fil principal ; le facteur est déjà borné par `stepped_zoom`.
        unsafe { self.webview.setPageZoom(zoom) };
    }

    /// Cherche le favicon de la page d'`origin` (celle de l'adresse qui vient de finir de charger) : la réponse arrive
    /// plus tard en `PageEvent::Favicon`, ou jamais (échec muet). Sans effet hors du fil principal.
    pub fn request_favicon(&self, origin: &str) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        favicon::request(mtm, &self.webview, origin.to_owned(), self.events.clone());
    }

    /// Ouvre l'inspecteur web par l'API privée `_inspector` (E-P4-c), la seule voie par programme. Faux quand la page
    /// n'y répond pas : l'entrée de menu n'apparaît alors pas, le clic droit « Inspecter l'élément » reste.
    pub fn show_inspector(&self) -> bool {
        if !self.webview.class().responds_to(sel!(_inspector)) {
            return false;
        }
        // SAFETY: le sélecteur existe (vérifié ci-dessus) et rend un objet ou nil.
        let inspector: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*self.webview, _inspector] };
        let Some(inspector) = inspector else {
            return false;
        };
        if !inspector.class().responds_to(sel!(show)) {
            return false;
        }
        // SAFETY: le sélecteur existe (vérifié ci-dessus) et ne prend aucun argument.
        let _: () = unsafe { msg_send![&*inspector, show] };
        true
    }
}

impl Drop for WebPage {
    fn drop(&mut self) {
        // Un instantané qui reviendrait après la page ne livre rien.
        self.cancel_snapshots();
        // Un téléchargement en cours ne survit pas à la page : annulé, son fichier partiel est retiré.
        self.delegate.cancel_downloads();
        // Les popups d'abord : le délégué qui les garde meurt avec la page, une fenêtre restée ouverte lui parlerait.
        self.delegate.close_popups();
        // SAFETY: fil principal. Puis la vue sort de la hiérarchie ; l'observateur, les délégués et la vue se libèrent
        // ensuite, dans l'ordre des champs.
        unsafe { self.webview.stopLoading() };
        self.webview.removeFromSuperview();
    }
}

/// Le message d'une exception JS : WebKit le range dans `userInfo`, sinon la description de l'erreur.
fn script_error_message(error: &NSError) -> String {
    let key = NSString::from_str("WKJavaScriptExceptionMessage");
    error
        .userInfo()
        .objectForKey(&key)
        .and_then(|value| value.downcast_ref::<NSString>().map(ToString::to_string))
        .unwrap_or_else(|| error.localizedDescription().to_string())
}

/// L'image en PNG, par la représentation TIFF puis `NSBitmapImageRep`.
fn png_of(image: &NSImage) -> Option<Vec<u8>> {
    let tiff = image.TIFFRepresentation()?;
    let representation = NSBitmapImageRep::imageRepWithData(&tiff)?;
    // SAFETY: dictionnaire de propriétés vide, type PNG supporté.
    let png = unsafe {
        representation
            .representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(png.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    #[test]
    fn appkit_frames_flip_the_y_axis_of_the_unflipped_gpui_view() {
        let bounds = Bounds::new(point(px(920.), px(88.)), size(px(520.), px(812.)));
        assert_eq!(appkit_frame(bounds, px(900.)), (920., 0., 520., 812.));
        let top = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(50.)));
        assert_eq!(appkit_frame(top, px(900.)), (0., 850., 100., 50.));
    }

    #[test]
    fn a_page_that_is_not_on_screen_gets_an_answer_instead_of_a_sheet() {
        use AutoAnswer::{Accept, Decline};
        use DialogAction::{Answer, Sheet};
        use DialogKind::{Alert, Confirm, FilePicker, Prompt};
        for kind in [Alert, Confirm, Prompt, FilePicker] {
            assert_eq!(dialog_action(kind, true), Sheet, "{kind:?}");
        }
        assert_eq!(dialog_action(Alert, false), Answer(Accept));
        // Jamais un « oui » silencieux : confirm() refuse, prompt() et le sélecteur annulent.
        assert_eq!(dialog_action(Confirm, false), Answer(Decline));
        assert_eq!(dialog_action(Prompt, false), Answer(Decline));
        assert_eq!(dialog_action(FilePicker, false), Answer(Decline));
    }

    #[test]
    fn command_is_alone_whatever_the_arrow_and_caps_lock_flags() {
        let command = NSEventModifierFlags::Command;
        assert!(is_command_only(command));
        // Une vraie flèche de clavier : Fonction et PavéNumérique en plus, et Verr. Maj éventuel.
        let arrow = command | NSEventModifierFlags::Function | NSEventModifierFlags::NumericPad;
        assert!(is_command_only(arrow));
        assert!(is_command_only(arrow | NSEventModifierFlags::CapsLock));
        // Les modificateurs de texte, eux, ne passent pas.
        for other in [
            NSEventModifierFlags::Shift,
            NSEventModifierFlags::Option,
            NSEventModifierFlags::Control,
        ] {
            assert!(!is_command_only(arrow | other));
        }
        assert!(!is_command_only(NSEventModifierFlags::Function));
        assert!(!is_command_only(NSEventModifierFlags::empty()));
    }

    #[test]
    fn command_arrows_alone_are_history_keys() {
        assert_eq!(history_key(123, true), Some(HistoryKey::Back));
        assert_eq!(history_key(124, true), Some(HistoryKey::Forward));
        // Avec `⇧`, `⌥` ou `⌃` : sélection ou saut de mot, à la page.
        assert_eq!(history_key(123, false), None);
        assert_eq!(history_key(124, false), None);
        // Ni les autres flèches ni les autres touches.
        for code in [125, 126, 33, 30, 0] {
            assert_eq!(history_key(code, true), None, "{code}");
        }
    }

    #[test]
    fn only_a_key_seen_twice_is_the_resend_of_a_key_the_page_left() {
        let mut guard = ResendGuard::default();
        // Première vue : la page juge. Seconde vue du même événement : elle l'a laissé.
        assert!(!guard.is_resend(123, 10.5));
        assert!(guard.is_resend(123, 10.5));
        // La frappe que la page a prise n'est vue qu'une fois : la suivante, d'une autre date, n'est pas un renvoi.
        assert!(!guard.is_resend(123, 11.));
        assert!(!guard.is_resend(123, 12.));
        // Une autre touche à la même date n'est pas le renvoi de la première.
        assert!(!guard.is_resend(124, 12.));
        assert!(guard.is_resend(124, 12.));
        // Un renvoi ne se compte qu'une fois.
        assert!(!guard.is_resend(124, 12.));
    }

    #[test]
    fn a_page_opens_tabs_for_web_addresses_only() {
        assert!(page_may_open_tab("https://example.com/a?b=c#d"));
        assert!(page_may_open_tab("HTTP://example.com/"));
        for refused in [
            "file:///etc/hosts",
            "FILE:///etc/hosts",
            "mailto:x@example.com",
            "javascript:alert(1)",
            "data:text/html,<p>x</p>",
            "about:blank",
            "ftp://example.com/",
            "not an address",
            "",
        ] {
            assert!(!page_may_open_tab(refused), "{refused}");
        }
    }

    #[test]
    fn a_popup_opens_for_web_empty_and_blank_addresses_only() {
        for allowed in [
            "",
            "about:blank",
            "https://example.com/cb?x=1",
            "http://127.0.0.1:8811/popup",
        ] {
            assert!(page_may_open_popup(allowed), "{allowed}");
        }
        for refused in [
            "file:///etc/hosts",
            "FILE:///etc/hosts",
            "about:srcdoc",
            "about:blank#x",
            "javascript:alert(1)",
            "data:text/html,<p>x</p>",
            "mailto:x@example.com",
            "not an address",
        ] {
            assert!(!page_may_open_popup(refused), "{refused}");
        }
    }

    #[test]
    fn the_scheme_of_an_address_is_lowercase() {
        assert_eq!(scheme_of_url("FILE:///a").as_deref(), Some("file"));
        assert_eq!(scheme_of_url("nope"), None);
    }

    #[test]
    fn a_page_touching_the_window_edge_leaves_the_outline_visible() {
        let viewport = size(px(1440.), px(900.));
        // Un volet collé à droite et en bas : un point de moins de chaque côté.
        let panel = Bounds::new(point(px(920.), px(88.)), size(px(520.), px(812.)));
        assert_eq!(
            inset_from_window_edge(panel, viewport),
            Bounds::new(point(px(920.), px(88.)), size(px(519.), px(811.)))
        );
        // Agrandi sur toute la fenêtre : la page recule aussi des deux côtés.
        let full = Bounds::new(point(px(0.), px(88.)), size(px(1440.), px(812.)));
        assert_eq!(
            inset_from_window_edge(full, viewport),
            Bounds::new(point(px(0.), px(88.)), size(px(1439.), px(811.)))
        );
        // Une page qui ne touche aucun bord reste telle quelle.
        let inside = Bounds::new(point(px(100.), px(100.)), size(px(300.), px(200.)));
        assert_eq!(inset_from_window_edge(inside, viewport), inside);
        // Une zone sans surface ne devient jamais négative.
        let flat = Bounds::new(point(px(1440.), px(900.)), size(px(0.), px(0.)));
        assert_eq!(
            inset_from_window_edge(flat, viewport).size,
            size(px(0.), px(0.))
        );
    }

    #[test]
    fn the_profile_identifier_is_a_non_zero_uuid() {
        let id = PROFILE_ID.replace('-', "");
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(id.chars().any(|c| c != '0'));
    }

    #[test]
    fn attachments_and_unshowable_types_are_downloaded() {
        assert!(should_download(
            true,
            Some("attachment; filename=\"a.zip\"")
        ));
        assert!(should_download(true, Some("  Attachment")));
        assert!(should_download(false, None));
        assert!(should_download(false, Some("inline")));
        assert!(!should_download(true, None));
        assert!(!should_download(true, Some("inline; filename=\"a.pdf\"")));
    }

    #[test]
    fn a_cancelled_or_interrupted_load_is_not_a_failure() {
        assert!(is_ignored_load_error("NSURLErrorDomain", -999));
        assert!(is_ignored_load_error("WebKitErrorDomain", 102));
        // Le même code dans un autre domaine, ou un autre code, reste un vrai échec.
        assert!(!is_ignored_load_error("WebKitErrorDomain", -999));
        assert!(!is_ignored_load_error("NSURLErrorDomain", 102));
        assert!(!is_ignored_load_error("NSURLErrorDomain", -1004));
        assert!(!is_ignored_load_error("NSURLErrorDomain", -1202));
    }

    #[test]
    fn the_host_comes_from_http_addresses_only() {
        assert_eq!(
            host_of_url("https://example.com:8443/a?b#c").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of_url("http://[::1]:3000/").as_deref(), Some("[::1]"));
        assert_eq!(host_of_url("file:///tmp/a.html"), None);
        assert_eq!(host_of_url("about:blank"), None);
        assert_eq!(host_of_url("pas une adresse"), None);
    }

    #[test]
    fn a_popup_is_never_tiny_nor_bigger_than_a_screen() {
        assert_eq!(popup_size(500., 600.), (500., 600.));
        assert_eq!(popup_size(10., 20.), (POPUP_MIN_SIDE, POPUP_MIN_SIDE));
        assert_eq!(popup_size(9000., 9000.), (POPUP_MAX_SIDE, POPUP_MAX_SIDE));
        assert_eq!(popup_size(f64::NAN, f64::INFINITY), (520., 680.));
    }

    #[test]
    fn the_zoom_moves_by_tenths_between_half_and_three() {
        use ZoomStep::{In, Out, Reset};
        assert_eq!(stepped_zoom(1., In), 1.1);
        assert_eq!(stepped_zoom(1., Out), 0.9);
        assert_eq!(stepped_zoom(2.3, Reset), 1.);
        // Dix pas dans le même sens ne dérivent pas.
        let ten_in = (0..10).fold(1., |zoom, _| stepped_zoom(zoom, In));
        assert_eq!(ten_in, 2.);
        let five_out = (0..5).fold(1., |zoom, _| stepped_zoom(zoom, Out));
        assert_eq!(five_out, ZOOM_MIN);
        // Les bornes tiennent, quel que soit le nombre de gestes.
        let floor = (0..50).fold(1., |zoom, _| stepped_zoom(zoom, Out));
        assert_eq!(floor, ZOOM_MIN);
        let ceiling = (0..50).fold(1., |zoom, _| stepped_zoom(zoom, In));
        assert_eq!(ceiling, ZOOM_MAX);
        // Un zoom hors de la grille y retombe au premier pas.
        assert_eq!(stepped_zoom(1.04, In), 1.1);
        assert_eq!(stepped_zoom(9., Out), ZOOM_MAX);
    }

    #[test]
    fn a_suggested_name_never_escapes_the_downloads_folder() {
        assert_eq!(safe_file_name("report.pdf"), "report.pdf");
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("a\\b.txt"), "b.txt");
        assert_eq!(safe_file_name(""), "download");
        assert_eq!(safe_file_name(".."), "download");
        assert_eq!(safe_file_name("dir/"), "download");
    }

    #[test]
    fn a_taken_name_gets_the_next_free_suffix() {
        let directory = Path::new("/home/me/Downloads");
        let taken = [
            "/home/me/Downloads/report.pdf",
            "/home/me/Downloads/report (2).pdf",
            "/home/me/Downloads/notes",
        ];
        let exists = |path: &Path| taken.iter().any(|known| Path::new(known) == path);
        assert_eq!(
            unique_destination(directory, "fresh.zip", &[], exists),
            directory.join("fresh.zip")
        );
        assert_eq!(
            unique_destination(directory, "report.pdf", &[], exists),
            directory.join("report (3).pdf")
        );
        assert_eq!(
            unique_destination(directory, "notes", &[], exists),
            directory.join("notes (2)")
        );
    }

    #[test]
    fn a_name_taken_by_a_download_in_flight_is_skipped_too() {
        let directory = Path::new("/home/me/Downloads");
        let in_flight = [
            directory.join("export.csv"),
            directory.join("export (2).csv"),
        ];
        // Rien n'est encore sur le disque : seuls les téléchargements en cours tiennent les noms.
        assert_eq!(
            unique_destination(directory, "export.csv", &in_flight, |_| false),
            directory.join("export (3).csv")
        );
        assert_eq!(
            unique_destination(directory, "other.csv", &in_flight, |_| false),
            directory.join("other.csv")
        );
    }

    #[test]
    fn only_the_main_frame_can_download_a_subframe_one_is_cancelled() {
        use ResponseAction::{Allow, Cancel, Download};
        assert_eq!(response_action(true, true, None), Allow);
        assert_eq!(response_action(false, true, Some("inline")), Allow);
        assert_eq!(response_action(true, false, None), Download);
        assert_eq!(response_action(true, true, Some("attachment")), Download);
        assert_eq!(response_action(false, false, None), Cancel);
        assert_eq!(response_action(false, true, Some("attachment")), Cancel);
    }

    #[test]
    fn the_cookie_summary_counts_domains_in_order_and_never_shows_a_value() {
        let domains = [".github.com", ".google.com", ".github.com"].map(String::from);
        assert_eq!(
            cookie_summary(domains),
            "browser cookies total=3 .github.com=2 .google.com=1"
        );
        assert_eq!(cookie_summary(Vec::new()), "browser cookies total=0");
    }
}
