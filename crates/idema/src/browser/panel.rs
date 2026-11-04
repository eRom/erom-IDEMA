//! Le volet Browser (P4, spec §4) : les onglets, la rangée d'adresse et la page. Une seule entité pour toute la fenêtre,
//! gardée par `Workspace`. La page web est une vue AppKit posée par-dessus GPUI (`webkit::WebPage`) : un `canvas` plein
//! pose son cadre à chaque image, et toute page qui n'est pas rendue est cachée explicitement, la vue native ne se
//! cache pas d'elle-même.

mod chrome;
mod find;
mod journal;
mod menu;
mod start_page;

pub use self::start_page::servers_badge_count;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use gpui::{
    Action, AnyElement, App, ClipboardItem, Context, DispatchPhase, Entity, EventEmitter,
    FocusHandle, Focusable, Image, ImageFormat, KeyBinding, KeyBindingContextPredicate,
    MouseDownEvent, Pixels, PlatformKeyboardMapper, Point, Render, RenderImage, Subscription, Task,
    Window, actions, canvas, div, img, prelude::*,
};
use gpui_component::input::{InputEvent, InputState};
use url::Url;

use self::chrome::{ErrorAction, TabView, more_button, new_tab_button, strip_width, tab_strip};
use self::find::{FindBar, FindStatus};
use self::journal::JournalView;
use self::menu::{MenuAction, menu_lines};
use self::start_page::{RowAction, StartFollow, StartWatch};
use super::BrowserSettings;
use super::address::{self, same_address};
use super::favicons::FaviconCache;
use super::load_error::{LoadError, StartOffer, host_with_port, load_error, start_offer};
use super::tabs::{BrowserTabs, OpenFrom, SavedTabs, TabId, TabKind};
use super::webkit::{
    self, EditCommand, HistoryKey, PageEvent, WebPage, ZoomStep, inset_from_window_edge,
    stepped_zoom,
};
use crate::assets::Icon;
use crate::hud;
use idema_tmux::server::TmuxServer;

use crate::popover::{MenuEntry, MenuItemSpec, MenuSpec};
use crate::servers::model::{Servers, ServersEvent};
use crate::servers::registry::{RowKind, ServerKey, ServerRow};
use crate::terminal_panel::{HEADER_HEIGHT, header_button, panel_shell};
use crate::theme::{PANEL, Theme, color};
use crate::workspace::{Workspace, drag_window};

/// Largeur de référence du volet (maquette, écran 02) : elle fixe le plancher commun des volets droits
/// (`RIGHT_PANEL_MIN_WIDTH`), pas sa largeur par défaut.
pub const BROWSER_PANEL_WIDTH: f32 = 520.;

/// Contexte de touches du volet : ses liaisons (`⌘L`) s'y accrochent, le focus GPUI du volet l'active.
pub const KEY_CONTEXT: &str = "Browser";

/// Contexte de la boîte de la barre d'adresse : le champ y garde ses propres raccourcis d'édition.
pub const ADDRESS_KEY_CONTEXT: &str = "BrowserAddress";

/// Contexte de la rangée de recherche : Échap y ferme la barre.
pub const FIND_KEY_CONTEXT: &str = "BrowserFind";

/// L'indication du champ d'adresse quand il est vide (page d'accueil).
const ADDRESS_PLACEHOLDER: &str = "Adresse ou recherche";

actions!(
    browser,
    [
        NewTab,
        CloseTab,
        FocusAddress,
        Reload,
        GoBack,
        GoForward,
        GoBackKey,
        GoForwardKey,
        NextTab,
        PreviousTab,
        FindInPage,
        FindNext,
        FindPrevious,
        CloseFind,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        PageUndo,
        PageRedo
    ]
);

/// Le contexte où `⌘G` et `⌘⇧G` sont « suivant » et « précédent » : le volet, barre de recherche ouverte. Barre fermée,
/// ces touches ne sont pas liées ici et `⌘⇧G` reste à Changes.
const FINDING_PREDICATE: &str = "Browser && finding";

/// Le volet hors de ses deux champs de saisie : l'adresse et la recherche gardent là leurs touches (`⌘[`, `⌘Z`...), qui
/// sont celles du kit. Le volet reste le contexte le plus profond pour la page.
const OUTSIDE_FIELDS_PREDICATE: &str = "Browser && !BrowserAddress && !BrowserFind";

/// Les liaisons du volet. `⌘C`, `⌘V`, `⌘X` et `⌘A` n'y sont pas : le menu « Édition » les envoie au premier répondant,
/// la page (rapport de sonde §3), et aucun gestionnaire de ces actions ne doit exister sous le volet.
pub fn key_bindings() -> Vec<KeyBinding> {
    let browser = Some(KEY_CONTEXT);
    let finding = Some(FINDING_PREDICATE);
    let outside_fields = Some(OUTSIDE_FIELDS_PREDICATE);
    vec![
        // Au lieu de ceux de la session (contexte `Workspace`, moins profond).
        KeyBinding::new("cmd-t", NewTab, browser),
        KeyBinding::new("cmd-w", CloseTab, browser),
        KeyBinding::new("cmd-r", Reload, browser),
        // Annuler et rétablir de la page : le champ du kit garde les siens.
        // Reculer et avancer sur les flèches : le champ d'adresse et la recherche gardent début et fin de ligne, et l'action
        // laisse passer la touche quand la page a le clavier (le curseur de son champ éditable).
        KeyBinding::new("cmd-left", GoBackKey, outside_fields),
        KeyBinding::new("cmd-right", GoForwardKey, outside_fields),
        KeyBinding::new("cmd-z", PageUndo, outside_fields),
        KeyBinding::new("cmd-shift-z", PageRedo, outside_fields),
        KeyBinding::new("cmd-l", FocusAddress, browser),
        KeyBinding::new("cmd-f", FindInPage, browser),
        KeyBinding::new("cmd-g", FindNext, finding),
        KeyBinding::new("cmd-shift-g", FindPrevious, finding),
        KeyBinding::new("escape", CloseFind, Some(FIND_KEY_CONTEXT)),
        // Le zoom de la page prend `⌘=`, `⌘-` et `⌘0` à la taille du texte (contexte `Workspace`, moins profond). Le
        // clavier français tape `+` en `⇧=` : les trois écritures de la touche sont liées.
        KeyBinding::new("cmd-=", ZoomIn, browser),
        KeyBinding::new("cmd-+", ZoomIn, browser),
        KeyBinding::new("cmd-shift-=", ZoomIn, browser),
        KeyBinding::new("cmd--", ZoomOut, browser),
    ]
}

/// Les liaisons qui suivent la disposition du clavier, liées comme les chiffres de `main.rs` : avec les équivalents de
/// touche de la disposition courante (sur un clavier AZERTY, le 0 est une touche qui donne `à` sans Maj).
pub fn layout_key_bindings(mapper: &dyn PlatformKeyboardMapper) -> Vec<KeyBinding> {
    let predicate = |source: &str| {
        Rc::new(KeyBindingContextPredicate::parse(source).expect("a valid context predicate"))
    };
    let browser = predicate(KEY_CONTEXT);
    let outside_fields = predicate(OUTSIDE_FIELDS_PREDICATE);
    let load =
        |keystroke: &str, action: Box<dyn Action>, context: &Rc<KeyBindingContextPredicate>| {
            KeyBinding::load(
                keystroke,
                action,
                Some(Rc::clone(context)),
                true,
                None,
                mapper,
            )
            .expect("a valid keystroke")
        };
    // Sur un clavier à crochets sous Option (AZERTY, QWERTZ), `⌘[` et `⌘]` sont d'autres touches : le mapper les rend.
    // `⇧` change le caractère de la touche (`}` pour `]`), GPUI rapporte alors `cmd-}` : les deux écritures sont liées.
    vec![
        load("cmd-0", Box::new(ZoomReset), &browser),
        load("cmd-[", Box::new(GoBack), &outside_fields),
        load("cmd-]", Box::new(GoForward), &outside_fields),
        load("cmd-shift-]", Box::new(NextTab), &browser),
        load("cmd-}", Box::new(NextTab), &browser),
        load("cmd-shift-[", Box::new(PreviousTab), &browser),
        load("cmd-{", Box::new(PreviousTab), &browser),
    ]
}

/// Ce que le volet demande à `Workspace`.
pub enum BrowserEvent {
    /// Le focus GPUI est entré dans le volet ou en est sorti : le clavier se resynchronise.
    FocusChanged,
    /// La liste des onglets, une adresse ou un titre a changé : `state.json` se réécrit.
    TabsChanged,
    /// Le dernier onglet vient d'être fermé : le volet se ferme aussi (`Workspace` rend le clavier au pane).
    CloseRequested,
    /// Le lien « Ouvrir config.toml » de la page d'accueil.
    OpenConfig,
    /// Un menu d'IdemA à ouvrir à `position` (le menu `more_vert`).
    OpenMenu {
        position: Point<Pixels>,
        spec: MenuSpec,
    },
    /// Un téléchargement fini à montrer dans le Finder.
    Reveal(PathBuf),
}

/// Ce que le corps d'un onglet web montre : sa page, ou l'état qui la remplace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabState {
    /// La page native est à l'écran.
    Live,
    /// Le chargement a échoué : la page native se cache, le corps dit la cause.
    Failed(LoadError),
    /// Le processus WebContent s'est arrêté.
    Terminated,
}

impl TabState {
    /// L'état après un événement de la page. Un échec ou un arrêt tient jusqu'à ce qu'une navigation reçoive du contenu
    /// (`Committed`) : un « Réessayer » qui échoue encore ne fait pas clignoter la page précédente.
    fn after(self, event: &PageEvent) -> TabState {
        match event {
            PageEvent::Failed {
                code,
                host,
                url,
                description,
            } => {
                // Le message nomme l'hôte et son port (« localhost:8819 ») : c'est le port qui dit quel serveur manque.
                let label = url
                    .as_deref()
                    .and_then(host_with_port)
                    .or_else(|| host.clone());
                TabState::Failed(load_error(*code, label.as_deref(), description))
            }
            PageEvent::Terminated => TabState::Terminated,
            PageEvent::Committed(_) => TabState::Live,
            _ => self,
        }
    }
}

/// Ce que fait un geste « recharger » selon ce que l'onglet montre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReloadGesture {
    Stop,
    Reload,
    ReloadFromOrigin,
    /// Rejouer l'adresse de l'onglet : WebKit n'a pas de page pour elle, son `reload` rechargerait celle d'avant.
    Retry,
}

/// Le geste d'un bouton « recharger » (`from_origin` : « Recharger sans le cache »). Un chargement en cours s'arrête ;
/// une erreur ou un arrêt de la page rejoue l'adresse de l'onglet.
fn reload_gesture(loading: bool, view: &TabState, from_origin: bool) -> ReloadGesture {
    if loading && !from_origin {
        ReloadGesture::Stop
    } else if *view != TabState::Live {
        ReloadGesture::Retry
    } else if from_origin {
        ReloadGesture::ReloadFromOrigin
    } else {
        ReloadGesture::Reload
    }
}

/// Le nom d'un fichier tel que le journal l'écrit : ni `?`, ni `#`, ni caractère de contrôle (le nom vient du serveur).
fn journal_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control() && !matches!(c, '?' | '#'))
        .collect()
}

/// La ligne de téléchargement échoué de l'onglet après un événement : elle se pose à l'échec, et s'efface à la
/// navigation suivante.
fn download_line_after(current: Option<String>, event: &PageEvent) -> Option<String> {
    match event {
        PageEvent::DownloadFailed(cause) => Some(format!("Téléchargement échoué : {cause}")),
        // Un téléchargement qui réussit dit plus vrai que l'échec d'avant : la ligne part aussi.
        PageEvent::Committed(_) | PageEvent::DownloadFinished(_) => None,
        _ => current,
    }
}

/// Le couvrement de la page par un menu d'IdemA (P4-4) : la vue native passe toujours par-dessus GPUI, donc tant qu'un
/// menu est ouvert elle est remplacée par son instantané, peint en GPUI à sa place. `I` est l'image décodée
/// (`RenderImage` en vrai, un entier dans les tests).
#[derive(Clone)]
enum Cover<I = Arc<RenderImage>> {
    /// Aucun menu sur la page : la page native est à l'écran.
    Off,
    /// L'instantané de la page de l'onglet `tab` est demandé (demande `request`) ; la page reste à l'écran, un
    /// instantané d'une vue cachée n'est pas fiable. `COVER_TIMEOUT` borne l'attente.
    Pending { tab: TabId, request: u64 },
    /// La page de `tab` est cachée, `image` peinte à sa place. `None` : pas d'instantané (erreur, délai dépassé, page pas
    /// à l'écran), la zone montre le fond du volet (repli de la spec, §9).
    Covering { tab: TabId, image: Option<I> },
}

