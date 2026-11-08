//! La vue d'un pane : l'émulateur relié à tmux, la saisie (clavier, IME, souris, collage), les signaux, l'état,
//! le lancement d'un agent et le HUD. Le client de contrôle, le routage des événements, la minuterie, la taille
//! demandée à tmux et l'agrégation des états sont ceux de la session (`session_view`).

use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, Bounds, ClipboardItem, Context, DragMoveEvent, Entity, EntityInputHandler, EventEmitter,
    ExternalPaths, FocusHandle, Focusable, KeyDownEvent, Modifiers, ModifiersChangedEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, PromptButton,
    PromptLevel, Rgba, ScrollWheelEvent, Subscription, Task, UTF16Selection, Window, div, point,
    prelude::*, px, rgb, size,
};
use idema_term::activity::{
    ActivityState, HeldInput, HeldStep, LaunchGate, LaunchPacer, LaunchStep, PaneActivity,
    is_input_echo, paced_step,
};
use idema_term::emulator::{
    GridSize, Link, LinkTarget, PaneEmulator, PathMatch, SearchMatch, SelectionKind, Source,
    ViewPoint, link_target,
};
use idema_term::input::{
    KeyModes, Mods, MouseAction, MouseButtonKind, MouseModes, WheelAction, dropped_paths_text,
    encode_key, encode_mouse, encode_paste, encode_wheel,
};
use idema_term::local_pty::{KILL_GRACE, LocalPty, PtyCommand, PtyEvent};
use idema_term::palette;
use idema_term::pane_session::{EMULATOR_HISTORY, Effect, PaneSession, build_emulator};
use idema_term::signals::SignalDetector;
use idema_tmux::client::ClientWriter;
use idema_tmux::command::{PaneWatch, color_report, unset_pane_option};
use idema_tmux::protocol::{ControlEvent, PaneId};
use idema_tmux::remote::{SSH_BINARY, shell_quote, ssh_arguments};

use url::Url;

use crate::browser::address::{self, is_local_document};
use crate::companion::input::{RemoteKey, accepts_remote_input, encode_remote_key};
use crate::file_open::{self, PathTarget};
use crate::hud::{self, DumpGrid, HudStats, ModeWatch, ToggleHud};
use crate::machines::registry::Lease;
use crate::menus::{self, Paste};
use crate::restore::plan::LAUNCH_STAGGER;
use crate::search_bar::{
    self, OpenSearch, SearchBar, SearchEvent, SearchNext, SearchPrevious, closest_above_bottom,
    counter_label, current_after_reflow, index_of, next_index, previous_index,
};
use crate::servers::tail::FileTail;
use crate::signpost;
use crate::terminal_element::TerminalElement;
use crate::theme::{self, Theme};

/// Contexte de touches de la vue : les raccourcis du terminal (hors ⌘V, global dans `menus`) s'y accrochent.
pub const KEY_CONTEXT: &str = "Terminal";

/// Silence de la taille avant de relire l'écran visible de tmux. Il doit rester plus long que le délai de la
/// demande de taille de la session (`SIZE_REQUEST_DELAY`) : la relecture ne part qu'une fois la taille posée.
pub(crate) const RESIZE_SETTLE: Duration = Duration::from_millis(150);

/// Durée d'affichage de la bulle du pane : un vidage de grille, un fichier introuvable (valeur à ratifier).
const NOTICE_DURATION: Duration = Duration::from_secs(2);

/// La bulle d'une saisie qui n'est pas partie parce que le lien vers la machine est tombé (spec ssh-tmux §3.2).
const LINK_DOWN_NOTICE: &str = "Connexion perdue, saisie non envoyée";

/// La bulle du pane, en haut à droite : ce qu'un geste a fait, ou son échec.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneNotice {
    text: String,
    failed: bool,
}

impl PaneNotice {
    /// Le vidage de grille : le fichier écrit, ou l'échec.
    fn dump(result: &io::Result<PathBuf>) -> Self {
        match result {
            Ok(path) => Self {
                text: format!(
                    "Grille écrite : {}",
                    path.file_name().map_or_else(
                        || path.display().to_string(),
                        |name| name.to_string_lossy().into_owned()
                    )
                ),
                failed: false,
            },
            Err(_) => Self {
                text: "Vidage impossible".to_owned(),
                failed: true,
            },
        }
    }
}

/// Les occurrences de la recherche dans la vue, chacune de sa première à sa dernière cellule incluse.
#[derive(Default)]
pub struct Highlights {
    pub matches: Vec<(ViewPoint, ViewPoint)>,
    pub current: Option<(ViewPoint, ViewPoint)>,
}

/// La recherche ⌘F d'un pane : sa barre, la requête, les occurrences de la dernière recherche complète (le compteur et
/// le pas à pas en viennent) et la courante. Le surlignage, lui, se recalcule à chaque peinture (écart E29).
struct SearchState {
    bar: Entity<SearchBar>,
    query: String,
    matches: Vec<SearchMatch>,
    current: Option<usize>,
    _bar_events: Subscription,
}

/// La session d'où vient un pane : son nom tmux, pour le journal, et le dossier de son projet.
#[derive(Debug, Clone, Copy)]
pub struct SessionRef<'a> {
    pub name: &'a str,
    pub project: &'a Path,
    /// Le shell de la machine distante de la session ; `None` pour une session de ce Mac (la variable `SHELL`).
    pub shell: Option<&'a str>,
    /// La session vit sur une machine distante : les chemins du pane ne sont pas ceux du Mac.
    pub remote: bool,
}

/// Ce que le pane signale à sa session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEvent {
    /// Le titre ou la commande au premier plan a changé : la barre se repeint.
    MetaChanged,
    /// L'état du pane (repos, en cours, attend, erreur) a changé : la session recalcule le sien.
    StateChanged {
        previous: ActivityState,
        current: ActivityState,
    },
    /// Un appui (tout bouton) sur un pane qui n'est pas l'actif : la session lui donne le focus (écart E20).
    FocusRequested,
    /// Le focus GPUI du pane a changé (pris ou perdu) : l'espace de travail recalcule qui tient le clavier, de la
    /// session ou du volet Terminal (spec §6.3).
    FocusChanged,
    /// Le shell d'un pane local est parti (`exit`, ou plus personne ne tient le terminal) : le volet retire ce PTY
    /// (écart E26). Un pane tmux ne l'émet jamais.
    Closed,
    /// `⌘`-clic sur un chemin du pane qui désigne un fichier ou un dossier existant : l'espace de travail l'ouvre dans
    /// le volet Fichiers (spec §4.7).
    OpenPath(PathTarget),
    /// `⌘`-clic sur une adresse `http(s)`, un document local ou un lien `file://` vers lui : l'espace de travail
    /// l'ouvre dans le navigateur (P4-7, P4-8).
    OpenUrl(String),
}

/// Le geste de souris en cours dans le pane : un bouton enfoncé dans le pane, suivi jusqu'à son relâchement, même
/// hors du pane ou de la fenêtre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gesture {
    /// Sélection native : bouton gauche tenu, sans mode souris du programme ou avec `⇧` (écart E19).
    Selecting,
    /// Le programme capte la souris : l'appui est rapporté, les glissés et le relâché le sont aussi.
    Reporting(MouseButtonKind),
}

/// Ce que sélectionne un appui selon son nombre de clics : un glisser, le mot, la ligne.
fn selection_kind(click_count: usize) -> SelectionKind {
    match click_count {
        0 | 1 => SelectionKind::Simple,
        2 => SelectionKind::Word,
        _ => SelectionKind::Line,
    }
}

/// La cellule de la vue sous `position`, bornée à la grille (`origin` est le coin de la grille, `cell` la taille d'une
/// cellule). `right_half` dit que la souris est dans la moitié droite de la cellule : une sélection qui y commence ne
/// la prend pas. Au-delà du bord droit, la dernière cellule compte en entier ; au-delà du gauche, elle ne compte pas.
fn view_point_at(
    position: Point<Pixels>,
    origin: Point<Pixels>,
    cell: (Pixels, Pixels),
    grid: GridSize,
) -> ViewPoint {
    let x = (position.x - origin.x) / cell.0;
    let y = (position.y - origin.y) / cell.1;
    let column = x.floor().max(0.) as usize;
    let row = y.floor().max(0.) as usize;
    let last_column = grid.columns.saturating_sub(1);
    ViewPoint {
        row: row.min(grid.lines.saturating_sub(1)),
        column: column.min(last_column),
        right_half: column > last_column || x.fract() >= 0.5,
    }
}

/// La cellule sous `position` si elle est dans la grille, `None` dans la marge au-delà de la dernière colonne ou
/// rangée (pane bordé, bande plus petite qu'une cellule) : le survol et le `⌘`-clic ne visent pas la cellule du bord.
fn view_point_inside(
    position: Point<Pixels>,
    origin: Point<Pixels>,
    cell: (Pixels, Pixels),
    grid: GridSize,
) -> Option<ViewPoint> {
    let x = (position.x - origin.x) / cell.0;
    let y = (position.y - origin.y) / cell.1;
    let inside = x >= 0. && y >= 0. && x < grid.columns as f32 && y < grid.lines as f32;
    inside.then(|| view_point_at(position, origin, cell, grid))
}

/// Un appui n'est que la prise du focus (écart E20) quand le pane n'est pas l'actif, ou quand il l'est pour tmux
/// mais que le clavier est ailleurs (l'autre côté de la frontière session / volet Terminal) : le premier clic
/// ne sélectionne rien et n'atteint pas le programme qui capte la souris.
fn press_only_takes_focus(active: bool, keyboard_focused: bool) -> bool {
    !active || !keyboard_focused
}

/// L'appui ne fait que prendre le focus : `press_only_takes_focus`, sauf un `⌘`-clic (décision 16). Il ne vise pas un pane,
/// il désigne un chemin ou un lien : jamais rapporté au programme, il agit au premier appui, et le focus va où
/// l'ouverture l'envoie (le volet Fichiers pour un chemin).
fn press_is_focus_only(
    button: MouseButton,
    modifiers: &Modifiers,
    active: bool,
    keyboard_focused: bool,
) -> bool {
    press_only_takes_focus(active, keyboard_focused) && !is_link_press(button, modifiers)
}

/// Un appui gauche avec `⌘` est un clic de lien : il ne sélectionne jamais et n'est jamais rapporté au programme,
/// qu'il y ait un lien sous la souris ou non.
fn is_link_press(button: MouseButton, modifiers: &Modifiers) -> bool {
    button == MouseButton::Left && modifiers.platform
}

/// Le lien se survole, donc se souligne, seulement si un `⌘`-clic en ferait quelque chose.
fn is_followable(uri: &str) -> bool {
    !matches!(link_target(uri), LinkTarget::Refused)
}

/// Où mène un lien `⌘`-cliqué (spec P4 §4.10).
#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkDestination {
    /// Le navigateur d'IdemA : une adresse `http(s)`, ou un document local (HTML, PDF) en `file://`.
    Browser(String),
    /// Le système : `mailto:`, la messagerie par défaut.
    System(String),
    /// Le Finder : un autre fichier ou dossier local, jamais ouvert (un `.command` s'exécuterait).
    Reveal(PathBuf),
    Refused,
}

/// La destination de `uri` : `link_target` décide du schéma, puis les adresses web et les documents locaux passent
/// au navigateur. Le fragment et la requête d'un lien `file://` (`#page=2`) ne comptent pas pour reconnaître le document.
fn link_destination(uri: &str) -> LinkDestination {
    match link_target(uri) {
        LinkTarget::Open(uri) if uri.to_ascii_lowercase().starts_with("mailto:") => {
            LinkDestination::System(uri)
        }
        LinkTarget::Open(uri) => LinkDestination::Browser(uri),
        LinkTarget::Reveal(path) => {
            let bare = uri.split(['?', '#']).next().unwrap_or(uri);
            match link_target(bare) {
                LinkTarget::Reveal(document) if is_local_document(&document) => {
                    LinkDestination::Browser(file_address(uri))
                }
                _ => LinkDestination::Reveal(path),
            }
        }
        LinkTarget::Refused => LinkDestination::Refused,
    }
}

/// `link_destination` pour un pane qui est (`remote`) ou non celui d'une machine distante : le Finder et Mail ne sont
/// jamais ouverts depuis un pane distant, un chemin de la machine n'existant pas sur ce Mac.
fn routed_destination(remote: bool, uri: &str) -> LinkDestination {
    match link_destination(uri) {
        LinkDestination::Reveal(_) | LinkDestination::System(_) if remote => {
            LinkDestination::Refused
        }
        destination => destination,
    }
}

/// L'adresse `file://` d'un lien, sans `localhost` ni majuscules au schéma : la forme que le navigateur garde.
fn file_address(uri: &str) -> String {
    let rest = uri.get("file://".len()..).unwrap_or_default();
    let rest = match rest.get(.."localhost".len()) {
        Some(head) if head.eq_ignore_ascii_case("localhost") => &rest["localhost".len()..],
        _ => rest,
    };
    format!("file://{rest}")
}

/// L'adresse du navigateur pour un chemin absolu qui est un document local (HTML, PDF). `None` pour tout autre chemin.
fn document_address(path: &Path) -> Option<String> {
    if !path.is_absolute() || !is_local_document(path) {
        return None;
    }
    Url::from_file_path(path).ok().map(String::from)
}

/// Ce qu'un pane sait du glisser de fichiers du Finder en cours : les fichiers du glisser (donnés par GPUI à chaque
/// mouvement) et si la souris est sur lui, sans qu'un menu le recouvre (mesuré par la peinture du terminal).
#[derive(Debug, Default)]
struct DropState {
    files: Option<ExternalPaths>,
    hit: bool,
}

impl DropState {
    /// Un mouvement du glisser porte `files` : un nouveau glisser remplace ceux de l'ancien. Vrai si l'état change.
    fn dragged(&mut self, files: &ExternalPaths) -> bool {
        if self.files.as_ref() == Some(files) {
            return false;
        }
        self.files = Some(files.clone());
        true
    }

    /// Le pane est (ou n'est plus) sous la souris. Vrai si le liseré change.
    fn hit_test(&mut self, hit: bool) -> bool {
        let before = self.outlined();
        self.hit = hit;
        before != self.outlined()
    }

    /// Plus aucun glisser en cours (Échap, relâché hors de la fenêtre) : tout est oublié.
    fn ended(&mut self) {
        *self = Self::default();
    }

    /// Le liseré se dessine : le pane connaît les fichiers et la souris est dessus.
    fn outlined(&self) -> bool {
        self.files.is_some() && self.hit
    }

    /// Le bouton se relâche. Les fichiers ne sortent que si un glisser est en cours et que le relâché est sur le pane
    /// (ni dehors, ni sous un menu) ; dans tous les cas le glisser est fini pour le pane.
    fn release(&mut self, hit: bool, dragging: bool) -> Option<ExternalPaths> {
        let files = self.files.take();
        self.hit = false;
        if dragging && hit { files } else { None }
    }
}

/// Le texte à insérer pour des fichiers déposés : leurs chemins échappés, séparés par une espace. Un chemin qui n'est
/// pas de l'UTF-8 est refusé comme un chemin à caractère de contrôle (l'octet brut ne passerait pas la saisie) ; le
/// refus rend le chemin fautif pour le message.
fn dropped_text(paths: &[PathBuf]) -> Result<String, String> {
    let mut texts = Vec::with_capacity(paths.len());
    for path in paths {
        match path.to_str() {
            Some(text) => texts.push(text),
            None => return Err(path.to_string_lossy().into_owned()),
        }
    }
    dropped_paths_text(&texts)
}