/// Combien de temps la page reste à l'écran en attendant son instantané (mesuré à 25 ms à la sonde). Passé ce délai, un
/// WebContent bloqué ne laisse pas le menu invisible sous la vue native : la page se cache sur le fond du volet
/// (arbitrage 17).
const COVER_TIMEOUT: Duration = Duration::from_millis(150);

/// Un changement de titre ne réécrit pas `state.json` sur-le-champ : les titres qui arrivent dans ce délai partent en une
/// seule écriture. Un titre suit presque toujours une navigation, dont l'adresse a déjà été écrite.
const TITLE_SAVE_DELAY: Duration = Duration::from_millis(500);

impl<I> Cover<I> {
    /// L'onglet couvert et l'image à rendre au cache de GPUI ; rien quand il n'y a pas de couvrement.
    fn into_parts(self) -> Option<(TabId, Option<I>)> {
        match self {
            Cover::Off => None,
            Cover::Pending { tab, .. } => Some((tab, None)),
            Cover::Covering { tab, image } => Some((tab, image)),
        }
    }
}

/// Le couvrement de départ de la page de `tab`. Une page à l'écran est photographiée (`request` demande l'instantané et
/// rend son numéro) ; une page qui n'y est pas (née sous le menu, onglet qui vient d'être activé) ne se photographie pas :
/// elle se cache aussitôt sur le fond du volet, sans jamais apparaître par-dessus le menu.
fn cover_start<I>(tab: TabId, page_visible: bool, request: impl FnOnce() -> u64) -> Cover<I> {
    if page_visible {
        Cover::Pending {
            tab,
            request: request(),
        }
    } else {
        Cover::Covering { tab, image: None }
    }
}

/// Le couvrement après la réponse de l'instantané `request` de `tab` (`image` : décodé, `None` si WebKit ou le décodage
/// a échoué). Une réponse qui n'est pas celle de la demande en cours (menu refermé, rouvert, autre onglet, délai
/// dépassé) est jetée : elle tomberait sur un couvrement plus récent.
fn cover_after_snapshot<I>(
    cover: Cover<I>,
    tab: TabId,
    request: u64,
    image: Option<I>,
) -> Cover<I> {
    match cover {
        Cover::Pending {
            tab: wanted,
            request: current,
        } if wanted == tab && current == request => Cover::Covering { tab, image },
        other => other,
    }
}

/// Le couvrement quand le délai de `COVER_TIMEOUT` de la demande `request` de `tab` est passé sans réponse : la page se
/// cache sur le fond du volet. Sans effet si la demande n'est plus l'attendue.
fn cover_after_timeout<I>(cover: Cover<I>, tab: TabId, request: u64) -> Cover<I> {
    match cover {
        Cover::Pending {
            tab: wanted,
            request: current,
        } if wanted == tab && current == request => Cover::Covering { tab, image: None },
        other => other,
    }
}

/// La page native de `active` est-elle à l'écran ? Elle ne l'est plus qu'une fois son couvrement prêt (image, ou repli).
fn page_shown_under<I>(cover: &Cover<I>, active: TabId) -> bool {
    !matches!(cover, Cover::Covering { tab, .. } if *tab == active)
}

/// Ce que l'on sait d'une page vivante : l'état que la barre d'adresse dessine.
#[derive(Debug, Default)]
struct PageState {
    loading: bool,
    progress: f64,
    can_go_back: bool,
    can_go_forward: bool,
}

/// La page d'un onglet web, avec l'état qu'elle rapporte et la tâche qui lit ses événements. La tâche meurt avec la
/// page : un événement d'un onglet fermé n'arrive nulle part.
struct PageSlot {
    page: Rc<WebPage>,
    state: PageState,
    /// La page, ou l'erreur qui la remplace dans le corps de l'onglet.
    view: TabState,
    /// « Téléchargement échoué : <cause> », sous la rangée d'adresse jusqu'à la navigation suivante.
    download_line: Option<String>,
    /// L'icône de la page, quand son origine en a une : l'onglet la peint à la place du globe.
    favicon: Option<Arc<RenderImage>>,
    _reader: Task<()>,
}

pub struct BrowserPanel {
    tabs: BrowserTabs,
    /// Une page par onglet web qui a été montré, gardée jusqu'à la fermeture de l'onglet.
    pages: HashMap<TabId, PageSlot>,
    /// Les pages dont la création est différée (jamais pendant un `update` en cours, P4-R4).
    creating: HashSet<TabId>,
    address: Entity<InputState>,
    /// La barre de recherche `⌘F`, ouverte sur la page de l'onglet actif ; elle se ferme au changement d'onglet.
    find: Option<FindBar>,
    /// Faux dès que l'inspecteur a refusé de s'ouvrir (API privée absente, E-P4-c) : l'entrée quitte le menu.
    inspector_available: bool,
    focus_handle: FocusHandle,
    /// La page de l'onglet actif doit prendre le clavier dès qu'elle est à l'écran (`⌘⇧B` qui ouvre le volet : la page
    /// naît ou revient au `prepaint` suivant). Le `canvas` de la page prend la demande ; tout départ du clavier la retire.
    page_focus_wanted: Rc<Cell<bool>>,
    /// Une écriture des titres est déjà en attente (`TITLE_SAVE_DELAY`).
    title_save_pending: bool,
    /// Le volet est-il celui que l'écran montre ? Sinon toute page est cachée.
    shown: bool,
    /// Un menu d'IdemA recouvre-t-il la zone de la page ? `Workspace` le dit (`set_covered`).
    covered: bool,
    /// Ce que la zone de la page montre pendant qu'un menu la recouvre.
    cover: Cover,
    /// Les favicons lus pendant la session, par origine.
    favicons: FaviconCache<Arc<RenderImage>>,
    /// « Inspecter » a été choisi pendant que le menu recouvrait la page : l'inspecteur s'ouvre à sa fermeture.
    inspect_after_uncover: bool,
    /// Les serveurs de dev des projets (spec P4 §5), quand tmux a répondu : la page d'accueil les liste, l'erreur d'un
    /// port à l'arrêt propose de le démarrer.
    servers: Option<Entity<Servers>>,
    /// Le serveur tmux d'IdemA, pour attacher le journal d'une session serveur.
    tmux: Option<Arc<TmuxServer>>,
    /// Le corps de chaque onglet journal ouvert, né à la première apparition de l'onglet.
    journals: HashMap<TabId, Entity<JournalView>>,
    /// L'écoute de chaque journal (l'adresse ⌘-cliquée dans sa sortie).
    journal_subscriptions: HashMap<TabId, Subscription>,
    /// Le projet de contexte de la fenêtre : son groupe de serveurs vient en premier sur la page d'accueil.
    context: Option<PathBuf>,
    /// Les démarrages demandés depuis la page (▶, « Démarrer », relancer), suivis jusqu'à leur issue.
    watched: HashMap<ServerKey, StartWatch>,
    /// Les onglets en erreur dont « Démarrer » a été cliqué : leur page se recharge quand le serveur écoute.
    awaiting_server: HashMap<TabId, ServerKey>,
    /// Les onglets dont la page se recharge parce que leur serveur vient de démarrer : le corps le dit au lieu de
    /// l'erreur d'avant, jusqu'à ce que la page reçoive du contenu ou échoue de nouveau.
    reloading_after_start: HashSet<TabId>,
    _servers_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BrowserEvent> for BrowserPanel {}

impl Focusable for BrowserPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BrowserPanel {
    /// Le volet avec les onglets de `state.json` (`None` ou illisibles : un seul onglet Accueil). Aucune page n'est
    /// créée ici : chacune naît à la première apparition de son onglet.
    pub fn new(saved: Option<SavedTabs>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        webkit::log_profile_cookies();
        webkit::sweep_orphaned_downloads();
        let address = cx.new(|cx| InputState::new(window, cx).placeholder(ADDRESS_PLACEHOLDER));
        let focus_handle = cx.focus_handle();
        let subscriptions = vec![
            cx.subscribe_in(&address, window, Self::on_address_event),
            cx.on_focus_in(&focus_handle, window, |_, _, cx| {
                cx.emit(BrowserEvent::FocusChanged);
            }),
            // Le focus GPUI quitte le volet par une autre voie qu'un clic (`⌘1` à `⌘9`, `⌃⇥`, `⌘N`, un autre volet, un
            // menu...) : la page rend le clavier à GPUI, sinon la frappe suivante irait dans son champ éditable (revue
            // de la tâche 6, I-1 ; rapport de sonde §4). Une fenêtre qui se désactive n'est pas un départ : au retour
            // la page garde le clavier.
            cx.on_focus_out(&focus_handle, window, |panel, _, window, cx| {
                panel.page_focus_wanted.set(false);
                if window.is_window_active() {
                    panel.release_page_keyboard();
                }
                cx.emit(BrowserEvent::FocusChanged);
            }),
            // La page d'accueil suit les favoris du `config.toml` rechargé.
            cx.observe_global::<BrowserSettings>(|_, cx| cx.notify()),
        ];
        let mut panel = Self {
            tabs: BrowserTabs::from_saved(saved),
            pages: HashMap::new(),
            creating: HashSet::new(),
            address,
            find: None,
            inspector_available: true,
            focus_handle,
            page_focus_wanted: Rc::new(Cell::new(false)),
            title_save_pending: false,
            shown: false,
            covered: false,
            cover: Cover::Off,
            favicons: FaviconCache::default(),
            inspect_after_uncover: false,
            servers: None,
            tmux: None,
            journals: HashMap::new(),
            journal_subscriptions: HashMap::new(),
            context: None,
            watched: HashMap::new(),
            awaiting_server: HashMap::new(),
            reloading_after_start: HashSet::new(),
            _servers_subscription: None,
            _subscriptions: subscriptions,
        };
        panel.sync_address(true, window, cx);
        panel
    }

    /// Branche les serveurs de dev (créés par `Workspace` quand tmux répond) : la page d'accueil les suit.
    pub fn set_servers(
        &mut self,
        servers: Entity<Servers>,
        tmux: Arc<TmuxServer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.servers.as_ref() == Some(&servers) {
            return;
        }
        self._servers_subscription =
            Some(cx.subscribe_in(&servers, window, Self::on_servers_event));
        self.servers = Some(servers);
        self.tmux = Some(tmux);
        // Un journal restauré attendait ses serveurs.
        if self.shown {
            self.sync_journals(window, cx);
        }
        cx.notify();
    }

    /// Le projet de contexte de la fenêtre : son groupe de serveurs vient en premier. Un projet inchangé ne repeint pas.
    pub fn set_context(&mut self, project: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.context != project {
            self.context = project;
            cx.notify();
        }
    }

    /// Un serveur a changé d'état : la page d'accueil se repeint, et les démarrages demandés d'ici se suivent.
    fn on_servers_event(
        &mut self,
        _: &Entity<Servers>,
        _: &ServersEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.follow_servers(window, cx);
        // Un serveur relancé a une nouvelle session : les journaux ouverts la rejoignent.
        self.sync_journals(window, cx);
        cx.notify();
    }

    /// Fait avancer les démarrages demandés depuis la page : un serveur qui écoute recharge l'onglet qui l'attendait,
    /// un démarrage perdu s'oublie.
    fn follow_servers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(servers) = self.servers.clone() else {
            return;
        };
        let now = Instant::now();
        let mut ended: Vec<(ServerKey, StartFollow)> = Vec::new();
        for (key, watch) in &mut self.watched {
            let row = servers.read(cx).row_of(&RowKind::Config(key.clone()));
            match watch.follow(row.as_ref().map(|row| &row.state), now) {
                StartFollow::Wait => {}
                outcome => ended.push((key.clone(), outcome)),
            }
        }
        for (key, outcome) in ended {
            self.watched.remove(&key);
            let tabs: Vec<TabId> = self
                .awaiting_server
                .iter()
                .filter(|(_, awaited)| **awaited == key)
                .map(|(tab, _)| *tab)
                .collect();
            for tab in tabs {
                self.awaiting_server.remove(&tab);
                if outcome == StartFollow::Running {
                    self.reloading_after_start.insert(tab);
                    self.retry_tab(tab, cx);
                }
            }
            if outcome == StartFollow::Failed {
                hud::log_event(&format!(
                    "browser server start failed name={} project={}",
                    key.name,
                    key.project.display()
                ));
                // Un démarrage qui échoue ouvre de lui-même le journal du serveur, s'il y a une sortie à lire (spec
                // §5.3) : le message d'erreur est dans la session serveur qui reste.
                let row = servers.read(cx).row_of(&RowKind::Config(key));
                if let Some(row) = row
                    && self.has_journal(&row, cx)
                {
                    self.open_journal(&row, window, cx);
                }
            }
        }
    }

    /// Suit le démarrage que la page vient de demander pour `key`.
    fn watch_start(&mut self, key: &ServerKey, restart: bool, cx: &mut Context<Self>) {
        let Some(servers) = self.servers.clone() else {
            return;
        };
        let Some(row) = servers.read(cx).row_of(&RowKind::Config(key.clone())) else {
            return;
        };
        self.watched.insert(
            key.clone(),
            StartWatch::new(&row.state, restart, Instant::now()),
        );
    }

    /// Une action d'une ligne de serveur (page d'accueil).
    pub(super) fn run_row_action(
        &mut self,
        action: RowAction,
        row: &ServerRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(servers) = self.servers.clone() else {
            return;
        };
        match (action, &row.kind) {
            (RowAction::Start, RowKind::Config(key)) => {
                self.watch_start(key, false, cx);
                servers.update(cx, |servers, cx| servers.start(key, cx));
            }
            (RowAction::Restart, RowKind::Config(key)) => {
                self.watch_start(key, true, cx);
                servers.update(cx, |servers, cx| servers.restart(key, cx));
            }
            (RowAction::Stop, kind) => {
                servers.update(cx, |servers, cx| servers.stop(kind, cx));
            }
            (RowAction::Open, _) if !row.url.is_empty() => {
                self.open_url(&row.url, OpenFrom::Here, window, cx);
            }
            (RowAction::Journal, _) => self.open_journal(row, window, cx),
            _ => {}
        }
    }