/// Une coordonnée de cellule pour `encode_mouse`.
fn cell_coordinate(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// Le bouton que `encode_mouse` sait rapporter ; les autres (précédent, suivant) ne partent pas au programme.
fn button_kind(button: MouseButton) -> Option<MouseButtonKind> {
    match button {
        MouseButton::Left => Some(MouseButtonKind::Left),
        MouseButton::Middle => Some(MouseButtonKind::Middle),
        MouseButton::Right => Some(MouseButtonKind::Right),
        MouseButton::Navigate(_) => None,
    }
}

fn mods_of(modifiers: &Modifiers) -> Mods {
    Mods {
        control: modifiers.control,
        alt: modifiers.alt,
        shift: modifiers.shift,
        command: modifiers.platform,
    }
}

/// Les lignes de molette cumulées : la fraction de ligne pas encore consommée attend le prochain cran, et repart de
/// zéro quand le sens change (un demi-cran vers le haut ne retire rien au premier cran vers le bas).
#[derive(Debug, Default)]
struct WheelAccumulator {
    remainder: f32,
}

impl WheelAccumulator {
    /// Ajoute `lines` (positif : vers le haut) et rend le nombre entier de lignes à faire défiler.
    fn add(&mut self, lines: f32) -> i32 {
        if lines != 0. && self.remainder != 0. && lines.signum() != self.remainder.signum() {
            self.remainder = 0.;
        }
        self.remainder += lines;
        let whole = self.remainder.trunc();
        self.remainder -= whole;
        whole as i32
    }
}

/// Libellé d'un programme dont on ignore le nom, pour la confirmation de fermeture.
const UNKNOWN_PROGRAM: &str = "un programme";

/// Nom de base du shell de l'utilisateur d'après `$SHELL` ; `zsh` quand la variable manque ou est vide.
fn shell_basename(shell: Option<&str>) -> String {
    shell
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("zsh")
        .to_owned()
}

/// Le nom du shell d'un pane : celui de la machine distante de sa session quand il y en a un, sinon la règle locale
/// (`local`, la variable `SHELL` du Mac). Il sert à distinguer « au prompt » d'une commande au premier plan.
fn pane_shell_name(remote_shell: Option<&str>, local: Option<&str>) -> String {
    match remote_shell {
        Some(shell) => shell_basename(Some(shell)),
        None => shell_basename(local),
    }
}

/// Les programmes qui tournent au premier plan du pane : la commande si ce n'est pas le shell, sinon un libellé
/// générique quand le shell dit qu'une commande tourne (133;C sans 133;D). Vide au prompt.
fn running_programs_of(
    command_running: bool,
    current_command: Option<&str>,
    shell_name: &str,
) -> Vec<String> {
    let foreground = current_command.filter(|command| *command != shell_name);
    if command_running || foreground.is_some() {
        vec![foreground.unwrap_or(UNKNOWN_PROGRAM).to_owned()]
    } else {
        Vec::new()
    }
}

/// Ce que le pane suit par abonnement : la commande au premier plan, l'agent et le dossier courant du pane.
const WATCHES: [PaneWatch; 3] = [PaneWatch::Command, PaneWatch::Agent, PaneWatch::Path];

/// Abonne le client aux `WATCHES` du pane ; tmux en donne la valeur initiale. Déclare aussi les couleurs du texte et
/// du fond (OSC 10 et 11) : un pane neuf les reçoit à sa naissance, tous les panes à une reconnexion (AC-40).
fn subscribe_watches(pane_session: &mut PaneSession<ClientWriter>, pane: PaneId) {
    for watch in WATCHES {
        pane_session.send_command(&watch.subscribe(pane));
    }
    pane_session.send_command(&color_report(pane, 10, palette::DEFAULT_FOREGROUND));
    pane_session.send_command(&color_report(pane, 11, palette::DEFAULT_BACKGROUND));
}

/// D'où le pane tient ses octets : un pane de tmux (relu, redimensionné et nourri par le client de contrôle de la
/// session), ou le shell local du volet Terminal, qui n'a ni signaux, ni états, ni lancement d'agent, ni abonnements.
enum PaneBackend {
    Tmux(PaneSession<ClientWriter>),
    Local {
        pty: LocalPty,
        emulator: PaneEmulator,
    },
    /// Le journal d'un serveur dont la sortie va dans un fichier (spec P4 §4.5) : l'émulateur reçoit les octets que le
    /// suivi du fichier lui envoie, et rien ne repart vers un programme.
    Tail {
        emulator: PaneEmulator,
    },
}

impl PaneBackend {
    fn emulator(&self) -> &PaneEmulator {
        match self {
            Self::Tmux(session) => session.emulator(),
            Self::Local { emulator, .. } | Self::Tail { emulator } => emulator,
        }
    }

    /// Octets pour le programme du pane : touches par `send-keys` pour tmux, écriture directe pour un PTY local.
    fn send_bytes(&mut self, bytes: &[u8]) {
        match self {
            Self::Tmux(session) => session.send_bytes(bytes),
            Self::Local { pty, .. } => pty.write(bytes),
            Self::Tail { .. } => {}
        }
    }

    fn scroll_history(&mut self, lines: i32) {
        match self {
            Self::Tmux(session) => session.scroll_history(lines),
            Self::Local { emulator, .. } | Self::Tail { emulator } => {
                emulator.scroll_history(lines)
            }
        }
    }

    fn scroll_to_bottom(&mut self) {
        match self {
            Self::Tmux(session) => session.scroll_to_bottom(),
            Self::Local { emulator, .. } | Self::Tail { emulator } => emulator.scroll_to_bottom(),
        }
    }

    /// La cellule en pixels de l'appareil, pour `CSI 14 t` : un pane local seulement, tmux répond pour ses panes.
    fn set_cell_pixels(&mut self, width: f32, height: f32) {
        if let Self::Local { emulator, .. } | Self::Tail { emulator } = self {
            emulator.set_cell_pixels(width, height);
        }
    }

    /// La taille que le layout donne au pane. Vrai si elle a changé. Un pane local la pose lui-même : l'émulateur
    /// et `TIOCSWINSZ` (le noyau envoie SIGWINCH au programme), sans relecture à attendre.
    fn fit(&mut self, size: GridSize) -> bool {
        match self {
            Self::Tmux(session) => session.fit(size),
            Self::Local { pty, emulator } => {
                if emulator.size() == size {
                    return false;
                }
                emulator.resize(size);
                if let Err(error) = pty.resize(size) {
                    hud::log_event(&format!("terminal resize failed: {error}"));
                }
                true
            }
            Self::Tail { emulator } => {
                if emulator.size() == size {
                    return false;
                }
                emulator.resize(size);
                true
            }
        }
    }

    fn start_selection(&mut self, at: ViewPoint, kind: SelectionKind) {
        match self {
            Self::Tmux(session) => session.start_selection(at, kind),
            Self::Local { emulator, .. } | Self::Tail { emulator } => {
                emulator.start_selection(at, kind)
            }
        }
    }

    fn extend_selection(&mut self, at: ViewPoint) {
        match self {
            Self::Tmux(session) => session.extend_selection(at),
            Self::Local { emulator, .. } | Self::Tail { emulator } => emulator.extend_selection(at),
        }
    }

    fn clear_selection(&mut self) {
        match self {
            Self::Tmux(session) => session.clear_selection(),
            Self::Local { emulator, .. } | Self::Tail { emulator } => emulator.clear_selection(),
        }
    }

    fn select_all(&mut self) {
        match self {
            Self::Tmux(session) => session.select_all(),
            Self::Local { emulator, .. } | Self::Tail { emulator } => emulator.select_all(),
        }
    }

    fn link_at(&mut self, at: ViewPoint) -> Option<Link> {
        match self {
            Self::Tmux(session) => session.link_at(at),
            Self::Local { emulator, .. } | Self::Tail { emulator } => emulator.link_at(at),
        }
    }

    fn path_at(&mut self, at: ViewPoint) -> Option<PathMatch> {
        match self {
            Self::Tmux(session) => session.path_at(at),
            Self::Local { emulator, .. } | Self::Tail { emulator } => emulator.path_at(at),
        }
    }

    fn search(&mut self, query: &str) -> Vec<SearchMatch> {
        match self {
            Self::Tmux(session) => session.search(query),
            Self::Local { emulator, .. } | Self::Tail { emulator } => emulator.search(query),
        }
    }

    fn visible_matches(&mut self, query: &str) -> Vec<(ViewPoint, ViewPoint)> {
        match self {
            Self::Tmux(session) => session.visible_matches(query),
            Self::Local { emulator, .. } | Self::Tail { emulator } => {
                emulator.visible_matches(query)
            }
        }
    }

    fn match_in_view(&self, found: &SearchMatch) -> Option<(ViewPoint, ViewPoint)> {
        match self {
            Self::Tmux(session) => session.match_in_view(found),
            Self::Local { emulator, .. } | Self::Tail { emulator } => emulator.match_in_view(found),
        }
    }

    fn scroll_to_match(&mut self, found: &SearchMatch) {
        match self {
            Self::Tmux(session) => session.scroll_to_match(found),
            Self::Local { emulator, .. } | Self::Tail { emulator } => {
                emulator.scroll_to_match(found)
            }
        }
    }

    fn track_scroll(&mut self, on: bool) {
        match self {
            Self::Tmux(session) => session.track_scroll(on),
            Self::Local { emulator, .. } | Self::Tail { emulator } => emulator.track_scroll(on),
        }
    }

    fn take_clipboard_store(&mut self) -> Option<String> {
        match self {
            Self::Tmux(session) => session.take_clipboard_store(),
            Self::Local { emulator, .. } | Self::Tail { emulator } => {
                emulator.take_clipboard_store()
            }
        }
    }

    fn tmux(&self) -> Option<&PaneSession<ClientWriter>> {
        match self {
            Self::Tmux(session) => Some(session),
            Self::Local { .. } | Self::Tail { .. } => None,
        }
    }

    fn tmux_mut(&mut self) -> Option<&mut PaneSession<ClientWriter>> {
        match self {
            Self::Tmux(session) => Some(session),
            Self::Local { .. } | Self::Tail { .. } => None,
        }
    }
}

/// Le shell du volet : `$SHELL` de l'environnement des panes (liste blanche), sinon `/bin/zsh`.
fn shell_program(env: &[(String, String)]) -> PathBuf {
    env.iter()
        .find(|(key, _)| key == "SHELL")
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from("/bin/zsh"), PathBuf::from)
}

/// Ce que le shell distant du volet Terminal lit : entrer dans `cwd` (cité, un dossier disparu ne gêne pas), puis
/// remplacer le shell de commande par le shell de login de la machine.
pub fn remote_shell_script(cwd: &str) -> String {
    format!("cd {} 2>/dev/null; exec \"$SHELL\" -l", shell_quote(cwd))
}

/// La commande d'agent d'un pane, avec ce qui règle son envoi.
pub struct LaunchRequest {
    pub command: String,
    /// Avant ce délai, rien ne part.
    pub delay: Duration,
    /// Le rythme commun des lancements d'une restauration.
    pub pacer: Option<LaunchPacer>,
}

/// Une restauration interroge ses lancements à cette cadence plutôt qu'à celle du tick de la session (1 s) : l'écart
/// entre deux envois ne se quantifie pas à la seconde.
const PACED_LAUNCH_POLL: Duration = Duration::from_millis(100);

/// La commande d'un agent qui attend que le pane soit prêt, et le moment où l'attente a commencé.
struct PendingLaunch {
    gate: LaunchGate,
    /// Le rythme commun d'une restauration ; `None` pour une session neuve, dont la porte décide seule.
    pacer: Option<LaunchPacer>,
    command: String,
    started: Instant,
    /// La saisie de l'utilisateur reçue pendant l'attente : elle part derrière la commande de l'agent (m1).
    held: HeldInput,
}

pub struct PaneView {
    /// Nom tmux de la session du pane : il distingue les lignes du journal.
    name: String,
    /// Le pane relié à tmux (il écrit par un `ClientWriter`, le client de contrôle restant à la session), ou le shell
    /// local du volet Terminal.
    backend: PaneBackend,
    /// Fond du pane : celui de la fenêtre, ou celui du volet Terminal.
    background: u32,
    /// `@idema_agent` : l'entrée du lanceur du pane, vide tant que tmux n'a pas donné la valeur.
    agent: String,
    /// Vrai pour le pane actif de la session : il reçoit le focus, les autres sont atténués et un appui sur eux
    /// ne fait que demander le focus.
    active: bool,
    /// `pane_current_command`, tel que tmux l'a signalé.
    current_command: Option<String>,
    /// Vrai quand le programme du pane est tenu pour au premier plan de la fenêtre active (mode 1004).
    focused: bool,
    focus_handle: FocusHandle,
    /// Texte en composition (IME) : affiché au curseur, jamais envoyé avant validation.
    marked_text: Option<String>,
    /// Dernière zone peinte et cellule : servent à placer la souris et la fenêtre de l'IME.
    content: Option<Bounds<Pixels>>,
    cell: (Pixels, Pixels),
    /// Compteur qui annule les minuteries de relecture dépassées par un nouveau redimensionnement.
    resize_generation: u64,
    /// Lignes de molette pas encore consommées.
    wheel: WheelAccumulator,
    /// Le bouton enfoncé dans le pane, suivi jusqu'à son relâchement.
    gesture: Option<Gesture>,
    /// Le glisser de fichiers du Finder vu par le pane : ils partent au relâché (`drop_files`).
    drop: DropState,
    /// Dernière cellule rapportée au programme : un glissé ou un mouvement ne repart qu'au changement de cellule.
    last_cell: Option<(u16, u16)>,
    /// Le lien sous la souris pendant que `⌘` est tenu, que l'élément souligne.
    hovered_link: Option<Link>,
    /// Cellule (rangée, colonne) dont le lien a été cherché avec `⌘` : la recherche ne repart qu'au changement.
    hover_key: Option<(usize, usize)>,
    /// Le fichier que désigne le chemin survolé : `hovered_link` en porte alors la plage, pour le souligner.
    hovered_file: Option<PathTarget>,
    /// Le pane vit sur une machine distante : un chemin qu'il affiche n'est pas un fichier du Mac, il ne s'ouvre pas.
    remote: bool,
    /// Le dossier du projet de la session : un chemin relatif s'y résout tant que tmux n'a pas donné celui du pane.
    project: PathBuf,
    /// `pane_current_path`, tel que tmux l'a signalé ; vide pour le shell local, dont le PTY donne son dossier.
    pane_path: Option<PathBuf>,
    hud: HudStats,
    /// La bulle de la dernière action du pane (vidage de grille), tant que ses 2 s ne sont pas écoulées.
    notice: Option<PaneNotice>,
    /// Compteur qui annule la minuterie d'une bulle remplacée par une nouvelle.
    notice_generation: u64,
    /// La barre de recherche ouverte avec ⌘F, s'il y en a une.
    search: Option<SearchState>,
    /// Les occurrences de la partie affichée et la courante dans la vue, recalculées à chaque peinture.
    highlights: Highlights,
    /// Police du HUD, choisie à la première fois qu'il s'affiche : lister les polices installées coûte environ
    /// 80 ms, payés sinon par chaque session attachée au démarrage (P-1).
    hud_font: Option<&'static str>,
    /// Vrai une fois le premier émulateur construit installé : le démarrage utile se mesure à la peinture suivante.
    content_installed: bool,
    modes: ModeWatch,
    /// Lit la sortie brute du pane à côté de l'émulateur (cloche, notifications, marqueurs OSC 133).
    detector: SignalDetector,
    activity: PaneActivity,
    /// Vrai quand la session est celle du centre dans la fenêtre active : « attend » et « erreur » retombent alors.
    seen: bool,
    /// Dernier état publié : `StateChanged` ne part qu'à un changement.
    last_state: ActivityState,
    /// Nom de base du shell d'Idema : `pane_current_command` qui lui est égal veut dire « au prompt ».
    shell_name: String,
    /// La commande d'agent à envoyer quand le pane est prêt ; `None` pour un pane attaché tel quel.
    launch: Option<PendingLaunch>,
    /// Instant de la dernière saisie de l'utilisateur vers le pane : la sortie qui la suit de près est son écho.
    last_input: Option<Instant>,
    /// Journal en lecture seule : la vue suit la sortie (le bas, ou le début d'une sortie qui tient à l'écran) tant que
    /// l'utilisateur n'a pas fait défiler vers le haut.
    following: bool,
    /// Lecture seule (le journal d'un serveur, spec P4 §4.5) : aucune touche, aucun collage ni aucun rapport de souris
    /// n'atteint le programme ; la sélection, `⌘C`, `⌘F` et le défilement restent.
    read_only: bool,
    /// Le lien vers la machine de la session est tombé (posé par la `SessionView`, comme `rebind`) : rien de ce qu'on
    /// taperait n'arriverait, et rien n'est écrit sur l'entrée de ssh (une saisie retenue ne serait jamais rejouée).
    link_down: bool,
    /// Une saisie a été refusée parce que le lien était tombé : la bulle la dit au prochain passage qui a la main.
    dropped_input: bool,
    _subscriptions: Vec<Subscription>,
    /// La tâche qui vide les événements du PTY local, ou les octets du fichier suivi, dans la vue ; elle meurt avec elle.
    _pty_task: Option<Task<()>>,
    /// Le bail du canal ssh d'un shell distant du volet Terminal : tenu tant que la vue vit, sa chute libère le canal.
    _lease: Option<Lease>,
    /// Le suivi du fichier d'un journal (`PaneBackend::Tail`) ; lâché avec la vue, il arrête son fil.
    _tail: Option<FileTail>,
    /// La minuterie rapide d'un lancement de restauration en attente.
    _launch_task: Option<Task<()>>,
}