    /// « Démarrer » du corps d'une erreur : démarre le serveur dont l'onglet actif a échoué à charger le port, et recharge
    /// l'onglet quand la ligne écoute.
    pub(super) fn start_server_from_error(
        &mut self,
        key: ServerKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(servers) = self.servers.clone() else {
            return;
        };
        self.watch_start(&key, false, cx);
        self.awaiting_server
            .insert(self.tabs.active().id, key.clone());
        servers.update(cx, |servers, cx| servers.start(&key, cx));
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// L'offre de démarrage du corps d'erreur de l'onglet : le serveur dont l'adresse du chargement raté est le port.
    fn start_offer_of(&self, tab: &TabKind, cx: &Context<Self>) -> Option<StartOffer> {
        let TabKind::Web { url } = tab else {
            return None;
        };
        let row = self.servers.as_ref()?.read(cx).row_for_url(url);
        start_offer(row.as_ref())
    }

    /// Les onglets à garder dans `state.json`.
    pub fn saved_tabs(&self) -> SavedTabs {
        self.tabs.saved()
    }

    /// Le focus GPUI est-il dans le volet (sa racine ou le champ d'adresse) ?
    pub fn contains_focused(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    /// Les onglets tels que l'en-tête les dessine.
    pub fn tab_views(&self) -> Vec<TabView> {
        let active = self.tabs.active().id;
        self.tabs
            .tabs()
            .iter()
            .map(|tab| {
                let favicon = self
                    .pages
                    .get(&tab.id)
                    .and_then(|slot| slot.favicon.clone());
                TabView::of(tab, tab.id == active, favicon)
            })
            .collect()
    }

    /// Le volet est-il celui que l'écran montre ? Caché, il cache aussi la vue native de toute page : elle ne se
    /// cache pas d'elle-même. Montré, la page de l'onglet actif apparaît au prochain `prepaint` de son `canvas`.
    pub fn set_shown(&mut self, shown: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.shown == shown {
            return;
        }
        self.shown = shown;
        self.apply_visibility();
        if shown {
            self.create_missing_pages(window, cx);
            self.sync_journals(window, cx);
            self.sync_address(true, window, cx);
        }
        cx.notify();
    }

    /// Un menu d'IdemA s'ouvre ou se ferme sur la zone de la page (P4-4). Ouvert : l'instantané de la page est demandé, et
    /// dès qu'il arrive la page native se cache et l'image se peint à sa place, jusqu'à la fermeture. Fermé : la page
    /// revient (au prochain `prepaint` de son `canvas`) et l'image est oubliée. `Workspace` l'appelle par `defer`, jamais
    /// pendant un `update` : la panique de ré-entrance de la sonde tombait sur ces signaux.
    pub fn set_covered(&mut self, covered: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.covered == covered {
            return;
        }
        self.covered = covered;
        hud::log_debug(if covered {
            "browser covered"
        } else {
            "browser uncovered"
        });
        self.refresh_cover(window, cx);
        // Le journal de debug est tamponné : le recouvrement se lit à l'écran comme au journal, sans attendre un autre
        // événement qui le vide. Hors du fil d'interface, comme le fait `pane_view`, et après le travail du couvrement
        // pour que ses lignes (image prête, image rendue) partent aussi.
        if hud::debug_enabled() {
            cx.background_spawn(async { hud::flush() }).detach();
        }
        // « Inspecter » choisi dans le menu : la page est revenue, l'inspecteur s'ouvre alors (jamais pendant que le menu
        // la recouvre). La vue native revient au `prepaint` de l'image qui suit ce signal : le rappel de l'image
        // d'après, dans l'ordre de GPUI (`on_next_frame` part avant le dessin), la trouve à l'écran.
        if !covered && std::mem::take(&mut self.inspect_after_uncover) {
            let panel = cx.weak_entity();
            window.on_next_frame(move |window, _| {
                window.on_next_frame(move |_, cx| {
                    let _ = panel.update(cx, |panel, _| panel.open_inspector());
                });
            });
        }
        cx.notify();
    }

    /// Met le couvrement d'accord avec `covered` et l'onglet actif : un instantané de la page à l'écran quand un menu la
    /// recouvre, rien sinon. Un couvrement déjà prêt pour l'onglet actif est gardé. Appelé aussi à la naissance d'une page
    /// et au changement d'onglet : une page née sous un menu ouvert n'apparaît jamais par-dessus lui.
    fn refresh_cover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.tabs.active().id;
        let kept = match &self.cover {
            Cover::Pending { tab, .. } | Cover::Covering { tab, .. } => *tab == active,
            Cover::Off => false,
        };
        if self.covered && kept {
            return;
        }
        self.drop_cover(window, cx);
        if !self.covered || !self.shown {
            return;
        }
        let Some(page) = self.live_page() else {
            return;
        };
        let mut answer = None;
        self.cover = cover_start(active, page.is_visible(), || {
            let (sender, receiver) = oneshot::channel();
            answer = Some(receiver);
            page.snapshot(move |png| {
                let _ = sender.send(png);
            })
        });
        let (Some(answer), Cover::Pending { request, .. }) = (answer, &self.cover) else {
            hud::log_debug("browser cover without image, page not on screen");
            return;
        };
        let request = *request;
        // Le PNG se décode hors du fil d'interface, avant que la page ne se cache : l'image est prête à peindre à la
        // même image que celle qui retire la page (pas d'éclat du fond du volet).
        let renderer = cx.svg_renderer();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(png) = answer.await else {
                return;
            };
            let image = cx
                .background_executor()
                .spawn(async move {
                    png.and_then(|bytes| {
                        Image::from_bytes(ImageFormat::Png, bytes)
                            .to_image_data(renderer)
                            .ok()
                    })
                })
                .await;
            let _ = this.update_in(cx, |panel, _, cx| {
                panel.finish_cover(active, request, image, cx);
            });
        })
        .detach();
        // Garde : un WebContent qui ne répond pas ne laisse pas le menu sous la vue native.
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(COVER_TIMEOUT).await;
            let _ = this.update_in(cx, |panel, _, cx| {
                panel.cover_timed_out(active, request, cx);
            });
        })
        .detach();
    }

    /// L'instantané est arrivé (`None` : WebKit ou le décodage a échoué). Celui d'un couvrement dépassé est jeté.
    fn finish_cover(
        &mut self,
        tab: TabId,
        request: u64,
        image: Option<Arc<RenderImage>>,
        cx: &mut Context<Self>,
    ) {
        let cover = std::mem::replace(&mut self.cover, Cover::Off);
        self.cover = cover_after_snapshot(cover, tab, request, image);
        if let Cover::Covering { tab: shown, image } = &self.cover
            && *shown == tab
        {
            hud::log_debug(if image.is_some() {
                "browser cover image ready"
            } else {
                "browser cover image missing, panel background"
            });
        }
        cx.notify();
    }

    /// Le délai de garde de la demande `request` est passé : sans réponse, la page se cache sur le fond du volet, et la
    /// réponse tardive de WebKit est rendue muette.
    fn cover_timed_out(&mut self, tab: TabId, request: u64, cx: &mut Context<Self>) {
        let waiting = matches!(
            &self.cover,
            Cover::Pending { tab: t, request: r } if *t == tab && *r == request
        );
        if !waiting {
            return;
        }
        let cover = std::mem::replace(&mut self.cover, Cover::Off);
        self.cover = cover_after_timeout(cover, tab, request);
        if let Some(slot) = self.pages.get(&tab) {
            slot.page.cancel_snapshots();
        }
        hud::log_debug("browser cover timed out, panel background");
        cx.notify();
    }

    /// Oublie le couvrement : l'instantané encore en vol devient muet, et l'image peinte quitte la texture de GPUI. Elle
    /// n'entre pas dans le cache d'assets (`img` reçoit l'image décodée), seul l'atlas des textures la garde, par fenêtre :
    /// sans ce retrait chaque menu laisserait une capture de la page en mémoire jusqu'à la fin du processus.
    fn drop_cover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cover = std::mem::replace(&mut self.cover, Cover::Off);
        let Some((tab, image)) = cover.into_parts() else {
            return;
        };
        if let Some(slot) = self.pages.get(&tab) {
            slot.page.cancel_snapshots();
        }
        if let Some(image) = image {
            cx.drop_image(image, Some(window));
            hud::log_debug("browser cover image released");
        }
    }

    /// Ouvre l'inspecteur de la page de l'onglet actif (API privée, E-P4-c). Le journal dit la demande, pas son effet :
    /// la fenêtre de l'inspecteur n'a été vue à l'écran par aucun test.
    fn open_inspector(&mut self) {
        let Some(slot) = self.active_slot() else {
            return;
        };
        if slot.page.show_inspector() {
            hud::log_event("browser inspector requested");
        } else {
            // L'API privée manque : l'entrée n'est plus proposée, le clic droit de la page reste.
            self.inspector_available = false;
            hud::log_debug("browser inspector unavailable");
        }
    }

    /// Cache toute page qui n'est pas l'écran : le volet caché, ou un autre onglet actif. Montrer la page de
    /// l'onglet actif revient au `prepaint` de son `canvas` (cadre, puis `set_visible(true)`).
    fn apply_visibility(&self) {
        let active = self.tabs.active().id;
        for (id, slot) in &self.pages {
            if !(self.shown && *id == active) {
                slot.page.set_visible(false);
            }
        }
    }

    /// Ouvre une adresse. Un onglet qui la montre déjà (au fragment près) devient actif ; sinon `Here` remplace
    /// l'Accueil ou navigue l'onglet actif, et `Link` ouvre un onglet juste après l'actif.
    pub fn open_url(
        &mut self,
        url: &str,
        from: OpenFrom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_url_with(url, from, false, window, cx);
    }

    /// `reload_known` : la validation de la barre d'adresse, qui charge même l'adresse que l'onglet actif montre déjà.
    fn open_url_with(
        &mut self,
        url: &str,
        from: OpenFrom,
        reload_known: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Un fichier perd sa requête et son fragment ici : `load_file` ne sait pas les honorer (arbitrage 13).
        let url = address::tab_address(url);
        let url = url.as_str();
        let before = TabSnapshot::of(&self.tabs);
        let known = self.tabs.tabs().iter().any(
            |tab| matches!(&tab.kind, TabKind::Web { url: shown } if same_address(shown, url)),
        );
        let opened = self.tabs.open_url(url, from);
        if page_load_needed(
            known,
            opened.created,
            reload_known,
            opened.id == before.active,
        ) {
            if known {
                // Le fragment tapé devient celui de l'onglet, même si sa page naît plus tard.
                self.tabs.navigated(opened.id, url);
            }
            // L'onglet existant navigue : sa page charge l'adresse. Une page encore à naître la lira dans l'onglet.
            if let Some(slot) = self.pages.get_mut(&opened.id) {
                slot.state = PageState::default();
                load_url(&slot.page, url);
            }
        }
        before.log_changes(&self.tabs);
        self.after_tab_change(&before, window, cx);
    }

    /// Un onglet Accueil neuf, actif, le champ d'adresse prêt à recevoir une adresse.
    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let before = TabSnapshot::of(&self.tabs);
        self.tabs.open_start();
        before.log_changes(&self.tabs);
        self.after_tab_change(&before, window, cx);
        self.focus_address_field(window, cx);
    }

    pub fn activate_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let before = TabSnapshot::of(&self.tabs);
        if !self.tabs.activate(id) {
            return;
        }
        before.log_changes(&self.tabs);
        self.after_tab_change(&before, window, cx);
    }

    /// Ferme un onglet : sa page part avec lui, et ses événements en vol sont jetés. Fermer le dernier (Accueil ou page
    /// web) ferme aussi le volet ; `state.json` garde alors un seul Accueil neuf, celui que `⌘⇧B` montrera.
    pub fn close_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let last = self.tabs.is_single() && self.tabs.get(id).is_some();
        let before = TabSnapshot::of(&self.tabs);
        if self.tabs.close(id).is_none() {
            return;
        }
        if let Some(slot) = self.pages.remove(&id) {
            // Le `canvas` de la dernière image garde un `Rc` sur la page : la vue native ne disparaît pas avec
            // `Drop` tout de suite, on la cache (et le clavier revient à GPUI) avant de lâcher le créneau.
            slot.page.set_visible(false);
        }
        self.creating.remove(&id);
        self.journals.remove(&id);
        self.journal_subscriptions.remove(&id);
        self.awaiting_server.remove(&id);
        self.reloading_after_start.remove(&id);
        before.log_changes(&self.tabs);
        self.after_tab_change(&before, window, cx);
        if last {
            cx.emit(BrowserEvent::CloseRequested);
        }
    }

    /// Ce qui suit tout geste sur les onglets : seule la page de l'onglet actif reste montrée, elle naît si besoin, le
    /// champ d'adresse et `state.json` suivent.
    fn after_tab_change(
        &mut self,
        before: &TabSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // La barre de recherche cherche dans une page : elle ne suit pas un autre onglet.
        self.find = None;
        // La demande de clavier de la page est celle de l'onglet d'avant le geste.
        let active = self.tabs.active();
        let view = self.pages.get(&active.id).map(|slot| &slot.view);
        let kept = page_focus_still_wanted(
            self.page_focus_wanted.get(),
            active.id != before.active,
            view,
            false,
        );
        self.page_focus_wanted.set(kept);
        self.apply_visibility();
        self.create_missing_pages(window, cx);
        self.sync_journals(window, cx);
        // Un menu ouvert recouvre la page de l'onglet qui vient d'être actif : son instantané suit l'onglet.
        self.refresh_cover(window, cx);
        self.sync_address(true, window, cx);
        cx.emit(BrowserEvent::TabsChanged);
        cx.notify();
    }

    /// Demande la création des pages qui manquent. Une `WKWebView` ne se crée jamais pendant un `update` en cours
    /// (panique de ré-entrance de GPUI, P4-R4) : la création part dans le tour suivant.
    fn create_missing_pages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let existing: HashSet<TabId> = self
            .pages
            .keys()
            .chain(self.creating.iter())
            .copied()
            .collect();
        for id in pages_to_create(&self.tabs, &existing, self.shown) {
            self.creating.insert(id);
            let panel = cx.weak_entity();
            window.defer(cx, move |window, cx| {
                let _ = panel.update(cx, |panel, cx| panel.create_page(id, window, cx));
            });
        }
    }

    /// Crée la page de l'onglet `id` et la charge. Le premier appel coûte une demi-seconde (WebKit froid).
    fn create_page(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        self.creating.remove(&id);
        // L'onglet a pu changer ou disparaître pendant le délai : seul ce qui est encore voulu est créé.
        let existing: HashSet<TabId> = self.pages.keys().copied().collect();
        if !pages_to_create(&self.tabs, &existing, self.shown).contains(&id) {
            return;
        }
        let Some(TabKind::Web { url }) = self.tabs.get(id).map(|tab| tab.kind.clone()) else {
            return;
        };
        let (sender, mut receiver) = mpsc::unbounded();
        let Some(page) = WebPage::new(window, sender) else {
            hud::log_event(&format!("browser page unavailable id={id}"));
            return;
        };
        load_url(&page, &url);
        let reader = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = receiver.next().await {
                let delivered = this.update_in(cx, |panel, window, cx| {
                    panel.on_page_event(id, event, window, cx);
                });
                if delivered.is_err() {
                    break;
                }
            }
        });
        self.pages.insert(
            id,
            PageSlot {
                page: Rc::new(page),
                state: PageState::default(),
                view: TabState::Live,
                download_line: None,
                favicon: None,
                _reader: reader,
            },
        );
        // Un menu ouvert à cet instant recouvre la page qui vient de naître : elle ne doit pas apparaître par-dessus.
        self.refresh_cover(window, cx);
        cx.notify();
    }

    /// Un événement d'une page. Celui d'un onglet fermé est jeté.
    fn on_page_event(
        &mut self,
        id: TabId,
        event: PageEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.get(id).is_none() || !self.pages.contains_key(&id) {
            return;
        }
        let is_active = self.tabs.active().id == id;
        // La page a reçu du contenu ou a de nouveau échoué : l'attente d'un serveur qui démarre est finie.
        if matches!(event, PageEvent::Committed(_) | PageEvent::Failed { .. }) {
            self.awaiting_server.remove(&id);
            self.reloading_after_start.remove(&id);
        }
        if let Some(slot) = self.pages.get_mut(&id) {
            slot.view = std::mem::replace(&mut slot.view, TabState::Live).after(&event);
            slot.download_line = download_line_after(slot.download_line.take(), &event);
            // La page native ne se cache pas d'elle-même : un échec ou un arrêt la retire de l'écran, et le corps
            // n'y repose plus son cadre (`render_body`).
            if slot.view != TabState::Live {
                slot.page.set_visible(false);
                // La barre de recherche cherche dans une page qui n'est plus à l'écran : elle se ferme.
                if is_active {
                    self.find = None;
                }
            }
        }
        match event {
            // `state.json` s'écrit sur le fil principal : les titres sont regroupés, au plus une écriture par
            // `TITLE_SAVE_DELAY`, au lieu d'une par changement.
            PageEvent::Title(title) => {
                self.tabs
                    .set_title(id, title.filter(|title| !title.trim().is_empty()));
                if !self.title_save_pending {
                    self.title_save_pending = true;
                    cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(TITLE_SAVE_DELAY).await;
                        let _ = this.update(cx, |panel, cx| {
                            panel.title_save_pending = false;
                            cx.emit(BrowserEvent::TabsChanged);
                        });
                    })
                    .detach();
                }
            }
            PageEvent::Url(Some(url)) => {
                // Une page en échec ou arrêtée garde l'adresse qui a échoué : celle que WebKit rapporte alors est celle
                // de la page d'avant.
                let live = self
                    .pages
                    .get(&id)
                    .is_some_and(|slot| slot.view == TabState::Live);
                if live && is_page_url(&url) {
                    self.tabs.navigated(id, &url);
                    cx.emit(BrowserEvent::TabsChanged);
                    if is_active {
                        self.sync_address(false, window, cx);
                    }
                }
            }
            PageEvent::Url(None) => {}
            PageEvent::Loading { loading, progress } => {
                if let Some(slot) = self.pages.get_mut(&id) {
                    slot.state.loading = loading;
                    slot.state.progress = progress;
                }
            }
            PageEvent::History { back, forward } => {
                if let Some(slot) = self.pages.get_mut(&id) {
                    slot.state.can_go_back = back;
                    slot.state.can_go_forward = forward;
                }
            }
            PageEvent::Committed(url) => {
                hud::log_event(&format!(
                    "browser navigated id={id} url={}",
                    address::log_form(&url)
                ));
                if is_page_url(&url) {
                    self.tabs.navigated(id, &url);
                    cx.emit(BrowserEvent::TabsChanged);
                    if is_active {
                        self.sync_address(false, window, cx);
                    }
                }
                // Une autre origine : l'icône d'avant n'est plus la bonne (celle du cache, ou le globe).
                let cached =
                    webkit::page_origin(&url).and_then(|origin| self.favicons.get(&origin));
                if let Some(slot) = self.pages.get_mut(&id) {
                    slot.favicon = cached;
                }
            }
            PageEvent::Finished(url) => {
                hud::log_event(&format!(
                    "browser loaded id={id} url={}",
                    address::log_form(&url)
                ));
                // Une origine dont l'icône n'est pas encore connue : une lecture de la page, sans bloquer.
                if let Some(origin) = webkit::page_origin(&url)
                    && let Some(slot) = self.pages.get(&id)
                    && slot.favicon.is_none()
                {
                    slot.page.request_favicon(&origin);
                }
            }
            PageEvent::Favicon { origin, png } => {
                // La réponse d'une page qui a changé d'origine depuis la demande est jetée.
                if self.tab_origin(id).as_deref() == Some(origin.as_str()) {
                    // Le PNG se décode hors du fil d'interface (comme l'instantané du couvrement).
                    let renderer = cx.svg_renderer();
                    cx.spawn_in(window, async move |this, cx| {
                        let image = cx
                            .background_executor()
                            .spawn(async move {
                                Image::from_bytes(ImageFormat::Png, png)
                                    .to_image_data(renderer)
                                    .ok()
                            })
                            .await;
                        let _ = this.update_in(cx, |panel, window, cx| {
                            panel.favicon_ready(origin, image, window, cx);
                        });
                    })
                    .detach();
                }
            }
            PageEvent::Failed { code, url, .. } => {
                // L'onglet prend l'adresse qui a échoué : « Réessayer » la recharge, et le journal la dit.
                if let Some(failed) = url.as_deref().filter(|failed| is_page_url(failed)) {
                    self.tabs.navigated(id, failed);
                    cx.emit(BrowserEvent::TabsChanged);
                    if is_active {
                        self.sync_address(false, window, cx);
                    }
                }
                // La description d'une erreur peut citer l'adresse avec sa requête : le journal n'en garde que le code.
                let url = match self.tabs.get(id).map(|tab| &tab.kind) {
                    Some(TabKind::Web { url }) => address::log_form(url),
                    _ => "-".to_owned(),
                };
                hud::log_event(&format!(
                    "browser load failed id={id} code={code} url={url}"
                ));
            }
            PageEvent::Terminated => {
                hud::log_event(&format!("browser page terminated id={id}"));
            }
            PageEvent::OpenTab(url) => self.open_url(&url, OpenFrom::Link, window, cx),
            PageEvent::PopupOpened(host) => {
                hud::log_event(&format!("browser popup opened host={host}"));
            }
            PageEvent::PopupClosed => hud::log_event("browser popup closed"),
            PageEvent::DownloadFinished(path) => {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                hud::log_event(&format!(
                    "browser download finished name={}",
                    journal_name(&name)
                ));
                cx.emit(BrowserEvent::Reveal(path));
            }
            PageEvent::DownloadFailed(_) => hud::log_event("browser download failed"),
            PageEvent::HistoryKey(key) => {
                // Seul l'onglet actif a le clavier : l'événement d'un autre est jeté.
                if is_active {
                    match key {
                        HistoryKey::Back => self.go_back(),
                        HistoryKey::Forward => self.go_forward(),
                    }
                }
            }
            PageEvent::FocusedByClick => {
                // Un clic dans la page : le clavier est à elle (AppKit), le focus GPUI au volet pour que ses
                // raccourcis restent les siens.
                window.focus(&self.focus_handle, cx);
                cx.emit(BrowserEvent::FocusChanged);
            }
        }
        cx.notify();
    }

    /// L'origine de la page de l'onglet `id` (rien pour l'Accueil, un fichier ou une adresse sans hôte).
    fn tab_origin(&self, id: TabId) -> Option<String> {
        match self.tabs.get(id).map(|tab| &tab.kind) {
            Some(TabKind::Web { url }) => webkit::page_origin(url),
            _ => None,
        }
    }

    /// Le favicon de `origin` est décodé (`None` : le décodage a échoué, l'onglet garde son globe). Il entre au cache, et
    /// tout onglet qui est encore sur cette origine le prend.
    fn favicon_ready(
        &mut self,
        origin: String,
        image: Option<Arc<RenderImage>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(image) = image else {
            return;
        };
        // Les onglets de cette origine prennent l'image d'abord : l'ancienne qu'ils montraient n'est plus à personne.
        let on_origin: Vec<TabId> = self
            .pages
            .keys()
            .copied()
            .filter(|id| self.tab_origin(*id).as_deref() == Some(origin.as_str()))
            .collect();
        for id in on_origin {
            if let Some(slot) = self.pages.get_mut(&id) {
                slot.favicon = Some(Arc::clone(&image));
            }
        }
        // Ce qui sort du cache quitte aussi la texture de GPUI (comme l'instantané du couvrement, `drop_cover`), sauf une
        // image qu'un onglet montre encore : la rendre la ferait disparaître de son onglet.
        let pages = &self.pages;
        let gone = self.favicons.insert(origin, image, |image| {
            pages.values().any(|slot| {
                slot.favicon
                    .as_ref()
                    .is_some_and(|shown| Arc::ptr_eq(shown, image))
            })
        });
        for old in gone {
            cx.drop_image(old, Some(window));
        }
        hud::log_debug("browser favicon applied");
        cx.notify();
    }

    /// Le texte de la barre d'adresse : l'adresse de la page, vide sur l'Accueil.
    fn address_text(&self) -> String {
        match &self.tabs.active().kind {
            TabKind::Start | TabKind::Log { .. } => String::new(),
            TabKind::Web { url } => url.clone(),
        }
    }

    /// Met le champ d'adresse à l'adresse de l'onglet actif. Pendant que l'utilisateur tape, une adresse qui bouge ne
    /// lui reprend pas sa saisie, sauf geste explicite (`force` : changement d'onglet, validation).
    fn sync_address(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.address.focus_handle(cx).is_focused(window);
        if focused && !force {
            return;
        }
        let text = self.address_text();
        if self.address.read(cx).value().as_ref() != text {
            self.address
                .update(cx, |field, cx| field.set_value(text, window, cx));
        }
    }

    fn on_address_event(
        &mut self,
        field: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => {
                let text = field.read(cx).value().to_string();
                self.submit_address(&text, window, cx);
            }
            // Le champ GPUI prend le clavier : la page le lui rend, sinon la frappe irait dans son champ éditable
            // (rapport de sonde §4).
            InputEvent::Focus => self.release_page_keyboard(),
            // Une saisie abandonnée rend le champ à l'adresse de la page.
            InputEvent::Blur => self.sync_address(false, window, cx),
            // La loupe du champ vide suit la saisie.
            InputEvent::Change => cx.notify(),
        }
    }

    /// Valide le texte de la barre : une adresse, un fichier ou une recherche, chargé dans l'onglet actif.
    fn submit_address(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let Some(address) = address::interpret(text, &home) else {
            return;
        };
        let url = address::url_of(&address);
        self.open_url_with(&url, OpenFrom::Here, true, window, cx);
        window.focus(&self.focus_handle, cx);
    }

    fn focus_address(&mut self, _: &FocusAddress, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_address_field(window, cx);
    }

    /// `⌘L` : le champ d'adresse prend le focus, son texte sélectionné.
    pub fn focus_address_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.release_page_keyboard();
        self.address.update(cx, |field, cx| {
            field.focus(window, cx);
            field.select_all(window, cx);
        });
    }

    /// La page de l'onglet actif prend le clavier dès qu'elle est à l'écran (`⌘⇧B` qui ouvre le volet). Sur l'Accueil il n'y
    /// a pas de page : le focus GPUI du volet suffit.
    pub fn focus_active_page(&mut self, cx: &mut Context<Self>) {
        let on_a_page = matches!(self.tabs.active().kind, TabKind::Web { .. });
        self.page_focus_wanted.set(on_a_page);
        // Un rendu : le `canvas` de la page pose le cadre, la montre, puis lui donne le clavier.
        cx.notify();
    }

    /// La `GPUIView` redevient le premier répondant si la page de l'onglet actif l'avait.
    fn release_page_keyboard(&self) {
        self.page_focus_wanted.set(false);
        if let Some(slot) = self.pages.get(&self.tabs.active().id)
            && slot.page.has_keyboard()
        {
            slot.page.release_focus();
        }
    }

    fn active_slot(&self) -> Option<&PageSlot> {
        self.pages.get(&self.tabs.active().id)
    }

    /// `⌘W` : ferme l'onglet actif.
    fn close_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.tabs.active().id;
        self.close_tab(id, window, cx);
    }

    /// `⌘⇧]` et `⌘⇧[` : l'onglet suivant ou précédent, en boucle.
    fn cycle_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let before = TabSnapshot::of(&self.tabs);
        if forward {
            self.tabs.next();
        } else {
            self.tabs.previous();
        }
        before.log_changes(&self.tabs);
        self.after_tab_change(&before, window, cx);
    }

    /// `⌘R` : recharge la page (ou rejoue l'adresse en erreur). Jamais « arrêter » : le geste du clavier ne se transforme
    /// pas en arrêt quand un chargement est en cours.
    fn reload_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slot) = self.active_slot() else {
            return;
        };
        match reload_gesture(false, &slot.view, false) {
            ReloadGesture::Retry => self.retry(window, cx),
            _ => slot.page.reload(),
        }
    }

    /// `⌘Z` et `⇧⌘Z` : annuler et rétablir de la page, quand elle a le clavier ; sinon l'action continue son chemin.
    fn page_edit(&mut self, command: EditCommand, cx: &mut Context<Self>) {
        match self.active_slot().filter(|slot| slot.page.has_keyboard()) {
            Some(slot) => slot.page.send_edit(command),
            None => cx.propagate(),
        }
    }

    /// `⌘←` et `⌘→` : reculer et avancer, quand GPUI a le clavier sans champ de saisie (le volet après un clic sur la rangée
    /// d'adresse, l'Accueil, une page en échec), comme `⌘[` et `⌘]`. Quand la page a le clavier, la touche continue son
    /// chemin : GPUI la prend avant la page, qui la garde pour le curseur d'un champ éditable, et ce que la page laisse
    /// revient par `IdemaWebView::keyDown:` (`PageEvent::HistoryKey`).
    fn history_key(&mut self, key: HistoryKey, cx: &mut Context<Self>) {
        if self
            .active_slot()
            .is_some_and(|slot| slot.page.has_keyboard())
        {
            cx.propagate();
            return;
        }
        match key {
            HistoryKey::Back => self.go_back(),
            HistoryKey::Forward => self.go_forward(),
        }
    }

    fn go_back(&self) {
        if let Some(slot) = self.active_slot() {
            slot.page.go_back();
        }
    }

    fn go_forward(&self) {
        if let Some(slot) = self.active_slot() {
            slot.page.go_forward();
        }
    }

    /// Le bouton recharger : recharge la page, arrête son chargement s'il est en cours, ou, quand l'onglet montre une
    /// erreur, rejoue l'adresse qui a échoué (le `reload` de WebKit rechargerait la page d'avant).
    fn reload_or_stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slot) = self.active_slot() else {
            return;
        };
        match reload_gesture(slot.state.loading, &slot.view, false) {
            ReloadGesture::Stop => slot.page.stop(),
            ReloadGesture::Reload | ReloadGesture::ReloadFromOrigin => slot.page.reload(),
            ReloadGesture::Retry => self.retry(window, cx),
        }
    }

    /// « Réessayer » et « Recharger » : charge de nouveau l'adresse de l'onglet. L'erreur reste affichée jusqu'à ce que
    /// la navigation reçoive du contenu (`TabState::after`), le filet de progression montre que quelque chose se passe.
    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.tabs.active().id;
        self.retry_tab(id, cx);
        window.focus(&self.focus_handle, cx);
    }

    /// Charge de nouveau l'adresse de l'onglet `id`, actif ou non (le serveur qu'il attendait vient de démarrer).
    fn retry_tab(&mut self, id: TabId, cx: &mut Context<Self>) {
        let Some(TabKind::Web { url }) = self.tabs.get(id).map(|tab| tab.kind.clone()) else {
            return;
        };
        if let Some(slot) = self.pages.get_mut(&id) {
            // L'historique de la page d'avant reste vrai : seuls le chargement et sa progression repartent.
            slot.state.loading = false;
            slot.state.progress = 0.;
            load_url(&slot.page, &url);
        }
        cx.notify();
    }

    /// L'adresse de l'onglet actif, s'il est web.
    fn active_url(&self) -> Option<String> {
        match &self.tabs.active().kind {
            TabKind::Web { url } => Some(url.clone()),
            TabKind::Start | TabKind::Log { .. } => None,
        }
    }

    /// Ouvre le menu `more_vert` en `position` : `Workspace` le pose, ses entrées rappellent `run_menu_action`.
    pub fn open_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let lines = menu_lines(
            self.active_url().is_some(),
            self.active_slot().is_some(),
            self.inspector_available,
        );
        let panel = cx.entity();
        let entries = lines
            .into_iter()
            .map(|line| {
                let panel = panel.clone();
                let action = line.action;
                MenuEntry::Item(
                    MenuItemSpec::new(action.label(), move |_, window, cx| {
                        panel.update(cx, |panel, cx| panel.run_menu_action(action, window, cx));
                    })
                    .enabled(line.enabled),
                )
            })
            .collect();
        cx.emit(BrowserEvent::OpenMenu {
            position,
            spec: MenuSpec {
                title: None,
                entries,
            },
        });
    }

    fn run_menu_action(&mut self, action: MenuAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            MenuAction::OpenInDefaultBrowser => {
                if let Some(url) = self.active_url() {
                    cx.open_url(&url);
                }
            }
            MenuAction::CopyAddress => {
                if let Some(url) = self.active_url() {
                    cx.write_to_clipboard(ClipboardItem::new_string(url));
                }
            }
            MenuAction::Inspect => {
                if self.covered {
                    // Le menu recouvre encore la page : l'inspecteur attend sa fermeture (`set_covered(false)`).
                    self.inspect_after_uncover = true;
                } else {
                    self.open_inspector();
                }
            }
            MenuAction::ReloadFromOrigin => {
                let Some(slot) = self.active_slot() else {
                    return;
                };
                match reload_gesture(false, &slot.view, true) {
                    ReloadGesture::Retry => self.retry(window, cx),
                    _ => slot.page.reload_from_origin(),
                }
            }
        }
    }

    /// Une page vivante à l'écran : celle de l'onglet actif, sans erreur. Le zoom et la recherche ne parlent qu'à elle.
    fn live_page(&self) -> Option<Rc<WebPage>> {
        self.active_slot()
            .filter(|slot| slot.view == TabState::Live)
            .map(|slot| Rc::clone(&slot.page))
    }

    /// `⌘F` : ouvre la barre sous la rangée d'adresse (ou, déjà ouverte, reprend le champ et sélectionne son texte).
    fn find_in_page(&mut self, _: &FindInPage, window: &mut Window, cx: &mut Context<Self>) {
        if self.live_page().is_none() {
            return;
        }
        if self.find.is_none() {
            let field = cx.new(|cx| InputState::new(window, cx).placeholder(find::PLACEHOLDER));
            let subscription = cx.subscribe_in(&field, window, Self::on_find_event);
            self.find = Some(FindBar {
                field,
                status: FindStatus::Idle,
                request: 0,
                _subscription: subscription,
            });
            cx.notify();
        }
        self.focus_find_field(window, cx);
    }

    /// Le champ de recherche prend le focus, son texte sélectionné : la frappe suivante le remplace.
    fn focus_find_field(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = &self.find else {
            return;
        };
        self.release_page_keyboard();
        bar.field.update(cx, |field, cx| {
            field.focus(window, cx);
            field.select_all(window, cx);
        });
    }

    fn on_find_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // Le texte change : la recherche repart, vers l'avant.
            InputEvent::Change => self.run_find(false, window, cx),
            InputEvent::PressEnter { shift, .. } => self.run_find(*shift, window, cx),
            InputEvent::Focus => self.release_page_keyboard(),
            InputEvent::Blur => {}
        }
    }

    fn find_next(&mut self, _: &FindNext, window: &mut Window, cx: &mut Context<Self>) {
        self.run_find(false, window, cx);
    }

    fn find_previous(&mut self, _: &FindPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.run_find(true, window, cx);
    }

    /// Cherche le texte du champ dans la page. La réponse de WebKit arrive plus tard : elle porte le numéro de la
    /// demande, et une réponse dépassée est jetée.
    fn run_find(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.live_page() else {
            return;
        };
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        bar.request += 1;
        let request = bar.request;
        let text = bar.field.read(cx).value().to_string();
        if text.is_empty() {
            bar.status = FindStatus::Idle;
            cx.notify();
            return;
        }
        let (sender, receiver) = oneshot::channel();
        page.find(&text, backwards, move |found| {
            let _ = sender.send(found);
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(found) = receiver.await {
                let _ = this.update_in(cx, |panel, _, cx| panel.finish_find(request, found, cx));
            }
        })
        .detach();
    }

    fn finish_find(&mut self, request: u64, found: bool, cx: &mut Context<Self>) {
        if let Some(bar) = self.find.as_mut()
            && bar.request == request
        {
            bar.status = if found {
                FindStatus::Found
            } else {
                FindStatus::NotFound
            };
            cx.notify();
        }
    }

    /// Un pas dans la barre ouverte (ses boutons) : le champ garde le focus.
    fn step_find(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.run_find(backwards, window, cx);
        if let Some(bar) = &self.find {
            window.focus(&bar.field.focus_handle(cx), cx);
        }
    }

    /// Ferme la barre ; le focus revient au volet.
    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.take().is_some() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    /// Le zoom de la page vivante, un pas ou retour à 100 %. Il reste à l'onglet, dans sa `WebPage`.
    fn zoom(&mut self, step: ZoomStep, cx: &mut Context<Self>) {
        if let Some(page) = self.live_page() {
            page.set_zoom(stepped_zoom(page.zoom(), step));
            cx.notify();
        }
    }

    /// Le corps : la page d'accueil, ou la zone que la vue native de la page recouvre.
    fn render_body(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let tab = self.tabs.active();
        // La demande de clavier ne survit ni à une page en échec, ni à un menu qui la recouvre : le `canvas` de la page,
        // seul à la prendre, n'est alors pas dessiné.
        let view = self.pages.get(&tab.id).map(|slot| &slot.view);
        let covered = !page_shown_under(&self.cover, tab.id);
        let kept = page_focus_still_wanted(self.page_focus_wanted.get(), false, view, covered);
        self.page_focus_wanted.set(kept);
        match &tab.kind {
            TabKind::Start => start_page::render(self, theme, cx).into_any_element(),
            TabKind::Log { .. } => self.journal_body(tab, theme, cx),
            TabKind::Web { .. } => match self.pages.get(&tab.id) {
                Some(slot) => match &slot.view {
                    TabState::Live if page_shown_under(&self.cover, tab.id) => {
                        page_area(Rc::clone(&slot.page), Rc::clone(&self.page_focus_wanted))
                            .into_any_element()
                    }
                    TabState::Live => {
                        let image = match &self.cover {
                            Cover::Covering { image, .. } => image.clone(),
                            _ => None,
                        };
                        covered_area(Rc::clone(&slot.page), image).into_any_element()
                    }
                    TabState::Failed(_) if self.reloading_after_start.contains(&tab.id) => {
                        chrome::error_body("Chargement de la page…", None, theme, cx)
                            .into_any_element()
                    }
                    TabState::Failed(error) => match self.start_offer_of(&tab.kind, cx) {
                        // Le port d'une entrée de configuration à l'arrêt : le serveur se nomme, « Démarrer » le lance.
                        Some(offer) => chrome::error_body(
                            &offer.message,
                            offer
                                .key
                                .map(|key| ("Démarrer", ErrorAction::StartServer(key))),
                            theme,
                            cx,
                        )
                        .into_any_element(),
                        None => chrome::error_body(
                            &error.message,
                            Some(("Réessayer", ErrorAction::Retry)),
                            theme,
                            cx,
                        )
                        .into_any_element(),
                    },
                    TabState::Terminated => chrome::error_body(
                        "La page s'est arrêtée",
                        Some(("Recharger", ErrorAction::Retry)),
                        theme,
                        cx,
                    )
                    .into_any_element(),
                },
                // La page naît dans le tour suivant (`create_page`) : en attendant, le fond du volet.
                None => div().size_full().into_any_element(),
            },
        }
    }
}

impl Render for BrowserPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        let nav = chrome::nav_row(self, &theme, cx);
        let find_row = self
            .find
            .as_ref()
            .map(|bar| chrome::find_row(bar, &theme, cx));
        let download_line = self
            .active_slot()
            .and_then(|slot| slot.download_line.clone())
            .map(|line| chrome::download_line(line, &theme));
        let body = self.render_body(&theme, cx);
        // `finding` ouvre `⌘G` et `⌘⇧G` (suivant, précédent) tant que la barre existe.
        let key_context = if self.find.is_some() {
            format!("{KEY_CONTEXT} finding")
        } else {
            KEY_CONTEXT.to_owned()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context(key_context.as_str())
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.new_tab(window, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                this.close_active_tab(window, cx);
            }))
            .on_action(cx.listener(|this, _: &Reload, window, cx| this.reload_page(window, cx)))
            .on_action(cx.listener(|this, _: &GoBack, _, _| this.go_back()))
            .on_action(cx.listener(|this, _: &GoForward, _, _| this.go_forward()))
            .on_action(cx.listener(|this, _: &GoBackKey, _, cx| {
                this.history_key(HistoryKey::Back, cx);
            }))
            .on_action(cx.listener(|this, _: &GoForwardKey, _, cx| {
                this.history_key(HistoryKey::Forward, cx);
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                this.cycle_tab(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousTab, window, cx| {
                this.cycle_tab(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PageUndo, _, cx| {
                this.page_edit(EditCommand::Undo, cx);
            }))
            .on_action(cx.listener(|this, _: &PageRedo, _, cx| {
                this.page_edit(EditCommand::Redo, cx);
            }))
            .on_action(cx.listener(Self::focus_address))
            .on_action(cx.listener(Self::find_in_page))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_previous))
            .on_action(cx.listener(|this, _: &CloseFind, window, cx| this.close_find(window, cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(ZoomStep::In, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(ZoomStep::Out, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(ZoomStep::Reset, cx)))
            .child(nav)
            .children(find_row)
            .children(download_line)
            .child(div().relative().flex_1().min_h_0().child(body))
    }
}

/// La zone de la page : un `canvas` plein qui pose le cadre de la vue native à chaque image, puis la montre. Un appui
/// de souris GPUI tombe forcément hors de la page (celle-ci reçoit les siens en AppKit) : il lui reprend le clavier.
/// `focus_wanted` : la page prend le clavier dès qu'elle est posée et montrée (`⌘⇧B`), la demande est consommée.
fn page_area(page: Rc<WebPage>, focus_wanted: Rc<Cell<bool>>) -> impl IntoElement {
    let on_paint = Rc::clone(&page);
    canvas(
        move |bounds, window, _| {
            let viewport = window.viewport_size();
            // Le filet de la fenêtre reste visible sous une page claire (bords droit et bas).
            page.set_frame(inset_from_window_edge(bounds, viewport), viewport.height);
            page.set_visible(true);
            if focus_wanted.take() {
                page.focus();
            }
        },
        move |_, (), window, _| {
            let page = Rc::clone(&on_paint);
            window.on_mouse_event(move |_: &MouseDownEvent, phase, _, _| {
                if phase == DispatchPhase::Capture && page.has_keyboard() {
                    page.release_focus();
                }
            });
        },
    )
    .size_full()
}

/// La zone de la page pendant qu'un menu la recouvre : la page native se cache (à chaque image, comme `page_area` la
/// montre) et son instantané se peint à sa place, par-dessous le menu GPUI. Sans image (repli), le fond du volet.
fn covered_area(page: Rc<WebPage>, image: Option<Arc<RenderImage>>) -> impl IntoElement {
    div()
        .relative()
        .size_full()
        .bg(color(PANEL))
        .children(image.map(|image| img(image).size_full()))
        .child(
            canvas(move |_, _, _| page.set_visible(false), |_, (), _, _| {})
                .absolute()
                .size_full(),
        )
}

/// Le volet dans la fenêtre : la coque commune des volets droits, l'en-tête des onglets, puis le volet lui-même
/// (rangée d'adresse et corps). `header_inset` est le retrait gauche de l'en-tête (les feux, barre repliée).
pub fn render_panel(
    panel: Entity<BrowserPanel>,
    maximized: bool,
    header_inset: Pixels,
    width: Pixels,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let tabs = panel.read(cx).tab_views();
    let (maximize_icon, maximize_size) = if maximized {
        (Icon::CloseFullscreen, 16.)
    } else {
        (Icon::OpenInFull, 16.)
    };
    // `width` est la largeur réelle du volet : agrandi, `Workspace` passe celle que la fenêtre laisse à côté de la barre
    // latérale, pour que le mode étroit des onglets s'y calcule aussi.
    let tabs_room = Some(strip_width(width, header_inset, theme));
    let header = div()
        .h(theme.px(HEADER_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.px(4.))
        .pl(header_inset)
        .pr(theme.px(8.))
        // L'en-tête déplace la fenêtre, et un appui y rend la saisie au volet.
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.focus_browser(window, cx);
                drag_window(event, window, cx);
            }),
        )
        .child(tab_strip(tabs, &panel, tabs_room, theme, cx))
        .child(new_tab_button(&panel, theme, cx))
        .child(div().flex_1())
        .child(more_button(&panel, theme, cx))
        .child(header_button(
            theme,
            "browser-maximize",
            maximize_icon,
            maximize_size,
            |this, window, cx| this.toggle_browser_maximized(window, cx),
            cx,
        ))
        .child(header_button(
            theme,
            "browser-close",
            Icon::Close,
            18.,
            |this, window, cx| this.close_browser(window, cx),
            cx,
        ));
    panel_shell("browser-panel", maximized, width)
        .child(header)
        .child(div().flex_1().min_h_0().child(panel))
}

/// Les onglets à qui il faut une page : l'actif seulement, s'il est web, sans page, et le volet montré. Une page naît
/// à la première apparition de son onglet, jamais avant : un onglet restauré ne charge rien tant qu'il n'est pas vu.
pub fn pages_to_create(tabs: &BrowserTabs, existing: &HashSet<TabId>, shown: bool) -> Vec<TabId> {
    let active = tabs.active();
    if shown && matches!(active.kind, TabKind::Web { .. }) && !existing.contains(&active.id) {
        vec![active.id]
    } else {
        Vec::new()
    }
}

/// Une adresse que le volet garde pour un onglet : web ou fichier, pas `about:blank` ni un schéma interne.
fn is_page_url(url: &str) -> bool {
    // `get` : une coupe à l'octet au milieu d'un caractère non ASCII donnerait `None`, pas une panique.
    ["http://", "https://", "file://"].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    })
}