impl PaneView {
    /// Relie le pane au client de la session par `writer` et lance sa relecture. `size` est la taille que le
    /// layout lui donne ; `launch` la commande d'agent à taper quand le pane est prêt, pas avant le délai donné.
    pub fn new(
        writer: ClientWriter,
        pane: PaneId,
        size: GridSize,
        launch: Option<LaunchRequest>,
        session: SessionRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut pane_session = PaneSession::attach(writer, pane, size);
        subscribe_watches(&mut pane_session, pane);
        let mut view = Self::with_backend(
            PaneBackend::Tmux(pane_session),
            theme::WINDOW,
            launch,
            session.name,
            session.project,
            window,
            cx,
        );
        view.shell_name = pane_shell_name(session.shell, std::env::var("SHELL").ok().as_deref());
        view.remote = session.remote;
        view
    }

    /// Le shell local du volet Terminal (spec §3.1) : un PTY dans `project`, avec l'environnement `env` des panes
    /// (intégration zsh comprise). Ses octets arrivent par un canal sans borne que le fil de lecture du PTY remplit
    /// (il ne doit jamais bloquer) et qu'une tâche de la vue vide. `Err` quand le shell ne peut pas démarrer
    /// (dossier du projet disparu, plus de terminal).
    pub fn spawn_local(
        project: &Path,
        env: &[(String, String)],
        size: GridSize,
        window: &mut Window,
        cx: &mut App,
    ) -> io::Result<Entity<Self>> {
        let program = shell_program(env);
        Self::spawn_pty_view(
            &PtyCommand {
                program: &program,
                args: &["-l"],
                directory: project,
                env,
                size,
            },
            &crate::project::display_name(project),
            project,
            None,
            window,
            cx,
        )
    }

    /// Le shell d'une machine distante dans le volet Terminal (spec ssh-tmux §3.7) : un PTY local qui lance
    /// `/usr/bin/ssh -t …` sur le maître du bail, puis `remote_shell_script(cwd)`. La vue garde le bail tant que le
    /// shell vit : sa chute libère le canal. Les chemins du pane sont ceux de la machine (`remote`) : ⌘-clic et dépôt du
    /// Finder n'y font rien. `Err` quand ssh ne peut pas démarrer.
    pub fn spawn_remote_shell(
        lease: Lease,
        cwd: &str,
        size: GridSize,
        window: &mut Window,
        cx: &mut App,
    ) -> io::Result<Entity<Self>> {
        let spec = lease
            .server
            .remote_spec()
            .ok_or_else(|| io::Error::other("the lease does not belong to a remote machine"))?;
        let mut arguments = ssh_arguments(spec, true);
        arguments.push(remote_shell_script(cwd));
        // ssh tourne dans le dossier personnel de ce Mac : le dossier distant est celui du script.
        let directory = spec
            .ssh_env
            .iter()
            .find(|(key, _)| key == "HOME")
            .map_or_else(|| PathBuf::from("/"), |(_, home)| PathBuf::from(home));
        let env = spec.ssh_env.clone();
        let name = spec.destination.clone();
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let command = PtyCommand {
            program: Path::new(SSH_BINARY),
            args: &arguments,
            directory: &directory,
            env: &env,
            size,
        };
        Self::spawn_pty_view(&command, &name, Path::new(cwd), Some(lease), window, cx)
    }