/// Faut-il (re)charger la page après `BrowserTabs::open_url` ? Une adresse neuve dans un onglet existant se charge ;
/// une adresse que l'onglet montre déjà ne se recharge que sur validation de la barre (`reload_known`, « Entrée
/// charge », spec §4.3) et seulement dans l'onglet actif d'avant le geste : un changement de fragment navigue alors.
/// Un onglet créé charge sa page à sa naissance.
fn page_load_needed(known: bool, created: bool, reload_known: bool, was_active: bool) -> bool {
    !created && (!known || (reload_known && was_active))
}

/// La demande de clavier de la page (`page_focus_wanted`) tient-elle encore ? Elle ne vaut que pour la page de l'onglet
/// qui l'a faite, tant que cette page est à l'écran ou à naître : un autre onglet actif, une page en échec ou arrêtée,
/// ou un menu qui la recouvre l'annulent, sinon la page reprendrait le clavier à un retour que personne n'a demandé
/// (revue de la tâche 10, M-3). `view` est `None` tant que la page n'est pas née.
fn page_focus_still_wanted(
    wanted: bool,
    tab_changed: bool,
    view: Option<&TabState>,
    covered: bool,
) -> bool {
    wanted && !tab_changed && !covered && !matches!(view, Some(state) if *state != TabState::Live)
}

/// Charge `url` dans la page : un fichier local par `load_file` (accès en lecture à son dossier), le reste par une
/// requête.
fn load_url(page: &WebPage, url: &str) {
    match Url::parse(url) {
        Ok(parsed) if parsed.scheme() == "file" => match parsed.to_file_path() {
            Ok(path) => page.load_file(&path),
            Err(()) => page.load(url),
        },
        _ => page.load(url),
    }
}

/// Les onglets avant un geste : de quoi écrire au journal ce qui a changé après lui.
struct TabSnapshot {
    ids: Vec<TabId>,
    active: TabId,
}

impl TabSnapshot {
    fn of(tabs: &BrowserTabs) -> Self {
        Self {
            ids: tabs.tabs().iter().map(|tab| tab.id).collect(),
            active: tabs.active().id,
        }
    }

    /// Les lignes du journal d'un geste : onglets fermés, ouverts, puis le nouvel actif.
    fn changes(&self, tabs: &BrowserTabs) -> Vec<String> {
        let mut lines = Vec::new();
        for id in &self.ids {
            if tabs.get(*id).is_none() {
                lines.push(format!("browser tab closed id={id}"));
            }
        }
        for tab in tabs.tabs() {
            if !self.ids.contains(&tab.id) {
                let kind = match tab.kind {
                    TabKind::Start => "start",
                    TabKind::Web { .. } => "web",
                    TabKind::Log { .. } => "log",
                };
                lines.push(format!("browser tab opened id={} kind={kind}", tab.id));
            }
        }
        let active = tabs.active().id;
        if active != self.active {
            lines.push(format!("browser tab activated id={active}"));
        }
        lines
    }