    /// Lance le PTY de `command` et en fait la vue d'un volet : l'émulateur, la tâche qui vide les événements du PTY, le
    /// suivi du focus. `lease` est le bail d'un shell distant, gardé avec la vue.
    fn spawn_pty_view(
        command: &PtyCommand<'_>,
        name: &str,
        project: &Path,
        lease: Option<Lease>,
        window: &mut Window,
        cx: &mut App,
    ) -> io::Result<Entity<Self>> {
        let (sender, mut receiver) = mpsc::unbounded();
        let size = command.size;
        let pty = LocalPty::spawn(
            command,
            // Une erreur d'envoi veut dire que la vue est fermée.
            move |event| {
                let _ = sender.unbounded_send(event);
            },
        )?;
        let emulator = PaneEmulator::new(size, EMULATOR_HISTORY, Source::LocalPty);
        let remote = lease.is_some();
        Ok(cx.new(|cx: &mut Context<Self>| {
            let task = cx.spawn_in(window, async move |this, cx| {
                while let Some(first) = receiver.next().await {
                    // Une rafale de sortie se traite en une seule mise à jour et une seule peinture.
                    let mut batch = vec![first];
                    while let Ok(next) = receiver.try_recv() {
                        batch.push(next);
                    }
                    if this
                        .update(cx, |view, cx| {
                            for event in batch {
                                view.on_pty_event(event, cx);
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let mut view = Self::with_backend(
                PaneBackend::Local { pty, emulator },
                theme::PANEL,
                None,
                name,
                project,
                window,
                cx,
            );
            // Un pane local est toujours l'actif : un appui sur lui est le sien, sans passer par une session.
            view.active = true;
            view.remote = remote;
            view._lease = lease;
            view._pty_task = Some(task);
            view._subscriptions
                .push(cx.observe_window_activation(window, |view, window, cx| {
                    view.sync_local_focus(window, cx);
                }));
            view
        }))
    }

    fn with_backend(
        backend: PaneBackend,
        background: u32,
        launch: Option<LaunchRequest>,
        name: &str,
        project: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // Une restauration interroge ses lancements vite ; la tâche s'arrête dès que le lancement est réglé.
        let launch_task = launch
            .as_ref()
            .is_some_and(|request| request.pacer.is_some())
            .then(|| {
                cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(PACED_LAUNCH_POLL).await;
                        let pending = this.update(cx, |view, _| {
                            view.poll_launch(Instant::now());
                            view.launch.is_some()
                        });
                        if !pending.unwrap_or(false) {
                            break;
                        }
                    }
                })
            });
        let subscriptions = vec![
            cx.on_focus(&focus_handle, window, |view, window, cx| {
                view.sync_local_focus(window, cx);
                cx.emit(PaneEvent::FocusChanged);
                cx.notify();
            }),
            cx.on_blur(&focus_handle, window, |view, window, cx| {
                view.sync_local_focus(window, cx);
                cx.emit(PaneEvent::FocusChanged);
                cx.notify();
            }),
        ];
        Self {
            name: name.to_owned(),
            backend,
            background,
            agent: String::new(),
            active: false,
            current_command: None,
            focused: false,
            focus_handle,
            marked_text: None,
            content: None,
            cell: (px(10.), px(25.)),
            resize_generation: 0,
            wheel: WheelAccumulator::default(),
            gesture: None,
            drop: DropState::default(),
            last_cell: None,
            hovered_link: None,
            hover_key: None,
            hovered_file: None,
            remote: false,
            project: project.to_owned(),
            pane_path: None,
            hud: HudStats::new(),
            notice: None,
            notice_generation: 0,
            search: None,
            highlights: Highlights::default(),
            content_installed: false,
            hud_font: None,
            modes: ModeWatch::default(),
            detector: SignalDetector::new(),
            activity: PaneActivity::attached(Instant::now()),
            seen: false,
            last_state: ActivityState::Idle,
            shell_name: pane_shell_name(None, std::env::var("SHELL").ok().as_deref()),
            launch: launch.map(
                |LaunchRequest {
                     command,
                     delay,
                     pacer,
                 }| {
                    let now = Instant::now();
                    PendingLaunch {
                        // La porte démarre à la fin du délai : avant, rien ne part ; l'abandon compte depuis là.
                        gate: LaunchGate::new(now + delay),
                        pacer,
                        command,
                        started: now,
                        held: HeldInput::new(),
                    }
                },
            ),
            last_input: None,
            following: true,
            read_only: false,
            link_down: false,
            dropped_input: false,
            _subscriptions: subscriptions,
            _pty_task: None,
            _lease: None,
            _tail: None,
            _launch_task: launch_task,
        }
    }

    /// Le pane devient (ou cesse d'être) une vue en lecture seule : plus aucun octet de l'utilisateur ne part vers le
    /// programme. Les touches ne sont pas retenues non plus, `⌘C` et `⌘F` restent aux actions du pane.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// Le lien vers la machine tombe ou revient. Tant qu'il est tombé, la saisie, le collage et les rapports du pane ne
    /// sont pas écrits.
    pub fn set_link_down(&mut self, link_down: bool) {
        self.link_down = link_down;
        if !link_down {
            self.dropped_input = false;
        }
    }

    /// Dit par la bulle qu'une saisie n'est pas partie, si c'est arrivé depuis le dernier passage. Une seule bulle par
    /// série de touches : elle se renouvelle à chaque appel tant qu'on tape.
    fn announce_dropped_input(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.dropped_input) {
            self.show_failure(LINK_DOWN_NOTICE.to_owned(), cx);
        }
    }

    /// La vue est-elle en lecture seule (un journal) ? L'élément n'y dessine pas de curseur.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Le pane de la session serveur de tmux, relu par le client de contrôle `writer` et affiché en lecture seule : le
    /// journal d'un serveur démarré par IdemA (spec P4 §4.5). Fond du volet Browser, toujours « actif » (un appui sur lui
    /// est le sien). `size` est la grille de départ : le premier `fit` pose la bonne.
    pub fn attach_journal(
        writer: ClientWriter,
        pane: PaneId,
        size: GridSize,
        session: SessionRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::with_backend(
            PaneBackend::Tmux(PaneSession::attach(writer, pane, size)),
            theme::PANEL,
            None,
            session.name,
            session.project,
            window,
            cx,
        );
        view.active = true;
        view.set_read_only(true);
        view
    }

    /// Le journal d'un serveur dont la sortie va dans le fichier `path` : sa fin (64 Kio) puis chaque ajout, dans un
    /// émulateur en lecture seule. Les octets arrivent par un canal sans borne que le fil du suivi remplit et qu'une
    /// tâche de la vue vide.
    pub fn spawn_tail(
        path: &Path,
        name: &str,
        project: &Path,
        size: GridSize,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let (sender, mut receiver) = mpsc::unbounded::<Vec<u8>>();
        // Une erreur d'envoi veut dire que la vue est fermée : le suivi s'arrête.
        let tail = FileTail::spawn(path, move |bytes| sender.unbounded_send(bytes).is_ok());
        let emulator = PaneEmulator::new(size, EMULATOR_HISTORY, Source::LocalPty);
        cx.new(|cx: &mut Context<Self>| {
            let task = cx.spawn_in(window, async move |this, cx| {
                while let Some(first) = receiver.next().await {
                    // Une rafale de sortie se traite en une seule mise à jour et une seule peinture.
                    let mut batch = vec![first];
                    while let Ok(next) = receiver.try_recv() {
                        batch.push(next);
                    }
                    if this
                        .update(cx, |view, cx| {
                            for bytes in batch {
                                view.on_tail_bytes(&bytes);
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let mut view = Self::with_backend(
                PaneBackend::Tail { emulator },
                theme::PANEL,
                None,
                name,
                project,
                window,
                cx,
            );
            view.active = true;
            view.set_read_only(true);
            view._pty_task = Some(task);
            view._tail = Some(tail);
            view
        })
    }

    /// Des octets du fichier suivi pour l'émulateur. Ses réponses aux requêtes du programme (position du curseur...) ne
    /// vont nulle part : un fichier n'écoute pas.
    fn on_tail_bytes(&mut self, bytes: &[u8]) {
        if let PaneBackend::Tail { emulator } = &mut self.backend {
            emulator.feed(bytes);
            let _ = emulator.take_pty_writes();
        }
        self.log_mode_changes();
        self.settle_journal_view();
    }

    /// L'utilisateur vient de faire défiler la vue (molette, `⌘F`) : un journal ne la suit plus s'il est monté.
    fn note_user_scroll(&mut self) {
        if self.read_only {
            self.following = following_after_scroll(self.backend.emulator().display_offset());
        }
    }

    /// Pose la vue d'un journal (voir `journal_scroll`), sauf si l'utilisateur l'a fait défiler vers le haut.
    fn settle_journal_view(&mut self) {
        if !self.read_only || !self.following {
            return;
        }
        let emulator = self.backend.emulator();
        let (cursor_row, _) = emulator.cursor();
        if let Some(lines) = journal_scroll(
            emulator.history_size(),
            emulator.size().lines,
            cursor_row,
            emulator.display_offset(),
        ) {
            self.backend.scroll_history(lines);
        }
    }

    /// Le pane de tmux ; `None` pour le shell local du volet.
    pub fn pane(&self) -> Option<PaneId> {
        self.backend.tmux().map(PaneSession::pane)
    }

    /// Le PID du shell local du volet Terminal ; `None` pour un pane de tmux. Les serveurs repérés sous ce shell
    /// sont « lancés par le volet Terminal ».
    pub fn local_shell_pid(&self) -> Option<i32> {
        match &self.backend {
            PaneBackend::Local { pty, .. } => i32::try_from(pty.shell_pid()).ok(),
            PaneBackend::Tmux(_) | PaneBackend::Tail { .. } => None,
        }
    }

    /// Fin du shell local à la sortie d'Idema : SIGHUP à son groupe et à celui de sa commande, puis un process détaché
    /// envoie le SIGKILL (écart E27). Sans effet sur un pane tmux, que tmux garde.
    pub fn terminate_local(&mut self) {
        if let PaneBackend::Local { pty, .. } = &mut self.backend {
            pty.terminate(KILL_GRACE);
        }
    }

    /// Le PTY local envoie sa sortie, ou annonce la fin du shell. Le terminal local répond lui-même aux requêtes du
    /// programme (spec §6.2) : les réponses de l'émulateur repartent aussitôt vers le shell.
    fn on_pty_event(&mut self, event: PtyEvent, cx: &mut Context<Self>) {
        match event {
            PtyEvent::Output(bytes) => {
                if let PaneBackend::Local { pty, emulator } = &mut self.backend {
                    emulator.feed(&bytes);
                    let answers = emulator.take_pty_writes();
                    if !answers.is_empty() {
                        pty.write(&answers);
                    }
                }
                self.flush_clipboard_store(cx);
                self.clear_hover(cx);
                self.log_mode_changes();
            }
            PtyEvent::Closed => cx.emit(PaneEvent::Closed),
        }
    }

    /// Le shell local sait s'il a le focus de la fenêtre active : ESC[I et ESC[O (mode 1004) pour un programme qui
    /// les demande, comme pour un pane tmux (que la session informe elle-même).
    fn sync_local_focus(&mut self, window: &Window, cx: &mut Context<Self>) {
        if matches!(self.backend, PaneBackend::Local { .. }) {
            let focused = self.focus_handle.is_focused(window) && window.is_window_active();
            self.set_focused(focused, cx);
        }
    }

    /// Le dernier état publié du pane.
    pub fn state(&self) -> ActivityState {
        self.last_state
    }

    /// Le pane devient l'actif de la session ou cesse de l'être.
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active != active {
            self.active = active;
            if !active {
                self.clear_hover(cx);
            }
            cx.notify();
        }
    }

    /// Un nouveau client de contrôle pour la session : le pane garde son émulateur, ses états et son historique ;
    /// une relecture complète repart et les abonnements sont repris (ils meurent avec l'ancien client).
    pub fn rebind(&mut self, writer: ClientWriter) {
        let Some(session) = self.backend.tmux_mut() else {
            return;
        };
        let pane = session.pane();
        session.rebind(writer);
        subscribe_watches(session, pane);
    }

    /// Le pane quitte le suivi de la session : ses abonnements sont retirés du client.
    pub fn release(&mut self) {
        let Some(session) = self.backend.tmux_mut() else {
            return;
        };
        let pane = session.pane();
        for watch in WATCHES {
            session.send_command(&watch.unsubscribe(pane));
        }
        session.unsubscribe_size();
    }

    /// Le shell est au prompt (marqueurs OSC 133, sinon commande au premier plan).
    pub fn at_prompt(&self) -> bool {
        self.activity.at_prompt()
    }

    /// Les programmes encore en cours au premier plan, pour confirmer une fermeture.
    pub fn running_programs(&self) -> Vec<String> {
        running_programs_of(
            self.activity.command_running(),
            self.current_command.as_deref(),
            &self.shell_name,
        )
    }

    /// Envoie la commande d'un agent dans le pane s'il est au prompt : les octets UTF-8 puis `0d` (spec §4.3).
    /// Faux, sans rien taper, quand un programme tourne. Une porte encore en attente est abandonnée : la commande
    /// ne partirait pas deux fois.
    pub fn launch_in_pane(&mut self, command: &str) -> bool {
        if !launch_reaches_program(self.at_prompt(), self.link_down) {
            return false;
        }
        let held = self.launch.take().map(|mut pending| pending.held.take());
        self.type_command(command);
        if let Some(held) = held {
            self.release_held(&held);
        }
        true
    }

    /// Tape la commande de reprise d'une session ramenée d'ailleurs, si le pane est au prompt. Faux, sans rien envoyer,
    /// quand un programme tourne. `⌃C` (`\x03`) abandonne d'abord la saisie en cours : `⌃U` n'efface qu'une ligne, et le
    /// `\r` de la commande exécuterait le reste d'un tampon collé sur plusieurs lignes (mesuré dans un vrai zsh : les deux
    /// premières lignes d'un collage de trois partent sans appui sur Entrée) ; à une invite de continuation (`dquote>`,
    /// `for>`), la commande serait avalée par la saisie ouverte. `⌃C` a tenu dans les cinq états sondés. La commande
    /// part ensuite par `launch_in_pane` : la porte de lancement en attente y est traitée une seule fois.
    pub fn reclaim_in_pane(&mut self, command: &str) -> bool {
        if !launch_reaches_program(self.at_prompt(), self.link_down) {
            return false;
        }
        self.backend.send_bytes(b"\x03");
        self.launch_in_pane(command)
    }

    /// Montre une bulle d'échec : ce qu'un geste n'a pas fait, et pourquoi.
    pub fn show_failure(&mut self, text: String, cx: &mut Context<Self>) {
        self.show_notice(PaneNotice { text, failed: true }, cx);
    }

    /// Envoie au pane la saisie retenue pendant un lancement, derrière la commande de l'agent ou seule.
    fn release_held(&mut self, held: &[u8]) {
        if !held.is_empty() {
            self.backend.send_bytes(held);
        }
    }

    /// Toute saisie de l'utilisateur vers le pane (touches, collage, texte validé de l'IME) passe ici. Elle
    /// désarme le tour d'agent (l'écho n'en est pas un) ; pendant qu'un lancement attend, elle est retenue, et un
    /// Ctrl-C abandonne le lancement (m1).
    fn send_user_input(&mut self, bytes: &[u8]) {
        if !input_reaches_program(self.read_only, false) {
            return;
        }
        if self.link_down {
            self.dropped_input = true;
            return;
        }
        self.activity.on_input();
        self.last_input = Some(Instant::now());
        if let Some(pending) = self.launch.as_mut() {
            match pending.held.push(bytes) {
                HeldStep::Held => return,
                HeldStep::Abandon(all) => {
                    self.launch = None;
                    hud::log_event(&format!("launch abandoned by input session={}", self.name));
                    self.backend.scroll_to_bottom();
                    self.backend.send_bytes(&all);
                    return;
                }
            }
        }
        self.backend.scroll_to_bottom();
        write_user_input(self.read_only, self.link_down, &mut self.backend, bytes);
    }

    /// Le pane prend-il une saisie venue du téléphone ? Fermé par défaut : hors lecture seule, et seulement si une
    /// commande tourne (au prompt du shell, ou tant qu'IdemA ne sait pas ce qui tourne, non), et si le lien de sa machine
    /// répond (`link_down` jetterait la saisie sans le dire).
    pub fn accepts_remote_input(&self) -> bool {
        accepts_remote_input(self.read_only, self.command_running(), self.link_down)
    }

    /// Le lien de la machine du pane est-il tombé ? Toujours faux pour un pane local.
    pub fn link_is_down(&self) -> bool {
        self.link_down
    }

    /// Une commande tourne-t-elle dans le pane (entre OSC 133;C et D ; sans marqueur, un programme que l'on sait n'être
    /// pas le shell) ? Faux tant qu'IdemA ne sait pas ce qui tourne.
    pub fn command_running(&self) -> bool {
        self.activity.command_running()
    }

    /// Les modes du programme qui changent l'encodage des touches, comme pour le clavier.
    fn key_modes(&self) -> KeyModes {
        let emulator = self.backend.emulator();
        KeyModes {
            app_cursor: emulator.mode().contains(TermMode::APP_CURSOR),
            extended_keys: emulator.extended_keys(),
        }
    }

    /// Tape une touche du téléphone. Faux, sans rien taper, quand le pane n'accepte rien ou que la touche n'a pas
    /// d'encodage.
    pub fn send_remote_key(&mut self, key: RemoteKey) -> bool {
        if !self.accepts_remote_input() {
            return false;
        }
        let Some(bytes) = encode_remote_key(key, self.key_modes()) else {
            return false;
        };
        self.send_user_input(&bytes);
        true
    }

    /// Tape un texte du téléphone, tel quel (jamais encadré comme un collage : une ligne, sans caractère de contrôle).
    pub fn send_remote_text(&mut self, text: &str) -> bool {
        if !self.accepts_remote_input() {
            return false;
        }
        self.send_user_input(text.as_bytes());
        true
    }

    /// Tape Entrée pour le téléphone, une écriture à part (`ENTER_DELAY` après la précédente).
    pub fn send_remote_enter(&mut self) -> bool {
        self.send_remote_key(RemoteKey::Enter)
    }

    /// Acquitte ce pane seul : « attend » et « erreur » retombent et l'état est republié. Le drapeau « vue » de la
    /// session n'est pas touché. Idempotent.
    pub fn acknowledge(&mut self, cx: &mut Context<Self>) {
        self.activity.mark_seen();
        self.publish_state(Instant::now(), cx);
    }

    /// Tape la commande d'un agent. Le démarrage de l'agent n'est pas un tour : la grâce d'activité repart d'ici,
    /// pas de l'attache. Ce n'est pas une saisie de l'utilisateur, donc pas de passage par `send_user_input`.
    fn type_command(&mut self, command: &str) {
        self.activity.on_launch(Instant::now());
        // Le `@idnma_agent` d'un agent précédent de ce pane nommerait le nouveau : retiré avant la frappe, par le même
        // client de contrôle, donc avant qu'un `idnma run` tapé ici ne pose le sien.
        if let Some(session) = self.backend.tmux_mut() {
            let line = unset_pane_option(session.pane(), "@idnma_agent");
            session.send_command(&line);
        }
        self.backend.scroll_to_bottom();
        self.backend.send_bytes(command.as_bytes());
        self.backend.send_bytes(b"\r");
    }

    /// Interroge la porte d'envoi : tape la commande quand le pane est prêt, l'abandonne passé le délai.
    /// La porte est retirée dans les deux cas : elle n'est plus interrogée ensuite.
    fn poll_launch(&mut self, now: Instant) {
        // Rien ne part sur un lien tombé : la porte attend, elle n'est pas consommée.
        if self.link_down {
            return;
        }
        let Some(pending) = &self.launch else {
            return;
        };
        let shell_in_foreground = self
            .current_command
            .as_deref()
            .map(|command| command == self.shell_name);
        let step = paced_step(
            pending.gate.poll(now, shell_in_foreground),
            pending.pacer.as_ref(),
            now,
            LAUNCH_STAGGER,
        );
        match step {
            LaunchStep::Wait => {}
            LaunchStep::Send => {
                if let Some(mut pending) = self.launch.take() {
                    self.type_command(&pending.command);
                    let held = pending.held.take();
                    self.release_held(&held);
                    hud::log_event(&format!(
                        "launch session={} after_ms={} held_bytes={}",
                        self.name,
                        now.saturating_duration_since(pending.started).as_millis(),
                        held.len()
                    ));
                }
            }
            LaunchStep::GiveUp => {
                if let Some(mut pending) = self.launch.take() {
                    // L'agent ne part pas : la saisie retenue part telle quelle au shell.
                    let held = pending.held.take();
                    self.release_held(&held);
                    hud::log_event(&format!(
                        "launch abandoned session={} held_bytes={}",
                        self.name,
                        held.len()
                    ));
                }
            }
        }
    }

    /// La session est vue ou ne l'est plus. Vue, « attend » et « erreur » retombent.
    pub fn set_seen(&mut self, seen: bool, cx: &mut Context<Self>) {
        let now = Instant::now();
        if self.seen && !seen {
            // Le départ est daté : `rearm_unanswered` rattrape les secondes qui suivent (verrouillage de l'écran).
            self.activity.left_view(now);
        } else if seen {
            self.activity.entered_view();
        }
        self.seen = seen;
        if seen {
            self.activity.mark_seen();
        }
        self.publish_state(now, cx);
    }

    /// Romain s'éloigne alors que ce pane est vu, ou ne l'a cessé d'être que depuis `REARM_GRACE` au plus : la question
    /// posée sous ses yeux, restée sans réponse, repasse en « attend ». Ne publie rien : `set_seen(false)` suit et publie
    /// le pane devenu non vu (AC-25).
    pub fn rearm_unanswered(&mut self) {
        self.activity.rearm_on_departure(self.seen, Instant::now());
    }

    /// Compare l'état du pane au dernier publié ; émet `StateChanged` à un changement seulement.
    fn publish_state(&mut self, now: Instant, cx: &mut Context<Self>) {
        let current = self.activity.state(now);
        if current != self.last_state {
            let previous = std::mem::replace(&mut self.last_state, current);
            cx.emit(PaneEvent::StateChanged { previous, current });
        }
    }

    /// Le pane a le focus de la fenêtre active ou l'a perdu. Le programme l'apprend par ESC[I / ESC[O (mode
    /// 1004), au changement seulement : la session appelle ici à chaque activation de la fenêtre.
    pub fn set_focused(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        self.report_focus(focused);
        cx.notify();
    }

    /// L'émulateur du pane, que l'élément peint.
    pub fn emulator(&self) -> &PaneEmulator {
        self.backend.emulator()
    }

    pub fn focus_handle_ref(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub fn marked_text(&self) -> Option<&str> {
        self.marked_text.as_deref()
    }

    /// Le lien que `⌘` et la souris désignent : l'élément le souligne.
    pub fn hovered_link(&self) -> Option<&Link> {
        self.hovered_link.as_ref()
    }

    /// Vrai tant qu'un bouton enfoncé dans le pane n'est pas relâché : l'élément écoute alors la souris au-delà du pane.
    pub fn has_gesture(&self) -> bool {
        self.gesture.is_some()
    }

    /// Appelé par l'élément à la fin de chaque peinture : durée de l'image, latence d'écho, démarrage.
    /// Aucun accès disque ici : le journal reste dans son tampon.
    pub fn record_frame(&mut self, duration: Duration, refresh_interval: Option<Duration>) {
        let now = Instant::now();
        self.hud.record_frame(now, duration);
        self.hud.set_refresh_interval(refresh_interval);
        // La latence d'écho court de l'arrivée de la sortie à la fin de la peinture qui la montre.
        if let Some(arrived) = self
            .backend
            .tmux_mut()
            .and_then(|session| session.last_output_at.take())
        {
            // Fermé ici et non à chaque peinture : une image en cours à l'arrivée ne montre pas encore la sortie.
            signpost::echo_painted();
            self.hud.record_echo(now.saturating_duration_since(arrived));
        }
        let startup = hud::since_process_start();
        if self.hud.record_startup(startup) {
            hud::log_line(&format!("first paint startup_ms={}", startup.as_millis()));
        }
        // Le premier paint précède la relecture : le démarrage qui compte est celui du premier paint avec le contenu.
        if self.content_installed && self.hud.record_content_startup(startup) {
            hud::log_line(&format!(
                "first content paint startup_ms={}",
                startup.as_millis()
            ));
        }
    }

    /// Un pas de la minuterie de la session (une seconde) : fait tomber les états à délai, interroge la porte de
    /// lancement et rafraîchit le HUD s'il est visible.
    pub fn on_tick(&mut self, now: Instant, cx: &mut Context<Self>) {
        let bracketed_paste = self
            .backend
            .emulator()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        self.activity.tick(now, bracketed_paste, self.seen);
        self.publish_state(now, cx);
        self.poll_launch(now);
        // Une saisie refusée sans geste qui puisse le dire (souris, molette) ne reste pas muette.
        self.announce_dropped_input(cx);
        if self.hud.visible {
            cx.notify();
        }
    }

    /// Écrit la mesure du HUD au journal. La session n'appelle ici que pour le pane affiché, une fois par pas de
    /// journal : dix sessions écriraient dix lignes indiscernables.
    pub fn log_measure(&mut self, cx: &mut Context<Self>) {
        hud::log_line(&format!(
            "{} session={}",
            self.hud.log_line(Instant::now()),
            self.name
        ));
        // Le vidage sur disque ne passe ni par le fil d'interface ni par la peinture.
        cx.background_spawn(async { hud::flush() }).detach();
    }

    /// Journalise les changements des modes 2004 et `extended_keys` (mesure R1).
    fn log_mode_changes(&mut self) {
        let emulator = self.backend.emulator();
        let changes = self.modes.changes(
            emulator.mode().contains(TermMode::BRACKETED_PASTE),
            emulator.extended_keys(),
        );
        for change in changes {
            hud::log_event(&change);
        }
    }

    fn toggle_hud(&mut self, _: &ToggleHud, _window: &mut Window, cx: &mut Context<Self>) {
        self.hud.visible = !self.hud.visible;
        cx.notify();
    }

    fn dump_grid(&mut self, _: &DumpGrid, _window: &mut Window, cx: &mut Context<Self>) {
        // Le shell local du volet n'a pas d'identifiant tmux : son vidage s'écrit sous `grid-local.txt`.
        let (file, label) = match self.pane() {
            Some(pane) => (pane.0.to_string(), format!("%{}", pane.0)),
            None => ("local".to_owned(), "local".to_owned()),
        };
        let text = format!("{}\n", self.backend.emulator().dump_text());
        let result = hud::write_grid_dump(&file, &text);
        match &result {
            Ok(path) => hud::log_event(&format!("grid dump pane={label} path={}", path.display())),
            Err(error) => hud::log_event(&format!("grid dump failed pane={label}: {error}")),
        }
        self.show_notice(PaneNotice::dump(&result), cx);
    }

    /// `⌘F` : ouvre la barre de recherche et lui donne le focus ; si elle est déjà ouverte, le champ reprend le focus et
    /// son texte est sélectionné.
    fn open_search(&mut self, _: &OpenSearch, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(search) = &self.search {
            search
                .bar
                .update(cx, |bar, cx| bar.focus_and_select(window, cx));
            return;
        }
        let bar = cx.new(|cx| SearchBar::new(window, cx));
        let events = cx.subscribe_in(&bar, window, Self::on_search_event);
        self.search = Some(SearchState {
            bar,
            query: String::new(),
            matches: Vec::new(),
            current: None,
            _bar_events: events,
        });
        cx.notify();
    }

    fn on_search_event(
        &mut self,
        _bar: &Entity<SearchBar>,
        event: &SearchEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SearchEvent::Changed(query) => self.search_changed(query.clone(), cx),
            SearchEvent::Next => self.step_search(true, cx),
            SearchEvent::Previous => self.step_search(false, cx),
            SearchEvent::Close => self.close_search(window, cx),
        }
    }

    fn search_next(&mut self, _: &SearchNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.step_search(true, cx);
    }

    fn search_previous(
        &mut self,
        _: &SearchPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Barre fermée, `⇧⌘G` n'a rien à chercher : la touche passe au `Workspace`, où elle ouvre le volet Changes.
        if self.search.is_none() {
            cx.propagate();
            return;
        }
        self.step_search(false, cx);
    }

    /// Cherche `query` dans tout l'historique et l'écran, et le note au journal. Le résultat va du plus récent au plus
    /// ancien : le numéro 1 du compteur est le plus récent, Entrée remonte vers l'ancien.
    fn run_search(&mut self, query: &str) -> Vec<SearchMatch> {
        let started = Instant::now();
        // Les coordonnées des occurrences doivent survivre à la sortie qui défile un historique plein.
        self.backend.track_scroll(true);
        let mut matches = self.backend.search(query);
        matches.reverse();
        let label = self
            .pane()
            .map_or_else(|| "local".to_owned(), |pane| format!("%{}", pane.0));
        hud::log_event(&format!(
            "search pane={label} matches={} ms={}",
            matches.len(),
            started.elapsed().as_millis()
        ));
        matches
    }

    /// Le texte du champ a changé : la recherche repart de zéro et prend l'occurrence la plus proche du bas de la vue.
    fn search_changed(&mut self, query: String, cx: &mut Context<Self>) {
        if self.search.is_none() {
            return;
        }
        let matches = if query.is_empty() {
            Vec::new()
        } else {
            self.run_search(&query)
        };
        let bottom = self.backend.emulator().view_bottom_line();
        let current = closest_above_bottom(&matches, bottom);
        self.show_search(query, matches, current, cx);
    }

    /// `⌘G`, `⇧⌘G`, Entrée, `⇧Entrée` et les boutons : la sortie arrivée depuis la dernière recherche s'y ajoute, puis
    /// la vue passe à l'occurrence suivante ou précédente, en boucle.
    fn step_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(search) = &self.search else {
            return;
        };
        if search.query.is_empty() {
            return;
        }
        let query = search.query.clone();
        let previous = search
            .current
            .and_then(|index| search.matches.get(index))
            .copied();
        let matches = self.run_search(&query);
        let here = previous.and_then(|found| index_of(&matches, &found));
        // L'occurrence courante n'existe plus (elle est partie du haut de l'historique) : `here` est déjà la plus
        // récente des plus anciennes, donc la suivante dans le sens de Entrée.
        let replaced = previous
            .zip(here)
            .is_some_and(|(found, index)| matches[index].start != found.start);
        let current = if forward && replaced {
            here
        } else if forward {
            next_index(here, matches.len())
        } else {
            previous_index(here, matches.len())
        };
        self.show_search(query, matches, current, cx);
    }

    /// Retient le résultat d'une recherche, met la vue sur l'occurrence courante et rend le compteur à la barre.
    fn show_search(
        &mut self,
        query: String,
        matches: Vec<SearchMatch>,
        current: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if let Some(found) = current.and_then(|index| matches.get(index)) {
            self.backend.scroll_to_match(found);
            // Le saut vers une occurrence plus haut est un défilement de l'utilisateur, comme la molette.
            self.note_user_scroll();
        }
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let label = if query.is_empty() {
            String::new()
        } else {
            counter_label(current, matches.len())
        };
        search.query = query;
        search.matches = matches;
        search.current = current;
        search.bar.update(cx, |bar, cx| bar.set_counter(label, cx));
        cx.notify();
    }

    /// Échap : la barre et le surlignage disparaissent, le focus revient au pane, la vue reste où elle est.
    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.take().is_none() {
            return;
        }
        self.highlights = Highlights::default();
        self.backend.track_scroll(false);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Appelé par l'élément à chaque peinture : cherche les occurrences de la partie affichée et situe la courante.
    pub fn refresh_search_highlights(&mut self) {
        let Some(search) = &self.search else {
            self.highlights = Highlights::default();
            return;
        };
        let query = search.query.clone();
        let current = search
            .current
            .and_then(|index| search.matches.get(index))
            .and_then(|found| self.backend.match_in_view(found));
        // Un émulateur remplacé par une relecture repart sans suivi : on le rétablit à chaque peinture.
        self.backend.track_scroll(true);
        self.highlights = Highlights {
            matches: self.backend.visible_matches(&query),
            current,
        };
    }

    /// Les occurrences affichées et la courante, que l'élément surligne.
    pub fn search_highlights(&self) -> &Highlights {
        &self.highlights
    }

    /// Montre la bulle pendant `NOTICE_DURATION` ; une bulle qui suit remplace la précédente et repart de zéro.
    fn show_notice(&mut self, notice: PaneNotice, cx: &mut Context<Self>) {
        self.notice = Some(notice);
        self.notice_generation += 1;
        let generation = self.notice_generation;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_DURATION).await;
            let _ = this.update(cx, |view, cx| {
                if view.notice_generation == generation {
                    view.notice = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Appelé par l'élément à chaque `prepaint` : retient la cellule et les bornes du pane, pour la souris et l'IME.
    /// Rien n'est redimensionné ici : la taille vient de la session (`fit`).
    pub fn set_geometry(&mut self, cell: (Pixels, Pixels), bounds: Bounds<Pixels>) {
        self.cell = cell;
        self.content = Some(bounds);
    }

    /// La cellule en pixels de l'appareil, posée avant `fit` pour que la réponse à `CSI 14 t` suive la taille. Sans
    /// effet sur un pane tmux.
    pub fn set_cell_pixels(&mut self, width: f32, height: f32) {
        self.backend.set_cell_pixels(width, height);
    }

    /// La taille que le layout donne au pane. Si elle a changé, l'émulateur la prend et l'écran visible est relu
    /// une fois la taille stable ; le programme se redessine seul, ce n'est pas un tour d'agent.
    pub fn fit(&mut self, size: GridSize, cx: &mut Context<Self>) {
        if self.backend.fit(size) {
            self.activity.on_resize(Instant::now());
            self.settle_journal_view();
            self.search_again_after_reflow(cx);
            // Un shell local se redessine seul au SIGWINCH : pas de relecture, seulement une peinture.
            if self.backend.tmux().is_some() {
                self.schedule_resync(cx);
            } else {
                cx.notify();
            }
        }
    }

    /// Un reflow replie les lignes : les coordonnées que la recherche ouverte a gardées sont périmées. Elle repart sur
    /// la requête, en gardant le rang de l'occurrence courante (voir `current_after_reflow`).
    fn search_again_after_reflow(&mut self, cx: &mut Context<Self>) {
        let Some(search) = &self.search else {
            return;
        };
        if search.query.is_empty() {
            return;
        }
        let query = search.query.clone();
        let (previous_count, previous) = (search.matches.len(), search.current);
        let matches = self.run_search(&query);
        let current = current_after_reflow(&matches, previous_count, previous);
        self.show_search(query, matches, current, cx);
    }

    /// Relit l'écran visible quand la taille n'a plus bougé depuis `RESIZE_SETTLE`.
    fn schedule_resync(&mut self, cx: &mut Context<Self>) {
        self.resize_generation += 1;
        let generation = self.resize_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESIZE_SETTLE).await;
            let _ = this.update(cx, |view, cx| {
                if view.resize_generation == generation
                    && let Some(session) = view.backend.tmux_mut()
                {
                    session.resync_visible();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Le fil de lecture a ouvert l'intervalle `echo` dès l'arrivée, avant de savoir si la sortie serait appliquée.
    /// Jetée (relecture visible en cours) ou tenue (construction), elle ne sera pas peinte : on referme l'intervalle,
    /// sauf si une sortie appliquée plus tôt attend encore sa peinture. Appliquée alors que le fil de lecture
    /// n'a rien ouvert (il l'avait fait pour une sortie jetée, refermée depuis), on l'ouvre ici, un peu tard.
    fn settle_echo_signpost(&self, applied: bool) {
        if applied {
            signpost::echo_arrived();
        } else if self
            .backend
            .tmux()
            .is_none_or(|session| session.last_output_at.is_none())
        {
            signpost::echo_painted();
        }
    }

    /// Un `%subscription-changed` des abonnements `PaneWatch` de cette vue : met à jour la méta et le signale.
    /// Rend faux pour tout autre événement, qui suit le chemin de la session.
    fn on_watch(&mut self, event: &ControlEvent, cx: &mut Context<Self>) -> bool {
        let ControlEvent::SubscriptionChanged { name, value } = event else {
            return false;
        };
        let Some((watch, pane)) = PaneWatch::parse(name) else {
            return false;
        };
        if Some(pane) != self.pane() {
            return false;
        }
        let changed = match watch {
            PaneWatch::Agent => replace_if_changed(&mut self.agent, value),
            // Le dossier ne change rien à la barre : il ne sert qu'au prochain ⌘-clic.
            PaneWatch::Path => {
                self.pane_path = (!value.is_empty()).then(|| PathBuf::from(value));
                false
            }
            PaneWatch::Command => {
                let command = (!value.is_empty()).then(|| value.clone());
                // Repli sans marqueur OSC 133 (agent lancé avant une relance d'Idema) : le shell est-il au premier plan ?
                if let Some(command) = &command {
                    self.activity
                        .set_foreground_is_shell(*command == self.shell_name);
                }
                let changed = self.current_command != command;
                self.current_command = command;
                changed
            }
        };
        if changed {
            cx.emit(PaneEvent::MetaChanged);
        }
        true
    }

    /// Un événement que la session a routé vers ce pane. `Exit` ne passe pas ici : il ferme la session.
    pub fn on_event(&mut self, event: &ControlEvent, arrival: Instant, cx: &mut Context<Self>) {
        // Un shell local n'écoute pas tmux.
        let Some(own) = self.pane() else {
            return;
        };
        if self.on_watch(event, cx) {
            return;
        }
        if let ControlEvent::Output { pane, data } = event
            && *pane == own
        {
            hud::log_debug(&format!(
                "output {} at {}",
                data.len(),
                hud::since_process_start().as_millis()
            ));
        }
        let output_of_pane = matches!(&event, ControlEvent::Output { pane, .. } if *pane == own);
        // Les signaux se lisent avant `PaneSession` : une relecture de l'écran jette la sortie, pas ses signaux.
        if let ControlEvent::Output { pane, data } = event
            && *pane == own
        {
            for signal in self.detector.feed(data) {
                self.activity.on_signal(signal, self.seen);
                if let Some(pending) = &mut self.launch {
                    pending.gate.on_signal(signal);
                }
            }
            self.activity.on_output(data.len(), arrival);
            // L'écho d'une frappe ou d'un collage arrive après la saisie : il ne compte pas comme un tour.
            if is_input_echo(self.last_input, arrival) {
                self.activity.on_input();
            }
            if let Some(pending) = &mut self.launch {
                pending.gate.on_output(arrival);
            }
            self.publish_state(Instant::now(), cx);
            self.poll_launch(Instant::now());
        }
        let Some(session) = self.backend.tmux_mut() else {
            return;
        };
        let effect = session.handle_at(event.clone(), arrival);
        if output_of_pane {
            self.flush_clipboard_store(cx);
            // Le contenu a pu bouger sous la souris : le lien trouvé n'est plus à sa place.
            self.clear_hover(cx);
        }
        // Le repère `echo` ne suit que le pane actif : la session n'ouvre l'intervalle que pour sa sortie.
        if output_of_pane && self.active {
            self.settle_echo_signpost(matches!(effect, Effect::Redraw));
        }
        self.log_mode_changes();
        self.settle_journal_view();
        match effect {
            Effect::Nothing => {}
            Effect::Redraw => cx.notify(),
            Effect::ResyncVisible => self.schedule_resync(cx),
            Effect::Build {
                generation,
                snapshot,
            } => {
                // La construction d'un émulateur neuf se fait hors du fil d'interface. Même périmée, elle est rendue
                // à `install`, qui ignore seul les générations dépassées.
                cx.spawn(async move |this, cx| {
                    let (emulator, replay_ms) = cx
                        .background_spawn(async move {
                            let started = Instant::now();
                            let emulator = build_emulator(&snapshot);
                            (emulator, started.elapsed().as_millis())
                        })
                        .await;
                    hud::log_event(&format!("replay_ms={replay_ms}"));
                    let _ = this.update(cx, |view, cx| {
                        if let Some(session) = view.backend.tmux_mut()
                            && matches!(session.install(generation, emulator), Effect::Redraw)
                        {
                            view.content_installed = true;
                        }
                        view.log_mode_changes();
                        view.settle_journal_view();
                        // La sortie arrivée pendant la construction est rejouée par `install`.
                        view.flush_clipboard_store(cx);
                        cx.notify();
                    });
                })
                .detach();
            }
            // Le client de contrôle appartient à la session, qui voit `Exit` elle-même.
            Effect::Closed => {}
        }
    }

    fn report_focus(&mut self, active: bool) {
        if input_reaches_program(self.read_only, self.link_down)
            && self
                .backend
                .emulator()
                .mode()
                .contains(TermMode::FOCUS_IN_OUT)
        {
            // Un programme qui se redessine au focus n'arme pas de tour, et un agent qui écrit au même moment ne
            // perd pas le sien : la décision est dans `PaneActivity::on_focus_report`.
            self.activity.on_focus_report(Instant::now());
            write_user_input(
                self.read_only,
                self.link_down,
                &mut self.backend,
                if active { b"\x1b[I" } else { b"\x1b[O" },
            );
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // Lecture seule : la touche n'est ni encodée ni retenue, les raccourcis (`⌘C`, `⌘F`, `⌘W`...) continuent leur chemin.
        if self.read_only {
            // La preuve que cette vue a le clavier (AC-91) : la touche est arrivée ici et n'est allée nulle part ; jamais la
            // touche elle-même au journal.
            hud::log_debug(&format!("journal key ignored view={}", self.name));
            return;
        }
        let keystroke = &event.keystroke;
        let mods = Mods {
            control: keystroke.modifiers.control,
            alt: keystroke.modifiers.alt,
            shift: keystroke.modifiers.shift,
            command: keystroke.modifiers.platform,
        };
        let emulator = self.backend.emulator();
        let modes = KeyModes {
            app_cursor: emulator.mode().contains(TermMode::APP_CURSOR),
            extended_keys: emulator.extended_keys(),
        };
        // `None` : le texte arrive par le gestionnaire de saisie.
        if let Some(bytes) = encode_key(&keystroke.key, mods, modes) {
            self.send_user_input(&bytes);
            self.announce_dropped_input(cx);
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.paste_text(&text);
        self.announce_dropped_input(cx);
        cx.notify();
    }

    /// Insère `text` au curseur comme un collage : encadré si le programme a activé le mode 2004 (§6.3).
    pub(crate) fn paste_text(&mut self, text: &str) {
        let bracketed = self
            .backend
            .emulator()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        self.send_user_input(&encode_paste(text, bracketed));
    }

    /// Le glisser du Finder bouge : le pane retient les fichiers du glisser en cours. L'écoute ne passe pas par
    /// `on_drop` ni `drag_over` de GPUI : ceux-ci exigent que l'élément soit « survolé », et GPUI ne tient pas pour
    /// survolé un élément tant que la dernière entrée de la fenêtre était une touche (le glisser d'un fichier n'est
    /// pas un mouvement de souris pour lui), donc après une frappe aucun dépôt n'arrivait. Le survol, lui, vient de
    /// `drag_hit` (la peinture du terminal le mesure sur sa zone, un menu ouvert par-dessus le masque).
    fn on_files_dragged(
        &mut self,
        event: &DragMoveEvent<ExternalPaths>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.drop.dragged(event.drag(cx)) {
            cx.notify();
        }
    }

    /// Un glisser de fichiers est en cours : le pane dit s'il est sous la souris, sans qu'un menu le recouvre.
    pub fn drag_hit(&mut self, hit: bool, cx: &mut Context<Self>) {
        if self.drop.hit_test(hit) {
            cx.notify();
        }
    }

    /// Le bouton se relâche (appelé par la peinture du terminal, qui voit tous les relâchés) : `hit` dit que le relâché
    /// est sur le pane sans qu'un menu le recouvre, `dragging` qu'un glisser de fichiers est en cours. Alors les
    /// fichiers sont déposés.
    pub fn drop_files(
        &mut self,
        hit: bool,
        dragging: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dropped) = self.drop.release(hit, dragging) {
            self.on_drop_paths(&dropped, window, cx);
        }
    }

    /// Des fichiers déposés sur le pane (§6.3, AC-24) : leurs chemins, échappés, sont insérés comme une saisie, et le
    /// pane devient l'actif par le même chemin qu'un clic (la session lui donne le focus). Un nom à caractère de
    /// contrôle refuse tout le dépôt avec une alerte : rien n'est tapé.
    fn on_drop_paths(
        &mut self,
        dropped: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Les chemins du Finder n'existent pas sur la machine distante : le dépôt ne tape rien.
        if self.remote {
            hud::log_event("drop ignored remote=true");
            cx.notify();
            return;
        }
        match dropped_text(dropped.paths()) {
            Ok(text) => {
                hud::log_event(&format!("drop paths={}", dropped.paths().len()));
                if self.active {
                    window.focus(&self.focus_handle, cx);
                } else {
                    cx.emit(PaneEvent::FocusRequested);
                }
                self.paste_text(&text);
            }
            Err(path) => {
                hud::log_event("drop refused");
                let detail = format!(
                    "Son nom contient un caractère de contrôle (retour à la ligne, tabulation…) : {path:?}"
                );
                // La réponse n'a pas de suite : on laisse tomber le récepteur.
                let _answer = window.prompt(
                    PromptLevel::Warning,
                    "Ce fichier ne peut pas être inséré",
                    Some(&detail),
                    &[PromptButton::ok("OK")],
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// ⌘C : le texte sélectionné part au presse-papier. Sans sélection, rien.
    fn copy(&mut self, _: &menus::Copy, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.backend.emulator().selection_text() else {
            return;
        };
        hud::log_event(&format!("copy bytes={}", text.len()));
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// ⌘A : tout l'historique et l'écran.
    fn select_all(&mut self, _: &menus::SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        self.backend.select_all();
        cx.notify();
    }

    /// Met la dernière écriture OSC 52 du programme dans le presse-papier de macOS.
    fn flush_clipboard_store(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.backend.take_clipboard_store() {
            // Un journal affiche la sortie d'un serveur, qui peut citer des octets venus d'ailleurs (une requête, un
            // fichier) : il n'écrit jamais dans le presse-papier de l'utilisateur.
            if self.read_only {
                return;
            }
            let pane = self
                .pane()
                .map_or_else(|| "terminal".to_owned(), |pane| format!("%{}", pane.0));
            hud::log_event(&format!("osc52 store bytes={} pane={pane}", text.len()));
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// La cellule de la vue sous `position`, bornée à la grille.
    fn view_point(&self, position: Point<Pixels>) -> ViewPoint {
        match self.content {
            Some(content) => view_point_at(
                position,
                content.origin,
                self.cell,
                self.backend.emulator().size(),
            ),
            None => ViewPoint::new(0, 0),
        }
    }

    /// La cellule sous `position` si elle est dans la grille de l'émulateur (pas dans la marge).
    fn view_point_inside(&self, position: Point<Pixels>) -> Option<ViewPoint> {
        let content = self.content?;
        view_point_inside(
            position,
            content.origin,
            self.cell,
            self.backend.emulator().size(),
        )
    }

    /// Les modes souris du programme, lus dans l'émulateur.
    fn mouse_modes(&self) -> MouseModes {
        // Lecture seule : les modes souris du programme sont ignorés, la sélection et la molette restent à la vue.
        if self.read_only {
            return MouseModes::default();
        }
        let mode = self.backend.emulator().mode();
        MouseModes {
            reporting: mode.intersects(TermMode::MOUSE_MODE),
            drag: mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION),
            motion: mode.contains(TermMode::MOUSE_MOTION),
            sgr: mode.contains(TermMode::SGR_MOUSE),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
        }
    }

    /// Rapporte un événement de souris au programme, encodé selon son mode. Une saisie de l'utilisateur : l'écho qui
    /// suit n'est pas un tour.
    fn report_mouse(&mut self, action: MouseAction, at: ViewPoint, modifiers: &Modifiers) {
        let modes = self.mouse_modes();
        if let Some(bytes) = encode_mouse(
            action,
            cell_coordinate(at.column),
            cell_coordinate(at.row),
            mods_of(modifiers),
            modes,
        ) {
            self.send_user_input(&bytes);
        }
    }

    /// Un appui sur un pane qui n'est pas l'actif, ou qui l'est mais n'a pas le clavier (il est dans le volet Terminal
    /// ou la session, de l'autre côté), ne fait que prendre le focus (écart E20) : ni sélection, ni rapport souris, et
    /// il ne remonte pas (la session). Seul le clic droit remonte, pour ouvrir le menu de la session sur ce pane
    /// devenu l'actif. Sur le pane actif qui a le clavier, le bouton gauche prend le focus et ne remonte pas non plus :
    /// la session ne reprend le focus que pour les clics hors de tout pane. Ensuite, dans l'ordre : `⌘`-clic sur un lien
    /// (écart E21), rapport au programme qui capte la souris (sauf `⇧`, écart E19), sinon la sélection native.
    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keyboard_focused = self.focus_handle.is_focused(window);
        if press_is_focus_only(
            event.button,
            &event.modifiers,
            self.active,
            keyboard_focused,
        ) {
            if self.active {
                // Actif pour tmux (ou local), mais le clavier est ailleurs : le focus GPUI vient ici, sans que
                // tmux ait rien à changer.
                window.focus(&self.focus_handle, cx);
            } else {
                cx.emit(PaneEvent::FocusRequested);
            }
            if event.button != MouseButton::Right {
                cx.stop_propagation();
            }
            return;
        }
        if event.button == MouseButton::Left {
            // Un `⌘`-clic sur un pane qui n'a pas le clavier ne le lui prend pas : le chemin ouvert prend le focus.
            if !press_only_takes_focus(self.active, keyboard_focused) {
                window.focus(&self.focus_handle, cx);
            }
            cx.stop_propagation();
        }
        if is_link_press(event.button, &event.modifiers) {
            if let Some(at) = self.view_point_inside(event.position) {
                self.open_link_at(at, cx);
            }
            return;
        }
        let at = self.view_point(event.position);
        if self.mouse_modes().reporting && !event.modifiers.shift {
            // Un bouton que le protocole ne connaît pas ne part pas, mais ne ferait pas non plus de sélection.
            if let Some(kind) = button_kind(event.button) {
                // Une sélection native laissée par un `⇧`-glisser ne reste pas surlignée sous le clic du programme.
                if self.backend.emulator().has_selection() {
                    self.backend.clear_selection();
                    cx.notify();
                }
                self.gesture = Some(Gesture::Reporting(kind));
                self.last_cell = Some((cell_coordinate(at.column), cell_coordinate(at.row)));
                self.report_mouse(MouseAction::Press(kind), at, &event.modifiers);
                // Le clic droit est celui du programme : pas de menu de session par-dessus.
                cx.stop_propagation();
            }
            return;
        }
        if event.button == MouseButton::Left {
            self.backend
                .start_selection(at, selection_kind(event.click_count));
            self.gesture = Some(Gesture::Selecting);
            cx.notify();
        }
    }

    /// `⌘`-clic sur la cellule `at` : le lien dessous (écart E21) est ouvert, montré ou refusé ; sans lien, un chemin
    /// qui désigne un fichier ou un dossier existant s'ouvre dans le volet Fichiers (spec §4.7).
    fn open_link_at(&mut self, at: ViewPoint, cx: &mut Context<Self>) {
        let Some(link) = self.backend.link_at(at) else {
            self.open_path_at(at, cx);
            return;
        };
        match routed_destination(self.remote, &link.uri) {
            LinkDestination::Browser(url) => self.open_in_browser(url, cx),
            LinkDestination::System(uri) => {
                hud::log_event(&format!("link opened uri={}", address::log_link(&uri)));
                cx.open_url(&uri);
            }
            LinkDestination::Reveal(path) => {
                hud::log_event(&format!("link revealed path={}", path.display()));
                cx.reveal_path(&path);
            }
            LinkDestination::Refused => {
                hud::log_event(&format!(
                    "link refused uri={}",
                    address::log_link(&link.uri)
                ));
            }
        }
    }

    /// Demande le navigateur d'IdemA pour `url`. Le journal n'en garde ni la requête ni le fragment.
    fn open_in_browser(&mut self, url: String, cx: &mut Context<Self>) {
        hud::log_event(&format!(
            "link opened in browser url={}",
            address::log_form(&url)
        ));
        cx.emit(PaneEvent::OpenUrl(url));
    }

    /// `⌘`-clic sur un chemin : la session et l'espace de travail l'ouvrent dans le volet Fichiers.
    fn open_path_at(&mut self, at: ViewPoint, cx: &mut Context<Self>) {
        let Some(found) = self.backend.path_at(at) else {
            return;
        };
        let Some(target) = self.target_of(&found) else {
            return;
        };
        // Un document local (HTML, PDF) s'affiche : le volet Fichiers n'en montrerait que la source (P4-8).
        if let PathTarget::File(file) = &target
            && let Some(url) = document_address(&file.path)
        {
            self.open_in_browser(url, cx);
            return;
        }
        cx.emit(PaneEvent::OpenPath(target));
    }

    /// La souris bouge au-dessus du pane, sans geste en cours : le lien sous `⌘` se souligne, et un programme en mode
    /// 1003 apprend le mouvement. Un glissé, lui, est suivi par `gesture_moved`, jusque hors du pane.
    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.gesture.is_some() || !self.active {
            return;
        }
        self.update_hover(event.position, event.modifiers.platform, cx);
        let modes = self.mouse_modes();
        if modes.reporting
            && modes.motion
            && !event.modifiers.shift
            && event.pressed_button.is_none()
        {
            let at = self.view_point(event.position);
            let cell = (cell_coordinate(at.column), cell_coordinate(at.row));
            if self.last_cell != Some(cell) {
                self.last_cell = Some(cell);
                self.report_mouse(MouseAction::Move, at, &event.modifiers);
            }
        }
    }

    /// Cherche le lien sous `position` quand `⌘` est tenu, une fois par cellule.
    fn update_hover(&mut self, position: Point<Pixels>, command: bool, cx: &mut Context<Self>) {
        let at = self.view_point_inside(position);
        let key = at
            .filter(|_| command)
            .map(|point| (point.row, point.column));
        if key == self.hover_key {
            return;
        }
        self.hover_key = key;
        let (link, file) = at
            .filter(|_| key.is_some())
            .and_then(|point| self.hover_at(point))
            .map_or((None, None), |(link, file)| (Some(link), file));
        if link != self.hovered_link || file != self.hovered_file {
            self.hovered_link = link;
            self.hovered_file = file;
            cx.notify();
        }
    }

    /// Ce que `⌘` souligne sous la cellule `point` : un lien que le clic suivrait, sinon un chemin qui désigne un
    /// fichier ou un dossier existant (la plage du chemin prend alors la place de celle d'un lien).
    fn hover_at(&mut self, point: ViewPoint) -> Option<(Link, Option<PathTarget>)> {
        if let Some(link) = self.backend.link_at(point) {
            return is_followable(&link.uri).then_some((link, None));
        }
        let found = self.backend.path_at(point)?;
        let target = self.target_of(&found)?;
        let link = Link {
            uri: found.text,
            start: found.start,
            end: found.end,
        };
        Some((link, Some(target)))
    }

    /// Ce qu'un chemin du pane désigne : le dossier du pane pour un chemin relatif, sinon le projet.
    fn target_of(&self, found: &PathMatch) -> Option<PathTarget> {
        // Le chemin d'un pane distant n'existe pas sur le Mac : l'ouvrir ouvrirait un autre fichier, ou rien.
        if self.remote {
            return None;
        }
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let pane_folder = match &self.backend {
            PaneBackend::Tmux(_) => self.pane_path.clone(),
            PaneBackend::Local { pty, .. } => pty.foreground_cwd(),
            PaneBackend::Tail { .. } => None,
        };
        file_open::resolve(found, pane_folder.as_deref(), Some(&self.project), &home)
    }

    /// Plus de lien survolé : `⌘` relâché, souris ailleurs, ou contenu qui a bougé sous la souris.
    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        self.hover_key = None;
        self.hovered_file = None;
        if self.hovered_link.take().is_some() {
            cx.notify();
        }
    }

    /// La souris quitte le pane : plus de lien, et le prochain mouvement est rapporté quelle que soit sa cellule.
    fn on_pointer_left(&mut self, cx: &mut Context<Self>) {
        self.clear_hover(cx);
        self.last_cell = None;
    }

    /// `⌘` tenu ou relâché : le lien sous la souris se souligne ou s'efface sans attendre un mouvement.
    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.active {
            return;
        }
        let position = window.mouse_position();
        if event.modifiers.platform
            && self
                .content
                .is_some_and(|content| content.contains(&position))
        {
            self.update_hover(position, true, cx);
        } else {
            self.clear_hover(cx);
        }
    }

    /// Le bouton enfoncé dans le pane bouge, où que soit la souris (écouté par l'élément sur la fenêtre). Une
    /// sélection suit la cellule bornée à la grille ; un programme qui capte la souris reçoit le glissé.
    pub(crate) fn gesture_moved(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(gesture) = self.gesture else {
            return;
        };
        // Le relâché a pu se perdre (fenêtre perdue de vue, bouton lâché ailleurs) : le geste s'arrête ici.
        if event.pressed_button.is_none() {
            self.end_gesture(event.position, &event.modifiers, cx);
            return;
        }
        let at = self.view_point(event.position);
        match gesture {
            Gesture::Selecting => {
                self.backend.extend_selection(at);
                cx.notify();
            }
            Gesture::Reporting(kind) => {
                let cell = (cell_coordinate(at.column), cell_coordinate(at.row));
                if self.last_cell != Some(cell) {
                    self.last_cell = Some(cell);
                    self.report_mouse(MouseAction::Drag(kind), at, &event.modifiers);
                }
            }
        }
    }

    /// Le bouton du geste est relâché, où que soit la souris (écouté par l'élément sur la fenêtre).
    pub(crate) fn gesture_released(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        let ends = match self.gesture {
            Some(Gesture::Selecting) => event.button == MouseButton::Left,
            Some(Gesture::Reporting(kind)) => button_kind(event.button) == Some(kind),
            None => false,
        };
        if ends {
            self.end_gesture(event.position, &event.modifiers, cx);
        }
    }

    /// Clôt le geste : une sélection vide (un clic sans glisser) disparaît, le programme apprend le relâché.
    fn end_gesture(
        &mut self,
        position: Point<Pixels>,
        modifiers: &Modifiers,
        cx: &mut Context<Self>,
    ) {
        let Some(gesture) = self.gesture.take() else {
            return;
        };
        self.last_cell = None;
        match gesture {
            Gesture::Selecting => {
                if !self.backend.emulator().has_selection() {
                    self.backend.clear_selection();
                }
            }
            Gesture::Reporting(kind) => {
                let at = self.view_point(position);
                self.report_mouse(MouseAction::Release(kind), at, modifiers);
            }
        }
        cx.notify();
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Les pixels se cumulent en lignes ; la fraction restante attend le prochain cran.
        let delta = event.delta.pixel_delta(self.cell.1);
        let lines = self.wheel.add(delta.y / self.cell.1);
        if lines == 0 {
            return;
        }
        let at = self.view_point(event.position);
        match encode_wheel(
            lines,
            cell_coordinate(at.column),
            cell_coordinate(at.row),
            self.mouse_modes(),
        ) {
            WheelAction::Send(bytes) => self.send_user_input(&bytes),
            WheelAction::ScrollHistory(count) => {
                self.backend.scroll_history(count);
                // Un journal que la molette a quitté ne suit plus la sortie ; ramené au bas, il la suit de nouveau.
                self.note_user_scroll();
                // La vue a bougé sous la souris : le lien trouvé n'est plus à sa place.
                self.clear_hover(cx);
                cx.notify();
            }
        }
        cx.stop_propagation();
    }
}

impl EventEmitter<PaneEvent> for PaneView {}

/// Ce que la saisie, le collage, le texte de l'IME, le rapport de souris et celui du focus ont le droit de faire : atteindre
/// le programme du pane, sauf dans un journal en lecture seule. Toutes les écritures de la vue vers le programme passent par
/// cette porte (`send_user_input`, `report_focus`) ; la touche enfoncée d'un journal n'est même pas encodée (`on_key_down`).
fn input_reaches_program(read_only: bool, link_down: bool) -> bool {
    !read_only && !link_down
}

/// Une commande tapée par IdemA (lancement d'un agent, reprise d'une session ramenée, avec son `^C`) part-elle ? Au prompt
/// seulement, et jamais sur un lien tombé : comme la saisie, elle n'est ni retenue ni rejouée.
fn launch_reaches_program(at_prompt: bool, link_down: bool) -> bool {
    at_prompt && !link_down
}

/// Écrit ce que l'utilisateur a saisi (ou le rapport de focus) vers le programme du pane, si la vue le permet. Faux : rien
/// n'est parti.
fn write_user_input(
    read_only: bool,
    link_down: bool,
    backend: &mut PaneBackend,
    bytes: &[u8],
) -> bool {
    if !input_reaches_program(read_only, link_down) {
        return false;
    }
    backend.send_bytes(bytes);
    true
}

/// Le journal suit-il la sortie après un défilement ? Oui seulement au bas de l'écran : monter (molette, saut de `⌘F` vers une
/// occurrence) le laisse où l'utilisateur l'a mis, et la sortie nouvelle ne le ramène plus.
fn following_after_scroll(display_offset: usize) -> bool {
    display_offset == 0
}

/// De combien de lignes faire défiler la vue d'un journal (positif : vers le haut), ou `None` quand elle est bien posée. Une
/// sortie qui tient à l'écran (l'historique, plus les lignes jusqu'au curseur) se lit depuis son début, comme dans un
/// fichier : tmux repousse dans son historique les premières lignes quand la fenêtre se rétrécit, et la vue ne doit pas
/// commencer au milieu. Une sortie plus longue se lit par le bas, la fin, où arrivent les lignes nouvelles. `offset` est le
/// défilement actuel de la vue (0 : le bas).
fn journal_scroll(history: usize, rows: usize, cursor_row: usize, offset: usize) -> Option<i32> {
    let fits = history + cursor_row < rows;
    let wanted = if fits { history } else { 0 };
    (wanted != offset).then(|| wanted as i32 - offset as i32)
}

/// Remplace `target` par `value` s'ils diffèrent. Vrai si la valeur a changé.
fn replace_if_changed(target: &mut String, value: &str) -> bool {
    if target == value {
        return false;
    }
    value.clone_into(target);
    true
}

impl Focusable for PaneView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PaneView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Le glisser s'est arrêté (Échap, relâché hors de la fenêtre : GPUI repeint) : le pane oublie ses fichiers.
        if !cx.has_active_drag() {
            self.drop.ended();
        }
        let hud = self.hud.visible.then(|| {
            let font = *self
                .hud_font
                .get_or_insert_with(|| hud::hud_font(&window.text_system().all_font_names()));
            hud::hud_layer(self.hud.hud_text(Instant::now()), font)
        });
        let theme = *cx.global::<Theme>();
        let notice = self.notice.as_ref().map(|notice| {
            // Bulle du coin haut droit, sans interaction : ni clic ni survol ne s'y arrêtent.
            div()
                .absolute()
                // Sous la barre de recherche quand elle est ouverte : elle occupe le même coin.
                .top(theme.px(if self.search.is_some() {
                    search_bar::MARGIN + search_bar::HEIGHT + 6.
                } else {
                    8.
                }))
                .right(theme.px(8.))
                .px(theme.px(8.))
                .py(theme.px(4.))
                .rounded(theme.px(6.))
                .bg(theme::color_alpha(theme::POPOVER))
                .text_size(theme.text(11.))
                .text_color(if notice.failed {
                    theme::color(theme::ERROR_TEXT)
                } else {
                    theme::white(theme::TEXT_PRIMARY)
                })
                .child(notice.text.clone())
        });
        let search_bar = self.search.as_ref().map(|search| search.bar.clone());
        // La barre de recherche est la sœur du pane et non son enfant : les touches tapées dans son champ remonteraient
        // sinon jusqu'à `on_key_down` du pane, qui les enverrait au programme.
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .id("pane-view")
                    .key_context(KEY_CONTEXT)
                    .track_focus(&self.focus_handle)
                    .size_full()
                    .bg(rgb(self.background))
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
                    .on_action(cx.listener(Self::paste))
                    .on_action(cx.listener(Self::copy))
                    .on_action(cx.listener(Self::select_all))
                    .on_action(cx.listener(Self::toggle_hud))
                    .on_action(cx.listener(Self::dump_grid))
                    .on_action(cx.listener(Self::open_search))
                    .on_action(cx.listener(Self::search_next))
                    .on_action(cx.listener(Self::search_previous))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !*hovered {
                            this.on_pointer_left(cx);
                        }
                    }))
                    .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                    .child(TerminalElement::new(cx.entity()))
                    .on_drag_move::<ExternalPaths>(cx.listener(Self::on_files_dragged))
                    // Liseré de dépôt : un calque de toute la taille du pane, sans interaction (il n'arrête aucun événement
                    // souris), peint par-dessus le texte, qui ne bouge pas.
                    .when(self.drop.outlined(), |pane| {
                        pane.child(div().absolute().inset_0().border_2().border_color(Rgba {
                            a: theme::DROP_OUTLINE,
                            ..theme::color(theme::FOCUS)
                        }))
                    })
                    .children(hud)
                    .children(notice),
            )
            .children(search_bar)
    }
}

/// UTF-16 : la mesure que l'IME de macOS attend pour les plages de texte.
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

impl EntityInputHandler for PaneView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.marked_text.as_deref().map_or(0, utf16_len);
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_text.as_deref().map(|text| 0..utf16_len(text))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.marked_text.take().is_some() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Texte validé : la composition en cours est remplacée, le terminal reçoit l'UTF-8 tel quel.
        self.marked_text = None;
        if !text.is_empty() {
            self.send_user_input(text.as_bytes());
            self.announce_dropped_input(cx);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = (!new_text.is_empty()).then(|| new_text.to_owned());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // La fenêtre de candidats de l'IME s'ancre sur la cellule du curseur.
        let content = self.content?;
        let renderable = self.backend.emulator().renderable();
        let row = renderable.cursor.point.line.0 + renderable.display_offset as i32;
        let column = renderable.cursor.point.column.0;
        Some(Bounds::new(
            point(
                content.origin.x + self.cell.0 * column as f32,
                content.origin.y + self.cell.1 * row.max(0) as f32,
            ),
            size(self.cell.0, self.cell.1),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRID: GridSize = GridSize {
        columns: 80,
        lines: 24,
    };

    #[test]
    fn links_and_documents_go_to_the_browser_and_the_rest_keeps_its_place() {
        assert_eq!(
            link_destination("https://claude.ai/x"),
            LinkDestination::Browser("https://claude.ai/x".into())
        );
        assert_eq!(
            link_destination("http://localhost:3000"),
            LinkDestination::Browser("http://localhost:3000".into())
        );
        assert_eq!(
            link_destination("mailto:x@example.com"),
            LinkDestination::System("mailto:x@example.com".into())
        );
        assert_eq!(
            link_destination("file:///tmp/r.pdf"),
            LinkDestination::Browser("file:///tmp/r.pdf".into())
        );
        assert_eq!(
            link_destination("file:///tmp/a.zip"),
            LinkDestination::Reveal("/tmp/a.zip".into())
        );
        assert_eq!(link_destination("ftp://x"), LinkDestination::Refused);
    }

    #[test]
    fn a_remote_pane_link_never_reaches_the_finder_or_mail() {
        // Un chemin ou une adresse de la machine distante ne désigne rien sur ce Mac (spec §3.5), comme un chemin tapé.
        for uri in ["file:///tmp/a.zip", "mailto:x@example.com"] {
            assert_eq!(
                routed_destination(true, uri),
                LinkDestination::Refused,
                "{uri}"
            );
        }
        // Le web passe encore (`link_route` le dirige vers le navigateur du système).
        assert_eq!(
            routed_destination(true, "https://claude.ai/x"),
            LinkDestination::Browser("https://claude.ai/x".into())
        );
        // Un pane local garde toutes ses destinations.
        assert_eq!(
            routed_destination(false, "file:///tmp/a.zip"),
            LinkDestination::Reveal("/tmp/a.zip".into())
        );
        assert_eq!(
            routed_destination(false, "mailto:x@example.com"),
            LinkDestination::System("mailto:x@example.com".into())
        );
    }

    #[test]
    fn a_file_link_to_a_document_keeps_its_fragment_and_loses_a_localhost_host() {
        // Le fragment d'un PDF (`#page=2`) n'empêche pas de reconnaître le document, et reste dans l'adresse.
        assert_eq!(
            link_destination("file:///tmp/r.PDF#page=2"),
            LinkDestination::Browser("file:///tmp/r.PDF#page=2".into())
        );
        assert_eq!(
            link_destination("FILE://localhost/tmp/a%20b.html"),
            LinkDestination::Browser("file:///tmp/a%20b.html".into())
        );
        // Un autre hôte, ou un dossier nommé comme un document, ne passe pas pour un document local.
        assert_eq!(
            link_destination("file://server/tmp/r.pdf"),
            LinkDestination::Refused
        );
        assert_eq!(
            link_destination("file:///tmp/notes.txt?x=r.pdf"),
            LinkDestination::Reveal("/tmp/notes.txt?x=r.pdf".into())
        );
    }

    #[test]
    fn a_local_document_path_becomes_a_file_address() {
        assert_eq!(
            document_address(Path::new("/tmp/site/index.html")).as_deref(),
            Some("file:///tmp/site/index.html")
        );
        assert_eq!(
            document_address(Path::new("/tmp/a b/é.pdf")).as_deref(),
            Some("file:///tmp/a%20b/%C3%A9.pdf")
        );
        // Ni un autre fichier, ni un chemin relatif ne sont adressés au navigateur.
        assert_eq!(document_address(Path::new("/tmp/main.rs")), None);
        assert_eq!(document_address(Path::new("docs/r.pdf")), None);
    }

    #[test]
    fn a_press_on_a_pane_without_the_keyboard_only_takes_focus() {
        // Le pane actif de tmux n'a pas le clavier quand le volet Terminal le tient : le premier clic le prend.
        assert!(press_only_takes_focus(true, false));
        assert!(press_only_takes_focus(false, false));
        // Un pane qui n'est pas l'actif ne fait que demander le focus, clavier ou non.
        assert!(press_only_takes_focus(false, true));
        // Seul l'actif qui a déjà le clavier traite l'appui (sélection, rapport souris, lien).
        assert!(!press_only_takes_focus(true, true));
    }

    #[test]
    fn the_dump_notice_names_the_file_written_or_the_failure() {
        let written = PaneNotice::dump(&Ok(PathBuf::from("/tmp/home/debug/grid-12.txt")));
        assert_eq!(written.text, "Grille écrite : grid-12.txt");
        assert!(!written.failed);
        let local = PaneNotice::dump(&Ok(PathBuf::from("/tmp/home/debug/grid-local.txt")));
        assert_eq!(local.text, "Grille écrite : grid-local.txt");
        let failed = PaneNotice::dump(&Err(io::Error::other("disk full")));
        assert_eq!(failed.text, "Vidage impossible");
        assert!(failed.failed);
    }

    fn view_point_of(x: f32, y: f32) -> ViewPoint {
        view_point_at(
            point(px(x), px(y)),
            point(px(100.), px(50.)),
            (px(10.), px(20.)),
            GRID,
        )
    }

    #[test]
    fn the_cell_under_the_pointer_is_counted_from_the_grid_origin() {
        let at = view_point_of(100. + 25., 50. + 45.);
        assert_eq!((at.row, at.column), (2, 2));
    }

    #[test]
    fn the_right_half_of_a_cell_is_told_from_the_left_half() {
        assert!(!view_point_of(100. + 24.9, 50.).right_half);
        assert!(view_point_of(100. + 25., 50.).right_half);
        assert!(view_point_of(100. + 29.9, 50.).right_half);
        assert!(!view_point_of(100. + 30., 50.).right_half);
    }

    #[test]
    fn a_pointer_beyond_the_grid_is_clamped_to_its_edge_cells() {
        // À gauche et au-dessus : la première cellule, que la sélection ne prend pas.
        let before = view_point_of(10., 5.);
        assert_eq!(
            (before.row, before.column, before.right_half),
            (0, 0, false)
        );
        // À droite et en dessous : la dernière cellule, prise en entier.
        let after = view_point_of(5_000., 5_000.);
        assert_eq!((after.row, after.column, after.right_half), (23, 79, true));
    }

    #[test]
    fn a_pointer_in_the_margin_past_the_grid_is_not_over_any_cell() {
        let inside = |x: f32, y: f32| {
            view_point_inside(
                point(px(x), px(y)),
                point(px(100.), px(50.)),
                (px(10.), px(20.)),
                GRID,
            )
        };
        assert!(inside(100., 50.).is_some());
        assert!(inside(100. + 799., 50. + 479.).is_some());
        // Au-delà de la dernière colonne (80 x 10 px) ou rangée (24 x 20 px), ou avant l'origine.
        assert!(inside(100. + 800., 50.).is_none());
        assert!(inside(100., 50. + 480.).is_none());
        assert!(inside(99., 50.).is_none());
        assert!(inside(100., 49.).is_none());
    }

    #[test]
    fn a_command_press_acts_at_once_on_a_pane_without_the_keyboard() {
        let command = Modifiers {
            platform: true,
            ..Modifiers::default()
        };
        let plain = Modifiers::default();
        for (active, keyboard) in [(true, false), (false, false), (false, true)] {
            // Un appui simple garde l'écart E20 : le focus seulement, rien n'est transmis.
            assert!(press_is_focus_only(
                MouseButton::Left,
                &plain,
                active,
                keyboard
            ));
            // Un `⌘`-clic sur un chemin ou un lien agit au premier appui.
            assert!(!press_is_focus_only(
                MouseButton::Left,
                &command,
                active,
                keyboard
            ));
        }
        // Le clic droit, même avec `⌘`, n'est pas un clic de lien : il reste un appui de focus.
        assert!(press_is_focus_only(
            MouseButton::Right,
            &command,
            true,
            false
        ));
        // Le pane actif qui a le clavier traite tout appui.
        assert!(!press_is_focus_only(MouseButton::Left, &plain, true, true));
    }

    #[test]
    fn a_left_press_with_command_is_always_a_link_press() {
        let command = Modifiers {
            platform: true,
            ..Modifiers::default()
        };
        assert!(is_link_press(MouseButton::Left, &command));
        assert!(!is_link_press(MouseButton::Right, &command));
        assert!(!is_link_press(MouseButton::Left, &Modifiers::default()));
        // `⌘` + `⇧` reste un clic de lien : jamais de sélection ni de rapport avec `⌘`.
        let both = Modifiers {
            platform: true,
            shift: true,
            ..Modifiers::default()
        };
        assert!(is_link_press(MouseButton::Left, &both));
    }

    #[test]
    fn only_links_a_command_click_would_follow_are_hovered() {
        assert!(is_followable("https://example.com/a"));
        assert!(is_followable("mailto:a@b.c"));
        assert!(is_followable("file:///tmp/a%20b.txt"));
        assert!(!is_followable("x-man-page://ls"));
        assert!(!is_followable("file://otherhost/etc/passwd"));
    }

    #[test]
    fn the_click_count_picks_what_a_press_selects() {
        assert_eq!(selection_kind(1), SelectionKind::Simple);
        assert_eq!(selection_kind(2), SelectionKind::Word);
        assert_eq!(selection_kind(3), SelectionKind::Line);
        assert_eq!(selection_kind(7), SelectionKind::Line);
    }

    #[test]
    fn the_wheel_keeps_the_fraction_of_a_line_until_it_adds_up() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(wheel.add(0.4), 0);
        assert_eq!(wheel.add(0.4), 0);
        assert_eq!(wheel.add(0.4), 1);
        // Le reste (0,2) attend le prochain cran.
        assert_eq!(wheel.add(0.9), 1);
    }

    #[test]
    fn the_wheel_forgets_its_remainder_when_the_direction_changes() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(wheel.add(0.6), 0);
        // Sans l'oubli, 0,6 - 1,2 donnerait -0,6 : aucune ligne. Un cran complet vers le bas défile d'une ligne.
        assert_eq!(wheel.add(-1.2), -1);
        let mut wheel = WheelAccumulator::default();
        assert_eq!(wheel.add(-0.6), 0);
        assert_eq!(wheel.add(1.2), 1);
    }

    #[test]
    fn only_the_buttons_the_protocol_knows_are_reported() {
        assert_eq!(button_kind(MouseButton::Left), Some(MouseButtonKind::Left));
        assert_eq!(
            button_kind(MouseButton::Right),
            Some(MouseButtonKind::Right)
        );
        assert_eq!(
            button_kind(MouseButton::Navigate(gpui::NavigationDirection::Back)),
            None
        );
    }

    #[test]
    fn the_shell_name_is_the_basename_of_the_shell_variable() {
        assert_eq!(shell_basename(Some("/opt/homebrew/bin/fish")), "fish");
        assert_eq!(shell_basename(Some("/bin/zsh")), "zsh");
    }

    #[test]
    fn a_remote_pane_takes_the_shell_of_its_machine() {
        // Le shell de la machine prime ; sans lui, la règle locale (la variable `SHELL` du Mac).
        assert_eq!(
            pane_shell_name(Some("/usr/bin/bash"), Some("/bin/zsh")),
            "bash"
        );
        assert_eq!(
            pane_shell_name(None, Some("/opt/homebrew/bin/fish")),
            "fish"
        );
        assert_eq!(pane_shell_name(None, None), "zsh");
    }

    #[test]
    fn a_missing_or_empty_shell_variable_falls_back_to_zsh() {
        assert_eq!(shell_basename(None), "zsh");
        assert_eq!(shell_basename(Some("")), "zsh");
        assert_eq!(shell_basename(Some("/usr/bin/")), "zsh");
    }

    #[test]
    fn a_pane_at_its_shell_runs_nothing() {
        assert!(running_programs_of(false, Some("zsh"), "zsh").is_empty());
        assert!(running_programs_of(false, None, "zsh").is_empty());
    }

    #[test]
    fn a_foreground_program_is_named() {
        assert_eq!(
            running_programs_of(false, Some("claude"), "zsh"),
            ["claude"]
        );
        assert_eq!(running_programs_of(true, Some("sleep"), "zsh"), ["sleep"]);
    }

    #[test]
    fn a_command_marked_running_under_the_shell_is_a_generic_program() {
        assert_eq!(
            running_programs_of(true, Some("zsh"), "zsh"),
            [UNKNOWN_PROGRAM]
        );
        assert_eq!(running_programs_of(true, None, "zsh"), [UNKNOWN_PROGRAM]);
    }

    #[test]
    fn dropped_files_become_one_line_of_escaped_paths() {
        let paths = [
            PathBuf::from("/private/tmp/idema test.txt"),
            PathBuf::from("/tmp/plain.md"),
        ];
        assert_eq!(
            dropped_text(&paths).as_deref(),
            Ok("/private/tmp/idema\\ test.txt /tmp/plain.md")
        );
    }

    #[test]
    fn a_dropped_name_with_a_control_character_refuses_the_whole_drop() {
        let paths = [
            PathBuf::from("/tmp/ok"),
            PathBuf::from("/tmp/idema\nbad.txt"),
        ];
        assert_eq!(dropped_text(&paths), Err("/tmp/idema\nbad.txt".to_string()));
    }

    #[test]
    fn a_dropped_path_that_is_not_utf8_is_refused() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let paths = [
            PathBuf::from("/tmp/ok"),
            PathBuf::from(OsStr::from_bytes(b"/tmp/caf\xe9.txt")),
        ];
        let refused = dropped_text(&paths).unwrap_err();
        assert!(refused.starts_with("/tmp/caf"), "{refused}");
    }

    fn files(name: &str) -> ExternalPaths {
        ExternalPaths(std::iter::once(PathBuf::from(name)).collect())
    }

    #[test]
    fn files_dragged_over_the_pane_are_outlined_and_dropped_on_release() {
        let mut state = DropState::default();
        assert!(state.dragged(&files("/tmp/a")));
        assert!(!state.outlined(), "no mouse over the pane yet");
        assert!(state.hit_test(true));
        assert!(state.outlined());
        assert_eq!(state.release(true, true), Some(files("/tmp/a")));
        assert!(!state.outlined());
    }

    #[test]
    fn a_new_drag_replaces_the_files_of_a_drag_that_was_cancelled_over_the_pane() {
        let mut state = DropState::default();
        state.dragged(&files("/tmp/x"));
        state.hit_test(true);
        // Échap : le glisser s'arrête, le pane repeint et oublie.
        state.ended();
        // Le glisser suivant arrive avec la souris déjà sur le pane.
        state.hit_test(true);
        assert!(state.dragged(&files("/tmp/y")));
        assert_eq!(state.release(true, true), Some(files("/tmp/y")));
    }

    #[test]
    fn the_files_follow_the_drag_even_if_the_pane_was_not_repainted_in_between() {
        // Le glisser d'avant n'a pas été oublié (pas de repeinture) : le premier mouvement du suivant corrige.
        let mut state = DropState::default();
        state.dragged(&files("/tmp/x"));
        state.hit_test(true);
        assert!(state.dragged(&files("/tmp/y")));
        assert_eq!(state.release(true, true), Some(files("/tmp/y")));
    }

    #[test]
    fn nothing_is_dropped_when_the_pointer_left_or_a_menu_covers_the_pane() {
        let mut state = DropState::default();
        state.dragged(&files("/tmp/a"));
        state.hit_test(true);
        // La souris sort, ou un menu la recouvre : même mesure, `hit` faux.
        assert!(state.hit_test(false));
        assert!(!state.outlined());
        assert_eq!(state.release(false, true), None);
        // Le relâché hors du pane ne rend pas les fichiers : le glisser est fini.
        state.dragged(&files("/tmp/b"));
        state.hit_test(true);
        assert_eq!(state.release(false, true), None);
        assert_eq!(state.release(true, true), None);
    }

    #[test]
    fn nothing_is_dropped_when_no_file_drag_is_active() {
        let mut state = DropState::default();
        state.dragged(&files("/tmp/a"));
        state.hit_test(true);
        assert_eq!(state.release(true, false), None);
        // Un simple clic sans glisser ne dépose rien non plus.
        assert_eq!(DropState::default().release(true, false), None);
        assert_eq!(DropState::default().release(true, true), None);
    }

    #[test]
    fn an_ended_drag_leaves_no_outline_and_no_files() {
        let mut state = DropState::default();
        state.dragged(&files("/tmp/a"));
        state.hit_test(true);
        state.ended();
        assert!(!state.outlined());
        assert_eq!(state.release(true, true), None);
    }

    fn env_of(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn remote_shell_script_quotes_the_folder() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        assert_eq!(
            remote_shell_script("/home/claude/mon projet"),
            "cd '/home/claude/mon projet' 2>/dev/null; exec \"$SHELL\" -l"
        );
        // Un vrai tour : le dossier a une apostrophe, un `$`, un espace et un `;`, et `SHELL` est un petit script qui
        // imprime son dossier courant. Le shell de login part dans ce dossier, sans rien exécuter d'autre.
        let root = std::env::temp_dir().join(format!("idema-t8-{}", std::process::id()));
        let folder = root.join("o'neil $HOME; touch pwned");
        std::fs::create_dir_all(&folder).unwrap();
        let fake_shell = root.join("fake-shell");
        std::fs::write(&fake_shell, "#!/bin/sh\npwd -P\n").unwrap();
        std::fs::set_permissions(&fake_shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = remote_shell_script(folder.to_str().unwrap());
        let output = Command::new("/bin/sh")
            .args(["-c", &script])
            .env("SHELL", &fake_shell)
            .output()
            .expect("sh starts");
        let printed = String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string();
        let expected = folder.canonicalize().unwrap();
        let pwned = folder.join("pwned").exists() || root.join("pwned").exists();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(printed, expected.to_str().unwrap());
        assert!(!pwned, "the folder name was read as a command");
        // Un dossier disparu ne gêne pas : le shell part là où ssh arrive, sans message.
        let missing = Command::new("/bin/sh")
            .args(["-c", &remote_shell_script("/nonexistent/o'neil")])
            .env("SHELL", "/bin/echo")
            .output()
            .expect("sh starts");
        assert!(missing.status.success());
        assert!(missing.stderr.is_empty());
    }

    #[test]
    fn the_panel_shell_is_the_one_of_the_pane_environment_else_zsh() {
        assert_eq!(
            shell_program(&env_of(&[
                ("PATH", "/bin"),
                ("SHELL", "/opt/homebrew/bin/fish")
            ])),
            PathBuf::from("/opt/homebrew/bin/fish")
        );
        assert_eq!(
            shell_program(&env_of(&[("SHELL", "")])),
            PathBuf::from("/bin/zsh")
        );
        assert_eq!(shell_program(&[]), PathBuf::from("/bin/zsh"));
    }

    /// Un pane local qui change de taille la pose dans son émulateur et dans le noyau (`TIOCSWINSZ`) : le programme
    /// la lit. Le shell attend un instant avant `stty size`, le temps du redimensionnement.
    #[test]
    fn a_local_backend_resizes_its_emulator_and_its_terminal_once() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let size = GridSize {
            columns: 80,
            lines: 24,
        };
        let pty = LocalPty::spawn(
            &PtyCommand {
                program: Path::new("/bin/sh"),
                args: &["-c", "sleep 0.5; stty size; exec sleep 30"],
                directory: Path::new("/tmp"),
                env: &env_of(&[("PATH", "/bin:/usr/bin")]),
                size,
            },
            move |event| {
                let _ = sender.send(event);
            },
        )
        .expect("pty spawned");
        let mut backend = PaneBackend::Local {
            pty,
            emulator: PaneEmulator::new(size, 100, Source::LocalPty),
        };
        let resized = GridSize {
            columns: 100,
            lines: 40,
        };
        assert!(backend.fit(resized));
        assert!(!backend.fit(resized), "the same size changes nothing");
        assert_eq!(backend.emulator().size(), resized);

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = String::new();
        while !seen.contains("40 100") {
            let left = deadline
                .checked_duration_since(Instant::now())
                .expect("the shell never printed its size");
            if let Ok(PtyEvent::Output(bytes)) = receiver.recv_timeout(left) {
                seen.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        // Le PTY lâché raccroche le shell et sa commande.
        drop(backend);
    }

    /// Une commande d'IdemA (agent, reprise) suit la même règle que la saisie.
    #[test]
    fn a_command_typed_by_idema_needs_a_prompt_and_a_live_link() {
        assert!(launch_reaches_program(true, false));
        assert!(!launch_reaches_program(false, false));
        assert!(!launch_reaches_program(true, true));
        assert!(!launch_reaches_program(false, true));
    }

    /// La porte de la lecture seule et celle du lien tombé, sur un vrai PTY (`cat`, dont le terminal répète ce qu'on lui
    /// écrit) : ce qu'un journal « tape », ou ce qu'on tape quand le lien vers la machine est tombé, ne part pas ; la même
    /// écriture sur une vue ordinaire part et revient en écho. Le test échouerait si la porte laissait passer un octet.
    #[test]
    fn a_closed_view_writes_nothing_to_its_program_and_an_ordinary_one_does() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let size = GridSize {
            columns: 80,
            lines: 24,
        };
        let pty = LocalPty::spawn(
            &PtyCommand {
                program: Path::new("/bin/cat"),
                args: &[],
                directory: Path::new("/tmp"),
                env: &env_of(&[("PATH", "/bin:/usr/bin")]),
                size,
            },
            move |event| {
                let _ = sender.send(event);
            },
        )
        .expect("pty spawned");
        let mut backend = PaneBackend::Local {
            pty,
            emulator: PaneEmulator::new(size, 100, Source::LocalPty),
        };
        let echoed = |wanted: &str, within: Duration| {
            let deadline = Instant::now() + within;
            let mut seen = String::new();
            while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                if let Ok(PtyEvent::Output(bytes)) = receiver.recv_timeout(left) {
                    seen.push_str(&String::from_utf8_lossy(&bytes));
                    if seen.contains(wanted) {
                        return true;
                    }
                }
            }
            false
        };
        assert!(!write_user_input(true, false, &mut backend, b"tapeici\n"));
        assert!(
            !echoed("tapeici", Duration::from_millis(800)),
            "a read-only view must not reach the program"
        );
        assert!(!write_user_input(false, true, &mut backend, b"coupeici\n"));
        assert!(
            !echoed("coupeici", Duration::from_millis(800)),
            "a view whose link is down must not reach the program"
        );
        assert!(write_user_input(false, false, &mut backend, b"tapeici\n"));
        assert!(
            echoed("tapeici", Duration::from_secs(10)),
            "an ordinary view reaches it (the control)"
        );
        drop(backend);
    }

    /// Une fois monté (molette, saut de `⌘F`), un journal ne suit plus la sortie ; redescendu au bas, il la suit de nouveau.
    #[test]
    fn a_journal_follows_the_output_only_at_the_bottom() {
        assert!(following_after_scroll(0));
        assert!(!following_after_scroll(1));
        assert!(!following_after_scroll(40));
    }

    /// Un journal dont l'historique est trop court pour remplir l'écran se lit depuis son début ; un plus long, par le bas ;
    /// la vue ne change que quand elle n'est pas où elle doit être.
    #[test]
    fn a_journal_that_fits_is_read_from_its_start_and_a_longer_one_from_its_end() {
        // Deux lignes d'historique, trois lignes à l'écran sur trente-six : tout tient, la vue monte de deux lignes.
        assert_eq!(journal_scroll(2, 36, 3, 0), Some(2));
        // Déjà au début : rien à faire.
        assert_eq!(journal_scroll(2, 36, 3, 2), None);
        // Pas d'historique : l'écran est déjà lu depuis son haut.
        assert_eq!(journal_scroll(0, 36, 3, 0), None);
        // Ça ne tient plus (l'historique et le curseur dépassent l'écran) : la vue redescend au bas pour suivre la sortie.
        assert_eq!(journal_scroll(40, 36, 20, 40), Some(-40));
        assert_eq!(journal_scroll(40, 36, 20, 0), None);
        // Le curseur sur la dernière ligne d'un écran plein : ça ne tient pas, on reste au bas.
        assert_eq!(journal_scroll(1, 36, 35, 0), None);
    }

    /// Le journal d'un fichier : l'émulateur prend la taille de la zone et les octets qu'on lui donne, et rien de ce que
    /// l'utilisateur tape ne va nulle part.
    #[test]
    fn a_tail_backend_takes_the_size_and_the_bytes_it_is_given_and_writes_nowhere() {
        let size = GridSize {
            columns: 80,
            lines: 24,
        };
        let mut backend = PaneBackend::Tail {
            emulator: PaneEmulator::new(size, 100, Source::LocalPty),
        };
        let resized = GridSize {
            columns: 100,
            lines: 40,
        };
        assert!(backend.fit(resized));
        assert!(!backend.fit(resized), "the same size changes nothing");
        assert_eq!(backend.emulator().size(), resized);
        assert!(backend.tmux().is_none() && backend.tmux_mut().is_none());
        let PaneBackend::Tail { emulator } = &mut backend else {
            unreachable!("a tail backend");
        };
        emulator.feed(b"premiere ligne\r\nseconde\r\n");
        // Taper dans un journal ne fait rien (et ne panique pas).
        backend.send_bytes(b"echo perdu\r");
        let text = backend.emulator().dump_text();
        assert!(text.contains("premiere ligne") && text.contains("seconde"));
        assert!(!text.contains("perdu"));
    }
}