    fn log_changes(&self, tabs: &BrowserTabs) {
        for line in self.changes(tabs) {
            hud::log_event(&line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::tabs::OpenFrom;

    fn failed(code: isize) -> PageEvent {
        PageEvent::Failed {
            code,
            host: Some("localhost".to_owned()),
            url: Some("http://localhost:8819/".to_owned()),
            description: "x".to_owned(),
        }
    }

    #[test]
    fn a_page_keyboard_request_dies_with_its_tab_and_its_screen() {
        let failed = TabState::Failed(LoadError {
            message: "Connexion refusée : localhost:8819".to_owned(),
        });
        // Une page vivante à l'écran, ou encore à naître : la demande tient.
        assert!(page_focus_still_wanted(
            true,
            false,
            Some(&TabState::Live),
            false
        ));
        assert!(page_focus_still_wanted(true, false, None, false));
        // Un autre onglet devient actif : la demande de l'ancien ne passe pas au nouveau.
        assert!(!page_focus_still_wanted(
            true,
            true,
            Some(&TabState::Live),
            false
        ));
        assert!(!page_focus_still_wanted(true, true, None, false));
        // Page en échec ou arrêtée, ou recouverte par un menu : elle ne reprendra pas le clavier à son retour.
        assert!(!page_focus_still_wanted(true, false, Some(&failed), false));
        assert!(!page_focus_still_wanted(
            true,
            false,
            Some(&TabState::Terminated),
            false
        ));
        assert!(!page_focus_still_wanted(
            true,
            false,
            Some(&TabState::Live),
            true
        ));
        // Pas de demande, pas de demande.
        assert!(!page_focus_still_wanted(
            false,
            false,
            Some(&TabState::Live),
            false
        ));
    }

    #[test]
    fn a_failed_load_or_a_dead_process_holds_until_a_navigation_commits() {
        let refused = TabState::Failed(LoadError {
            message: "Connexion refusée : localhost:8819".to_owned(),
        });
        assert_eq!(TabState::Live.after(&failed(-1004)), refused);
        // Un second échec remplace la cause du premier.
        assert_eq!(
            refused.clone().after(&failed(-1003)),
            TabState::Failed(LoadError {
                message: "Hôte introuvable : localhost:8819".to_owned()
            })
        );
        // Ni un titre, ni la progression, ni l'historique n'effacent l'erreur : seule une navigation qui reçoit du contenu.
        for event in [
            PageEvent::Title(None),
            PageEvent::Loading {
                loading: true,
                progress: 0.3,
            },
            PageEvent::History {
                back: true,
                forward: false,
            },
        ] {
            assert_eq!(refused.clone().after(&event), refused, "{event:?}");
            assert_eq!(
                TabState::Terminated.after(&event),
                TabState::Terminated,
                "{event:?}"
            );
        }
        let committed = PageEvent::Committed("http://localhost:8811/".to_owned());
        assert_eq!(refused.after(&committed), TabState::Live);
        assert_eq!(TabState::Terminated.after(&committed), TabState::Live);
        // L'arrêt du processus vaut pour une page vivante comme pour une page déjà en échec.
        assert_eq!(
            TabState::Live.after(&PageEvent::Terminated),
            TabState::Terminated
        );
        assert_eq!(
            TabState::Live.after(&PageEvent::Title(None)),
            TabState::Live
        );
    }

    #[test]
    fn reload_retries_the_failed_address_instead_of_reloading_the_previous_page() {
        let failed = TabState::Failed(LoadError {
            message: "x".to_owned(),
        });
        use ReloadGesture::*;
        assert_eq!(reload_gesture(false, &TabState::Live, false), Reload);
        assert_eq!(
            reload_gesture(false, &TabState::Live, true),
            ReloadFromOrigin
        );
        assert_eq!(reload_gesture(true, &TabState::Live, false), Stop);
        // Une erreur ou un arrêt : on rejoue l'adresse, au bouton comme au menu.
        assert_eq!(reload_gesture(false, &failed, false), Retry);
        assert_eq!(reload_gesture(false, &failed, true), Retry);
        assert_eq!(reload_gesture(false, &TabState::Terminated, false), Retry);
        // Un rechargement qui tourne encore s'arrête, même sur une erreur.
        assert_eq!(reload_gesture(true, &failed, false), Stop);
    }

    /// Un couvrement dont l'image décodée est un entier : seule la logique d'état se teste ici.
    type TestCover = Cover<u32>;

    fn pending(tab: TabId, request: u64) -> TestCover {
        Cover::Pending { tab, request }
    }

    #[test]
    fn the_snapshot_that_was_asked_for_replaces_the_page() {
        let cover = cover_after_snapshot(pending(3, 7), 3, 7, Some(42));
        assert!(matches!(
            cover,
            Cover::Covering {
                tab: 3,
                image: Some(42)
            }
        ));
        // Pas d'image (erreur de WebKit) : le repli cache quand même la page, sur le fond du volet.
        let cover = cover_after_snapshot(pending(3, 7), 3, 7, None);
        assert!(matches!(
            cover,
            Cover::Covering {
                tab: 3,
                image: None
            }
        ));
        assert!(!page_shown_under(&cover, 3));
    }

    #[test]
    fn a_late_snapshot_never_lands_on_a_newer_cover() {
        // Une demande plus récente est en cours : la réponse de la précédente est jetée.
        let cover = cover_after_snapshot(pending(3, 8), 3, 7, Some(1));
        assert!(matches!(cover, Cover::Pending { tab: 3, request: 8 }));
        // Un autre onglet n'a pas la même page.
        let cover = cover_after_snapshot(pending(4, 7), 3, 7, Some(1));
        assert!(matches!(cover, Cover::Pending { tab: 4, request: 7 }));
        // Menu refermé avant l'arrivée : rien à peindre.
        let cover: TestCover = cover_after_snapshot(Cover::Off, 3, 7, Some(1));
        assert!(matches!(cover, Cover::Off));
        // Délai dépassé : la page est déjà cachée sur le fond du volet, l'image tardive ne la remplace pas.
        let late = cover_after_snapshot(
            Cover::Covering {
                tab: 3,
                image: None,
            },
            3,
            7,
            Some(1),
        );
        assert!(matches!(
            late,
            Cover::Covering {
                tab: 3,
                image: None
            }
        ));
    }

    #[test]
    fn the_page_stays_on_screen_until_its_image_is_ready() {
        assert!(page_shown_under(&TestCover::Off, 3));
        // Pendant l'attente de l'instantané, la page reste : une vue cachée ne se photographie pas bien.
        assert!(page_shown_under(&pending(3, 1), 3));
        let image: TestCover = Cover::Covering {
            tab: 3,
            image: Some(1),
        };
        assert!(!page_shown_under(&image, 3));
        // L'image est celle d'un autre onglet : la page de celui-ci n'est pas couverte.
        assert!(page_shown_under(&image, 4));
    }

    #[test]
    fn a_stuck_snapshot_hides_the_page_after_the_guard_delay() {
        let cover = cover_after_timeout(pending(3, 7), 3, 7);
        assert!(matches!(
            cover,
            Cover::Covering {
                tab: 3,
                image: None
            }
        ));
        assert!(!page_shown_under(&cover, 3));
        // Le délai d'une demande dépassée ne touche ni une demande plus récente, ni un autre onglet, ni un menu fermé.
        assert!(matches!(
            cover_after_timeout(pending(3, 8), 3, 7),
            Cover::Pending { tab: 3, request: 8 }
        ));
        assert!(matches!(
            cover_after_timeout(pending(4, 7), 3, 7),
            Cover::Pending { tab: 4, request: 7 }
        ));
        assert!(matches!(
            cover_after_timeout(TestCover::Off, 3, 7),
            Cover::Off
        ));
    }

    #[test]
    fn a_page_that_is_not_on_screen_is_hidden_at_once_and_never_photographed() {
        let mut asked = 0;
        let cover: TestCover = cover_start(3, false, || {
            asked += 1;
            9
        });
        assert_eq!(asked, 0, "un instantané d'une vue cachée a été demandé");
        assert!(matches!(
            cover,
            Cover::Covering {
                tab: 3,
                image: None
            }
        ));
        assert!(!page_shown_under(&cover, 3));
        // Une page à l'écran est photographiée, et reste là jusqu'à la réponse.
        let cover: TestCover = cover_start(3, true, || {
            asked += 1;
            9
        });
        assert_eq!(asked, 1);
        assert!(matches!(cover, Cover::Pending { tab: 3, request: 9 }));
    }

    #[test]
    fn dropping_a_cover_hands_back_its_image_to_release() {
        let covering: TestCover = Cover::Covering {
            tab: 3,
            image: Some(5),
        };
        assert_eq!(covering.into_parts(), Some((3, Some(5))));
        let fallback: TestCover = Cover::Covering {
            tab: 3,
            image: None,
        };
        assert_eq!(fallback.into_parts(), Some((3, None)));
        assert_eq!(pending(4, 1).into_parts(), Some((4, None)));
        assert_eq!(TestCover::Off.into_parts(), None);
    }

    #[test]
    fn a_file_name_from_the_server_is_cleaned_for_the_journal() {
        assert_eq!(journal_name("rapport.txt"), "rapport.txt");
        assert_eq!(journal_name("a?token=1#frag.txt"), "atoken=1frag.txt");
        assert_eq!(journal_name("x\u{1b}[31m\ny\t.txt"), "x[31my.txt");
    }

    #[test]
    fn a_failed_download_leaves_a_line_until_the_next_navigation() {
        let line = download_line_after(None, &PageEvent::DownloadFailed("Réseau coupé".to_owned()));
        assert_eq!(
            line.as_deref(),
            Some("Téléchargement échoué : Réseau coupé")
        );
        // Les événements d'une page qui vit n'effacent pas la ligne.
        let kept = download_line_after(line.clone(), &PageEvent::Title(Some("x".to_owned())));
        assert_eq!(kept, line);
        let kept = download_line_after(
            line.clone(),
            &PageEvent::Finished("http://localhost:8811/".to_owned()),
        );
        assert_eq!(kept, line);
        // Un téléchargement suivant qui réussit l'efface aussi.
        let finished = download_line_after(
            line.clone(),
            &PageEvent::DownloadFinished(PathBuf::from("/tmp/a.txt")),
        );
        assert_eq!(finished, None);
        // La navigation suivante l'efface.
        let cleared = download_line_after(
            line,
            &PageEvent::Committed("http://localhost:8811/page2".to_owned()),
        );
        assert_eq!(cleared, None);
    }

    #[test]
    fn browser_keys_beat_the_workspace_ones_and_the_find_keys_exist_only_while_finding() {
        use crate::workspace::{
            DecreaseFontSize, IncreaseFontSize, KEY_CONTEXT as WORKSPACE_CONTEXT, ResetFontSize,
            ToggleChanges,
        };
        use gpui::{Action, DummyKeyboardMapper, KeyContext, Keymap, Keystroke};

        // Les liaisons de `main.rs` que celles du volet doivent battre ou laisser passer, puis celles du volet.
        let workspace = Some(WORKSPACE_CONTEXT);
        let mut bindings = vec![
            KeyBinding::new("cmd-shift-g", ToggleChanges, workspace),
            KeyBinding::new("cmd-=", IncreaseFontSize, workspace),
            KeyBinding::new("cmd-+", IncreaseFontSize, workspace),
            KeyBinding::new("cmd-shift-=", IncreaseFontSize, workspace),
            KeyBinding::new("cmd--", DecreaseFontSize, workspace),
            KeyBinding::new("cmd-0", ResetFontSize, workspace),
        ];
        bindings.extend(key_bindings());
        bindings.extend(layout_key_bindings(&DummyKeyboardMapper));
        let keymap = Keymap::new(bindings);
        let first = |keys: &str, within: &[&str]| -> Option<&'static str> {
            let stack: Vec<KeyContext> = within
                .iter()
                .map(|name| KeyContext::parse(name).unwrap())
                .collect();
            let (found, _) = keymap.bindings_for_input(&[Keystroke::parse(keys).unwrap()], &stack);
            found.first().map(|binding| binding.action().name())
        };
        let name = |action: &dyn Action| action.name();
        let browser = [WORKSPACE_CONTEXT, KEY_CONTEXT];
        let finding = [WORKSPACE_CONTEXT, "Browser finding"];

        for stack in [&browser[..], &finding[..]] {
            assert_eq!(first("cmd-f", stack), Some(name(&FindInPage)), "{stack:?}");
            assert_eq!(first("cmd-l", stack), Some(name(&FocusAddress)));
            for key in ["cmd-=", "cmd-+", "cmd-shift-="] {
                assert_eq!(first(key, stack), Some(name(&ZoomIn)), "{key} {stack:?}");
            }
            assert_eq!(first("cmd--", stack), Some(name(&ZoomOut)), "{stack:?}");
            assert_eq!(first("cmd-0", stack), Some(name(&ZoomReset)), "{stack:?}");
        }
        // Barre ouverte : suivant et précédent. Barre fermée : `⌘G` ne fait rien, `⌘⇧G` reste à Changes.
        assert_eq!(first("cmd-g", &finding), Some(name(&FindNext)));
        assert_eq!(first("cmd-shift-g", &finding), Some(name(&FindPrevious)));
        assert_eq!(first("cmd-g", &browser), None);
        assert_eq!(first("cmd-shift-g", &browser), Some(name(&ToggleChanges)));
        // Hors du volet, le zoom reste la taille du texte.
        assert_eq!(
            first("cmd-=", &[WORKSPACE_CONTEXT]),
            Some(name(&IncreaseFontSize))
        );
        // Échap ferme la barre, depuis la rangée de recherche seulement.
        let row = [WORKSPACE_CONTEXT, "Browser finding", FIND_KEY_CONTEXT];
        assert_eq!(first("escape", &row), Some(name(&CloseFind)));
        assert_eq!(first("escape", &browser), None);
    }

    #[test]
    fn browser_keys_are_bound_in_the_browser_context() {
        use crate::menus::{Copy, Cut, Paste, SelectAll};
        use crate::workspace::{CloseSession, KEY_CONTEXT as WORKSPACE_CONTEXT, NewSession};
        use gpui::{Action, DummyKeyboardMapper, KeyContext, Keymap, Keystroke};

        let name = |action: &dyn Action| action.name();
        // Le clavier de `main.rs` pour ces touches : celles de la fenêtre, les globales du menu « Édition », puis celles
        // du volet.
        let workspace = Some(WORKSPACE_CONTEXT);
        let mut bindings = vec![
            KeyBinding::new("cmd-t", NewSession, workspace),
            KeyBinding::new("cmd-w", CloseSession, workspace),
            KeyBinding::new("cmd-c", Copy, None),
            KeyBinding::new("cmd-x", Cut, None),
            KeyBinding::new("cmd-v", Paste, None),
            KeyBinding::new("cmd-a", SelectAll, None),
        ];
        bindings.extend(key_bindings());
        bindings.extend(layout_key_bindings(&DummyKeyboardMapper));
        let keymap = Keymap::new(bindings);
        let first = |keys: &str, within: &[&str]| -> Option<&'static str> {
            let stack: Vec<KeyContext> = within
                .iter()
                .map(|name| KeyContext::parse(name).unwrap())
                .collect();
            let (found, _) = keymap.bindings_for_input(&[Keystroke::parse(keys).unwrap()], &stack);
            found.first().map(|binding| binding.action().name())
        };
        let browser = [WORKSPACE_CONTEXT, KEY_CONTEXT];
        let address = [WORKSPACE_CONTEXT, KEY_CONTEXT, ADDRESS_KEY_CONTEXT];
        let finding = [WORKSPACE_CONTEXT, "Browser finding", FIND_KEY_CONTEXT];

        // Dans le volet, page comprise : les gestes du navigateur.
        let gestures: [(&str, &str); 14] = [
            ("cmd-t", name(&NewTab)),
            ("cmd-w", name(&CloseTab)),
            ("cmd-l", name(&FocusAddress)),
            ("cmd-r", name(&Reload)),
            ("cmd-[", name(&GoBack)),
            ("cmd-]", name(&GoForward)),
            ("cmd-shift-]", name(&NextTab)),
            ("cmd-}", name(&NextTab)),
            ("cmd-shift-[", name(&PreviousTab)),
            ("cmd-{", name(&PreviousTab)),
            ("cmd-f", name(&FindInPage)),
            ("cmd-z", name(&PageUndo)),
            ("cmd-shift-z", name(&PageRedo)),
            ("cmd-0", name(&ZoomReset)),
        ];
        for (keys, action) in gestures {
            assert_eq!(first(keys, &browser), Some(action), "{keys} {browser:?}");
        }
        // Hors du volet, `⌘T` et `⌘W` restent à la session, et les gestes du navigateur n'existent pas.
        assert_eq!(
            first("cmd-t", &[WORKSPACE_CONTEXT]),
            Some(name(&NewSession))
        );
        assert_eq!(
            first("cmd-w", &[WORKSPACE_CONTEXT]),
            Some(name(&CloseSession))
        );
        for keys in ["cmd-r", "cmd-[", "cmd-]", "cmd-z", "cmd-shift-z"] {
            assert_eq!(first(keys, &[WORKSPACE_CONTEXT]), None, "{keys}");
        }
        // `⌘C` `⌘V` `⌘X` `⌘A` ne sont pas liés dans le volet : ils gardent la liaison globale du menu « Édition », que
        // AppKit envoie à la page (rapport de sonde §3). Aucune action `Page*` ne les prend.
        for (keys, action) in [
            ("cmd-c", name(&Copy)),
            ("cmd-x", name(&Cut)),
            ("cmd-v", name(&Paste)),
            ("cmd-a", name(&SelectAll)),
        ] {
            assert_eq!(first(keys, &browser), Some(action), "{keys}");
        }
        // `⌘←` et `⌘→` reculent et avancent comme `⌘[` et `⌘]` (hors des deux champs) ; l'action laisse passer la touche quand
        // la page a le clavier, et le champ d'adresse et la recherche gardent leur début et fin de ligne.
        assert_eq!(first("cmd-left", &browser), Some(name(&GoBackKey)));
        assert_eq!(first("cmd-right", &browser), Some(name(&GoForwardKey)));
        for keys in ["cmd-left", "cmd-right"] {
            for stack in [&address[..], &finding[..]] {
                assert_eq!(first(keys, stack), None, "{keys} {stack:?}");
            }
        }
        // Le champ d'adresse et la rangée de recherche gardent leurs touches d'édition et de ligne : ni historique, ni
        // annuler de la page. Les gestes du volet y restent (`⌘T`, `⌘W`, `⌘L`, `⌘R`).
        for stack in [&address[..], &finding[..]] {
            for keys in ["cmd-[", "cmd-]", "cmd-z", "cmd-shift-z"] {
                assert_eq!(first(keys, stack), None, "{keys} {stack:?}");
            }
            assert_eq!(first("cmd-t", stack), Some(name(&NewTab)), "{stack:?}");
            assert_eq!(first("cmd-w", stack), Some(name(&CloseTab)), "{stack:?}");
            assert_eq!(first("cmd-r", stack), Some(name(&Reload)), "{stack:?}");
        }
    }

    #[test]
    fn restored_web_tabs_load_only_when_shown() {
        let mut tabs = BrowserTabs::new();
        let web = tabs.open_url("http://localhost:8811/", OpenFrom::Link).id;
        let none = HashSet::new();
        // Volet caché : aucune page, même pour l'onglet actif.
        assert!(pages_to_create(&tabs, &none, false).is_empty());
        // Montré : seulement l'onglet actif, et seulement s'il est web et sans page.
        assert_eq!(pages_to_create(&tabs, &none, true), vec![web]);
        assert!(pages_to_create(&tabs, &HashSet::from([web]), true).is_empty());
        // L'Accueil n'en demande jamais.
        let start = tabs.tabs()[0].id;
        tabs.activate(start);
        assert!(pages_to_create(&tabs, &none, true).is_empty());
    }

    #[test]
    fn tab_gestures_write_their_journal_lines() {
        let mut tabs = BrowserTabs::new();
        let first = tabs.active().id;
        let before = TabSnapshot::of(&tabs);
        let web = tabs.open_url("http://localhost:8811/", OpenFrom::Link).id;
        assert_eq!(
            before.changes(&tabs),
            [
                format!("browser tab opened id={web} kind=web"),
                format!("browser tab activated id={web}"),
            ]
        );
        let before = TabSnapshot::of(&tabs);
        tabs.activate(first);
        assert_eq!(
            before.changes(&tabs),
            [format!("browser tab activated id={first}")]
        );
        // Fermer le dernier onglet web laisse l'Accueil qui existe déjà : seul l'onglet fermé est dit.
        let before = TabSnapshot::of(&tabs);
        tabs.close(web);
        assert_eq!(
            before.changes(&tabs),
            [format!("browser tab closed id={web}")]
        );
        // Fermer le seul onglet en rend un neuf, ouvert puis actif.
        let before = TabSnapshot::of(&tabs);
        let fresh = tabs.close(first).unwrap();
        assert_eq!(
            before.changes(&tabs),
            [
                format!("browser tab closed id={first}"),
                format!("browser tab opened id={fresh} kind=start"),
                format!("browser tab activated id={fresh}"),
            ]
        );
    }

    #[test]
    fn only_web_and_file_addresses_are_kept_for_a_tab() {
        assert!(is_page_url("http://localhost:8811/"));
        assert!(is_page_url("HTTPS://example.com/"));
        assert!(is_page_url("file:///tmp/a.html"));
        assert!(!is_page_url("about:blank"));
        assert!(!is_page_url("javascript:alert(1)"));
        assert!(!is_page_url(""));
    }

    #[test]
    fn a_non_ascii_address_never_panics_the_scheme_check() {
        // Le caractère de 3 octets chevauche la limite de `https://` (8 octets) et de `http://` (7).
        assert!(!is_page_url("éé€€x"));
        assert!(!is_page_url("htt€ps"));
        assert!(!is_page_url("日本語のアドレス"));
        assert!(is_page_url("https://exemple.fr/é€"));
    }

    #[test]
    fn enter_in_the_address_field_reloads_the_address_the_active_tab_already_shows() {
        let mut tabs = BrowserTabs::new();
        let web = tabs.open_url("http://localhost:8811/", OpenFrom::Link).id;
        let before = TabSnapshot::of(&tabs);
        // Entrée sur la même adresse, avec un fragment de plus : l'onglet actif se recharge.
        let url = "http://localhost:8811/#section";
        let known = tabs.tabs().iter().any(
            |tab| matches!(&tab.kind, TabKind::Web { url: shown } if same_address(shown, url)),
        );
        let opened = tabs.open_url(url, OpenFrom::Here);
        assert!(known);
        assert!(page_load_needed(
            known,
            opened.created,
            true,
            opened.id == before.active
        ));
        assert_eq!(opened.id, web);
        // Ouverte par un lien (pas la barre), la même adresse ne recharge rien.
        assert!(!page_load_needed(known, false, false, true));
        // Une adresse que montre un autre onglet : on y passe, sans la recharger.
        assert!(!page_load_needed(known, false, true, false));
        // Une adresse neuve dans un onglet existant se charge, un onglet créé charge à sa naissance.
        assert!(page_load_needed(false, false, false, true));
        assert!(!page_load_needed(false, true, true, true));
    }
}
