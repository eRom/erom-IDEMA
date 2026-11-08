//! La vue racine : bande de titre, barre latérale, colonne principale et écran vide (spec §7.1). tmux s'amorce
//! hors du fil d'interface : la fenêtre se peint avant qu'il ait répondu (AC-9).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use futures::StreamExt;
use gpui::{
    AnyElement, App, AsyncApp, BoxShadow, ClipboardItem, Context, CursorStyle, DispatchPhase, Div,
    Entity, FocusHandle, Focusable, FontWeight, HitboxBehavior, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PathPromptOptions, Pixels, Point, PromptButton, PromptLevel,
    Stateful, Subscription, Task, WeakEntity, Window, actions, canvas, div, point, prelude::*, px,
    svg,
};
use idema_term::activity::LaunchPacer;
use idema_term::activity::{ActivityState, should_chime};
use idema_term::emulator::GridSize;
use idema_tmux::layout::Preset;
use idema_tmux::protocol::PaneId;
use idema_tmux::remote::RemoteSpec;
use idema_tmux::server::{
    CreatedSession, ListedPane, ListedSession, NewSession as NewTmuxSession, RestorePane,
    RestoreSession, ServerError, TmuxServer,
};

use crate::artifacts::source::ArtifactsSource;
use crate::artifacts::view::{ArtifactsEvent, ArtifactsView};
use crate::assets::{Icon, logo_path};
use crate::bootstrap::{self, Bootstrap};
use crate::browser::BrowserSettings;
use crate::browser::address::log_form;
use crate::browser::panel::{
    BROWSER_PANEL_WIDTH, BrowserEvent, BrowserPanel, render_panel as render_browser_panel,
    servers_badge_count,
};
use crate::browser::tabs::OpenFrom;
use crate::changes::panel::{
    ChangeTarget, ChangesEvent, ChangesPanel, TargetItem, changes_menu, diff_menu,
    render_panel as render_changes_panel, target_menu,
};
use crate::commit_message::CommitMessageSettings;
use crate::companion::episodes::{
    EpisodeChange, EpisodeKind, episode_changes, no_pane_waiting, urgent_pane,
};
use crate::companion::fleet::{FleetPane, SessionInput, compose_fleet, mac_name, state_label};
use crate::companion::input::{
    ENTER_DELAY, Gate, RemoteInput, enter_still_allowed, gate, is_expired,
};
use crate::companion::presence;
use crate::companion::{
    AckOrigin, CompanionController, InputOutcome, PaneCommand, PaneProbe, ServiceEvent,
    ServiceStatus, key_label, log_safe,
};
use crate::config::{
    AgentEntry, Config, ConfigError, FileWatch, IdeationConfig, ReloadEffects, SHELL_ENTRY,
    SHELL_SESSION_NAME, ensure_config_file, harness_groups, load_config,
};
use crate::dock;
use crate::errors::{Explained, UNEXPECTED, alert, explain_server, tmux_text};
use crate::file_open::{FileTarget, PathTarget};
use crate::files::editor_theme::apply_editor_theme;
use crate::files::gestures::{EntryGit, MenuLine, TreeTarget, editor_menu, tree_menu};
use crate::files::panel::{
    FilesEvent, FilesPanel, ProjectSearch, ToggleTree, render_panel as render_files_panel,
};
use crate::gutter::session_columns;
use crate::help_bubble::HelpBubble;
use crate::hud;
use crate::ideation::{
    ToolbarItem, ideation_command, next_remote_name, remote_prefix, toolbar_items,
};
use crate::integrations;
use crate::lifecycle::{AfterClose, REATTACH_ATTEMPTS, machine_sync};
use crate::machines::idnma::{IdnmaAgentInfo, read_agents};
use crate::machines::key::{
    LOCAL_MACHINE, SessionTarget, machine_of, remote_session_key, split_session_key,
};
use crate::machines::lifecycle::{
    RemoteAfterClose, RemoteMenuLine, after_remote_close, close_master, default_remote_agent,
    idnma_run_command, is_wake_jump, link_stalled, list_pinned_windows, master_pid,
    release_windows, remote_menu_lines, set_extended_keys, windows_to_unpin,
};
use crate::machines::network::NetworkMonitor;
use crate::machines::panels::{
    LinkRoute, MachineTerminalPin, OPENING_TERMINAL, UNAVAILABLE_ON_REMOTE, link_route,
    offers_local_tools, remote_start_folder, terminal_target, unavailable_open_panel,
};
use crate::machines::probe::{
    MachineFacts, RemoteFailure, ensure_control_dir, probe, probe_spec, retry_delay,
};
use crate::machines::registry::{Lease, MachineStatus, RemoteMachine, SlotPool};
use crate::machines::sessions::{
    Route, SessionServers, machine_keys, no_servers, pane_agent_names, qualify_listing, route,
    should_reselect, wanted_servers,
};
use crate::pane_view::{PaneEvent, PaneView};
use crate::popover::{
    MenuEntry, MenuItemSpec, MenuSpec, OpenMenu, Placement, SUBMENU_DELAY, hidden_panes_dot,
    render_menu,
};
use crate::project::{self, FolderKind};
use crate::project_panel::panel::{
    ProjectEvent, ProjectPanel, render_panel as render_project_panel,
};
use crate::reclaim::discover::OutsideSession;
use crate::reclaim::disk::{
    ReclaimCache, ReclaimPaths, ReclaimScan, paths as reclaim_paths, scan as scan_outside,
    scan_is_stuck,
};
use crate::reclaim::menu::{agent_entry, reclaim_groups};
use crate::reclaim::stop::{
    MESSAGE_COPIED, NextStep, STOP_TIMINGS, StopOutcome, SystemControl, after_stop, may_start,
    stop_session,
};
use crate::rename_field::{self, RenameEvent, RenameField};
use crate::restore::capture_now;
use crate::restore::claude::sessions_dir;
use crate::restore::plan::{
    SessionPlan, SkipReason, ideation_restorable, launch_delay, notice_lines, plan_session,
    resume_delay, should_restore,
};
use crate::restore::snapshot::{
    SavedSession, Snapshot, SnapshotKeeper, SnapshotLoad, SnapshotWriter, load_snapshot,
};
use crate::scratchpad::panel::{
    ScratchpadEvent, ScratchpadPanel, render_panel as render_scratchpad_panel,
};
use crate::scratchpad::rules::{scratchpad_menu, scratchpad_path};
use crate::servers::model::{PaneInputs, Servers, ServersEvent, SessionInfo, observe_panes};
use crate::servers::scan::{Process, SCAN_PERIOD};
use crate::session_view::{PaneLaunch, SessionEntryInfo, SessionEvent, SessionView};
use crate::settings::nav::Section;
use crate::sidebar::{
    AgentMark, SIDEBAR_WIDTH, TITLE_BAND, agent_marks, remote_agent_logo, remote_agent_marks,
    render_sidebar, state_dot,
};
use crate::sidebar_model::{
    SessionSummary, cycle, default_session_name, dock_badge_count, ideation_shown,
    is_alerting_session, machine_navigation_order, navigation_order, neighbor_after_close,
    nth_session, session_order, sort_sessions,
};
use crate::state::{State, StateNotice, StateStore, UiState, rfc3339};
use crate::terminal_element::{MARGIN_SIDES, TerminalFont, cell_metrics, content_bounds};
use crate::terminal_panel::{
    TerminalKey, TerminalPanel, initial_grid, panel_header, panel_shell, render_panel,
};
use crate::theme::{
    BUTTON_HOVER, CONTROL, HOVER, HOVER_STRONG, RUNNING, TEXT_DISABLED, TEXT_FULL, TEXT_HOVER,
    TEXT_MIN, TEXT_PRIMARY, TEXT_PROJECT, Theme, UI_FONT, WINDOW, black, color, white,
};

// Chaque action est câblée par la tâche qui la nomme ; `ToggleSidebar` l'est ici.
actions!(
    workspace,
    [
        NewProject,
        NewSession,
        CloseSession,
        ToggleSidebar,
        OpenConfig,
        InstallIntegrations,
        OpenFile,
        NextSession,
        PreviousSession,
        SelectSession1,
        SelectSession2,
        SelectSession3,
        SelectSession4,
        SelectSession5,
        SelectSession6,
        SelectSession7,
        SelectSession8,
        SelectSession9,
        CloseMenu,
        ToggleTerminal,
        ToggleFiles,
        ToggleChanges,
        ToggleBrowser,
        ToggleScratchpad,
        ToggleProject,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize
    ]
);

/// La bulle d'aide du bouton Projet de la barre d'outils.
const PROJECT_HELP: &str = "Projet";

/// Âge maximal du relevé des serveurs dont la capture de l'instantané veut bien lire la table de processus.
const SNAPSHOT_MAX_SCAN_AGE: Duration = Duration::from_secs(10);

/// Contexte de touches de la fenêtre : `⌘B` et les raccourcis de la barre s'y accrochent.
pub const KEY_CONTEXT: &str = "Workspace";

/// Pas de la surveillance de `config.toml` : une relecture n'a lieu que si la date ou la taille du fichier a bougé.
const CONFIG_POLL: Duration = Duration::from_secs(1);

/// Pas de l'évaluation de la présence de Romain, compagnon allumé seulement (spec §3.2).
const PRESENCE_POLL: Duration = Duration::from_secs(5);

/// Retrait gauche de l'en-tête quand la barre est repliée : il laisse la place aux feux et au bouton de la barre.
const COLLAPSED_HEADER_INSET: f32 = 132.;

/// Délai entre la liste des sessions et leur attache : laisse partir l'image de la barre d'abord (P-1).
const ATTACH_AFTER_PAINT: Duration = Duration::from_millis(50);

/// Où vivent le dossier d'Idema et le serveur tmux.
pub struct Paths {
    pub dir: PathBuf,
    /// L'amorçage de tmux, déjà lancé par `main` : sa réponse arrive quand elle arrive.
    pub bootstrap: futures::channel::oneshot::Receiver<bootstrap::Outcome>,
}

/// Un échec de démarrage, avec la commande qui le règle quand elle existe.
struct StartupError {
    message: String,
    /// Le message brut de l'erreur, sous le texte, quand ce n'est pas IdemA qui l'a écrit.
    detail: Option<String>,
    hint: Option<&'static str>,
}

impl From<ServerError> for StartupError {
    fn from(error: ServerError) -> Self {
        let hint = match error {
            ServerError::NotFound => Some("brew install tmux"),
            ServerError::Unsupported(_) => Some("brew upgrade tmux"),
            _ => None,
        };
        let Explained { text, detail } = explain_server(&error);
        Self {
            message: text,
            detail,
            hint,
        }
    }
}

impl From<std::io::Error> for StartupError {
    fn from(error: std::io::Error) -> Self {
        Self {
            message: UNEXPECTED.to_string(),
            detail: Some(error.to_string()),
            hint: None,
        }
    }
}

/// Le volet droit ouvert : un seul à la fois, Terminal, Fichiers, Changes, Browser, Scratchpad ou Projet (spec P2, §4.1 ;
/// spec P3, §4.1 ; spec P4, §4.1 ; spec Scratchpad, §3.1 ; spec volet Projet, §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightPanel {
    Terminal,
    Files,
    Changes,
    Browser,
    Scratchpad,
    Project,
}

/// Le volet droit après un appel à `toggled` (bouton ou raccourci) : le rouvrir le ferme, ouvrir l'autre prend sa
/// place. Le shell du volet Terminal survit à ce remplacement (il est gardé à part, par projet).
fn next_right_panel(current: Option<RightPanel>, toggled: RightPanel) -> Option<RightPanel> {
    if current == Some(toggled) {
        None
    } else {
        Some(toggled)
    }
}

/// Où en est le shell que le volet Terminal doit montrer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalReady {
    /// Le shell existe (ou vient d'être créé) : le volet le montre.
    Ready,
    /// Le shell d'une machine s'ouvre : le volet reste ouvert sur son texte d'attente.
    Opening,
    /// Le shell ne peut pas démarrer : le volet ne s'ouvre pas (ou se ferme).
    Failed,
}

/// Ce que le journal dit d'un shell du volet : son projet ou sa machine.
fn terminal_label(key: &TerminalKey) -> String {
    match key {
        TerminalKey::Project(project) => format!("project={}", project.display()),
        TerminalKey::Machine(connection) => format!("machine={connection}"),
    }
}

/// Comment la voisine reprend la sélection quand la session sélectionnée disparaît.
#[derive(Debug, PartialEq, Eq)]
enum NeighborHandoff {
    /// `select` : la voisine est affichée, vue, et reçoit le focus.
    Select,
    /// `select_behind_gallery` : la galerie reste au centre, la voisine n'est ni affichée ni vue.
    BehindGallery,
}

/// La galerie couvre le centre : la voisine ne doit être ni affichée ni vue (son badge d'activité resterait
/// intact, son programme ne recevrait aucun focus). Sinon, le chemin d'origine.
fn neighbor_handoff(gallery_shown: bool) -> NeighborHandoff {
    if gallery_shown {
        NeighborHandoff::BehindGallery
    } else {
        NeighborHandoff::Select
    }
}

/// La page native est-elle recouverte par un menu d'IdemA ? Oui dès qu'un menu est ouvert et que le volet Browser est
/// affiché, où que le menu se pose : aucun calcul de recouvrement (spec P4-4).
fn page_covered(menu_open: bool, browser_shown: bool) -> bool {
    menu_open && browser_shown
}

/// Les projets, hors `current`, dont le volet Fichiers a déjà un onglet du fichier qui s'ouvre dans `current` : leur
/// onglet doit se fermer, un fichier n'ayant qu'un onglet tous volets confondus. `panels` dit pour chaque volet s'il
/// tient le fichier. Triés, pour que le journal et l'ordre des fermetures ne dépendent pas de la table.
fn other_holders<'a>(
    current: &Path,
    panels: impl IntoIterator<Item = (&'a Path, bool)>,
) -> Vec<PathBuf> {
    let mut holders: Vec<PathBuf> = panels
        .into_iter()
        .filter(|(project, holds)| *holds && *project != current)
        .map(|(project, _)| project.to_path_buf())
        .collect();
    holders.sort();
    holders
}

/// Le clavier est-il dans le volet droit affiché (Terminal ou Fichiers) ? Il y est quand le volet est montré et tient
/// le focus ; il en part quand la session prend le focus ; passé par un menu, un champ ou la racine (focus ni de l'un
/// ni de l'autre), il reste où il était : fermer le menu ou valider le renommage le lui rend.
fn panel_keeps_keyboard(
    panel_shown: bool,
    panel_focused: bool,
    session_focused: bool,
    before: bool,
) -> bool {
    panel_shown && (panel_focused || (before && !session_focused))
}

/// Le preset d'une session d'après `@idema_layout` ; `Single` quand l'option manque ou n'est pas connue.
fn preset_of(layout: &str) -> Preset {
    Preset::from_id(layout).unwrap_or(Preset::Single)
}

/// La grille à laquelle une session s'attache : la zone utile de `content_width` × `content_height` pixels, les
/// colonnes laissant la place des `gaps` gouttières du preset mémorisé de la session (`gutter.rs`).
fn attach_grid(content_width: f32, content_height: f32, cell: (f32, f32), gaps: u16) -> GridSize {
    GridSize {
        columns: session_columns(content_width, gaps, cell),
        lines: ((content_height / cell.1).floor() as usize).max(1),
    }
}

/// La largeur gardée d'un bord, en unités de la maquette, ramenée dans `[0,70 × mockup, min(1,50 × mockup,
/// max_for_center)]`. Les 40 colonnes du centre limitent l'élargissement, jamais le rendu par défaut : la borne
/// haute ne descend pas sous la largeur de la maquette, même quand la fenêtre est trop étroite (le centre peut alors
/// tomber sous 40 colonnes, comme avant le jalon).
pub(crate) fn clamp_width(mockup: f32, wanted: f32, max_for_center: f32) -> f32 {
    let lowest = 0.7 * mockup;
    let highest = (1.5 * mockup).min(max_for_center).max(mockup);
    wanted.max(lowest).min(highest)
}

/// La largeur par défaut du volet droit, la même pour tous : celle de la maquette du Terminal.
const RIGHT_PANEL_DEFAULT_WIDTH: f32 = 420.;
/// Le plancher du volet droit, le même pour tous : celui du Browser (70 % de sa largeur).
const RIGHT_PANEL_MIN_WIDTH: f32 = 0.7 * BROWSER_PANEL_WIDTH;
/// Le plafond du volet droit, en part de la largeur de la fenêtre.
const RIGHT_PANEL_MAX_WINDOW_SHARE: f32 = 0.5;

/// La largeur affichée du volet droit : la gardée (ou celle par défaut), sous le plafond de la fenêtre et sous ce que le
/// centre laisse, jamais sous le plancher. Les 40 colonnes du centre limitent l'élargissement, jamais la largeur par
/// défaut (le centre peut alors tomber sous 40 colonnes, comme pour la barre). Une fenêtre trop étroite fait céder le
/// plafond, pas le plancher.
fn clamp_panel_width(wanted: Option<f32>, window_width: f32, max_for_center: f32) -> f32 {
    let highest = (RIGHT_PANEL_MAX_WINDOW_SHARE * window_width)
        .min(max_for_center.max(RIGHT_PANEL_DEFAULT_WIDTH))
        .max(RIGHT_PANEL_MIN_WIDTH);
    wanted
        .unwrap_or(RIGHT_PANEL_DEFAULT_WIDTH)
        .clamp(RIGHT_PANEL_MIN_WIDTH, highest)
}

/// Garde la largeur d'un bord : celle de la barre, ou celle du volet droit, commune à tous les volets. Un bord de volet
/// sans volet n'existe pas : rien n'est écrit.
fn set_edge_width(ui: &mut UiState, edge: Edge, panel: Option<RightPanel>, width: Option<f32>) {
    match (edge, panel) {
        (Edge::Sidebar, _) => ui.sidebar_width = width,
        (Edge::Panel, Some(_)) => ui.panel_width = width,
        (Edge::Panel, None) => {}
    }
}

/// Les largeurs affichées de la barre et du volet, en unités de la maquette (0 pour la barre masquée), pour une
/// fenêtre de `window_width` et un centre qui exige `center_minimum` (40 colonnes et marges). Chaque largeur part de
/// la gardée (ou de celle par défaut), puis `clamp_width` ou `clamp_panel_width` la ramène à ses bornes et à ce que la
/// fenêtre laisse. La barre passe la première, face à la largeur du volet bornée par la fenêtre seulement ; le volet
/// vient ensuite, face à la largeur réelle de la barre. `panel_beside` : le volet droit est ouvert à côté de la colonne
/// principale, faux quand il est fermé ou agrandi (sa largeur rendue est alors 0 et il ne limite pas la barre).
/// Fonction pure : rien n'est écrit, une fenêtre rétrécie puis agrandie rend la largeur gardée.
fn edge_widths_for(
    window_width: f32,
    center_minimum: f32,
    ui: &UiState,
    panel_beside: bool,
) -> (f32, f32) {
    let panel_loose = if panel_beside {
        clamp_panel_width(ui.panel_width, window_width, f32::MAX)
    } else {
        0.
    };
    let sidebar = if ui.sidebar_visible {
        clamp_width(
            SIDEBAR_WIDTH,
            ui.sidebar_width.unwrap_or(SIDEBAR_WIDTH),
            window_width - panel_loose - center_minimum,
        )
    } else {
        0.
    };
    let panel = if panel_beside {
        clamp_panel_width(
            ui.panel_width,
            window_width,
            window_width - sidebar - center_minimum,
        )
    } else {
        0.
    };
    (sidebar, panel)
}

/// Largeur de la poignée de glisser, en pixels d'écran, centrée sur le bord intérieur.
pub(crate) const HANDLE_WIDTH: f32 = 8.;
/// Colonnes de terminal que la zone centrale garde quand on élargit la barre ou le volet.
const MIN_CENTER_COLUMNS: f32 = 40.;

/// Le pas, le plancher et le plafond de la taille du texte (⌘+, ⌘−), en points.
pub(crate) const FONT_STEP: f32 = 1.;
pub(crate) const FONT_MIN: f32 = 10.;
pub(crate) const FONT_MAX: f32 = 32.;

/// La taille du texte après un pas de `delta` points, bornée. Une taille déjà hors des bornes (la config en admet
/// de plus larges) garde son côté : un pas ne va jamais contre la touche pressée.
pub(crate) fn stepped_font_size(current: f32, delta: f32) -> f32 {
    (current + delta).clamp(FONT_MIN.min(current), FONT_MAX.max(current))
}

/// Le zoom que représente la taille `size` : aucun quand elle est celle de la config.
pub(crate) fn font_zoom_for(size: f32, config_size: f32) -> Option<f32> {
    (size != config_size).then_some(size)
}

/// Le zoom qui survit à un rechargement de la config : un changement de police dans `config.toml` le remet à la
/// nouvelle valeur, tout autre changement le laisse.
pub(crate) fn zoom_after_reload(zoom: Option<f32>, effects: ReloadEffects) -> Option<f32> {
    if effects.terminal_font { None } else { zoom }
}

/// Le bord que l'on glisse : celui de la barre (à sa droite) ou celui du volet (à sa gauche).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    Sidebar,
    Panel,
}

/// Un glisser en cours : le bord, l'abscisse de l'appui et la largeur d'alors (unités de la maquette).
#[derive(Debug, Clone, Copy)]
struct EdgeDrag {
    edge: Edge,
    start_x: Pixels,
    start_width: f32,
    /// Le volet droit ouvert à l'appui : un glisser qui survit à un changement de volet s'arrête (`drag_edge_to`).
    panel: Option<RightPanel>,
    /// La souris a bougé depuis l'appui : un simple clic ne réécrit pas `state.json`.
    moved: bool,
}

/// Les projets que la relecture peut adopter : ceux des sessions de projet. Un bouillon garde `@idema_project` sur
/// le dossier d'idéation, qui ne devient jamais un projet de la barre.
fn adoptable_projects(sessions: &[ListedSession]) -> Vec<&str> {
    sessions
        .iter()
        .filter(|session| !session.ideation && !session.project.is_empty())
        .map(|session| session.project.as_str())
        .collect()
}

/// Le projet `path` est-il celui que `⌘T` vise devant la session affichée ? Jamais quand un bouillon est
/// sélectionné : `⌘T` y crée un bouillon, aucune ligne de projet ne porte le style de sélection.
fn is_targeted(
    project_wins: bool,
    ideation_selected: bool,
    selected_project: Option<&Path>,
    path: &Path,
) -> bool {
    project_wins && !ideation_selected && selected_project == Some(path)
}

/// Retire des attentes les noms d'une création terminée : l'entrée de son projet et de son libellé, et son nom
/// Remote Control. Les attentes des autres créations restent.
fn clear_pending(
    names: &mut Vec<(PathBuf, String)>,
    remote_names: &mut Vec<String>,
    project: &Path,
    display_name: &str,
    remote_name: Option<&str>,
) {
    if let Some(at) = names
        .iter()
        .position(|(pending, name)| pending == project && name == display_name)
    {
        names.remove(at);
    }
    if let Some(remote_name) = remote_name
        && let Some(at) = remote_names
            .iter()
            .position(|pending| pending == remote_name)
    {
        remote_names.remove(at);
    }
}

/// Les noms Remote Control déjà pris : ceux des bouillons vivants et ceux des créations en attente.
fn taken_remote_names<'a>(
    living: impl Iterator<Item = &'a str>,
    pending: &'a [String],
) -> Vec<&'a str> {
    living.chain(pending.iter().map(String::as_str)).collect()
}

/// Ce que la création hors du fil d'interface rend à `on_restored`.
struct Restored {
    plans: Vec<SessionPlan>,
    /// Un résultat par plan, dans l'ordre : les identifiants des panes créés.
    outcomes: Vec<Result<Vec<PaneId>, ServerError>>,
    /// La relecture de tmux faite juste après la création.
    listing: (
        Result<Vec<ListedSession>, ServerError>,
        Result<Vec<ListedPane>, ServerError>,
    ),
    /// Les sessions déjà sautées avant la création.
    skipped: Vec<(String, SkipReason)>,
    started: Instant,
}

/// Le nom Remote Control d'un bouillon sauvé, quand il en a un : il est retenu pendant la restauration comme pendant
/// une création.
fn pending_remote_name(saved: &SavedSession) -> Option<&str> {
    saved
        .ideation
        .then_some(saved.remote_name.as_str())
        .filter(|name| !name.is_empty())
}

/// Une session sautée : la ligne du journal (avec le détail), et ce que la notice en dira.
fn skip_session(saved: &SavedSession, reason: SkipReason) -> (String, SkipReason) {
    hud::log_event(&format!(
        "restore skipped name={} reason={}",
        saved.name,
        reason.journal_reason()
    ));
    (saved.display_name.clone(), reason)
}

/// Les commandes de lancement d'une session restaurée : les panes rendus par tmux, dans l'ordre du plan, ceux qui ont
/// une commande reçoivent un délai qui continue la file de la restauration (`first` lancements déjà placés). Rend aussi
/// le nombre de reprises (`--resume`) et de départs à neuf.
fn restored_launches(
    plan: &SessionPlan,
    ids: Vec<PaneId>,
    first: usize,
    pacer: &LaunchPacer,
) -> (Vec<PaneLaunch>, usize, usize) {
    let mut launches = Vec::new();
    let (mut resumed, mut fresh) = (0, 0);
    for (pane, planned) in ids.into_iter().zip(&plan.panes) {
        let Some(command) = &planned.launch else {
            continue;
        };
        launches.push(PaneLaunch {
            pane,
            command: command.clone(),
            delay: launch_delay(first + launches.len()),
            pacer: Some(pacer.clone()),
        });
        if planned.resumed {
            resumed += 1;
        } else {
            fresh += 1;
        }
    }
    (launches, resumed, fresh)
}

/// Recrée une session du plan dans tmux et rend les identifiants de ses panes, dans l'ordre du plan.
fn restore_plan(
    server: &TmuxServer,
    plan: &SessionPlan,
    columns: u16,
    lines: u16,
) -> Result<Vec<PaneId>, ServerError> {
    let saved = &plan.saved;
    let panes: Vec<RestorePane> = plan
        .panes
        .iter()
        .map(|pane| RestorePane {
            cwd: &pane.cwd,
            agent: &pane.agent,
            hidden_at: pane.hidden_at.map(u128::from),
            active: pane.active,
            resume: pane.resume_option.as_deref(),
        })
        .collect();
    server.restore_session(&RestoreSession {
        name: &saved.name,
        project: &saved.project,
        display_name: &saved.display_name,
        created: u128::from(saved.created),
        layout: &saved.layout,
        remote_name: saved.ideation.then_some(saved.remote_name.as_str()),
        columns,
        lines,
        panes: &panes,
    })
}

/// Une entrée par session listée, dans l'ordre de la barre. Le pane suivi est l'actif de la session, sinon son
/// premier ; l'agent est celui de ce pane.
fn session_entries(sessions: &[ListedSession], panes: &[ListedPane]) -> Vec<SessionEntry> {
    let mut summaries: Vec<SessionSummary> = sessions
        .iter()
        .map(|session| SessionSummary {
            name: session.name.clone(),
            project: PathBuf::from(&session.project),
            display_name: session.display_name.clone(),
            created: session.created,
            state: ActivityState::Idle,
            ideation: session.ideation,
            machine: match split_session_key(&session.name) {
                SessionTarget::Local(_) => None,
                SessionTarget::Remote { connection, .. } => Some(connection.to_owned()),
            },
        })
        .collect();
    sort_sessions(&mut summaries);
    summaries
        .into_iter()
        .filter_map(|summary| {
            let mut own = panes.iter().filter(|pane| pane.session == summary.name);
            let first = own.next()?;
            let tracked = std::iter::once(first)
                .chain(own)
                .find(|pane| pane.active)
                .unwrap_or(first);
            let listed = sessions.iter().find(|session| session.name == summary.name);
            let preset = listed.map_or(Preset::Single, |session| preset_of(&session.layout));
            let auto_layout = summary.machine.is_none()
                || listed.is_some_and(|session| !session.layout.is_empty());
            Some(SessionEntry {
                agent: tracked.agent.clone(),
                remote_name: listed.map_or_else(String::new, |session| session.remote_name.clone()),
                pane: tracked.pane,
                preset,
                auto_layout,
                summary,
                view: None,
                attempts: 0,
                unreachable: None,
                state_since: SystemTime::now(),
                _subscription: None,
            })
        })
        .collect()
}

/// Ce que tmux ne dit pas des sessions du Mac, pour les serveurs de dev : les entrées locales seulement. Les panes lus
/// sont ceux du tmux du Mac, et le registre des serveurs de dev ignore les machines distantes (spec ssh-tmux §3.9).
fn local_session_infos(entries: &[SessionEntry]) -> Vec<SessionInfo> {
    entries
        .iter()
        .filter(|entry| entry.summary.machine.is_none())
        .map(|entry| SessionInfo {
            name: entry.summary.name.clone(),
            project: entry.summary.project.clone(),
            display_name: entry.summary.display_name.clone(),
        })
        .collect()
}

/// L'instantané de restauration ne garde que les sessions du Mac (spec ssh-tmux §3.9) : la capture ne lit que le tmux
/// d'IdemA, mais une clé distante ne doit jamais s'écrire dans `snapshot.json`, ni être recréée au lancement.
fn local_sessions_only(capture: Vec<SavedSession>) -> Vec<SavedSession> {
    capture
        .into_iter()
        .filter(|session| machine_of(&session.name) == LOCAL_MACHINE)
        .collect()
}

/// Des noms de session empruntés, rendus possédés (l'ordre de navigation mêle des sources de durées de vie différentes).
fn owned_names(names: Vec<&str>) -> Vec<String> {
    names.into_iter().map(str::to_owned).collect()
}

/// Le résultat d'un amorçage dont le fil s'est arrêté sans répondre : un blocage de démarrage, pas une panique.
fn lost_bootstrap() -> bootstrap::Outcome {
    (
        Err(ServerError::Io(std::io::Error::other(
            "the bootstrap thread ended without a result",
        ))),
        Duration::ZERO,
    )
}

/// Appui gauche sur une zone vide de la bande de titre ou de l'en-tête : déplace la fenêtre, et un double-clic
/// applique le réglage macOS de la barre de titre.
pub fn drag_window(event: &MouseDownEvent, window: &mut Window, _cx: &mut App) {
    if event.click_count >= 2 {
        window.titlebar_double_click();
    } else {
        window.start_window_move();
    }
}

/// Ce que le lanceur démarre dans une session neuve : une entrée du lanceur, ou le shell seul.
#[derive(Debug, Clone, PartialEq)]
pub enum LaunchChoice {
    Agent(AgentEntry),
    Shell,
}

impl LaunchChoice {
    /// Le nom de l'entrée : celui de l'agent, « Shell » pour le shell seul. Il sert de nom de session par
    /// défaut et de valeur à `@idema_agent`.
    fn label(&self) -> &str {
        match self {
            Self::Agent(agent) => &agent.name,
            Self::Shell => SHELL_SESSION_NAME,
        }
    }
}

/// Le renommage en cours : la session visée et son champ. L'abonnement meurt avec lui.
struct Renaming {
    session: String,
    field: Entity<RenameField>,
    _subscription: Subscription,
}

/// Délai avant chaque nouvelle attache d'une vue dont le client s'est arrêté alors que la session vit encore.
const REATTACH_DELAY: Duration = Duration::from_secs(1);

/// Le temps que tmux finisse de créer une session avant la première relecture qui suit `%sessions-changed`.
const SESSIONS_SETTLE: Duration = Duration::from_millis(50);

/// Une seconde relecture, un peu plus tard : un script extérieur pose `@idema_project` après `new-session`, et la
/// session n'est listée qu'à ce moment (spec §5.4).
const SESSIONS_RECHECK: Duration = Duration::from_secs(1);

/// Le sondage sans client : quand aucune entrée n'a de vue, aucune notification n'arrive, la liste se relit.
/// Une seconde, comme `CONFIG_POLL` : AC-37 demande moins de 2 s, et le pire cas est un tour de sondage plus la lecture
/// (mesuré à la porte du jalon 4b : avec 2 s, 4 séries sur 5 dépassaient, jusqu'à 2082 ms).
const SESSIONS_POLL: Duration = Duration::from_secs(1);

/// La relecture des sessions d'une machine distante connectée : elle apporte les `@idnma_agent` posés après coup et les
/// sessions d'une machine sans vue attachée. Une seule à la fois par machine (un canal bref, jamais empilé).
const REMOTE_POLL: Duration = Duration::from_secs(2);

/// Pas de la minuterie qui compare l'horloge murale pour reconnaître la sortie de veille du Mac.
const WAKE_TICK: Duration = Duration::from_secs(2);

/// Les relectures de suite qui échouent avant que la connexion d'une machine soit tenue pour perdue.
const REMOTE_LISTING_FAILURES: u8 = 3;

/// Le brut d'un arrêt du client sans raison de tmux (`%exit` nu).
const NO_EXIT_REASON: &str = "control client exited";

/// L'Entrée différée d'une saisie du téléphone : tapée seulement si elle part avant `ENTER_DEADLINE`, si le pane existe
/// encore, si le client de contrôle de sa session n'est pas sorti et si le pane accepte toujours une saisie. Faux : rien
/// n'a été tapé. Tout se décide sans attendre, sur le fil d'interface : aucune pause entre le contrôle et l'écriture.
fn type_delayed_enter(
    pane: &WeakEntity<PaneView>,
    session: &WeakEntity<SessionView>,
    received_at: Instant,
    cx: &mut AsyncApp,
) -> bool {
    if !enter_still_allowed(received_at, Instant::now()) {
        return false;
    }
    // Une session disparue ou un client sorti : l'Entrée n'arriverait pas, ou pas où le téléphone croit.
    if session
        .update(cx, |session, _| session.control_client_exited())
        .unwrap_or(true)
    {
        return false;
    }
    pane.update(cx, |pane, _| pane.send_remote_enter())
        .unwrap_or(false)
}

/// `companion input …` au journal, avec le nom de session déjà passé par `session_log_label`.
fn log_input(session: &str, pane: u32, outcome: InputOutcome, main_thread_us: u128) {
    hud::log_event(&format!(
        "companion input session={session} pane={pane} outcome={} main_thread_us={main_thread_us}",
        outcome.as_str()
    ));
}

/// Une saisie du téléphone, telle que le service l'a transmise, et l'instant où le fil d'interface la prend.
struct RemoteRequest {
    session: String,
    pane: u32,
    input: RemoteInput,
    command_id: String,
    received_at: Instant,
    started: Instant,
}

/// Une session d'Idema telle que l'espace de travail la tient : ce que la barre montre, et la vue attachée.
pub(crate) struct SessionEntry {
    pub(crate) summary: SessionSummary,
    /// `@idema_agent` : l'entrée du lanceur utilisée.
    pub(crate) agent: String,
    /// `@idema_remote_name` : le nom Remote Control d'un bouillon, vide pour une session de projet.
    pub(crate) remote_name: String,
    /// Le pane suivi : l'actif de la session au listage.
    pane: PaneId,
    /// `@idema_layout` au listage : le preset avec lequel la vue démarre.
    preset: Preset,
    /// IdemA répare le layout de la session : toujours en local ; sur une machine distante, seulement si la session
    /// porte un `@idema_layout` (une session créée hors d'IdemA s'affiche telle que tmux la rend).
    auto_layout: bool,
    /// `None` quand l'attache a échoué ou que les essais sont épuisés : l'entrée reste dans la barre jusqu'à ce
    /// que la session disparaisse.
    pub(crate) view: Option<Entity<SessionView>>,
    /// Le compte de la série d'essais de reconnexion en cours (`lifecycle::after_close`) : repart de zéro après un
    /// client qui a vécu `REATTACH_STABLE`.
    attempts: u8,
    /// Le message brut de la dernière chute quand les essais sont épuisés : le centre dit « Session injoignable »
    /// tant qu'il y en a un. Remis à `None` par une attache réussie.
    pub(crate) unreachable: Option<String>,
    /// L'instant d'entrée dans l'état courant de `summary.state` : le compagnon l'affiche (« attend · 3 min »).
    state_since: SystemTime,
    _subscription: Option<Subscription>,
}

impl SessionEntry {
    /// Pose l'état affiché de la session et l'instant où elle y entre. Rend l'état d'avant, s'il a changé.
    fn enter_state(&mut self, state: ActivityState, at: SystemTime) -> Option<ActivityState> {
        if self.summary.state == state {
            return None;
        }
        self.state_since = at;
        Some(std::mem::replace(&mut self.summary.state, state))
    }
}

/// Ce que le clic sur « Ramener dans ce pane » retient jusqu'à la fin de l'arrêt (`finish_reclaim`).
struct ReclaimClick {
    /// La vue de la session au clic : le nom peut changer pendant l'attente, la vue reste la même.
    held: WeakEntity<SessionView>,
    /// Le pane actif au clic : celui qui doit recevoir la commande.
    pane: PaneId,
    /// L'entrée du lanceur choisie au clic (`None` : aucune ne correspond).
    agent: Option<String>,
    /// La commande de reprise à taper.
    command: String,
    target: OutsideSession,
}

/// Ce que la création d'une session fixe avant l'appel à tmux.
struct SessionRequest {
    project: PathBuf,
    display_name: String,
    /// `@idema_agent` : le libellé de l'agent, ou « Shell ».
    agent: String,
    /// La commande tapée quand le pane est prêt.
    command: Option<String>,
    /// Le nom Remote Control d'un bouillon ; `None` pour une session de projet.
    remote_name: Option<String>,
    /// La connexion de la machine distante qui héberge la session ; `None` : le Mac.
    machine: Option<String>,
}

pub struct Workspace {
    focus_handle: FocusHandle,
    config: Config,
    /// La taille du texte en vigueur quand ⌘+ ou ⌘− l'éloigne de la config ; rien n'est écrit sur disque.
    font_zoom: Option<f32>,
    /// Le dossier de configuration d'IdemA (`IDEMA_HOME` ou `~/.config/idema`).
    dir: PathBuf,
    config_path: PathBuf,
    config_watch: FileWatch,
    /// L'erreur de la dernière lecture de `config.toml` : `config` reste alors la dernière version valide.
    config_error: Option<ConfigError>,
    /// `None` quand `state.json` n'a pas pu être lu (erreur d'E/S) : l'état vit en mémoire et rien n'est écrit.
    store: Option<StateStore>,
    in_memory_state: State,
    server: Option<Arc<TmuxServer>>,
    /// Les machines distantes de `[machines]`, dans l'ordre du fichier, chacune avec son état de connexion.
    pub(crate) machines: Vec<RemoteMachine>,
    /// Les noms des agents qui tournent dans les panes de chaque session distante (clé de session), au dernier listage de
    /// sa machine : la barre en tire les logos.
    remote_agents: HashMap<String, Vec<String>>,
    /// Les sessions listées, dans l'ordre de la barre (`sort_sessions`).
    pub(crate) entries: Vec<SessionEntry>,
    /// Projet choisi par le dernier geste de sélection : l'écran vide en montre le contexte.
    selected_project: Option<PathBuf>,
    /// Vrai quand `selected_project` a été choisi après la session affichée (clic sur sa ligne, ajout) : le dernier
    /// geste gagne, `context_project` le vise alors devant la session. `select` le remet à faux.
    project_wins: bool,
    startup_error: Option<StartupError>,
    /// Vrai une fois l'amorçage réussi : la première peinture qui suit mesure celle de la barre.
    bootstrapped: bool,
    /// Un drapeau par mesure, pour qu'elle s'écrive une seule fois par lancement.
    window_paint_logged: Rc<Cell<bool>>,
    sidebar_paint_logged: Rc<Cell<bool>>,
    /// Les projets dont le dossier n'existe plus : recalculés à l'amorçage et à chaque activation de la fenêtre.
    missing: HashSet<PathBuf>,
    /// Le menu flottant ouvert, s'il y en a un. Il prend le focus pendant qu'il vit.
    menu: Option<OpenMenu>,
    /// Ce que le volet Browser sait du couvrement de sa page (`page_covered`) : le dernier signal envoyé.
    page_covered: bool,
    /// Le champ de renommage ouvert dans la barre, s'il y en a un.
    renaming: Option<Renaming>,
    /// Les noms de sessions en cours de création (projet, nom affiché) : deux créations rapprochées ne
    /// prennent pas le même nom par défaut.
    pending_names: Vec<(PathBuf, String)>,
    /// Les noms Remote Control des bouillons en cours de création : un second clic avant la fin du premier ne
    /// reprend pas le même.
    pending_remote_names: Vec<String>,
    /// Vrai pendant que le dialogue du Finder est ouvert : un second `⌘O` n'en ouvre pas un autre.
    project_dialog_open: bool,
    /// Le dialogue « Ouvrir un fichier » est ouvert : un second `⌘⇧O` ne fait rien.
    file_dialog_open: bool,
    /// Projet de l'écran vide dont la branche est affichée, et cette branche une fois lue.
    branch_project: Option<PathBuf>,
    branch: Option<String>,
    /// Compte les `%sessions-changed` reçus : une relecture en attente s'arrête quand une notification plus récente
    /// la remplace.
    sessions_generation: u64,
    _sessions_task: Option<Task<()>>,
    /// Le sondage de `list-sessions` pour le cas sans client.
    _sessions_poll: Option<Task<()>>,
    /// La minuterie du réveil du Mac (`start_wake_clock`) : elle vit autant que la fenêtre.
    _wake_clock: Option<Task<()>>,
    /// L'observateur du chemin réseau et la tâche qui reçoit ses retours (`start_network_watch`) : la chute arrête les deux.
    _network_watch: Option<(NetworkMonitor, Task<()>)>,
    _bootstrap_task: Option<Task<()>>,
    /// L'attache différée des sessions listées au démarrage.
    _attach_task: Option<Task<()>>,
    /// Le compagnon iPhone : éteint tant que `[companion]` est absente de la config.
    companion: CompanionController,
    /// Le serveur de chaque machine, partagé avec le fil du compagnon qui y capture l'écran d'un pane
    /// (`sync_session_servers`).
    session_servers: SessionServers,
    /// La session distante que `state.json` gardait au lancement, tant qu'elle attend la connexion de sa machine
    /// (spec ssh-tmux §3.5, `should_reselect`).
    reselect_saved: Option<String>,
    /// Romain a choisi une session depuis le lancement : la sélection mémorisée ne l'emporte plus.
    chosen_since_launch: bool,
    /// La tâche qui traite sur le fil d'interface les commandes que le service du compagnon lui envoie.
    _companion_task: Option<Task<()>>,
    /// La minuterie de présence (5 s) : elle n'existe que tant que le service du compagnon tourne, et tombe avec lui.
    _presence_task: Option<Task<()>>,
    /// `companion disabled reason=tmux_unavailable` est déjà au journal pour cette section : une ligne par état, pas une
    /// par rechargement de la config.
    companion_unavailable_logged: bool,
    _config_task: Task<()>,
    _branch_task: Option<Task<()>>,
    /// Le volet droit ouvert, s'il y en a un. Fermé, chacun garde ses entités : les shells du Terminal et les volets
    /// Fichiers vivent tant qu'IdemA tourne.
    right_panel: Option<RightPanel>,
    /// Le volet Terminal : l'état « agrandi » du volet affiché et un shell local par projet.
    terminal: TerminalPanel,
    /// Les machines dont le shell du volet Terminal est en cours d'ouverture (recherche du dossier du pane, puis ssh) : au
    /// plus une ouverture par machine.
    pending_terminals: HashSet<String>,
    /// « Ouvrir un terminal » d'un en-tête de machine : le volet montre le shell de cette machine tant que le contexte est
    /// celui de la demande.
    machine_terminal: Option<MachineTerminalPin>,
    /// Un volet Fichiers par projet, créé à la première ouverture pour ce projet (spec P2, §4.5). La clé est le
    /// chemin tel que `state.json` le garde ; le volet canonise sa propre racine.
    files_panels: HashMap<PathBuf, Entity<FilesPanel>>,
    _files_subscriptions: Vec<Subscription>,
    /// Un volet Changes par projet, créé à la première ouverture pour ce projet (spec P3, §4.1), gardé comme ceux de Fichiers.
    changes_panels: HashMap<PathBuf, Entity<ChangesPanel>>,
    _changes_subscriptions: Vec<Subscription>,
    /// Le volet Browser : un seul pour toute la fenêtre (spec P4, P4-5), créé à la première ouverture. Fermé ou caché
    /// par un autre volet, il garde ses onglets et ses pages.
    browser: Option<Entity<BrowserPanel>>,
    _browser_subscription: Option<Subscription>,
    /// La galerie Artifacts : créée à la première demande, gardée ensuite (elle garde sa liste, sa recherche et sa
    /// disposition). Jamais persistée : au relancement, le centre montre la session sélectionnée.
    artifacts: Option<Entity<ArtifactsView>>,
    _artifacts_subscription: Option<Subscription>,
    /// Vrai quand la galerie occupe la colonne centrale à la place de la session sélectionnée.
    artifacts_shown: bool,
    /// Le volet Scratchpad : une seule note pour toute la fenêtre, créée au démarrage (le fichier est petit). Fermé, il
    /// garde son texte, son curseur et sa minuterie de sauvegarde.
    scratchpad: Entity<ScratchpadPanel>,
    _scratchpad_subscription: Subscription,
    /// Le volet Projet : une seule entité pour la fenêtre, créée au démarrage (elle ne lit le disque que pour un projet
    /// affiché). Fermé, elle garde son onglet choisi et les éditeurs d'`AGENTS.md` des projets déjà vus.
    project_panel: Entity<ProjectPanel>,
    _project_subscription: Subscription,
    /// Les serveurs de dev des projets (spec P4 §5) : une entité par fenêtre, créée quand tmux répond.
    servers: Option<Entity<Servers>>,
    /// Le dernier nombre de la pastille du bouton Browser écrit au journal (`servers badge count=N`).
    logged_badge: Option<usize>,
    _servers_subscription: Option<Subscription>,
    /// Pousse les projets et les racines des panes à l'entité toutes les 2 s.
    _servers_task: Option<Task<()>>,
    /// Le gardien de l'instantané de restauration (spec §3.2-3.3) : la tâche de capture le nourrit, la fermeture d'une
    /// session le vide de cette session.
    snapshot: SnapshotKeeper,
    /// L'écrivain de `snapshot.json` (à côté de `state.json`), hors du fil d'interface : un seul écrivain, la
    /// dernière version gagne. Un enregistrement échoué est retenté à la prochaine capture.
    snapshot_writer: SnapshotWriter,
    /// Capture les sessions de tmux toutes les 2 s, hors du fil d'interface.
    _snapshot_task: Option<Task<()>>,
    /// Les sessions Claude Code qui tournent hors d'IdemA, rafraîchies à chaque capture (`start_reclaim_scan`).
    reclaim: ReclaimScan,
    /// Ce que la découverte retient d'un tour à l'autre (racines de projet, environnements de lancement).
    reclaim_cache: Arc<Mutex<ReclaimCache>>,
    /// Le début du tour de découverte en cours sur le fil de fond, s'il y en a un.
    reclaim_scan_started: Option<Instant>,
    /// Le tour en cours a déjà été signalé comme bloqué au journal : une ligne par tour, pas une toutes les 2 s.
    reclaim_stuck_logged: bool,
    /// Le `pid` de la session qu'un geste « Ramener » est en train d'arrêter : un seul geste à la fois.
    reclaiming: Option<i32>,
    /// La minuterie qui rend ses captures à l'instantané une fois les agents restaurés démarrés.
    _restore_task: Option<Task<()>>,
    /// Les lignes de la notice de restauration, sous la liste des projets, jusqu'à son clic de fermeture.
    restore_notice: Option<Vec<String>>,
    /// Le clavier est dans le volet droit affiché. Il suit le focus réel (`sync_keyboard`) mais survit au passage par un
    /// menu, un champ de renommage ou la racine : fermer ou valider rend le focus à celui qui l'avait.
    panel_keyboard: bool,
    /// Le glisser du bord de la barre ou du volet, s'il y en a un : la largeur vit dans `state.json` en mémoire
    /// pendant le glisser ; le relâché l'écrit.
    edge_drag: Option<EdgeDrag>,
    _activation: Subscription,
    _root_focus: Subscription,
    /// À la sortie d'Idema, chaque shell local est raccroché avant que le futur de sortie ne se rende.
    _quit: Subscription,
}

impl Workspace {
    pub fn new(paths: Paths, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Paths { dir, bootstrap } = paths;
        let config_path = dir.join("config.toml");
        let snapshot_path = dir.join("snapshot.json");
        // Une version inconnue (un IdemA plus récent l'a écrite) : rien n'est restauré et le fichier n'est jamais écrit.
        let (snapshot, snapshot_writer) = match load_snapshot(&snapshot_path) {
            SnapshotLoad::UnknownVersion(version) => {
                hud::log_event(&format!("snapshot read-only version={version}"));
                (
                    Snapshot::default(),
                    SnapshotWriter::read_only(snapshot_path),
                )
            }
            loaded => (loaded.into_snapshot(), SnapshotWriter::new(snapshot_path)),
        };
        if let Err(error) = ensure_config_file(&config_path) {
            hud::log_event(&format!("config create failed: {error}"));
        }
        // La surveillance retient le fichier avant la première lecture : une modification entre les deux est vue.
        let mut config_watch = FileWatch::new(config_path.clone());
        config_watch.changed();
        let (config, config_error) = match load_config(&config_path) {
            Ok(config) => (config, None),
            Err(error) => {
                hud::log_event(&format!("config rejected: {error}"));
                (Config::default(), Some(error))
            }
        };
        cx.set_global(Theme::new(config.ui_font_size));
        cx.set_global(TerminalFont::from_config(&config));
        cx.set_global(CommitMessageSettings(config.commit_message.clone()));
        cx.set_global(BrowserSettings::from_config(&config));
        apply_editor_theme(
            &Theme::new(config.ui_font_size),
            &TerminalFont::from_config(&config),
            cx,
        );

        let (store, in_memory_state) = match StateStore::load(&dir.join("state.json")) {
            Ok(store) => (Some(store), State::default()),
            Err(error) => {
                hud::log_event(&format!("state load failed: {error}"));
                (None, State::default())
            }
        };

        // L'amorçage tourne déjà (lancé par `main`, avant la création de la fenêtre). Quand il a fini (en général
        // avant la première image, tmux répond en 100 ms et la fenêtre en demande 200), son résultat s'applique
        // ici même et la première image montre déjà la liste des sessions ; sinon une tâche l'attend.
        // `bootstrap_ms` compte depuis son départ.
        let mut bootstrap = bootstrap;
        let (early, task) = match bootstrap.try_recv() {
            Ok(None) => {
                let waiting = cx.spawn_in(window, async move |this, cx| {
                    let outcome = bootstrap.await.unwrap_or_else(|_| lost_bootstrap());
                    let (result, elapsed) = outcome;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.on_bootstrap(result, elapsed, window, cx)
                    });
                });
                (None, Some(waiting))
            }
            Ok(Some(outcome)) => (Some(outcome), None),
            Err(_) => (Some(lost_bootstrap()), None),
        };

        let config_task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(CONFIG_POLL).await;
                if this
                    .update_in(cx, |this, window, cx| this.reload_config(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        // Une session fermée par un autre client n'envoie rien à une entrée sans vue : on les recompte à chaque retour.
        // Elle recalcule aussi qui est « vue » : la session du centre ne l'est que dans la fenêtre active.
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            this.update_seen(window, cx);
            if window.is_window_active() {
                this.refresh_missing(cx);
                this.reconcile(window, cx);
            }
        });
        // Tout appui de souris sous la racine lui donne le focus (barre, bande de titre, boutons) : la saisie
        // revient aussitôt au terminal affiché. Un champ à lui (futur menu, renommage) est un descendant qui
        // prend le focus lui-même, la racine n'est alors pas touchée. Sans session affichée, rien ne bouge.
        let root_focus = cx.on_focus(&focus_handle, window, |this, window, cx| {
            // Le clic droit qui ouvre un menu finit sur la racine (le menu naît pendant la phase de remontée de
            // l'appui, puis la racine prend le focus) : tant qu'un menu vit, le focus lui revient.
            if let Some(menu) = &this.menu {
                window.focus(&menu.focus, cx);
            } else if let Some(handle) = this.focus_target(cx) {
                window.focus(&handle, cx);
            }
        });
        // Le SIGHUP part ici et le SIGKILL d'un process détaché (écart E27) : GPUI n'attend que `SHUTDOWN_TIMEOUT`.
        let quit = cx.on_app_quit(|this, cx| {
            for view in this.terminal.views() {
                view.update(cx, |view, _| view.terminate_local());
            }
            this.release_size_pins(cx);
            // Les onglets partent sur le disque avant la sortie (spec P2 §4.6).
            this.save_files_panels("quit", cx);
            this.save_browser_tabs(cx);
            this.flush_scratchpad("quit", cx);
            this.flush_project("quit", cx);
            std::future::ready(())
        });
        // Le bouton rouge ferme la fenêtre avant que `cx.quit()` ne parte : le `Workspace` est alors déjà lâché et
        // `on_app_quit` ci-dessus ne l'atteint plus. La fermeture de la fenêtre sauvegarde donc elle-même.
        let closing = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            if let Some(workspace) = closing.upgrade() {
                workspace.update(cx, |this, cx| {
                    this.release_size_pins(cx);
                    this.save_files_panels("quit", cx);
                    this.save_browser_tabs(cx);
                    this.flush_scratchpad("quit", cx);
                    this.flush_project("quit", cx);
                });
            }
            true
        });
        // La note vit tout le temps de la fenêtre : sa lecture est synchrone et le fichier n'est jamais créé ici.
        let scratchpad_file = scratchpad_path(&dir);
        let scratchpad = cx.new(|cx| ScratchpadPanel::new(scratchpad_file, window, cx));
        let scratchpad_subscription =
            cx.subscribe_in(&scratchpad, window, Self::on_scratchpad_event);
        let project_panel = cx.new(|cx| ProjectPanel::new(window, cx));
        let project_subscription = cx.subscribe_in(&project_panel, window, Self::on_project_event);
        let mut workspace = Self {
            focus_handle,
            config,
            font_zoom: None,
            dir,
            config_path,
            config_watch,
            config_error,
            store,
            in_memory_state,
            server: None,
            machines: Vec::new(),
            remote_agents: HashMap::new(),
            entries: Vec::new(),
            selected_project: None,
            project_wins: false,
            startup_error: None,
            bootstrapped: false,
            window_paint_logged: Rc::new(Cell::new(false)),
            sidebar_paint_logged: Rc::new(Cell::new(false)),
            missing: HashSet::new(),
            menu: None,
            page_covered: false,
            renaming: None,
            pending_names: Vec::new(),
            pending_remote_names: Vec::new(),
            project_dialog_open: false,
            file_dialog_open: false,
            branch_project: None,
            branch: None,
            sessions_generation: 0,
            _sessions_task: None,
            _sessions_poll: None,
            _wake_clock: None,
            _network_watch: None,
            _bootstrap_task: task,
            _attach_task: None,
            companion: CompanionController::new(),
            session_servers: no_servers(),
            reselect_saved: None,
            chosen_since_launch: false,
            _companion_task: None,
            _presence_task: None,
            companion_unavailable_logged: false,
            _config_task: config_task,
            _branch_task: None,
            right_panel: None,
            terminal: TerminalPanel::default(),
            pending_terminals: HashSet::new(),
            machine_terminal: None,
            files_panels: HashMap::new(),
            _files_subscriptions: Vec::new(),
            changes_panels: HashMap::new(),
            _changes_subscriptions: Vec::new(),
            browser: None,
            _browser_subscription: None,
            artifacts: None,
            _artifacts_subscription: None,
            artifacts_shown: false,
            scratchpad,
            _scratchpad_subscription: scratchpad_subscription,
            project_panel,
            _project_subscription: project_subscription,
            servers: None,
            logged_badge: None,
            _servers_subscription: None,
            _servers_task: None,
            snapshot: SnapshotKeeper::new(snapshot),
            snapshot_writer,
            _snapshot_task: None,
            reclaim: ReclaimScan::default(),
            reclaim_cache: Arc::new(Mutex::new(ReclaimCache::default())),
            reclaim_scan_started: None,
            reclaim_stuck_logged: false,
            reclaiming: None,
            _restore_task: None,
            restore_notice: None,
            panel_keyboard: false,
            edge_drag: None,
            _activation: activation,
            _root_focus: root_focus,
            _quit: quit,
        };
        // Une session distante sélectionnée à la fermeture laisse le centre vide jusqu'à la connexion de sa machine.
        workspace.reselect_saved = workspace
            .state()
            .ui
            .selected_session
            .clone()
            .filter(|key| machine_of(key) != LOCAL_MACHINE);
        if let Some((result, elapsed)) = early {
            workspace.on_bootstrap(result, elapsed, window, cx);
        }
        // Les machines distantes se connectent à part : une machine injoignable ne retarde ni l'amorçage ni la barre.
        workspace.sync_machines(window, cx);
        workspace
    }

    /// Écrit les onglets de tous les volets Fichiers sur le disque (`⌘Q`, fermeture de la fenêtre).
    fn save_files_panels(&mut self, on: &str, cx: &mut Context<Self>) {
        for panel in self.files_panels.values() {
            let _ = panel.update(cx, |panel, cx| panel.save_all(on, cx));
        }
    }

    /// Écrit les onglets du navigateur dans `state.json` (`ui.browser_tabs`). Sans volet créé, les onglets lus au
    /// démarrage restent tels quels : un fichier qu'on n'a pas ouvert n'est pas réécrit.
    fn save_browser_tabs(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = &self.browser else {
            return;
        };
        let saved = panel.read(cx).saved_tabs();
        // Une navigation rapporte son adresse deux fois (validation, puis KVO) : le même état n'est écrit qu'une fois.
        if self.state().ui.browser_tabs.as_ref() == Some(&saved) {
            return;
        }
        self.state_mut().ui.browser_tabs = Some(saved);
        self.save();
    }

    pub(crate) fn state(&self) -> &State {
        self.store
            .as_ref()
            .map_or(&self.in_memory_state, |store| &store.state)
    }

    fn state_mut(&mut self) -> &mut State {
        match self.store.as_mut() {
            Some(store) => &mut store.state,
            None => &mut self.in_memory_state,
        }
    }

    pub(crate) fn notice(&self) -> Option<&StateNotice> {
        self.store.as_ref().and_then(|store| store.notice.as_ref())
    }

    /// Écrit `state.json`, sauf quand il n'a pas pu être lu : un fichier qu'on n'a pas lu n'est jamais réécrit.
    fn save(&self) {
        if let Some(store) = &self.store
            && let Err(error) = store.save()
        {
            hud::log_event(&format!("state save failed: {error}"));
        }
    }

    fn on_bootstrap(
        &mut self,
        result: Result<Bootstrap, ServerError>,
        elapsed: Duration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(Bootstrap {
                server,
                sessions,
                panes,
            }) => {
                let applied = Instant::now();
                // Décidé sur les sessions en main et gelé tout de suite : la première capture de `start_snapshot` verrait
                // sinon un serveur vide et commencerait à effacer l'instantané qu'on va restaurer.
                let restoring = should_restore(&sessions, self.snapshot.snapshot());
                if restoring {
                    self.snapshot.suspend();
                }
                self.adopt_projects(&sessions);
                self.refresh_missing(cx);
                self.server = Some(Arc::clone(&server));
                self.sync_companion(window, cx);
                // Les machines distantes ont pu se connecter avant la fin de l'amorçage : leurs entrées (et leurs vues) restent.
                let remote: Vec<SessionEntry> = std::mem::take(&mut self.entries)
                    .into_iter()
                    .filter(|entry| entry.summary.machine.is_some())
                    .collect();
                self.entries = session_entries(&sessions, &panes);
                self.entries.extend(remote);
                self.entries
                    .sort_by(|a, b| session_order(&a.summary, &b.summary));
                self.publish_fleet(cx);
                self.drop_hidden_selection();
                self.bootstrapped = true;
                self.start_servers(Arc::clone(&server), window, cx);
                self.start_snapshot(Arc::clone(&server), window, cx);
                hud::log_event(&format!("bootstrap_ms={}", elapsed.as_millis()));
                hud::log_step("bootstrap_applied", applied);
                self.sync_branch(cx);
                if restoring {
                    self.restore_sessions(server, window, cx);
                } else {
                    // La barre se peint avec la liste des sessions ; l'attache (un process tmux par session) vient
                    // après, pour ne pas retarder cette première peinture (P-1).
                    let attach = cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(ATTACH_AFTER_PAINT).await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            let step = Instant::now();
                            this.attach_all(window, cx);
                            hud::log_step("attach_all", step);
                        });
                    });
                    self._attach_task = Some(attach);
                }
                self.start_sessions_poll(window, cx);
            }
            Err(error) => {
                self.startup_error = Some(StartupError::from(error));
                self.sync_companion(window, cx);
            }
        }
        cx.notify();
    }

    /// Crée l'entité des serveurs et la tâche qui la nourrit : les projets de la barre (relus quand la liste change) et
    /// les racines des panes lues dans tmux hors du fil principal.
    fn start_servers(
        &mut self,
        tmux: Arc<TmuxServer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.servers.is_some() {
            return;
        }
        let servers = cx.new(|cx| Servers::new(Arc::clone(&tmux), cx));
        let ports = self.state().ui.servers_dev_ports.clone();
        servers.update(cx, |servers, _| servers.restore_dev_ports(ports));
        self._servers_subscription = Some(cx.subscribe(&servers, Self::on_servers_event));
        if let Some(panel) = self.browser.clone() {
            let (servers, tmux) = (servers.clone(), Arc::clone(&tmux));
            panel.update(cx, |panel, cx| panel.set_servers(servers, tmux, window, cx));
        }
        self.servers = Some(servers);
        self.sync_servers_projects(cx);
        self._servers_task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut failing = false;
            loop {
                let inputs = this.update_in(cx, |this, _, cx| {
                    this.sync_servers_projects(cx);
                    this.pane_inputs(cx)
                });
                let Ok(inputs) = inputs else {
                    break;
                };
                let reader = Arc::clone(&tmux);
                let observed = cx
                    .background_executor()
                    .spawn(async move { observe_panes(&reader, &inputs) })
                    .await;
                let pushed = this.update_in(cx, |this, _, cx| match observed {
                    Ok(observation) => {
                        failing = false;
                        if let Some(servers) = this.servers.clone() {
                            servers
                                .update(cx, |servers, cx| servers.set_pane_roots(observation, cx));
                        }
                    }
                    Err(error) => {
                        // Une ligne par panne, pas une toutes les 2 s tant qu'elle dure.
                        if !failing {
                            hud::log_event(&format!("servers pane listing failed: {error}"));
                        }
                        failing = true;
                    }
                });
                if pushed.is_err() {
                    break;
                }
                cx.background_executor().timer(SCAN_PERIOD).await;
            }
        }));
    }

    /// La tâche de capture de l'instantané (spec §3.2-3.3) : toutes les `SCAN_PERIOD`, tmux est lu hors du fil
    /// d'interface (`capture_now`), puis le gardien applique la capture. Les entrées (agents de la config, dossier des
    /// sessions Claude, table de processus du dernier relevé des serveurs, pour ne pas relancer un second `ps`) sont
    /// lues sur le fil d'interface. Sans relevé récent, le tour est sauté, comme une capture en erreur.
    fn start_snapshot(
        &mut self,
        tmux: Arc<TmuxServer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self._snapshot_task.is_some() {
            return;
        }
        self._snapshot_task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut failing = false;
            loop {
                let inputs = this.update_in(cx, |this, _, cx| {
                    let home = std::env::var_os("HOME");
                    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR");
                    let claude_dir = sessions_dir(home.as_deref(), config_dir.as_deref());
                    // Lue avant la capture : un pane créé entre les deux a son PID absent de la table, et sa session
                    // n'est simplement pas listée ce tour-ci.
                    let processes = this.servers.as_ref().and_then(|servers| {
                        servers.read(cx).recent_processes(SNAPSHOT_MAX_SCAN_AGE)
                    });
                    // Les shells du volet Terminal : une session lancée de là est dans IdemA.
                    let terminal_shells: Vec<i32> = this
                        .terminal
                        .shell_pids(cx)
                        .into_iter()
                        .map(|(_, pid)| pid)
                        .collect();
                    (
                        this.config.agents.clone(),
                        this.config.ideation.clone(),
                        claude_dir,
                        reclaim_paths(home.as_deref(), config_dir.as_deref()),
                        terminal_shells,
                        processes,
                    )
                });
                let Ok((agents, ideation, claude_dir, reclaim_dirs, terminal_shells, processes)) =
                    inputs
                else {
                    break;
                };
                let Some(processes) = processes else {
                    cx.background_executor().timer(SCAN_PERIOD).await;
                    continue;
                };
                let processes = Arc::new(processes);
                let reader = Arc::clone(&tmux);
                let captured = cx
                    .background_executor()
                    .spawn({
                        let processes = Arc::clone(&processes);
                        async move {
                            capture_now(
                                &reader,
                                &agents,
                                claude_dir.as_deref(),
                                &processes,
                                ideation.as_ref(),
                            )
                        }
                    })
                    .await;
                let applied = this.update_in(cx, |this, _, cx| match captured {
                    Ok(capture) => {
                        failing = false;
                        let projects: Vec<PathBuf> = capture
                            .sessions
                            .iter()
                            .map(|session| session.project.clone())
                            .collect();
                        this.apply_capture(capture.sessions);
                        if let Some(paths) = reclaim_dirs {
                            this.start_reclaim_scan(
                                paths,
                                capture.pane_pids,
                                terminal_shells,
                                processes,
                                projects,
                                cx,
                            );
                        }
                    }
                    // Une capture en erreur ne touche pas au gardien : « je ne sais pas » ne veut pas dire « vide ».
                    Err(error) => {
                        // Une ligne par panne, pas une toutes les 2 s tant qu'elle dure.
                        if !failing {
                            hud::log_event(&format!("snapshot capture failed: {error}"));
                        }
                        failing = true;
                    }
                });
                if applied.is_err() {
                    break;
                }
                cx.background_executor().timer(SCAN_PERIOD).await;
            }
        }));
    }

    /// Rafraîchit la liste des sessions hors d'IdemA avec la capture qui vient d'aboutir. La lecture (disque, `ps`,
    /// `git`) part sur le `background_executor` dans une tâche détachée : un `git` lent ne retarde jamais la boucle
    /// d'instantané. Un seul tour à la fois ; si le précédent n'a pas fini, celui-ci est sauté. Pas de `cx.notify()` : le
    /// menu relit `reclaim` à son ouverture.
    fn start_reclaim_scan(
        &mut self,
        paths: ReclaimPaths,
        pane_pids: Vec<i32>,
        terminal_shells: Vec<i32>,
        processes: Arc<HashMap<i32, Process>>,
        projects: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let now = Instant::now();
        if let Some(started) = self.reclaim_scan_started {
            // Un tour trop long (`git` ou un volume qui ne répond plus) gèle la liste sans rien casser d'autre : on
            // le dit une fois, et on continue de sauter.
            if scan_is_stuck(started, now) && !self.reclaim_stuck_logged {
                self.reclaim_stuck_logged = true;
                hud::log_event("reclaim scan stuck");
            }
            return;
        }
        self.reclaim_scan_started = Some(now);
        self.reclaim_stuck_logged = false;
        let cache = Arc::clone(&self.reclaim_cache);
        cx.spawn(async move |this, cx| {
            let scanned = cx
                .background_executor()
                .spawn(async move {
                    scan_outside(
                        &paths,
                        &pane_pids,
                        &terminal_shells,
                        std::process::id() as i32,
                        &processes,
                        &projects,
                        &cache,
                    )
                })
                .await;
            // L'espace de travail fermé entre-temps : plus personne à prévenir.
            let _ = this.update(cx, |this, _| {
                this.reclaim = scanned;
                this.reclaim_scan_started = None;
            });
        })
        .detach();
    }

    /// Passe une capture au gardien, journalise les retenues qui commencent et écrit l'instantané s'il a changé (ou si
    /// son dernier enregistrement a échoué).
    fn apply_capture(&mut self, capture: Vec<SavedSession>) {
        let applied = self
            .snapshot
            .apply(local_sessions_only(capture), Instant::now());
        for (name, reason) in &applied.held {
            hud::log_event(&format!(
                "snapshot hold name={name} reason={}",
                reason.as_str()
            ));
        }
        if applied.changed || self.snapshot_writer.failed() {
            self.persist_snapshot();
        }
    }

    /// Demande l'écriture de `snapshot.json` à l'écrivain de fond (dernière version gagnante). Un échec est écrit une
    /// fois au journal par l'écrivain et retenté à la capture suivante.
    fn persist_snapshot(&mut self) {
        self.snapshot_writer
            .submit(self.snapshot.snapshot().clone());
    }

    /// La restauration (spec §3.4) : l'amorçage a trouvé un tmux sans session d'IdemA et un instantané qui en a. Le
    /// gardien est déjà gelé (`on_bootstrap`). Les sessions dont le dossier a disparu sont sautées ; les autres sont
    /// recréées hors du fil d'interface, puis `on_restored` les attache et leur donne leurs commandes de lancement.
    fn restore_sessions(
        &mut self,
        server: Arc<TmuxServer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let started = Instant::now();
        let saved = self.snapshot.snapshot().sessions.clone();
        hud::log_event(&format!("restore start sessions={}", saved.len()));
        let mut plans = Vec::new();
        let mut skipped = Vec::new();
        for session in &saved {
            // Une session distante revient par la connexion de sa machine, jamais par la restauration.
            if machine_of(&session.name) != LOCAL_MACHINE {
                continue;
            }
            // Config lue sans erreur et sans section `[ideation]` : le bouillon n'est pas restauré, sans notice. Il quitte
            // ensuite l'instantané à la prochaine capture (après `snapshot.resume()`, la capture ne le voit plus, et il
            // disparaît de `snapshot.json` à la fin de la retenue de 10 s) ; sa conversation reste reprenable à la main.
            // Une config refusée ne masque rien : il revient avec ses commandes.
            if !ideation_restorable(
                session,
                self.ideation().is_some(),
                self.config_error.is_none(),
            ) {
                hud::log_event(&format!(
                    "ideation restore skipped name={} reason=disabled",
                    session.name
                ));
                continue;
            }
            match plan_session(session, Path::is_dir) {
                Ok(plan) => plans.push(plan),
                Err(reason) => skipped.push(skip_session(session, reason)),
            }
        }
        // Les noms sont retenus avant la création : la relecture périodique ne prend pas ces sessions pour des ajouts
        // faits hors d'IdemA, et un clic sur « + » ne reprend pas leur nom affiché.
        for plan in &plans {
            let saved = &plan.saved;
            self.pending_names
                .push((saved.project.clone(), saved.display_name.clone()));
            if let Some(remote_name) = pending_remote_name(saved) {
                self.pending_remote_names.push(remote_name.to_owned());
            }
        }
        let grid = self.central_grid(window, Preset::Single.column_gaps());
        let columns = u16::try_from(grid.columns).unwrap_or(u16::MAX).max(1);
        let lines = u16::try_from(grid.lines).unwrap_or(u16::MAX).max(1);
        let work_plans = plans.clone();
        let work = cx.background_spawn(async move {
            let outcomes: Vec<Result<Vec<PaneId>, ServerError>> = work_plans
                .iter()
                .map(|plan| restore_plan(&server, plan, columns, lines))
                .collect();
            let listing = (server.sessions(), server.panes());
            (outcomes, listing)
        });
        cx.spawn_in(window, async move |this, cx| {
            let (outcomes, listing) = work.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let done = Restored {
                    plans,
                    outcomes,
                    listing,
                    skipped,
                    started,
                };
                this.on_restored(done, window, cx);
            });
        })
        .detach();
    }

    /// La suite de `restore_sessions`, sur le fil d'interface : les noms en attente sont relâchés (un par plan), les
    /// sessions recréées entrent dans la barre et s'attachent avec leurs commandes, la notice s'affiche, et
    /// l'instantané reprend ses captures quand les agents ont eu le temps de démarrer.
    fn on_restored(&mut self, done: Restored, window: &mut Window, cx: &mut Context<Self>) {
        let Restored {
            plans,
            outcomes,
            listing,
            mut skipped,
            started,
        } = done;
        // La session sélectionnée lance ses agents la première : elle est celle qu'on regarde.
        let selected = self.selected_name().map(str::to_owned);
        let mut order: Vec<usize> = (0..plans.len()).collect();
        order.sort_by_key(|&at| selected.as_deref() != Some(plans[at].saved.name.as_str()));
        let mut outcomes: Vec<Option<_>> = outcomes.into_iter().map(Some).collect();
        let mut launches: HashMap<String, Vec<PaneLaunch>> = HashMap::new();
        let mut restored_names: HashSet<String> = HashSet::new();
        let mut launched = 0;
        // Un seul rythme pour toute la restauration : les envois sont espacés d'au moins `LAUNCH_STAGGER`.
        let pacer = LaunchPacer::new(Instant::now());
        for at in order {
            let plan = &plans[at];
            let saved = &plan.saved;
            clear_pending(
                &mut self.pending_names,
                &mut self.pending_remote_names,
                &saved.project,
                &saved.display_name,
                pending_remote_name(saved),
            );
            match outcomes[at].take() {
                Some(Ok(ids)) => {
                    let (own, resumed, fresh) = restored_launches(plan, ids, launched, &pacer);
                    launched += own.len();
                    hud::log_event(&format!(
                        "restore session name={} panes={} resumed={resumed} fresh={fresh}",
                        saved.name,
                        plan.panes.len()
                    ));
                    launches.insert(saved.name.clone(), own);
                    restored_names.insert(saved.name.clone());
                }
                Some(Err(error)) => {
                    skipped.push(skip_session(saved, SkipReason::Failed(error.to_string())));
                }
                None => {}
            }
        }
        match listing {
            (Ok(sessions), Ok(panes)) => {
                // Seules les sessions restaurées entrent ici : une création faite pendant ce temps a son propre chemin.
                let sessions: Vec<ListedSession> = sessions
                    .into_iter()
                    .filter(|session| restored_names.contains(&session.name))
                    .collect();
                self.adopt_projects(&sessions);
                self.refresh_missing(cx);
                for entry in session_entries(&sessions, &panes) {
                    if self.entry_index(&entry.summary.name).is_none() {
                        self.entries.push(entry);
                    }
                }
                self.entries
                    .sort_by(|a, b| session_order(&a.summary, &b.summary));
                self.drop_hidden_selection();
                self.sync_branch(cx);
                self.attach_sessions(launches, window, cx);
            }
            (Err(error), _) | (_, Err(error)) => {
                // Les sessions existent dans tmux : la relecture périodique les reprendra, sans leurs commandes.
                hud::log_event(&format!(
                    "restore launches dropped count={launched}: {error}"
                ));
                // Aucune porte n'existe : rien à attendre avant de reprendre les captures.
                launched = 0;
            }
        }
        hud::log_event(&format!(
            "restore done ms={}",
            started.elapsed().as_millis()
        ));
        self.restore_notice = Some(notice_lines(restored_names.len(), &skipped));
        // Tant que les agents démarrent, une capture les verrait sans conversation (spec §3.3).
        let wait = resume_delay(launched);
        self._restore_task = Some(cx.spawn_in(window, async move |this, cx| {
            if !wait.is_zero() {
                cx.background_executor().timer(wait).await;
            }
            let _ = this.update(cx, |this, _| this.snapshot.resume());
        }));
        cx.notify();
    }

    /// Les lignes de la notice de restauration, tant que le clic de fermeture ne l'a pas retirée.
    pub(crate) fn restore_notice(&self) -> Option<&[String]> {
        self.restore_notice.as_deref()
    }

    /// Un clic sur la notice de restauration la ferme.
    pub(crate) fn close_restore_notice(&mut self, cx: &mut Context<Self>) {
        if self.restore_notice.take().is_some() {
            cx.notify();
        }
    }

    /// Une session fermée par un geste d'IdemA sort de l'instantané tout de suite, sans attendre les 10 s de la règle de
    /// l'extinction. Le retrait du serveur par la mort de tmux, lui, passe par `remove_entry` et n'oublie rien.
    fn forget_closed_session(&mut self, name: &str) {
        if self.snapshot.forget(name) {
            self.persist_snapshot();
        }
    }

    /// Les projets de la barre, dans son ordre, poussés aux serveurs (une liste inchangée ne coûte rien).
    fn sync_servers_projects(&mut self, cx: &mut Context<Self>) {
        // Ajout, retrait ou ordre des projets : la flotte suit l'ordre de la barre.
        self.publish_fleet(cx);
        let Some(servers) = self.servers.clone() else {
            return;
        };
        let projects: Vec<PathBuf> = self
            .state()
            .ordered_projects()
            .iter()
            .map(|project| project.path.clone())
            .collect();
        servers.update(cx, |servers, cx| servers.set_projects(projects, cx));
    }

    /// Les marques des agents qui tournent dans la session (relevé des serveurs, toutes les 2 s) ; rien avant le premier
    /// relevé ni sans serveur tmux.
    pub(crate) fn session_agent_marks(&self, session: &str, cx: &App) -> Vec<AgentMark> {
        // Une session distante : ses agents viennent du listage de sa machine, leurs logos de son `~/.idnma.yml`.
        if let SessionTarget::Remote { connection, .. } = split_session_key(session) {
            let names = self
                .remote_agents
                .get(session)
                .map_or(&[][..], Vec::as_slice);
            let agents = self
                .machines
                .iter()
                .find(|machine| machine.entry.connection == connection)
                .and_then(|machine| machine.facts.as_ref())
                .and_then(|facts| facts.agents.as_deref())
                .unwrap_or_default();
            return remote_agent_marks(names, agents);
        }
        let Some(servers) = &self.servers else {
            return Vec::new();
        };
        agent_marks(servers.read(cx).agents_of(session), &self.config.agents)
    }

    /// Ce que tmux ne dit pas des panes : les sessions de la barre, les lanceurs de la config en mémoire et les shells
    /// du volet Terminal.
    fn pane_inputs(&self, cx: &App) -> PaneInputs {
        PaneInputs {
            sessions: local_session_infos(&self.entries),
            agents: self.config.agents.clone(),
            terminal_shells: self.terminal.shell_pids(cx),
        }
    }

    /// Les serveurs ont changé : le port retenu de `dev` part dans `state.json` quand il a changé.
    fn on_servers_event(
        &mut self,
        servers: Entity<Servers>,
        event: &ServersEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ServersEvent::Changed => {
                let ports = servers.read(cx).remembered_dev_ports();
                if ports != self.state().ui.servers_dev_ports {
                    self.state_mut().ui.servers_dev_ports = ports;
                    self.save();
                }
                // Un relevé a peut-être reconnu ou perdu un agent dans un pane.
                self.publish_fleet(cx);
                cx.notify();
            }
        }
    }

    /// Les projets des sessions listées entrent dans la barre. Les bouillons n'en ont pas.
    fn adopt_projects(&mut self, sessions: &[ListedSession]) {
        let now = rfc3339(SystemTime::now());
        let mut added = false;
        for project in adoptable_projects(sessions) {
            added |= self.state_mut().add_project(Path::new(project), &now);
        }
        if added {
            self.save();
        }
    }

    /// Sans client attaché, tmux n'envoie aucune notification : tant qu'aucune entrée n'a de vue, la liste des
    /// sessions se relit toutes les `SESSIONS_POLL`. `sessions()` ne démarre jamais de serveur.
    fn start_sessions_poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._sessions_poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(SESSIONS_POLL).await;
                let alive = this.update_in(cx, |this, window, cx| {
                    // Les entrées des machines distantes ont leur propre relecture (`refresh_machine`).
                    if this
                        .entries
                        .iter()
                        .filter(|entry| entry.summary.machine.is_none())
                        .all(|entry| entry.view.is_none())
                    {
                        this.refresh_sessions(window, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    /// `%sessions-changed` : relecture après `SESSIONS_SETTLE`, puis une seconde après `SESSIONS_RECHECK`. Chaque
    /// client le reçoit, donc la notification suivante remplace celle-ci (la génération) : seule la dernière va
    /// jusqu'au bout.
    fn on_sessions_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sessions_generation += 1;
        let generation = self.sessions_generation;
        self._sessions_task = Some(cx.spawn_in(window, async move |this, cx| {
            for delay in [SESSIONS_SETTLE, SESSIONS_RECHECK] {
                cx.background_executor().timer(delay).await;
                let current = this.update_in(cx, |this, window, cx| {
                    let current = this.sessions_generation == generation;
                    if current {
                        this.refresh_sessions(window, cx);
                    }
                    current
                });
                if !current.unwrap_or(false) {
                    return;
                }
            }
        }));
    }

    /// Relit `list-sessions` et `list-panes` hors du fil d'interface, puis met la barre d'accord : les sessions créées
    /// hors d'IdemA entrent, celles qui ont disparu sortent. Les noms d'aujourd'hui sont retenus au départ : une
    /// entrée née pendant la relecture n'est jamais retirée par elle (`session_sync`).
    fn refresh_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = self.server.clone() else {
            return;
        };
        let snapshot = self.machine_entry_keys(LOCAL_MACHINE);
        let listing = cx.background_spawn(async move { (server.sessions(), server.panes()) });
        cx.spawn_in(window, async move |this, cx| {
            let (sessions, panes) = listing.await;
            let (sessions, panes) = match (sessions, panes) {
                (Ok(sessions), Ok(panes)) => (sessions, panes),
                (Err(error), _) | (_, Err(error)) => {
                    hud::log_event(&format!("session listing failed: {error}"));
                    return;
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.apply_sessions(LOCAL_MACHINE, &snapshot, &sessions, &panes, window, cx);
            });
        })
        .detach();
    }

    /// Les clés des entrées de la machine `machine` (`LOCAL_MACHINE` ou une connexion).
    fn machine_entry_keys(&self, machine: &str) -> Vec<String> {
        machine_keys(
            self.entries.iter().map(|entry| entry.summary.name.as_str()),
            machine,
        )
    }

    /// Met la barre d'accord avec le listage de `machine` : seules les entrées de cette machine peuvent sortir (le listage
    /// d'un tmux ne dit rien des autres), les sessions neuves entrent et s'attachent. `snapshot` : les clés de la
    /// machine au départ de la relecture.
    fn apply_sessions(
        &mut self,
        machine: &str,
        snapshot: &[String],
        sessions: &[ListedSession],
        panes: &[ListedPane],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keys: Vec<String> = self
            .entries
            .iter()
            .map(|entry| entry.summary.name.clone())
            .collect();
        let sync = machine_sync(machine, &keys, snapshot, &self.pending_names, sessions);
        for name in &sync.gone {
            // L'entrée a pu partir pendant la relecture (fermeture, `reconcile`) : rien à journaliser alors.
            if self.entry_index(name).is_none() {
                continue;
            }
            hud::log_event(&format!("session vanished session={}", key_label(name)));
            self.remove_entry(name, window, cx);
        }
        let newcomers: Vec<ListedSession> = sessions
            .iter()
            .filter(|session| sync.added.contains(&session.name))
            .cloned()
            .collect();
        if newcomers.is_empty() {
            return;
        }
        // Les projets d'une session distante ne sont pas ceux du Mac : seule la relecture locale en adopte.
        if machine == LOCAL_MACHINE {
            self.adopt_projects(&newcomers);
            self.refresh_missing(cx);
        }
        // Une session sans pane n'a pas d'entrée (`session_entries`) : la relecture suivante la reprendra.
        let fresh = session_entries(&newcomers, panes);
        let names: Vec<String> = fresh
            .iter()
            .map(|entry| entry.summary.name.clone())
            .collect();
        for entry in fresh {
            hud::log_event(&format!(
                "session appeared session={} project={}",
                key_label(&entry.summary.name),
                entry.summary.project.display()
            ));
            self.entries.push(entry);
        }
        self.entries
            .sort_by(|a, b| session_order(&a.summary, &b.summary));
        // Attachées sans taille, comme au lancement : un échec suit les essais de `on_attach_lost` (écart E34).
        for name in names {
            if let Err(error) = self.attach(&name, None, Vec::new(), window, cx) {
                hud::log_event(&format!(
                    "attach failed session={}: {error}",
                    key_label(&name)
                ));
                self.on_attach_lost(&name, error.to_string(), None, window, cx);
            }
        }
        cx.notify();
    }

    /// Le projet de l'écran vide : le dernier geste gagne (projet cliqué ou session sélectionnée), sinon le projet
    /// sélectionné, sinon le premier de la barre. Un bouillon sélectionné n'apporte pas son dossier : le contexte
    /// reste le dernier projet, comme si le bouillon n'était pas là.
    pub(crate) fn context_project(&self) -> Option<PathBuf> {
        crate::sidebar_model::target_project(
            self.selected_entry()
                .filter(|entry| !entry.summary.ideation && entry.summary.machine.is_none())
                .map(|entry| entry.summary.project.clone()),
            self.selected_project.clone(),
            self.project_wins,
            self.state()
                .ordered_projects()
                .first()
                .map(|project| project.path.clone()),
        )
    }

    /// La session sélectionnée est-elle un bouillon ?
    pub(crate) fn selected_is_ideation(&self) -> bool {
        self.selected_entry()
            .is_some_and(|entry| entry.summary.ideation)
    }

    /// La session sélectionnée vit-elle sur une machine distante ?
    pub(crate) fn selected_is_remote(&self) -> bool {
        self.selected_machine().is_some()
    }

    /// La connexion de la machine de la session sélectionnée, quand elle est distante.
    fn selected_machine(&self) -> Option<&str> {
        self.selected_entry()?.summary.machine.as_deref()
    }

    /// Le projet que lisent la barre d'outils, la branche, les volets Terminal, Fichiers et Changes et le Browser :
    /// aucun quand un bouillon est sélectionné (il n'a ni dépôt ni projet) ni quand la session est sur une machine
    /// distante (ses dossiers ne sont pas ceux de ce Mac), sinon `context_project`.
    pub(crate) fn panel_project(&self) -> Option<PathBuf> {
        if self.selected_is_ideation() || self.selected_is_remote() {
            None
        } else {
            self.context_project()
        }
    }

    /// Nom de la session sélectionnée, tel que `state.json` le garde. Il peut ne plus être listé.
    pub(crate) fn selected_name(&self) -> Option<&str> {
        self.state().ui.selected_session.as_deref()
    }

    fn selected_entry(&self) -> Option<&SessionEntry> {
        let name = self.selected_name()?;
        self.entries.iter().find(|entry| entry.summary.name == name)
    }

    fn entry_index(&self, name: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.summary.name == name)
    }

    /// La vue de la session `name`, quand elle en a une.
    fn entry_view(&self, name: &str) -> Option<Entity<SessionView>> {
        self.entries[self.entry_index(name)?].view.clone()
    }

    /// La vue de la session sélectionnée, quand elle en a une.
    fn selected_view(&self) -> Option<Entity<SessionView>> {
        self.selected_entry()?.view.clone()
    }

    /// La police du terminal en vigueur : celle de la config, à la taille du zoom quand il y en a un.
    fn terminal_font(&self) -> TerminalFont {
        let font = TerminalFont::from_config(&self.config);
        TerminalFont {
            size: self.font_zoom.unwrap_or(font.size),
            ..font
        }
    }

    /// Un pas de taille du texte (`delta` points) à partir de la taille en vigueur.
    fn step_font_size(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.terminal_font().size;
        self.apply_font_zoom(stepped_font_size(current, delta), window, cx);
    }

    /// Pose la taille `size` pour tous les panes et le volet Terminal ; la taille de la config n'est plus un zoom.
    fn apply_font_zoom(&mut self, size: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.font_zoom = font_zoom_for(size, self.config.terminal_font_size);
        let font = self.terminal_font();
        hud::log_event(&format!("font size={}", font.size));
        apply_editor_theme(&Theme::new(self.config.ui_font_size), &font, cx);
        cx.set_global(font);
        window.refresh();
        cx.notify();
    }

    /// La grille de la zone centrale : la fenêtre moins la barre (si visible) et l'en-tête, puis les marges et la
    /// cellule du terminal, comme la mesurera la première peinture de la vue. `gaps` est le nombre de gouttières du
    /// preset de la session (`Preset::column_gaps`) : la grille leur laisse la place.
    pub(crate) fn central_grid(&self, window: &Window, gaps: u16) -> GridSize {
        let theme = Theme::new(self.config.ui_font_size);
        let terminal_font = self.terminal_font();
        let viewport = window.viewport_size();
        let sidebar = theme.px(self.edge_widths(window).0);
        let area = gpui::Bounds::new(
            gpui::point(px(0.), px(0.)),
            gpui::size(
                (viewport.width - sidebar).max(px(0.)),
                (viewport.height - theme.px(48.)).max(px(0.)),
            ),
        );
        let content = content_bounds(area, &theme);
        let cell = cell_metrics(window, &terminal_font);
        attach_grid(
            f32::from(content.size.width),
            f32::from(content.size.height),
            (f32::from(cell.0), f32::from(cell.1)),
            gaps,
        )
    }

    /// Les largeurs affichées de la barre et du volet (`edge_widths_for`), en unités de la maquette. Le volet est
    /// compté à côté de la colonne principale tant qu'il est ouvert et pas agrandi.
    pub(crate) fn edge_widths(&self, window: &Window) -> (f32, f32) {
        self.edge_widths_with(
            window,
            self.right_panel.is_some() && !self.panel_maximized(),
        )
    }

    fn edge_widths_with(&self, window: &Window, panel_beside: bool) -> (f32, f32) {
        let theme = Theme::new(self.config.ui_font_size);
        let cell = cell_metrics(window, &self.terminal_font());
        let window_width = f32::from(window.viewport_size().width) / theme.scale;
        // Un pixel de plus : `Theme::px` arrondit au pixel la largeur de la barre et celle du volet, soit jusqu'à
        // 0,5 px chacune aux deux bords du centre, et l'arrondi ne doit pas manger la 40e colonne.
        let center_minimum = ((MIN_CENTER_COLUMNS * f32::from(cell.0)
            + 2. * f32::from(theme.px(MARGIN_SIDES)))
        .ceil()
            + 1.)
            / theme.scale;
        edge_widths_for(window_width, center_minimum, &self.state().ui, panel_beside)
    }

    /// L'appui sur la poignée d'un bord : démarre le glisser depuis la largeur affichée (pas de saut).
    fn begin_edge_drag(&mut self, edge: Edge, x: Pixels, window: &Window, cx: &mut Context<Self>) {
        let (sidebar, panel) = self.edge_widths(window);
        let start_width = match edge {
            Edge::Sidebar => sidebar,
            Edge::Panel => panel,
        };
        self.edge_drag = Some(EdgeDrag {
            edge,
            start_x: x,
            start_width,
            panel: self.right_panel,
            moved: false,
        });
        cx.notify();
    }

    /// Le mouvement d'un glisser : la largeur suit la souris, bornée comme au rendu. La zone centrale suit, et la
    /// taille part à tmux par le délai de `SessionView::sync_size`. Rien n'est écrit sur disque avant le relâché.
    fn drag_edge_to(&mut self, x: Pixels, window: &Window, cx: &mut Context<Self>) {
        let Some(drag) = self.edge_drag else {
            return;
        };
        // Le volet a changé en plein glisser (`⌘P`, `` ⌃` ``) : le glisser s'arrête.
        if drag.edge == Edge::Panel && drag.panel != self.right_panel {
            self.edge_drag = None;
            cx.notify();
            return;
        }
        let scale = Theme::new(self.config.ui_font_size).scale;
        let delta = f32::from(x - drag.start_x) / scale;
        let wanted = match drag.edge {
            Edge::Sidebar => drag.start_width + delta,
            Edge::Panel => drag.start_width - delta,
        };
        if let Some(drag) = self.edge_drag.as_mut() {
            drag.moved = true;
        }
        self.set_width(drag.edge, drag.panel, Some(wanted));
        // La valeur gardée est la largeur affichée, bornée comme au rendu : relâcher n'écrit jamais au-delà.
        let (sidebar, panel) = self.edge_widths(window);
        let shown = match drag.edge {
            Edge::Sidebar => sidebar,
            Edge::Panel => panel,
        };
        self.set_width(drag.edge, drag.panel, Some(shown));
        cx.notify();
    }

    fn set_width(&mut self, edge: Edge, panel: Option<RightPanel>, width: Option<f32>) {
        set_edge_width(&mut self.state_mut().ui, edge, panel, width);
    }

    /// Le relâché : le glisser s'arrête et, s'il a bougé, la largeur est écrite.
    fn end_edge_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(drag) = self.edge_drag.take() {
            if drag.moved {
                self.save();
            }
            cx.notify();
        }
    }

    /// Le double-clic sur une poignée : la largeur de la maquette revient, et s'écrit.
    fn reset_edge_width(&mut self, edge: Edge, cx: &mut Context<Self>) {
        self.edge_drag = None;
        self.set_width(edge, self.right_panel, None);
        self.save();
        cx.notify();
    }

    /// Les gouttières du preset mémorisé (`@idema_layout`) de la session `name` ; aucune si elle est inconnue.
    fn stored_gaps(&self, name: &str) -> u16 {
        self.entry_index(name)
            .map_or(0, |index| self.entries[index].preset.column_gaps())
    }

    /// L'erreur de la dernière lecture de `config.toml`, affichée sous la liste des projets.
    pub(crate) fn config_error(&self) -> Option<&ConfigError> {
        self.config_error.as_ref()
    }

    /// Un pas de la surveillance : relit `config.toml` s'il a changé. Une config valide remplace l'ancienne ;
    /// une config refusée laisse la dernière valide en place et montre l'erreur.
    fn reload_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.config_watch.changed() {
            return;
        }
        match load_config(&self.config_path) {
            Ok(config) => {
                let effects = config.reload_effects(&self.config);
                // À chaque rechargement réussi, quels que soient les effets : la section ne demande aucun rafraîchissement.
                cx.set_global(CommitMessageSettings(config.commit_message.clone()));
                cx.set_global(BrowserSettings::from_config(&config));
                if effects.theme {
                    cx.set_global(Theme::new(config.ui_font_size));
                }
                if effects.terminal_font {
                    cx.set_global(TerminalFont::from_config(&config));
                }
                self.font_zoom = zoom_after_reload(self.font_zoom, effects);
                let ideation_was_on = self.ideation().is_some();
                self.config = config;
                self.config_error = None;
                self.sync_machines(window, cx);
                self.sync_companion(window, cx);
                // Les lanceurs (logos) et le libellé des bouillons viennent de la config.
                self.publish_fleet(cx);
                if effects.theme || effects.terminal_font {
                    apply_editor_theme(
                        &Theme::new(self.config.ui_font_size),
                        &self.terminal_font(),
                        cx,
                    );
                }
                self.leave_hidden_ideation(window, cx);
                if ideation_was_on != self.ideation().is_some() {
                    self.refresh_badge(cx);
                }
                hud::log_event(&format!(
                    "config reloaded theme={} terminal_font={} ideation={}",
                    effects.theme,
                    effects.terminal_font,
                    if self.ideation().is_some() {
                        "on"
                    } else {
                        "off"
                    }
                ));
            }
            Err(error) => {
                hud::log_event(&format!("config rejected: {error}"));
                self.config_error = Some(error);
            }
        }
        window.refresh();
        cx.notify();
    }

    /// Aligne le service du compagnon sur la config : le démarre, l'arrête ou le redémarre. Il ne démarre qu'avec le
    /// serveur tmux prêt ; si l'amorçage a échoué, la section est dite sans effet au journal, une fois : de nouveau
    /// seulement si la section est retirée puis remise.
    fn sync_companion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.config.companion.is_none() {
            self.companion_unavailable_logged = false;
        } else if self.server.is_none()
            && self.startup_error.is_some()
            && !self.companion_unavailable_logged
        {
            hud::log_event("companion disabled reason=tmux_unavailable");
            self.companion_unavailable_logged = true;
        }
        let scratchpad = scratchpad_path(&self.dir);
        self.sync_session_servers();
        let started = self.companion.sync(
            self.config.companion.as_ref(),
            self.server
                .is_some()
                .then(|| Arc::clone(&self.session_servers)),
            &self.dir,
            &scratchpad,
        );
        match started {
            Some(mut commands) => {
                // Le service qui démarre n'a encore aucun instantané : le premier part tout de suite.
                self.publish_fleet(cx);
                self._companion_task = Some(cx.spawn(async move |this, cx| {
                    while let Some(command) = commands.next().await {
                        if this
                            .update(cx, |this, cx| this.on_companion_command(command, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
                // La minuterie de présence naît avec le service (un redémarrage remplace la précédente).
                self._presence_task = Some(cx.spawn_in(window, async move |this, cx| {
                    loop {
                        cx.background_executor().timer(PRESENCE_POLL).await;
                        let keep = this
                            .update_in(cx, |this, window, cx| this.evaluate_presence(window, cx))
                            .unwrap_or(false);
                        if !keep {
                            break;
                        }
                    }
                }));
            }
            None if !self.companion.is_on() => {
                self._companion_task = None;
                // Plus de service : plus de minuterie, et Romain redevient présent (le « vu » d'avant).
                if self._presence_task.take().is_some() {
                    self.update_seen(window, cx);
                }
            }
            None => {}
        }
    }

    /// Un tour de la présence de Romain : absent après `away_after_seconds` sans entrée, ou écran verrouillé. À un
    /// changement, le service est prévenu et « vu » est recalculé (absent, aucune session n'est vue).
    ///
    /// Rend faux quand la minuterie doit s'arrêter : plus de section, ou prise non liée (port pris). Dans ce cas
    /// Romain redevient présent et le « vu » est recalculé une dernière fois ; un service relancé a sa propre minuterie.
    fn evaluate_presence(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let Some(section) = &self.config.companion else {
            return false;
        };
        if self.companion.listen_failed() {
            self.update_seen(window, cx);
            return false;
        }
        let away = presence::is_away(
            presence::hid_idle_seconds(),
            presence::screen_locked(),
            section.away_after_seconds,
        );
        if !self.companion.set_away(away) {
            return true;
        }
        hud::log_event(if away {
            "companion presence away"
        } else {
            "companion presence present"
        });
        self.companion.send(ServiceEvent::Presence { away });
        if away {
            // Une question posée sous les yeux de Romain, ou à l'instant où « vu » est tombé (verrouillage de l'écran,
            // `REARM_GRACE`), se réarme avant que `update_seen` retire le « vu » et publie les états (AC-25).
            let views: Vec<Entity<SessionView>> = self
                .entries
                .iter()
                .filter_map(|entry| entry.view.clone())
                .collect();
            for view in views {
                view.update(cx, |view, cx| view.rearm_unanswered(cx));
            }
        }
        self.update_seen(window, cx);
        true
    }

    /// Une commande du service du compagnon, traitée sur le fil d'interface.
    fn on_companion_command(&mut self, command: PaneCommand, cx: &mut Context<Self>) {
        let started = Instant::now();
        match command {
            PaneCommand::Input {
                session,
                pane,
                input,
                command_id,
                received_at,
                reply,
            } => self.on_remote_input(
                RemoteRequest {
                    session,
                    pane,
                    input,
                    command_id,
                    received_at,
                    started,
                },
                reply,
                cx,
            ),
            PaneCommand::Ack {
                session,
                pane,
                origin,
                reply,
            } => {
                let found = match self.remote_pane(&session, pane, cx) {
                    Some((_, view)) => {
                        view.update(cx, |pane, cx| pane.acknowledge(cx));
                        true
                    }
                    None => false,
                };
                if found && origin == AckOrigin::Phone {
                    self.companion.send(ServiceEvent::PhoneInteraction {
                        session: session.clone(),
                    });
                }
                hud::log_event(&format!(
                    "companion ack session={} pane={pane} main_thread_us={} origin={}",
                    self.session_log_label(&session),
                    started.elapsed().as_micros(),
                    match origin {
                        AckOrigin::Phone => "phone",
                        AckOrigin::Watched => "watched",
                    }
                ));
                let _ = reply.send(found);
            }
            PaneCommand::Probe {
                session,
                pane,
                reply,
            } => {
                let probe = self.remote_pane(&session, pane, cx).map(|(_, view)| {
                    let view = view.read(cx);
                    PaneProbe {
                        accepts_input: view.accepts_remote_input(),
                        state: state_label(view.state()),
                    }
                });
                let _ = reply.send(probe);
            }
        }
    }

    /// La session `session` (nom tmux) et son pane `pane`, dans ses `slots` : pas seulement l'actif, cachés compris.
    fn remote_pane(
        &self,
        session: &str,
        pane: u32,
        cx: &App,
    ) -> Option<(Entity<SessionView>, Entity<PaneView>)> {
        let view = self.entry_view(session)?;
        let pane_view = view.read(cx).remote_pane(PaneId(pane))?;
        Some((view, pane_view))
    }

    /// Une saisie du téléphone. Les refus se prennent dans l'ordre de `input::gate` ; seule une saisie qui passe tout est
    /// retenue comme tapée (un refus ne fait pas passer le même `command_id` rejoué pour un succès). Le texte ou la
    /// touche partent d'abord, l'Entrée demandée 80 ms plus tard par une tâche : la réponse attend l'Entrée.
    fn on_remote_input(
        &mut self,
        request: RemoteRequest,
        reply: tokio::sync::oneshot::Sender<InputOutcome>,
        cx: &mut Context<Self>,
    ) {
        let RemoteRequest {
            session,
            pane,
            input,
            command_id,
            received_at,
            started,
        } = request;
        // Le nom vient de l'URL : il n'entre au journal qu'une fois la session résolue.
        let label = self.session_log_label(&session);
        let (outcome, delayed_enter) = match self.entry_view(&session) {
            None => (InputOutcome::UnknownSession, None),
            Some(view) => match view.read(cx).remote_pane(PaneId(pane)) {
                None => (InputOutcome::NoPane, None),
                Some(pane_view) => {
                    let client_exited = view.read(cx).control_client_exited();
                    let (read_only, command_running, link_down) = {
                        let pane = pane_view.read(cx);
                        (
                            pane.is_read_only(),
                            pane.command_running(),
                            pane.link_is_down(),
                        )
                    };
                    let refused = gate(Gate {
                        client_exited,
                        link_down,
                        expired: is_expired(received_at, started),
                        duplicate: self.companion.command_typed_recently(&command_id, started),
                        read_only,
                        command_running,
                    });
                    match refused {
                        Some(outcome) => (outcome, None),
                        None => {
                            let (sent, enter) = pane_view.update(cx, |pane, _| match &input {
                                RemoteInput::Key { key, enter } => {
                                    (pane.send_remote_key(*key), *enter)
                                }
                                RemoteInput::Text { text, enter } => {
                                    (pane.send_remote_text(text), *enter)
                                }
                            });
                            if sent {
                                // Retenu seulement une fois l'écriture faite : un échec rejoué ne passe pas pour un doublon.
                                self.companion.remember_command(&command_id, started);
                                (
                                    InputOutcome::Sent,
                                    enter.then(|| (pane_view.downgrade(), view.downgrade())),
                                )
                            } else {
                                (InputOutcome::NoProgram, None)
                            }
                        }
                    }
                }
            },
        };
        let main_thread_us = started.elapsed().as_micros();
        if outcome == InputOutcome::Sent {
            self.companion
                .send(ServiceEvent::PhoneInteraction { session });
        }
        let Some((pane_view, session_view)) = delayed_enter else {
            log_input(&label, pane, outcome, main_thread_us);
            let _ = reply.send(outcome);
            return;
        };
        // L'Entrée part 80 ms après la première écriture, et seulement si le téléphone peut encore apprendre qu'elle est
        // partie. L'issue (journal et réponse HTTP) se décide ici, une fois l'Entrée tapée ou non.
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(ENTER_DELAY).await;
            let outcome = if type_delayed_enter(&pane_view, &session_view, received_at, cx) {
                InputOutcome::Sent
            } else {
                InputOutcome::SentWithoutEnter
            };
            log_input(&label, pane, outcome, main_thread_us);
            let _ = reply.send(outcome);
        })
        .detach();
    }

    /// Le nom de session d'une requête, tel qu'il peut figurer au journal : `?` tant qu'aucune session ne le porte.
    fn session_log_label(&self, session: &str) -> String {
        if self.entry_index(session).is_some() {
            key_label(session)
        } else {
            "?".to_owned()
        }
    }

    /// La section `[ideation]` vient de disparaître alors qu'un bouillon est sélectionné : la première session de l'ordre
    /// de navigation prend la main (comme à la fermeture du dernier bouillon), sinon l'écran vide revient.
    fn leave_hidden_ideation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = self.fallback_for_hidden_selection() else {
            return;
        };
        // Comme `remove_entry` : la galerie d'Artefacts reste au centre, la voisine passe derrière elle.
        let keep_gallery = self.artifacts_shown;
        match next {
            Some(name) => match neighbor_handoff(keep_gallery) {
                NeighborHandoff::Select => self.select(&name, window, cx),
                NeighborHandoff::BehindGallery => self.select_behind_gallery(&name, window, cx),
            },
            None => {
                self.menu = None;
                self.state_mut().ui.selected_session = None;
                self.save();
                if !keep_gallery {
                    window.focus(&self.focus_handle, cx);
                }
                self.sync_branch(cx);
                self.sync_right_panel(window, cx);
            }
        }
    }

    /// Quand la section `[ideation]` est absente et qu'un bouillon est sélectionné, ce qui doit prendre sa place :
    /// `Some(Some(nom))` la première session de l'ordre de navigation (comme à la fermeture du dernier bouillon),
    /// `Some(None)` l'écran vide, `None` rien à corriger.
    fn fallback_for_hidden_selection(&self) -> Option<Option<String>> {
        if self.ideation().is_some() || !self.selected_is_ideation() {
            return None;
        }
        let summaries = self.summaries();
        Some(self.navigation(&summaries).into_iter().next())
    }

    /// Au démarrage, `state.json` peut encore désigner un bouillon alors que la section est absente : la sélection
    /// est corrigée dans `state.json` seulement, avant l'attache des vues (qui affichera la session choisie).
    fn drop_hidden_selection(&mut self) {
        if let Some(next) = self.fallback_for_hidden_selection() {
            hud::log_event("ideation selection dropped reason=disabled");
            self.state_mut().ui.selected_session = next;
            self.save();
        }
    }

    /// La ligne « Installer la commande idema et l'action du Finder » du menu « IdemA » : pose les deux hors du fil
    /// d'interface, puis dit où, ou pourquoi c'est raté.
    fn install_integrations(
        &mut self,
        _: &InstallIntegrations,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(home) = crate::files::panel::home_dir() else {
            return;
        };
        let installing = cx.background_spawn(async move {
            let installed = integrations::install(&home)?;
            if let Err(error) = integrations::refresh_services() {
                hud::log_event(&format!("services refresh failed: {error}"));
            }
            Ok::<_, std::io::Error>(installed)
        });
        cx.spawn_in(window, async move |_, cx| {
            let result = installing.await;
            let _ = cx.update(|window, cx| match result {
                Ok(installed) => {
                    hud::log_event(&format!(
                        "integrations installed cli={} workflow={}",
                        installed.cli.display(),
                        installed.workflow.display()
                    ));
                    alert(
                        window,
                        cx,
                        "Commande et action du Finder installées.",
                        Some(&format!(
                            "Terminal : idema <dossier> ({}).\nFinder : clic droit sur un dossier, Services, Ouvrir dans IdemA.",
                            installed.cli.display()
                        )),
                    );
                }
                Err(error) => {
                    hud::log_event(&format!("integrations install failed: {error}"));
                    alert(
                        window,
                        cx,
                        "L'installation a échoué.",
                        Some(&error.to_string()),
                    );
                }
            });
        })
        .detach();
    }

    /// `⌘,`, la ligne « Personnaliser » et le menu : ouvre la fenêtre « Personnaliser », ou ramène celle qui existe au
    /// premier plan.
    fn open_config(&mut self, _: &OpenConfig, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_on(None, window, cx);
    }

    /// Ouvre « Personnaliser », ou ramène la fenêtre vivante au premier plan, sur `section` quand elle est donnée.
    fn open_settings_on(
        &mut self,
        section: Option<Section>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(handle) = window.window_handle().downcast::<Workspace>() else {
            return;
        };
        let system_enabled = self.panel_project().is_some();
        crate::settings::window::open_or_focus(
            handle,
            cx.entity(),
            system_enabled,
            self.config_path.clone(),
            section,
            cx,
        );
    }

    /// Ce que « Personnaliser » montre du compagnon : l'état du service (`None` tant qu'aucun ne tourne), et, quand la
    /// section est dans la config sans que le service tourne, le mot de la raison que l'espace de travail connaît seul
    /// (le service, qui n'existe pas, ne peut pas la dire). `config.companion` est à jour dès que `sync_companion` a
    /// tourné : une section présente sans service est un échec, pas une attente.
    pub(crate) fn companion_feed(
        &self,
    ) -> (
        Option<tokio::sync::watch::Receiver<ServiceStatus>>,
        Option<&'static str>,
    ) {
        let status = self.companion.status();
        let blocked = if status.is_some() || self.config.companion.is_none() {
            None
        } else if self.server.is_some() {
            Some("thread_spawn_failed")
        } else if self.startup_error.is_some() {
            Some("tmux_unavailable")
        } else {
            // tmux s'amorce encore : le service démarrera avec lui.
            None
        };
        (status, blocked)
    }

    /// Un geste de « Personnaliser » pour le service du compagnon (appairer, oublier, revérifier) ; sans effet éteint.
    pub(crate) fn companion_send(&self, event: ServiceEvent) {
        self.companion.send(event);
    }

    /// Une ligne « Système » de « Personnaliser » : ouvre `path` en onglet permanent dans le volet Fichiers du projet de
    /// contexte. Rend `true` quand le fichier est ouvert, pour que l'appelant ramène la fenêtre devant. Sans projet de
    /// contexte (`panel_project`), ou sans fichier à ce chemin, rien ne s'ouvre, et jamais dans une app externe (le
    /// repli de `open_config_file` n'a pas cours ici) : un fichier absent est seulement journalisé, `open_path` ne
    /// ferait que le confier à l'app par défaut.
    pub(crate) fn open_system_file(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.panel_project().is_none() {
            return false;
        }
        if !path.is_file() {
            hud::log_event(&format!("system file missing path={}", path.display()));
            return false;
        }
        hud::log_event(&format!("system file opened path={}", path.display()));
        self.open_in_files(
            PathTarget::File(FileTarget {
                path: path.to_path_buf(),
                line: None,
                column: None,
            }),
            true,
            window,
            cx,
        );
        true
    }

    /// « Éditer config.toml » : ouvre le fichier dans le volet Fichiers quand un projet de contexte existe (la sauvegarde
    /// automatique et `reload_config` font le reste), sinon dans l'éditeur de texte par défaut. Un bouillon sélectionné
    /// n'a pas de volet Fichiers (`panel_project`) : l'éditeur par défaut s'ouvre.
    pub(crate) fn open_config_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel_project().is_some() {
            let path = self.config_path.clone();
            self.open_in_files(
                PathTarget::File(FileTarget {
                    path,
                    line: None,
                    column: None,
                }),
                false,
                window,
                cx,
            );
            return;
        }
        match Command::new("open")
            .arg("-t")
            .arg(&self.config_path)
            .spawn()
        {
            // `open` rend la main aussitôt : on attend sa fin hors du fil d'interface pour ne pas laisser de zombie.
            Ok(mut child) => {
                std::thread::spawn(move || match child.wait() {
                    Ok(status) if status.success() => hud::log_event("config opened"),
                    Ok(status) => hud::log_event(&format!("config open failed: {status}")),
                    Err(error) => hud::log_event(&format!("config open failed: {error}")),
                });
            }
            Err(error) => hud::log_event(&format!("config open failed: {error}")),
        }
    }

    fn machine_index(&self, connection: &str) -> Option<usize> {
        self.machines
            .iter()
            .position(|machine| machine.entry.connection == connection)
    }

    /// Le serveur des appels brefs d'une session et son nom tmux. Local : le serveur du Mac et la clé. Distant : le
    /// serveur bref de la machine, si elle est connectée.
    fn server_for(&self, key: &str) -> Option<(Arc<TmuxServer>, String)> {
        match route(key) {
            Route::Local(name) => Some((self.server.clone()?, name)),
            Route::Remote { connection, tmux } => {
                let machine = &self.machines[self.machine_index(&connection)?];
                if machine.status != MachineStatus::Connected {
                    return None;
                }
                Some((machine.pool.as_ref()?.brief(), tmux))
            }
        }
    }

    /// Remet la table des serveurs du compagnon d'accord avec l'état des machines : le tmux du Mac une fois l'amorçage fait,
    /// le serveur bref de chaque machine connectée. Une machine qui n'est plus connectée n'y figure plus : le compagnon ne
    /// lit alors rien de ses sessions plutôt que de tomber sur un autre serveur. Rien n'est écrit quand la table est déjà
    /// la bonne.
    fn sync_session_servers(&self) {
        let wanted = wanted_servers(self.server.as_ref(), &self.machines);
        let mut table = self
            .session_servers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unchanged = table.len() == wanted.len()
            && wanted.iter().all(|(machine, server)| {
                table
                    .get(machine)
                    .is_some_and(|held| Arc::ptr_eq(held, server))
            });
        if !unchanged {
            *table = wanted;
        }
    }

    /// Un canal long (client de contrôle) pour une session, et son nom tmux. Local : un bail sans pool qui porte le
    /// serveur du Mac. Distant : un bail du pool de la machine, si elle est connectée.
    fn long_lived_server_for(&mut self, key: &str) -> Option<(Lease, String)> {
        match route(key) {
            Route::Local(name) => Some((Lease::unpooled(self.server.clone()?), name)),
            Route::Remote { connection, tmux } => {
                let index = self.machine_index(&connection)?;
                let machine = &mut self.machines[index];
                if machine.status != MachineStatus::Connected {
                    return None;
                }
                Some((machine.pool.as_mut()?.long_lived(), tmux))
            }
        }
    }

    /// Lance `work` hors du fil d'interface sur le serveur de la session `key`, avec son nom tmux. Sur une machine, les
    /// appels ponctuels passent un à un (le second canal bref autorisé, jamais plus). `None` sans serveur utilisable :
    /// local absent, ou machine non connectée.
    fn spawn_for<T: Send + 'static>(
        &self,
        key: &str,
        cx: &mut Context<Self>,
        work: impl FnOnce(&TmuxServer, &str) -> T + Send + 'static,
    ) -> Option<Task<T>> {
        let (server, tmux) = self.server_for(key)?;
        let turn = match route(key) {
            Route::Local(_) => None,
            Route::Remote { connection, .. } => self
                .machine_index(&connection)
                .map(|index| Arc::clone(&self.machines[index].adhoc)),
        };
        Some(cx.background_spawn(async move {
            let _held = turn.as_ref().map(|turn| {
                turn.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            });
            work(&server, &tmux)
        }))
    }

    /// Aligne les machines sur `[machines]` : une `RemoteMachine` par entrée (l'identité est la connexion, l'ordre celui
    /// du fichier). Une machine retirée détache ses vues (leurs entrées partent, rien n'est envoyé à la machine) ; une
    /// machine ajoutée passe à `Connecting` et se connecte.
    fn sync_machines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted: Vec<_> = self.config.remote_machines().to_vec();
        let mut previous = std::mem::take(&mut self.machines);
        let mut added = Vec::new();
        for entry in wanted {
            match previous
                .iter()
                .position(|machine| machine.entry.connection == entry.connection)
            {
                Some(at) => {
                    let mut machine = previous.remove(at);
                    // Le label peut changer sans que la machine change.
                    machine.entry = entry;
                    self.machines.push(machine);
                }
                None => {
                    added.push(entry.connection.clone());
                    self.machines.push(RemoteMachine::new(entry));
                }
            }
        }
        for gone in previous {
            let connection = gone.entry.connection.clone();
            hud::log_event(&format!("machine removed machine={connection}"));
            self.remote_agents
                .retain(|key, _| machine_of(key) != connection);
            for key in self.machine_entry_keys(&connection) {
                self.remove_entry(&key, window, cx);
            }
            // Son shell du volet Terminal part avec elle : la vue lâchée ferme le PTY et rend le bail.
            self.terminal.remove_machine(&connection);
            self.pending_terminals.remove(&connection);
            // Les tâches de la machine (relecture, essai) tombent avec elle ; ses vues, déjà retirées, lâchent leurs
            // clients hors du fil d'interface.
            drop(gone);
        }
        self.sync_session_servers();
        for connection in added {
            self.connect_machine(&connection, window, cx);
        }
        // Le volet Terminal ouvert sur le shell d'une machine retirée retombe sur le projet, ou se ferme.
        self.sync_right_panel(window, cx);
        if !self.machines.is_empty() {
            self.start_wake_clock(window, cx);
            self.start_network_watch(window, cx);
        }
        cx.notify();
    }

    /// L'observateur du chemin réseau du Mac (spec ssh-tmux §3.2) : au retour du réseau, chaque machine non connectée se
    /// reconnecte tout de suite, comme avec « Réessayer ». L'observateur tourne sur sa file `dispatch` ; seul le message
    /// de retour atteint le fil d'interface.
    fn start_network_watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._network_watch.is_some() {
            return;
        }
        let (sender, mut receiver) = futures::channel::mpsc::unbounded::<()>();
        let monitor = NetworkMonitor::start(sender);
        let task = cx.spawn_in(window, async move |this, cx| {
            while receiver.next().await.is_some() {
                let alive = this.update_in(cx, |this, window, cx| {
                    this.on_network_back(window, cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        self._network_watch = Some((monitor, task));
    }

    /// Le réseau est revenu : les machines qui ne sont pas connectées réessaient sans attendre leur délai.
    fn on_network_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        hud::log_event("network path satisfied");
        let offline: Vec<String> = self
            .machines
            .iter()
            .filter(|machine| machine.status != MachineStatus::Connected)
            .map(|machine| machine.entry.connection.clone())
            .collect();
        for connection in offline {
            self.retry_machine(&connection, window, cx);
        }
    }

    /// Une minuterie de `WAKE_TICK` compare l'horloge murale à l'horloge monotone : une murale en avance de plus de 10 s sur
    /// la monotone (qui ne compte pas la veille) est un réveil du Mac (`is_wake_jump`), et toutes les machines se
    /// reconnectent tout de suite (spec ssh-tmux §3.2). Une minuterie en retard fait avancer les deux horloges autant.
    fn start_wake_clock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._wake_clock.is_some() {
            return;
        }
        let mut last = (SystemTime::now(), Instant::now());
        self._wake_clock = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(WAKE_TICK).await;
                let now = (SystemTime::now(), Instant::now());
                let woke = is_wake_jump(last.0, now.0, now.1.saturating_duration_since(last.1));
                last = now;
                let alive = this.update_in(cx, |this, window, cx| {
                    if woke {
                        this.on_wake(window, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    /// Le Mac sort de veille : ses connexions ssh sont mortes sans qu'aucun client ne l'ait vu.
    fn on_wake(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        hud::log_event("wake detected");
        let connections: Vec<String> = self
            .machines
            .iter()
            .map(|machine| machine.entry.connection.clone())
            .collect();
        for connection in connections {
            self.recycle_machine(&connection, window, cx);
        }
    }

    /// Ferme les maîtres ssh de la machine (`ssh -O exit`, hors du fil d'interface), puis la reconnecte tout de suite. Les
    /// vues gardent leurs entrées : la reconnexion les rattache (`resume_machine_views`). Le compte des essais repart de 0.
    fn recycle_machine(&mut self, connection: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.machine_index(connection) else {
            return;
        };
        let machine = &mut self.machines[index];
        let masters = machine
            .pool
            .as_ref()
            .map(SlotPool::master_specs)
            .unwrap_or_default();
        let was_connected = machine.status == MachineStatus::Connected;
        machine.status = MachineStatus::Connecting;
        machine.reconnecting |= was_connected;
        machine.poll = None;
        machine.retry = None;
        machine.attempt = 0;
        machine.refresh_failures = 0;
        machine.refreshing = false;
        machine.refresh_again = false;
        machine.refresh_started = None;
        self.sync_session_servers();
        if was_connected {
            self.set_machine_link(connection, true, cx);
        }
        // Un maître d'avant la veille ne répond plus : la nouvelle sonde passerait par lui. Il est fermé d'abord.
        let closing = cx.background_spawn(async move {
            for spec in &masters {
                close_master(spec);
            }
        });
        let target = connection.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            closing.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.connect_machine(&target, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// Sonde la machine hors du fil d'interface, puis l'installe `Connected` (ou `Failed`, avec un nouvel essai après
    /// `retry_delay`). Un seul sondage en vol par machine. Le dossier de contrôle est vérifié avant chaque connexion.
    fn connect_machine(&mut self, connection: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.machine_index(connection) else {
            return;
        };
        let machine = &mut self.machines[index];
        if machine.probing {
            return;
        }
        machine.probing = true;
        machine.retry = None;
        // SAFETY: `geteuid` n'a aucune précondition.
        let uid = unsafe { libc::geteuid() };
        // Le même dossier de contrôle que « Tester » des réglages : ils partagent le maître `%C-0`.
        let spec = probe_spec(connection, &self.dir, uid, |name| std::env::var(name).ok());
        let work = cx.background_spawn(async move {
            ensure_control_dir(&spec.control_dir).map_err(|error| {
                hud::log_event(&format!(
                    "ssh control dir unusable {}: {error}",
                    spec.control_dir.display()
                ));
                RemoteFailure::ControlDir
            })?;
            let facts = probe(&spec)?;
            // Les deux options de serveur permises, puis les fenêtres épinglées : celles qu'un IdemA tombé a laissées sont
            // libérées par `release_stale_pins`, une fois la session affichée connue au moment où la sonde rend (et non
            // à son départ : on a pu changer de session pendant).
            let server = TmuxServer::remote(RemoteSpec {
                home: facts.home.clone(),
                shell: facts.shell.clone(),
                ..spec.clone()
            });
            set_extended_keys(&server);
            let pinned = list_pinned_windows(&server);
            // Le pid du maître ssh, noté au journal : les scripts d'acceptation le tuent pour simuler une coupure.
            let master = master_pid(&spec);
            Ok((spec, facts, pinned, master))
        });
        let connection = connection.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let outcome = work.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.on_probed(&connection, outcome, window, cx);
            });
        })
        .detach();
    }

    /// Le sondage a rendu : la machine est connectée (pool créé la première fois, faits mis à jour ensuite) ou échoue.
    fn on_probed(
        &mut self,
        connection: &str,
        outcome: Result<(RemoteSpec, MachineFacts, String, Option<u32>), RemoteFailure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.machine_index(connection) else {
            return;
        };
        let machine = &mut self.machines[index];
        machine.probing = false;
        match outcome {
            Ok((spec, facts, pinned, master)) => {
                match machine.pool.as_mut() {
                    Some(pool) => pool.update_facts(facts.home.clone(), facts.shell.clone()),
                    None => {
                        machine.pool = Some(SlotPool::new(RemoteSpec {
                            home: facts.home.clone(),
                            shell: facts.shell.clone(),
                            ..spec
                        }));
                    }
                }
                machine.facts = Some(facts);
                machine.status = MachineStatus::Connected;
                machine.last_failure = None;
                machine.reconnecting = false;
                machine.attempt = 0;
                machine.refresh_failures = 0;
                machine.retry = None;
                let master = master.map_or_else(|| "?".to_owned(), |pid| pid.to_string());
                hud::log_event(&format!(
                    "machine connected machine={connection} master_pid={master}"
                ));
                self.release_stale_pins(connection, &pinned, cx);
                self.start_machine_poll(connection, window, cx);
                self.refresh_machine(connection, window, cx);
                self.resume_machine_views(connection, window, cx);
            }
            Err(failure) => {
                let delay = retry_delay(machine.attempt);
                machine.attempt += 1;
                hud::log_event(&format!(
                    "machine failed machine={connection} reason={} retry_s={}",
                    failure.short(),
                    delay.as_secs()
                ));
                machine.last_failure = Some(failure.clone());
                machine.status = MachineStatus::Failed(failure);
                machine.poll = None;
                let target = connection.to_owned();
                machine.retry = Some(cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(delay).await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        if let Some(index) = this.machine_index(&target) {
                            this.machines[index].status = MachineStatus::Connecting;
                        }
                        this.connect_machine(&target, window, cx);
                        cx.notify();
                    });
                }));
            }
        }
        // Le compagnon lit les sessions d'une machine tant qu'elle est connectée, pas après.
        self.sync_session_servers();
        cx.notify();
    }

    /// Libère les épingles de taille que la machine porte et qu'aucune vue d'IdemA ne tient : celles d'un IdemA tombé.
    /// Seule est épargnée la fenêtre visible de la session que cette instance affiche à cet instant (la vue la repose à la
    /// reconnexion) ; une autre fenêtre épinglée de la même session est périmée.
    fn release_stale_pins(&mut self, connection: &str, listing: &str, cx: &mut Context<Self>) {
        let spared = match self.selected_name().map(route) {
            Some(Route::Remote {
                connection: shown,
                tmux,
            }) if shown == connection => self
                .selected_view()
                .and_then(|view| view.read(cx).visible_window())
                .map(|window| (tmux, window)),
            _ => None,
        };
        let stale = windows_to_unpin(
            listing,
            spared
                .as_ref()
                .map(|(session, window)| (session.as_str(), *window)),
        );
        if stale.is_empty() {
            return;
        }
        let Some(index) = self.machine_index(connection) else {
            return;
        };
        let machine = &self.machines[index];
        let Some(server) = machine.pool.as_ref().map(SlotPool::brief) else {
            return;
        };
        let turn = Arc::clone(&machine.adhoc);
        cx.background_spawn(async move {
            let _held = turn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            release_windows(&server, &stale);
        })
        .detach();
    }

    /// La relecture toutes les `REMOTE_POLL` d'une machine, tant qu'elle est connectée (`refresh_machine` ne fait rien
    /// sinon). La tâche vit dans la machine : la retirer l'arrête.
    fn start_machine_poll(
        &mut self,
        connection: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.machine_index(connection) else {
            return;
        };
        let target = connection.to_owned();
        self.machines[index].poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(REMOTE_POLL).await;
                let alive = this.update_in(cx, |this, window, cx| {
                    this.refresh_machine(&target, window, cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    /// Relit `list-sessions` et `list-panes` de la machine sur son canal bref, hors du fil d'interface, puis met la
    /// barre d'accord (`apply_sessions`). Une seule relecture en vol : une demande qui arrive pendant l'une repart
    /// aussitôt après, sans empiler les canaux.
    fn refresh_machine(&mut self, connection: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.machine_index(connection) else {
            return;
        };
        let machine = &mut self.machines[index];
        if machine.status != MachineStatus::Connected {
            return;
        }
        if machine.refreshing {
            machine.refresh_again = true;
            // Une relecture qui ne rend pas : ssh ne dira la coupure qu'après ses 30 s, la saisie est coupée dès maintenant.
            if link_stalled(machine.refresh_started, Instant::now()) {
                self.set_machine_link(connection, true, cx);
            }
            return;
        }
        let Some(server) = machine.pool.as_ref().map(SlotPool::brief) else {
            return;
        };
        machine.refreshing = true;
        machine.refresh_started = Some(Instant::now());
        let snapshot = self.machine_entry_keys(connection);
        let listing =
            cx.background_spawn(async move { (server.remote_sessions(), server.panes()) });
        let target = connection.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let listing = listing.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.on_machine_listing(&target, snapshot, listing, window, cx);
            });
        })
        .detach();
    }

    fn on_machine_listing(
        &mut self,
        connection: &str,
        snapshot: Vec<String>,
        listing: (
            Result<Vec<ListedSession>, ServerError>,
            Result<Vec<ListedPane>, ServerError>,
        ),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.machine_index(connection) else {
            return;
        };
        let machine = &mut self.machines[index];
        machine.refreshing = false;
        machine.refresh_started = None;
        let again = std::mem::take(&mut machine.refresh_again);
        // La machine a été perdue pendant la relecture : son résultat est périmé, la reconnexion relira.
        if machine.status != MachineStatus::Connected {
            return;
        }
        match listing {
            (Ok(sessions), Ok(panes)) => {
                machine.refresh_failures = 0;
                // La machine répond : les clients encore vivants reprennent la saisie.
                self.live_clients_resume(connection, cx);
                let (sessions, panes) = qualify_listing(connection, sessions, panes);
                // Les agents des panes de cette machine remplacent ceux du listage précédent.
                self.remote_agents
                    .retain(|key, _| machine_of(key) != connection);
                self.remote_agents.extend(pane_agent_names(&panes));
                self.apply_sessions(connection, &snapshot, &sessions, &panes, window, cx);
                self.reselect_remote_selection(connection, window, cx);
            }
            (Err(error), _) | (_, Err(error)) => {
                machine.refresh_failures = machine.refresh_failures.saturating_add(1);
                // Une ligne par série d'échecs, pas une toutes les 2 s.
                if machine.refresh_failures == 1 {
                    hud::log_event(&format!(
                        "session listing failed machine={connection}: {error}"
                    ));
                }
                if machine.refresh_failures >= REMOTE_LISTING_FAILURES {
                    self.lose_machine(connection, &error.to_string(), window, cx);
                    return;
                }
            }
        }
        if again {
            self.refresh_machine(connection, window, cx);
        }
    }

    /// Au premier listage de la machine de la session distante mémorisée au lancement : la resélectionne si elle existe
    /// encore et si Romain n'a rien choisi d'autre depuis (`should_reselect`). Un seul essai, que la session soit là ou non :
    /// une session qui apparaîtrait plus tard sous le même nom n'est pas celle que Romain regardait.
    fn reselect_remote_selection(
        &mut self,
        connection: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.reselect_saved.clone() else {
            return;
        };
        if machine_of(&saved) != connection || !self.bootstrapped {
            return;
        }
        self.reselect_saved = None;
        let connected: Vec<String> = self.machine_entry_keys(connection);
        if let Some(key) = should_reselect(Some(&saved), self.chosen_since_launch, &connected) {
            hud::log_event(&format!("selection restored session={}", key_label(&key)));
            self.select(&key, window, cx);
        }
    }

    /// La connexion à la machine est perdue (relecture ou contrôle de session en échec, pas une absence de session) :
    /// elle repasse en `Connecting` et se re-sonde. Ses entrées restent dans la barre ; leurs vues sont reprises à la
    /// reconnexion (`resume_machine_views`).
    fn lose_machine(
        &mut self,
        connection: &str,
        reason: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.machine_index(connection) else {
            return;
        };
        let machine = &mut self.machines[index];
        if machine.status != MachineStatus::Connected {
            return;
        }
        hud::log_event(&format!(
            "machine lost machine={connection}: {}",
            log_safe(reason)
        ));
        machine.status = MachineStatus::Connecting;
        // La barre dit « Connexion… » dès la perte, jusqu'au premier résultat de la reprise.
        machine.reconnecting = true;
        machine.poll = None;
        machine.refresh_failures = 0;
        // Une relecture partie avant la perte ne rendra peut-être jamais : la reconnexion en lance une neuve.
        machine.refreshing = false;
        machine.refresh_again = false;
        machine.refresh_started = None;
        self.sync_session_servers();
        // Plus rien n'arrive à la machine : un client ssh peut rester vivant 30 s après une vraie coupure, la saisie ne doit
        // pas s'y accumuler (`SessionView::set_link_down`).
        self.set_machine_link(connection, true, cx);
        cx.notify();
        self.connect_machine(connection, window, cx);
    }

    /// Sortie propre d'IdemA : chaque session distante qu'il affichait rend sa taille à tmux (spec ssh-tmux §3.3). Un
    /// plantage ne passe pas ici : l'épingle reste, et la connexion suivante de la machine la retire (`settle_server`).
    fn release_size_pins(&self, cx: &mut Context<Self>) {
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.summary.machine.is_some())
        {
            if let Some(view) = &entry.view {
                view.update(cx, |view, _| view.release_size_pin());
            }
        }
    }

    /// La machine répond : les vues dont le client est encore vivant reprennent la saisie. Celles dont le client est sorti
    /// attendent leur reconnexion.
    fn live_clients_resume(&self, connection: &str, cx: &mut Context<Self>) {
        for key in self.machine_entry_keys(connection) {
            if let Some(view) = self.entry_view(&key)
                && !view.read(cx).control_client_exited()
            {
                view.update(cx, |view, cx| view.set_link_down(false, cx));
            }
        }
    }

    /// Lève ou baisse le drapeau « lien tombé » de toutes les vues de la machine.
    fn set_machine_link(&self, connection: &str, down: bool, cx: &mut Context<Self>) {
        for key in self.machine_entry_keys(connection) {
            if let Some(view) = self.entry_view(&key) {
                view.update(cx, |view, cx| view.set_link_down(down, cx));
            }
        }
    }

    /// La machine vient de se (re)connecter : les entrées sans vue s'attachent, celles dont le client est tombé se
    /// reconnectent en place (`reattach_later`). Un client vivant n'est pas touché.
    fn resume_machine_views(
        &mut self,
        connection: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for key in self.machine_entry_keys(connection) {
            let needs = match self.entry_view(&key) {
                Some(view) => view.read(cx).control_client_exited(),
                None => true,
            };
            if needs {
                self.reattach_later(&key, window, cx);
            } else if let Some(view) = self.entry_view(&key) {
                // Un client resté vivant pendant un sondage raté : la machine répond, la saisie repart.
                view.update(cx, |view, cx| view.set_link_down(false, cx));
            }
        }
    }

    /// Attache le client de contrôle de la session `name` et abonne l'espace de travail à ses événements.
    /// `Some(grid)` pour la session affichée, `None` pour une session de fond. `launches` sont les commandes d'agent
    /// à taper, une par pane, quand le pane est prêt (session neuve ou restaurée seulement).
    fn attach(
        &mut self,
        name: &str,
        size: Option<GridSize>,
        launches: Vec<PaneLaunch>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), ServerError> {
        let Some(index) = self.entry_index(name) else {
            return Ok(());
        };
        // Un canal long : le bail de la machine (ou, en local, le serveur du Mac). Une machine non connectée n'a rien à
        // offrir : sa reconnexion rattachera ses sessions (`resume_machine_views`).
        let Some((lease, tmux)) = self.long_lived_server_for(name) else {
            return Ok(());
        };
        let entry = &self.entries[index];
        let remote = lease.server.is_remote();
        let info = SessionEntryInfo {
            name: tmux,
            project: entry.summary.project.clone(),
            preset: entry.preset,
            shell: lease.server.remote_spec().map(|spec| spec.shell.clone()),
            auto_layout: entry.auto_layout,
            remote,
        };
        let view = SessionView::spawn(lease, &info, launches, size, window, cx)?;
        let subscription = cx.subscribe_in(&view, window, Self::on_session_event);
        let now = SystemTime::now();
        let entry = &mut self.entries[index];
        entry.view = Some(view);
        entry._subscription = Some(subscription);
        // Une vue neuve repart de zéro : l'état de la précédente ne survit pas à son remplacement.
        let left = entry
            .enter_state(ActivityState::Idle, now)
            .map(|previous| (entry.summary.name.clone(), previous));
        if let Some((session, previous)) = left {
            // La vue neuve repart au repos : ce n'est pas la fin d'une période « en cours ».
            self.companion.record_reset(&session, previous, now);
        }
        self.refresh_badge(cx);
        Ok(())
    }

    /// Attache toutes les sessions listées : la sélectionnée d'abord, à la taille de la zone centrale, puis les
    /// autres sans taille (elles gardent celle de leur pane jusqu'à leur affichage, et un agent n'a pas à se
    /// redessiner au lancement d'Idema).
    fn attach_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.attach_sessions(HashMap::new(), window, cx);
    }

    /// `attach_all`, avec les commandes de lancement par session (une restauration en a ; le démarrage ordinaire, aucune).
    fn attach_sessions(
        &mut self,
        mut launches: HashMap<String, Vec<PaneLaunch>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self
            .selected_entry()
            .map(|entry| entry.summary.name.clone());
        let mut names: Vec<String> = self
            .entries
            .iter()
            .map(|entry| entry.summary.name.clone())
            .collect();
        if let Some(selected) = &selected
            && let Some(at) = names.iter().position(|name| name == selected)
        {
            names.swap(0, at);
        }
        for name in names {
            // Une session créée entre-temps a déjà sa vue (et sa commande de lancement en attente).
            if self
                .entry_index(&name)
                .is_some_and(|index| self.entries[index].view.is_some())
            {
                continue;
            }
            let step = Instant::now();
            let size = (selected.as_deref() == Some(name.as_str()))
                .then(|| self.central_grid(window, self.stored_gaps(&name)));
            let own = launches.remove(&name).unwrap_or_default();
            if let Err(error) = self.attach(&name, size, own, window, cx) {
                hud::log_event(&format!(
                    "attach failed session={}: {error}",
                    key_label(&name)
                ));
                // Une attache qui échoue suit les mêmes essais qu'un client tombé (écart E34).
                self.on_attach_lost(&name, error.to_string(), None, window, cx);
            }
            hud::log_step(&format!("attach session={}", key_label(&name)), step);
        }
        self.show_selected(window, cx);
    }

    /// Montre la vue de la session sélectionnée et lui donne le focus ; sans vue, le focus revient à l'espace de travail
    /// pour que les raccourcis continuent de répondre.
    fn show_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Un menu ne reste jamais ouvert sans le focus : la session affichée le reprend.
        self.menu = None;
        // La galerie couvre le centre : la session reste cachée (`show_artifacts`), le focus va à la galerie.
        if !self.artifacts_shown
            && let Some(view) = self.selected_view()
        {
            view.update(cx, |view, cx| view.set_displayed(true, window, cx));
        }
        match self.focus_target(cx) {
            Some(handle) => window.focus(&handle, cx),
            None => window.focus(&self.focus_handle, cx),
        }
        self.update_seen(window, cx);
        cx.notify();
    }

    /// Une session est « vue » quand elle est au centre dans la fenêtre active ; les autres ne le sont jamais. Volet
    /// droit agrandi (Terminal ou Fichiers), la colonne principale n'est plus à l'écran : aucune ne l'est (spec §5.3).
    fn update_seen(&mut self, window: &Window, cx: &mut Context<Self>) {
        // La galerie couvre le centre : aucune session n'est vue, comme sous un volet agrandi.
        // Compagnon allumé et Romain absent : aucune session n'est vue, pour qu'une session laissée au centre puisse
        // passer en « attend ». Éteint, `present()` est toujours vrai.
        let active = window.is_window_active()
            && self.companion.present()
            && !self.panel_maximized()
            && !self.artifacts_shown;
        let selected = self.selected_name().map(str::to_owned);
        let views: Vec<(bool, Entity<SessionView>)> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let seen = active && selected.as_deref() == Some(entry.summary.name.as_str());
                entry.view.clone().map(|view| (seen, view))
            })
            .collect();
        for (seen, view) in views {
            view.update(cx, |view, cx| view.set_seen(seen, cx));
        }
        self.publish_fleet(cx);
    }

    /// Le volet droit ouvert est-il agrandi ? Il prend alors la place de la colonne principale.
    fn panel_maximized(&self) -> bool {
        self.terminal.state.is_maximized(self.right_panel)
    }

    /// Change le volet droit (ou le ferme) : l'ancien rend sa place, et l'état « agrandi » repart à la taille normale.
    fn set_right_panel(&mut self, next: Option<RightPanel>) {
        self.terminal.state.switch(self.right_panel, next);
        self.right_panel = next;
    }

    /// Le volet Terminal est-il ouvert ?
    pub(crate) fn terminal_open(&self) -> bool {
        self.right_panel == Some(RightPanel::Terminal)
    }

    /// Le volet Fichiers est-il ouvert ?
    pub(crate) fn files_open(&self) -> bool {
        self.right_panel == Some(RightPanel::Files)
    }

    /// La clé du shell que le volet montre : celui de la machine d'une session distante sélectionnée, celui qu'« Ouvrir
    /// un terminal » a demandé tant que son contexte dure, sinon celui du projet de contexte.
    fn terminal_key(&self) -> Option<TerminalKey> {
        let project = self.context_project();
        let pinned = self.machine_terminal.as_ref().and_then(|pin| {
            pin.machine(
                self.selected_name(),
                project.as_ref(),
                self.machine_index(&pin.connection).is_some(),
            )
        });
        terminal_target(self.selected_machine(), pinned, self.panel_project())
    }

    /// Le nom que l'en-tête du volet Terminal affiche à côté de « Terminal » : le projet, ou le libellé de la machine.
    fn terminal_title(&self, key: &TerminalKey) -> String {
        match key {
            TerminalKey::Project(project) => project::display_name(project),
            TerminalKey::Machine(connection) => self.machine_index(connection).map_or_else(
                || connection.clone(),
                |index| self.machines[index].entry.label.clone(),
            ),
        }
    }

    /// Le shell que le volet montre : celui de `terminal_key`, volet ouvert.
    fn shown_terminal(&self) -> Option<(TerminalKey, Entity<PaneView>)> {
        if !self.terminal_open() {
            return None;
        }
        let key = self.terminal_key()?;
        let view = self.terminal.view_of(&key)?;
        Some((key, view))
    }

    /// Le texte qui remplace le corps d'un volet droit ouvert : « Pas encore disponible » pour un volet de dossier sur
    /// une session distante, « Ouverture… » le temps que le shell d'une machine s'ouvre.
    fn right_panel_notice(&self) -> Option<(RightPanel, &'static str)> {
        if let Some(panel) = unavailable_open_panel(self.selected_is_remote(), self.right_panel) {
            return Some((panel, UNAVAILABLE_ON_REMOTE));
        }
        match self.terminal_open().then(|| self.terminal_key()).flatten() {
            Some(TerminalKey::Machine(connection))
                if self.pending_terminals.contains(&connection) =>
            {
                Some((RightPanel::Terminal, OPENING_TERMINAL))
            }
            _ => None,
        }
    }

    /// Donne le focus au shell du volet, quand il est affiché.
    pub(crate) fn focus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, view)) = self.shown_terminal() {
            window.focus(&view.focus_handle(cx), cx);
        }
    }

    /// `` ⌃` `` et le bouton Terminal : ouvre le volet sur le projet de contexte, ou le ferme. Sans projet de
    /// contexte, rien.
    pub(crate) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if next_right_panel(self.right_panel, RightPanel::Terminal).is_none() {
            self.close_terminal(window, cx);
        } else {
            self.open_terminal(window, cx);
        }
    }

    fn open_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.terminal_key() else {
            return;
        };
        if self.ensure_terminal(&key, window, cx) == TerminalReady::Failed {
            // La demande d'« Ouvrir un terminal » qui n'a pas abouti ne reste pas en attente.
            self.machine_terminal = None;
            return;
        }
        self.menu = None;
        if self.browser_open() {
            hud::log_event("browser closed");
        }
        if self.scratchpad_open() {
            hud::log_event("scratchpad closed");
        }
        if self.project_open() {
            hud::log_event("project closed");
        }
        self.set_right_panel(Some(RightPanel::Terminal));
        self.sync_files_shown(window, cx);
        self.sync_changes_shown(window, cx);
        self.sync_browser_shown(window, cx);
        hud::log_event(&format!("terminal opened {}", terminal_label(&key)));
        self.focus_terminal(window, cx);
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
    }

    /// Ferme le volet (les shells restent) et rend le focus à la session affichée.
    pub(crate) fn close_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.terminal_open() {
            return;
        }
        self.set_right_panel(None);
        // Fermé, le volet repart du contexte à sa prochaine ouverture : « Ouvrir un terminal » ne colle pas.
        self.machine_terminal = None;
        hud::log_event("terminal closed");
        self.restore_focus(window, cx);
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
    }

    /// Ferme le volet droit qui montre un texte à la place de son corps (`right_panel_notice`), et rend le focus à la
    /// session affichée. Le volet fermé ne reprend sa place qu'à la prochaine ouverture.
    pub(crate) fn close_notice_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.right_panel else {
            return;
        };
        self.menu = None;
        self.set_right_panel(None);
        self.sync_browser_shown(window, cx);
        hud::log_event(match panel {
            RightPanel::Terminal => "terminal closed",
            RightPanel::Files => "files closed",
            RightPanel::Changes => "changes closed",
            RightPanel::Browser => "browser closed",
            RightPanel::Project => "project closed",
            RightPanel::Scratchpad => "scratchpad closed",
        });
        self.restore_focus(window, cx);
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
    }

    /// Agrandit le volet qui montre un texte, ou lui rend sa largeur.
    pub(crate) fn toggle_notice_maximized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal.state.toggle_maximized(self.right_panel);
        self.menu = None;
        self.update_seen(window, cx);
        self.sync_keyboard(window, cx);
        cx.notify();
    }

    /// Agrandit le volet à la place de la colonne principale, ou lui rend sa largeur. La session n'est plus
    /// « affichée au centre » tant qu'il est agrandi : `update_seen` la tient pour non vue.
    pub(crate) fn toggle_terminal_maximized(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.terminal_open() {
            return;
        }
        self.terminal.state.toggle_maximized(self.right_panel);
        // Un menu de la colonne principale disparaît avec elle.
        self.menu = None;
        self.update_seen(window, cx);
        self.focus_terminal(window, cx);
        self.sync_keyboard(window, cx);
        cx.notify();
    }

    /// Le shell de `key` existe, sinon il est créé. Un projet : tout de suite, ou `Failed` quand il ne peut pas démarrer
    /// (dossier du projet disparu, pas de serveur tmux). Une machine : l'ouverture passe par un canal bref (le dossier du
    /// pane actif) puis ssh, elle est `Opening` et `finish_machine_terminal` la conclut. Le journal dit chaque échec.
    fn ensure_terminal(
        &mut self,
        key: &TerminalKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TerminalReady {
        if self.terminal.view_of(key).is_some() {
            return TerminalReady::Ready;
        }
        let project = match key {
            TerminalKey::Project(project) => project,
            TerminalKey::Machine(connection) => {
                return self.start_machine_terminal(connection, window, cx);
            }
        };
        let Some(server) = self.server.clone() else {
            hud::log_event(&format!(
                "terminal spawn failed project={}: no tmux server",
                project.display()
            ));
            return TerminalReady::Failed;
        };
        let theme = *cx.global::<Theme>();
        let panel_width = theme.px(self.edge_widths_with(window, true).1);
        let size = initial_grid(window.viewport_size(), panel_width, window, &theme, cx);
        match PaneView::spawn_local(project, server.environment(), size, window, cx) {
            Ok(view) => {
                let subscription = cx.subscribe_in(&view, window, Self::on_terminal_event);
                self.terminal.insert(key.clone(), view, subscription);
                TerminalReady::Ready
            }
            Err(error) => {
                hud::log_event(&format!(
                    "terminal spawn failed project={}: {error}",
                    project.display()
                ));
                TerminalReady::Failed
            }
        }
    }

    /// Ouvre le shell de la machine `connection`. Le dossier de départ est celui du pane actif quand la session
    /// sélectionnée est de cette machine (`#{pane_current_path}`, lu sur un canal bref hors du fil d'interface), sinon le
    /// `$HOME` de la machine.
    fn start_machine_terminal(
        &mut self,
        connection: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TerminalReady {
        if self.pending_terminals.contains(connection) {
            return TerminalReady::Opening;
        }
        let lookup = self
            .selected_entry()
            .filter(|entry| entry.summary.machine.as_deref() == Some(connection))
            .and_then(|entry| {
                let pane = entry.view.as_ref()?.read(cx).active_pane()?;
                Some((entry.summary.name.clone(), pane))
            });
        let Some((session, pane)) = lookup else {
            return self.create_machine_terminal(connection, None, window, cx);
        };
        let target = format!("%{}", pane.0);
        let Some(task) = self.spawn_for(&session, cx, move |server, _| {
            server
                .run(&[
                    "display-message",
                    "-p",
                    "-t",
                    &target,
                    "#{pane_current_path}",
                ])
                .ok()
        }) else {
            return self.create_machine_terminal(connection, None, window, cx);
        };
        self.pending_terminals.insert(connection.to_owned());
        let connection = connection.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let folder = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.pending_terminals.remove(&connection);
                this.finish_machine_terminal(&connection, folder.as_deref(), window, cx);
            });
        })
        .detach();
        TerminalReady::Opening
    }

    /// Le dossier du pane est connu (ou pas) : le shell de la machine naît, et le volet qui l'attendait le montre ou se
    /// ferme si la machine ne l'a pas donné.
    fn finish_machine_terminal(
        &mut self,
        connection: &str,
        folder: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ready = self.create_machine_terminal(connection, folder, window, cx);
        let wanted = self.terminal_open()
            && self.terminal_key() == Some(TerminalKey::Machine(connection.to_owned()));
        if !wanted {
            cx.notify();
        } else if ready == TerminalReady::Ready {
            self.focus_terminal(window, cx);
            self.sync_keyboard(window, cx);
            cx.notify();
        } else {
            self.close_terminal(window, cx);
        }
    }

    /// Crée le shell ssh de la machine `connection` dans `folder` (ou son `$HOME`), sur un canal long de son pool : le
    /// bail reste à la vue tant que le shell vit.
    fn create_machine_terminal(
        &mut self,
        connection: &str,
        folder: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TerminalReady {
        let fail = |reason: &str| {
            hud::log_event(&format!(
                "terminal spawn failed machine={connection}: {reason}"
            ));
            TerminalReady::Failed
        };
        let Some(index) = self.machine_index(connection) else {
            return fail("unknown machine");
        };
        let machine = &mut self.machines[index];
        if machine.status != MachineStatus::Connected {
            return fail("machine not connected");
        }
        let Some(pool) = machine.pool.as_mut() else {
            return fail("no ssh pool");
        };
        let lease = pool.long_lived();
        let Some(home) = lease.server.remote_spec().map(|spec| spec.home.clone()) else {
            return fail("not a remote server");
        };
        let cwd = remote_start_folder(folder, &home);
        let theme = *cx.global::<Theme>();
        let panel_width = theme.px(self.edge_widths_with(window, true).1);
        let size = initial_grid(window.viewport_size(), panel_width, window, &theme, cx);
        match PaneView::spawn_remote_shell(lease, &cwd, size, window, cx) {
            Ok(view) => {
                let subscription = cx.subscribe_in(&view, window, Self::on_terminal_event);
                self.terminal.insert(
                    TerminalKey::Machine(connection.to_owned()),
                    view,
                    subscription,
                );
                hud::log_event(&format!("terminal spawned machine={connection}"));
                TerminalReady::Ready
            }
            Err(error) => fail(&log_safe(&error.to_string())),
        }
    }

    /// « Ouvrir un terminal » de l'en-tête d'une machine : le volet Terminal s'ouvre sur le shell de cette machine, sans
    /// changer de session. Tant que la session et le projet de contexte restent ceux de la demande, le volet garde ce
    /// shell ; un changement de contexte le rend au projet.
    pub(crate) fn open_machine_terminal(
        &mut self,
        connection: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.machine_index(connection).is_none() {
            return;
        }
        self.machine_terminal = Some(MachineTerminalPin {
            connection: connection.to_owned(),
            selected: self.selected_name().map(str::to_owned),
            project: self.context_project(),
        });
        self.open_terminal(window, cx);
    }

    /// Le projet de contexte a peut-être changé : le volet ouvert montre (ou crée) le shell du nouveau, et se ferme
    /// s'il n'y en a plus ou que son shell ne démarre pas. Un shell de machine encore en ouverture garde le volet ouvert.
    fn sync_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.terminal_open() {
            return;
        }
        let Some(key) = self.terminal_key() else {
            self.close_terminal(window, cx);
            return;
        };
        match self.ensure_terminal(&key, window, cx) {
            TerminalReady::Ready => {
                // Agrandi, ou le clavier dans le volet, le shell du nouveau projet prend le focus que `show_selected`
                // n'a pu lui donner avant sa création (l'ancien shell n'est plus à l'écran).
                if self.panel_maximized() || self.panel_keyboard {
                    self.focus_terminal(window, cx);
                }
                cx.notify();
            }
            TerminalReady::Opening => cx.notify(),
            TerminalReady::Failed => self.close_terminal(window, cx),
        }
    }

    /// Le volet Fichiers du projet `project`, créé à la première demande et gardé ensuite.
    fn ensure_files_panel(
        &mut self,
        project: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<FilesPanel> {
        if let Some(panel) = self.files_panels.get(project) {
            return panel.clone();
        }
        let path = project.to_owned();
        let collapsed = self.state().ui.files_tree_collapsed;
        let tree_width = self.state().ui.files_tree_width;
        let panel = cx.new(|cx| FilesPanel::new(path, collapsed, tree_width, window, cx));
        self._files_subscriptions
            .push(cx.subscribe_in(&panel, window, Self::on_files_event));
        self.files_panels.insert(project.to_owned(), panel.clone());
        panel
    }

    /// Le volet Fichiers que l'écran montre : celui du projet de contexte, volet ouvert.
    fn shown_files(&self) -> Option<(PathBuf, Entity<FilesPanel>)> {
        if !self.files_open() {
            return None;
        }
        let project = self.panel_project()?;
        let panel = self.files_panels.get(&project)?.clone();
        Some((project, panel))
    }

    /// Dit à chaque volet Fichiers s'il est celui que l'écran montre : le volet affiché surveille le disque, les
    /// autres sauvegardent leurs onglets et ne gardent la surveillance que s'ils en ont d'ouverts.
    fn sync_files_shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.shown_files().map(|(project, _)| project);
        for (project, panel) in &self.files_panels {
            let is_shown = shown.as_ref() == Some(project);
            panel.update(cx, |panel, cx| panel.set_shown(is_shown, window, cx));
        }
    }

    /// `⌘⇧E` : replie ou déplie l'arbre du volet Fichiers affiché. Sans volet affiché, rien.
    fn toggle_files_tree(&mut self, cx: &mut Context<Self>) {
        if let Some((_, panel)) = self.shown_files() {
            panel.update(cx, |panel, cx| panel.toggle_tree(cx));
        }
    }

    /// Donne le focus au volet Fichiers, quand il est affiché.
    pub(crate) fn focus_files(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, panel)) = self.shown_files() {
            window.focus(&panel.focus_handle(cx), cx);
        }
    }

    /// `⌘P` et le bouton Fichiers : ouvre le volet sur le projet de contexte (celui du Terminal le cède), ou le
    /// ferme. Sans projet de contexte, rien.
    pub(crate) fn toggle_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if next_right_panel(self.right_panel, RightPanel::Files).is_none() {
            self.close_files(window, cx);
        } else {
            self.open_files(window, cx);
        }
    }

    /// `⌘⇧F` : ouvre le volet Fichiers s'il est fermé (le Terminal le cède), puis donne le champ de l'arbre à la
    /// recherche dans le projet. Elle vaut de partout, un pane au focus compris, comme `⌘P`. Sans projet de contexte, rien.
    fn project_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel_project().is_none() {
            return;
        }
        if !self.files_open() {
            self.open_files(window, cx);
        }
        let Some((_, panel)) = self.shown_files() else {
            return;
        };
        panel.update(cx, |panel, cx| panel.start_project_search(window, cx));
        self.sync_keyboard(window, cx);
    }

    fn open_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.panel_project() else {
            return;
        };
        let started = Instant::now();
        self.ensure_files_panel(&project, window, cx);
        self.menu = None;
        // Fichiers prend la place du volet Changes ouvert : Changes se ferme, et le journal le dit.
        if self.changes_open() {
            hud::log_event("changes closed");
        }
        if self.browser_open() {
            hud::log_event("browser closed");
        }
        if self.scratchpad_open() {
            hud::log_event("scratchpad closed");
        }
        if self.project_open() {
            hud::log_event("project closed");
        }
        self.set_right_panel(Some(RightPanel::Files));
        self.sync_files_shown(window, cx);
        self.sync_changes_shown(window, cx);
        self.sync_browser_shown(window, cx);
        hud::log_event(&format!("files opened project={}", project.display()));
        self.focus_files(window, cx);
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
        // AC-51 : `took_ms` est le travail synchrone du geste, `frame_ms` court jusqu'à l'image qui peint le volet
        // (`on_next_frame` part juste après le rendu de la prochaine image).
        let took = started.elapsed();
        window.on_next_frame(move |_, _| {
            hud::log_event(&format!(
                "files open took_ms={} frame_ms={}",
                took.as_millis(),
                started.elapsed().as_millis()
            ));
        });
    }

    /// `⌘`-clic sur un chemin d'un pane (spec §4.7) : ouvre le volet Fichiers sur le projet de contexte (le Terminal le
    /// cède), puis y ouvre le fichier à sa ligne et sa colonne, ou y révèle le dossier. Un fichier hors du projet
    /// s'ouvre quand même (spec §4.5). Sans projet de contexte, rien. `pin` ouvre l'onglet permanent (le dialogue) au
    /// lieu d'un aperçu (le `⌘`-clic) ; un onglet que le fichier avait dans le volet d'un autre projet le rend permanent
    /// aussi (`move_file_tab`).
    fn open_in_files(
        &mut self,
        target: PathTarget,
        pin: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.files_open() {
            self.open_files(window, cx);
        }
        let Some((project, panel)) = self.shown_files() else {
            return;
        };
        match target {
            PathTarget::File(file) => {
                let pin = self.move_file_tab(&project, &file.path, pin, window, cx);
                let position = file.line.map(|line| (line, file.column.unwrap_or(1)));
                panel.update(cx, |panel, cx| {
                    panel.open_path(&file.path, position, pin, window, cx);
                });
            }
            PathTarget::Folder(folder) => {
                // Le focus va à l'arbre, où le dossier est révélé, même si un onglet a son éditeur.
                panel.update(cx, |panel, cx| {
                    panel.reveal(&folder, cx);
                    panel.focus_tree(window, cx);
                });
            }
        }
    }

    /// L'onglet de `path` suit le geste : s'il est ouvert dans le volet d'un autre projet que `project`, il s'y ferme
    /// (son texte en attente est écrit d'abord) pour s'ouvrir dans celui de `project`, seule copie. Rend `pin`, relevé à
    /// vrai si l'onglet fermé était permanent. Un onglet qui ne peut pas se fermer faute d'avoir pu écrire reste où il
    /// est : le fichier s'ouvre quand même, et le texte non sauvegardé n'est pas perdu.
    fn move_file_tab(
        &mut self,
        project: &Path,
        path: &Path,
        pin: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let holders = other_holders(
            project,
            self.files_panels
                .iter()
                .map(|(other, panel)| (other.as_path(), panel.read(cx).has_open(path))),
        );
        let mut pin = pin;
        for from in holders {
            let Some(panel) = self.files_panels.get(&from).cloned() else {
                continue;
            };
            let released = panel.update(cx, |panel, cx| panel.release_file(path, window, cx));
            if let Some(was_pinned) = released {
                pin |= was_pinned;
                hud::log_event(&format!(
                    "files tab moved path={} from={} to={} pinned={pin}",
                    path.display(),
                    from.display(),
                    project.display()
                ));
            }
        }
        pin
    }

    /// Ferme le volet (les volets des projets restent) et rend le focus à la session affichée.
    pub(crate) fn close_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.files_open() {
            return;
        }
        // Un menu de l'arbre ouvert ne survit pas au volet : le focus ne lui revient pas.
        self.menu = None;
        self.set_right_panel(None);
        self.sync_files_shown(window, cx);
        self.sync_changes_shown(window, cx);
        hud::log_event("files closed");
        self.restore_focus(window, cx);
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
    }

    /// Agrandit le volet Fichiers à la place de la colonne principale, ou lui rend sa largeur.
    pub(crate) fn toggle_files_maximized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.files_open() {
            return;
        }
        self.terminal.state.toggle_maximized(self.right_panel);
        // Un menu de la colonne principale disparaît avec elle.
        self.menu = None;
        self.update_seen(window, cx);
        self.focus_files(window, cx);
        self.sync_keyboard(window, cx);
        cx.notify();
    }

    /// Le projet de contexte a peut-être changé : le volet Fichiers ouvert montre (ou crée) celui du nouveau, et se
    /// ferme s'il n'y a plus de projet.
    fn sync_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.files_open() {
            return;
        }
        match self.panel_project() {
            Some(project) => {
                self.ensure_files_panel(&project, window, cx);
                self.sync_files_shown(window, cx);
                self.sync_changes_shown(window, cx);
                if self.panel_maximized() || self.panel_keyboard {
                    self.focus_files(window, cx);
                }
                cx.notify();
            }
            None => self.close_files(window, cx),
        }
    }

    /// Le volet Changes est-il ouvert ?
    pub(crate) fn changes_open(&self) -> bool {
        self.right_panel == Some(RightPanel::Changes)
    }

    /// Le volet Changes du projet `project`, créé à la première demande et gardé ensuite.
    fn ensure_changes_panel(
        &mut self,
        project: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ChangesPanel> {
        if let Some(panel) = self.changes_panels.get(project) {
            return panel.clone();
        }
        let path = project.to_owned();
        let list_width = self.state().ui.changes_list_width;
        let list_hidden = self.state().ui.changes_list_hidden;
        let panel = cx.new(|cx| ChangesPanel::new(path, list_width, list_hidden, window, cx));
        self._changes_subscriptions
            .push(cx.subscribe_in(&panel, window, Self::on_changes_event));
        self.changes_panels
            .insert(project.to_owned(), panel.clone());
        panel
    }

    /// Le volet Changes que l'écran montre : celui du projet de contexte, volet ouvert.
    fn shown_changes(&self) -> Option<(PathBuf, Entity<ChangesPanel>)> {
        if !self.changes_open() {
            return None;
        }
        let project = self.panel_project()?;
        let panel = self.changes_panels.get(&project)?.clone();
        Some((project, panel))
    }

    /// Dit à chaque volet Changes s'il est celui que l'écran montre : le volet affiché surveille le dépôt, les autres non.
    fn sync_changes_shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.shown_changes().map(|(project, _)| project);
        for (project, panel) in &self.changes_panels {
            let is_shown = shown.as_ref() == Some(project);
            panel.update(cx, |panel, cx| panel.set_shown(is_shown, window, cx));
        }
    }

    /// Donne le focus au volet Changes, quand il est affiché.
    pub(crate) fn focus_changes(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, panel)) = self.shown_changes() {
            window.focus(&panel.focus_handle(cx), cx);
        }
    }

    /// `⌘⇧G` et le bouton Changes : ouvre le volet sur le projet de contexte (celui du Terminal ou de Fichiers le cède),
    /// ou le ferme. Sans projet de contexte, rien.
    pub(crate) fn toggle_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if next_right_panel(self.right_panel, RightPanel::Changes).is_none() {
            self.close_changes(window, cx);
        } else {
            self.open_changes(window, cx);
        }
    }

    fn open_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.panel_project() else {
            return;
        };
        let started = Instant::now();
        self.ensure_changes_panel(&project, window, cx);
        self.menu = None;
        // Changes prend la place du volet ouvert : Fichiers se ferme, et le journal le dit.
        if self.files_open() {
            hud::log_event("files closed");
        }
        if self.browser_open() {
            hud::log_event("browser closed");
        }
        if self.scratchpad_open() {
            hud::log_event("scratchpad closed");
        }
        if self.project_open() {
            hud::log_event("project closed");
        }
        self.set_right_panel(Some(RightPanel::Changes));
        self.sync_files_shown(window, cx);
        self.sync_changes_shown(window, cx);
        self.sync_browser_shown(window, cx);
        hud::log_event(&format!("changes opened project={}", project.display()));
        self.focus_changes(window, cx);
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
        // `took_ms` est le travail synchrone du geste, `frame_ms` court jusqu'à l'image qui peint le volet. Un rappel
        // `on_next_frame` part avant le dessin : le second, posé depuis le premier, part une fois la première image dessinée.
        let took = started.elapsed();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |_, _| {
                hud::log_event(&format!(
                    "changes open took_ms={} frame_ms={}",
                    took.as_millis(),
                    started.elapsed().as_millis()
                ));
            });
        });
    }

    /// Ferme le volet (les volets des projets restent) et rend le focus à la session affichée.
    pub(crate) fn close_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.changes_open() {
            return;
        }
        self.menu = None;
        self.set_right_panel(None);
        self.sync_changes_shown(window, cx);
        hud::log_event("changes closed");
        self.restore_focus(window, cx);
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
    }

    /// Agrandit le volet Changes à la place de la colonne principale, ou lui rend sa largeur.
    pub(crate) fn toggle_changes_maximized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.changes_open() {
            return;
        }
        self.terminal.state.toggle_maximized(self.right_panel);
        // Un menu de la colonne principale disparaît avec elle.
        self.menu = None;
        self.update_seen(window, cx);
        self.focus_changes(window, cx);
        self.sync_keyboard(window, cx);
        cx.notify();
    }

    /// Le projet de contexte a peut-être changé : le volet Changes ouvert montre (ou crée) celui du nouveau, et se ferme
    /// s'il n'y a plus de projet.
    fn sync_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.changes_open() {
            return;
        }
        match self.panel_project() {
            Some(project) => {
                self.ensure_changes_panel(&project, window, cx);
                self.sync_changes_shown(window, cx);
                if self.panel_maximized() || self.panel_keyboard {
                    self.focus_changes(window, cx);
                }
                cx.notify();
            }
            None => self.close_changes(window, cx),
        }
    }

    /// Le volet Browser est-il ouvert ?
    pub(crate) fn browser_open(&self) -> bool {
        self.right_panel == Some(RightPanel::Browser)
    }

    /// Le volet Browser, créé à la première demande avec les onglets de `state.json`, et gardé ensuite.
    fn ensure_browser(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<BrowserPanel> {
        if let Some(panel) = &self.browser {
            return panel.clone();
        }
        let saved = self.state().ui.browser_tabs.clone();
        let panel = cx.new(|cx| BrowserPanel::new(saved, window, cx));
        self._browser_subscription = Some(cx.subscribe_in(&panel, window, Self::on_browser_event));
        if let (Some(servers), Some(tmux)) = (self.servers.clone(), self.server.clone()) {
            panel.update(cx, |panel, cx| panel.set_servers(servers, tmux, window, cx));
        }
        self.browser = Some(panel.clone());
        panel
    }

    /// Le volet Browser que l'écran montre : le volet ouvert. Il ne dépend pas du projet de contexte (P4-5).
    pub(crate) fn shown_browser(&self) -> Option<Entity<BrowserPanel>> {
        // Sur une session distante, le volet reste ouvert mais montre « Pas encore disponible » : sa page native est cachée.
        if self.browser_open() && !self.selected_is_remote() {
            self.browser.clone()
        } else {
            None
        }
    }

    /// Dit au volet Browser s'il est celui que l'écran montre : caché, il cache la vue native de ses pages.
    fn sync_browser_shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.shown_browser().is_some();
        if let Some(panel) = &self.browser {
            panel.update(cx, |panel, cx| panel.set_shown(shown, window, cx));
        }
        self.sync_page_cover(window, cx);
    }

    /// Dit au volet Browser quand un menu recouvre sa page (P4-4) : à l'ouverture et à la fermeture du menu, quand le
    /// volet apparaît ou disparaît, et à chaque image pour les gestes qui retirent le menu sans passer par
    /// `close_menu`. Le signal part par `defer` : la sonde a paniqué (ré-entrance) en l'envoyant pendant un `update`.
    fn sync_page_cover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let covered = page_covered(self.menu.is_some(), self.shown_browser().is_some());
        if covered == self.page_covered {
            return;
        }
        self.page_covered = covered;
        if let Some(panel) = self.browser.clone() {
            window.defer(cx, move |window, cx| {
                panel.update(cx, |panel, cx| panel.set_covered(covered, window, cx));
            });
        }
    }

    /// Donne le focus GPUI au volet Browser, quand il est affiché.
    pub(crate) fn focus_browser(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.shown_browser() {
            window.focus(&panel.focus_handle(cx), cx);
        }
    }

    /// `⌘⇧B` et le bouton Browser : ouvre le volet à la place de celui qui est ouvert, ou le ferme.
    pub(crate) fn toggle_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if next_right_panel(self.right_panel, RightPanel::Browser).is_none() {
            self.close_browser(window, cx);
        } else {
            self.open_browser(window, cx);
            // Le geste ouvre le navigateur pour y taper : la page de l'onglet actif prend le clavier dès qu'elle est à
            // l'écran (spec §4.7). Les autres ouvertures (⌘-clic d'un lien) ne la lui donnent pas.
            if let Some(panel) = self.shown_browser() {
                panel.update(cx, |panel, cx| panel.focus_active_page(cx));
            }
        }
    }

    pub(crate) fn open_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_browser_with(true, window, cx);
    }

    /// `take_focus` : le volet prend le focus (⌘⇧B, le bouton). Faux pour le ⌘-clic d'un lien : créer ou restaurer un
    /// onglet ne prend jamais le clavier d'un pane (anti-objectif de la spec), il reste où il était.
    fn open_browser_with(&mut self, take_focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        // Pas de volet derrière l'écran d'échec du démarrage : sa page native resterait peinte par-dessus.
        if self.startup_error.is_some() || self.browser_open() {
            return;
        }
        // Le Navigateur n'existe pas sur une session distante (grisé) : ni le bouton, ni ⌘⇧B, ni un lien ne l'ouvrent.
        if self.selected_is_remote() {
            hud::log_event("browser open ignored remote=true");
            return;
        }
        let started = Instant::now();
        self.ensure_browser(window, cx);
        self.menu = None;
        // Browser prend la place du volet ouvert, et le journal dit lequel se ferme.
        match self.right_panel {
            Some(RightPanel::Files) => hud::log_event("files closed"),
            Some(RightPanel::Changes) => hud::log_event("changes closed"),
            Some(RightPanel::Terminal) => hud::log_event("terminal closed"),
            Some(RightPanel::Scratchpad) => hud::log_event("scratchpad closed"),
            Some(RightPanel::Project) => hud::log_event("project closed"),
            Some(RightPanel::Browser) | None => {}
        }
        // Le volet qui part tenait le clavier (le ⌘-clic d'un lien du Terminal) : son focus GPUI reste sur une vue qui n'est
        // plus affichée et la saisie mourrait là. Le clavier revient à la session.
        let keyboard_leaves = !take_focus && self.panel_keyboard && self.right_panel.is_some();
        self.set_right_panel(Some(RightPanel::Browser));
        self.sync_files_shown(window, cx);
        self.sync_changes_shown(window, cx);
        self.sync_browser_shown(window, cx);
        hud::log_event("browser opened");
        if take_focus {
            self.focus_browser(window, cx);
        } else if keyboard_leaves {
            self.panel_keyboard = false;
            self.restore_focus(window, cx);
        }
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
        // Comme `open_changes` : `took_ms` est le travail synchrone du geste, `frame_ms` court jusqu'à l'image qui peint le
        // volet.
        let took = started.elapsed();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |_, _| {
                hud::log_event(&format!(
                    "browser open took_ms={} frame_ms={}",
                    took.as_millis(),
                    started.elapsed().as_millis()
                ));
            });
        });
    }

    /// Ferme le volet (ses onglets et ses pages restent) et rend le focus à la session affichée.
    pub(crate) fn close_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.browser_open() {
            return;
        }
        self.menu = None;
        self.set_right_panel(None);
        self.sync_browser_shown(window, cx);
        hud::log_event("browser closed");
        self.restore_focus(window, cx);
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
    }

    /// Agrandit le volet Browser à la place de la colonne principale, ou lui rend sa largeur.
    pub(crate) fn toggle_browser_maximized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.browser_open() {
            return;
        }
        self.terminal.state.toggle_maximized(self.right_panel);
        // Un menu de la colonne principale disparaît avec elle.
        self.menu = None;
        self.update_seen(window, cx);
        self.focus_browser(window, cx);
        self.sync_keyboard(window, cx);
        cx.notify();
    }

    /// Le volet Scratchpad est-il ouvert ?
    pub(crate) fn scratchpad_open(&self) -> bool {
        self.right_panel == Some(RightPanel::Scratchpad)
    }

    /// Le volet Scratchpad que l'écran montre : le volet ouvert. Il ne dépend pas du projet de contexte (AC-8).
    fn shown_scratchpad(&self) -> Option<Entity<ScratchpadPanel>> {
        self.scratchpad_open().then(|| self.scratchpad.clone())
    }

    /// Donne le focus à l'éditeur du Scratchpad, quand le volet est affiché.
    pub(crate) fn focus_scratchpad(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.shown_scratchpad() {
            panel.update(cx, |panel, cx| panel.focus_editor(window, cx));
        }
    }

    /// `⌘J` et le bouton Scratchpad : ouvre le volet à la place de celui qui est ouvert (le curseur dans l'éditeur), ou le
    /// ferme. La note est globale : aucun projet de contexte n'est exigé.
    pub(crate) fn toggle_scratchpad(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if next_right_panel(self.right_panel, RightPanel::Scratchpad).is_none() {
            self.close_scratchpad(window, cx);
        } else {
            self.open_scratchpad(window, cx);
        }
    }

    fn open_scratchpad(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scratchpad_open() {
            return;
        }
        self.menu = None;
        // Le Scratchpad prend la place du volet ouvert, et le journal dit lequel se ferme.
        match self.right_panel {
            Some(RightPanel::Files) => hud::log_event("files closed"),
            Some(RightPanel::Changes) => hud::log_event("changes closed"),
            Some(RightPanel::Terminal) => hud::log_event("terminal closed"),
            Some(RightPanel::Browser) => hud::log_event("browser closed"),
            Some(RightPanel::Project) => hud::log_event("project closed"),
            Some(RightPanel::Scratchpad) | None => {}
        }
        self.set_right_panel(Some(RightPanel::Scratchpad));
        self.sync_files_shown(window, cx);
        self.sync_changes_shown(window, cx);
        self.sync_browser_shown(window, cx);
        hud::log_event("scratchpad opened");
        self.focus_scratchpad(window, cx);
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
    }

    /// Ferme le volet (la note, son curseur et sa sauvegarde en attente restent) et rend le focus à la session affichée.
    pub(crate) fn close_scratchpad(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.scratchpad_open() {
            return;
        }
        self.menu = None;
        self.set_right_panel(None);
        hud::log_event("scratchpad closed");
        self.restore_focus(window, cx);
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
    }

    /// Agrandit le volet Scratchpad à la place de la colonne principale, ou lui rend sa largeur.
    pub(crate) fn toggle_scratchpad_maximized(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.scratchpad_open() {
            return;
        }
        self.terminal.state.toggle_maximized(self.right_panel);
        // Un menu de la colonne principale disparaît avec elle.
        self.menu = None;
        self.update_seen(window, cx);
        self.focus_scratchpad(window, cx);
        self.sync_keyboard(window, cx);
        cx.notify();
    }

    /// Écrit ce que la note a en attente (`⌘Q`, fermeture de la fenêtre) : ces chemins sautent la minuterie de sauvegarde.
    fn flush_scratchpad(&mut self, on: &str, cx: &mut Context<Self>) {
        self.scratchpad.update(cx, |panel, cx| panel.flush(on, cx));
    }

    fn on_scratchpad_event(
        &mut self,
        _panel: &Entity<ScratchpadPanel>,
        event: &ScratchpadEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ScratchpadEvent::FocusChanged => self.sync_keyboard(window, cx),
            ScratchpadEvent::CopyToAgent(text) => {
                // Le journal du Scratchpad dit pourquoi rien ne part (AC-5) ; sans session, `paste_to_agent` n'a rien à coller.
                if self.selected_view().is_none() {
                    hud::log_event("scratchpad copy skipped reason=no-session");
                    return;
                }
                self.paste_to_agent(text, window, cx);
            }
            ScratchpadEvent::EditorMenu {
                position,
                has_selection,
            } => self.open_scratchpad_menu(*position, *has_selection, window, cx),
        }
    }

    /// Le menu contextuel de l'éditeur du Scratchpad (clic droit dans le texte) : Couper et Copier exigent une sélection,
    /// « Copier vers l'agent » une sélection et une session. Chaque entrée rejoue son geste sur le volet.
    fn open_scratchpad_menu(
        &mut self,
        position: Point<Pixels>,
        has_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lines = scratchpad_menu(has_selection, self.selected_view().is_some());
        let panel = self.scratchpad.clone();
        let entries = menu_entries(lines, move |gesture, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.run_editor_gesture(gesture, window, cx);
            });
        });
        let spec = MenuSpec {
            title: None,
            entries,
        };
        self.open_menu(position, spec, window, cx);
    }

    /// Le volet Projet est-il ouvert ?
    pub(crate) fn project_open(&self) -> bool {
        self.right_panel == Some(RightPanel::Project)
    }

    /// Le volet Projet que l'écran montre : le projet de contexte, volet ouvert. Sans projet (un bouillon), rien : le volet
    /// se ferme (`sync_project`).
    fn shown_project(&self) -> Option<(PathBuf, Entity<ProjectPanel>)> {
        if !self.project_open() {
            return None;
        }
        let project = self.panel_project()?;
        Some((project, self.project_panel.clone()))
    }

    /// Donne le focus au corps du volet Projet (l'éditeur d'`AGENTS.md`, sinon la racine), quand il est affiché.
    pub(crate) fn focus_project(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, panel)) = self.shown_project() {
            let panel = panel.clone();
            panel.update(cx, |panel, cx| panel.focus_body(window, cx));
        }
    }

    /// Le bouton Projet de la barre d'outils : ouvre le volet sur le projet de contexte (le volet ouvert le cède), ou le
    /// ferme. Sans projet de contexte, rien. Aucun raccourci.
    pub(crate) fn toggle_project_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if next_right_panel(self.right_panel, RightPanel::Project).is_none() {
            self.close_project(window, cx);
        } else {
            self.open_project(window, cx);
        }
    }

    fn open_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.panel_project() else {
            return;
        };
        if self.project_open() {
            return;
        }
        self.menu = None;
        // Le volet Projet prend la place du volet ouvert, et le journal dit lequel se ferme.
        match self.right_panel {
            Some(RightPanel::Files) => hud::log_event("files closed"),
            Some(RightPanel::Changes) => hud::log_event("changes closed"),
            Some(RightPanel::Terminal) => hud::log_event("terminal closed"),
            Some(RightPanel::Browser) => hud::log_event("browser closed"),
            Some(RightPanel::Scratchpad) => hud::log_event("scratchpad closed"),
            Some(RightPanel::Project) | None => {}
        }
        self.set_right_panel(Some(RightPanel::Project));
        self.sync_files_shown(window, cx);
        self.sync_changes_shown(window, cx);
        self.sync_browser_shown(window, cx);
        self.project_panel.update(cx, |panel, cx| {
            panel.show_project(&project, window, cx);
            panel.opened(window, cx);
        });
        hud::log_event(&format!("project opened project={}", project.display()));
        self.focus_project(window, cx);
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
    }

    /// Ferme le volet (l'onglet choisi et les éditeurs restent) et rend le focus à la session affichée.
    pub(crate) fn close_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.project_open() {
            return;
        }
        self.menu = None;
        self.set_right_panel(None);
        hud::log_event("project closed");
        self.restore_focus(window, cx);
        self.sync_keyboard(window, cx);
        self.update_seen(window, cx);
        cx.notify();
    }

    /// Agrandit le volet Projet à la place de la colonne principale, ou lui rend sa largeur.
    pub(crate) fn toggle_project_maximized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.project_open() {
            return;
        }
        self.terminal.state.toggle_maximized(self.right_panel);
        // Un menu de la colonne principale disparaît avec elle.
        self.menu = None;
        self.update_seen(window, cx);
        self.focus_project(window, cx);
        self.sync_keyboard(window, cx);
        cx.notify();
    }

    /// Le projet de contexte a peut-être changé : le volet Projet ouvert montre celui du nouveau, et se ferme s'il n'y en a
    /// plus.
    fn sync_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.project_open() {
            return;
        }
        match self.panel_project() {
            Some(project) => {
                self.project_panel
                    .update(cx, |panel, cx| panel.show_project(&project, window, cx));
                if self.panel_maximized() || self.panel_keyboard {
                    self.focus_project(window, cx);
                }
                cx.notify();
            }
            None => self.close_project(window, cx),
        }
    }

    /// Écrit ce que les éditeurs du volet Projet ont en attente (`⌘Q`, fermeture de la fenêtre).
    fn flush_project(&mut self, on: &str, cx: &mut Context<Self>) {
        self.project_panel
            .update(cx, |panel, cx| panel.flush(on, cx));
    }

    fn on_project_event(
        &mut self,
        _panel: &Entity<ProjectPanel>,
        event: &ProjectEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ProjectEvent::FocusChanged => self.sync_keyboard(window, cx),
            ProjectEvent::CopyToAgent(text) => {
                // La référence d'`AGENTS.md` porte déjà son espace de fin, comme celle de Fichiers.
                self.paste_to_agent(text, window, cx);
            }
            ProjectEvent::EditorMenu {
                position,
                has_selection,
            } => self.open_project_panel_menu(*position, *has_selection, window, cx),
        }
    }

    /// Le menu contextuel de l'éditeur d'`AGENTS.md` : celui de l'éditeur de Fichiers (« Copier vers l'agent » envoie la
    /// référence du fichier, il n'exige pas de sélection, seulement une session). Chaque entrée rejoue son geste sur le volet.
    fn open_project_panel_menu(
        &mut self,
        position: Point<Pixels>,
        has_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lines = editor_menu(has_selection, self.selected_view().is_some());
        let panel = self.project_panel.clone();
        let entries = menu_entries(lines, move |gesture, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.run_editor_gesture(gesture, window, cx);
            });
        });
        let spec = MenuSpec {
            title: None,
            entries,
        };
        self.open_menu(position, spec, window, cx);
    }

    /// Ouvre `url` dans le navigateur : le volet s'ouvre s'il est fermé (à la place du volet ouvert), puis l'adresse
    /// s'ouvre dans l'onglet que `from` désigne. Le clavier ne bouge pas.
    pub(crate) fn open_in_browser(
        &mut self,
        url: String,
        from: OpenFrom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Un menu ouvert ne survit pas au geste (comme `open_browser`, qui ne passe pas là si le volet est ouvert).
        self.menu = None;
        // Sur une session distante, le Navigateur n'existe pas : un lien http(s) s'ouvre dans le navigateur du Mac, le
        // reste est ignoré. Le journal ne garde ni requête ni fragment.
        match link_route(self.selected_is_remote(), &url) {
            LinkRoute::Browser => {}
            LinkRoute::SystemBrowser => {
                hud::log_event(&format!(
                    "link opened in system browser url={}",
                    log_form(&url)
                ));
                cx.open_url(&url);
                return;
            }
            LinkRoute::Ignored => {
                hud::log_event(&format!("link ignored remote=true url={}", log_form(&url)));
                return;
            }
        }
        if !self.browser_open() {
            self.open_browser_with(false, window, cx);
        }
        if let Some(panel) = self.shown_browser() {
            panel.update(cx, |panel, cx| panel.open_url(&url, from, window, cx));
            // Le clavier reste au pane du ⌘-clic : ni le volet ni la page ne le prennent (la page n'est jamais premier
            // répondant d'elle-même). `sync_keyboard` aligne seulement AppKit sur le focus GPUI.
            self.sync_keyboard(window, cx);
        }
    }

    /// La galerie Artifacts est-elle au centre ?
    pub(crate) fn artifacts_shown(&self) -> bool {
        self.artifacts_shown
    }

    /// La ligne « Artifacts » de la barre : la galerie prend le centre (créée au premier appel) et relit sa liste, à
    /// chaque fois. La session affichée est cachée sans être détachée : ses panes tournent toujours.
    pub(crate) fn show_artifacts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let view = match self.artifacts.clone() {
            Some(view) => view,
            None => {
                let source = cx.new(|_| ArtifactsSource::new(window.window_handle()));
                let (layout, filter) = {
                    let ui = &self.state().ui;
                    (ui.artifacts_layout, ui.artifacts_filter)
                };
                let view = cx.new(|cx| ArtifactsView::new(source, layout, filter, window, cx));
                self._artifacts_subscription =
                    Some(cx.subscribe_in(&view, window, Self::on_artifacts_event));
                self.artifacts = Some(view.clone());
                view
            }
        };
        if let Some(session) = self.selected_view() {
            session.update(cx, |session, cx| session.set_displayed(false, window, cx));
        }
        self.artifacts_shown = true;
        // Un volet agrandi cache la colonne centrale : il reprend sa largeur, comme par son propre bouton.
        if self.panel_maximized() {
            self.terminal.state.toggle_maximized(self.right_panel);
        }
        // Le clavier n'est plus à un volet : la galerie le prend.
        self.panel_keyboard = false;
        view.update(cx, |view, cx| view.reload(cx));
        window.focus(&view.focus_handle(cx), cx);
        self.update_seen(window, cx);
        cx.notify();
    }

    fn on_artifacts_event(
        &mut self,
        _view: &Entity<ArtifactsView>,
        event: &ArtifactsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // L'ouverture part au tour suivant, comme celle d'un lien de pane : elle touche AppKit.
            ArtifactsEvent::Open(url) => self.defer_open_in_browser(url.clone(), window, cx),
            ArtifactsEvent::SignIn => {
                self.defer_open_in_browser("https://claude.ai/login".to_owned(), window, cx);
            }
            ArtifactsEvent::LayoutChanged(layout) => {
                self.state_mut().ui.artifacts_layout = *layout;
                self.save();
            }
            ArtifactsEvent::FilterChanged(filter) => {
                self.state_mut().ui.artifacts_filter = *filter;
                self.save();
            }
            ArtifactsEvent::OpenMenu { position, spec } => {
                self.open_menu(*position, spec.clone(), window, cx);
            }
        }
    }

    /// `⌘`-clic d'un lien d'un pane ou du volet Terminal : l'ouverture part au tour suivant, hors de l'`update` du pane
    /// qui vient d'émettre. Rendre la saisie à GPUI et cacher la vue native d'une page touchent AppKit, qui peut
    /// rappeler GPUI en pleine mise à jour (P4-R4).
    fn defer_open_in_browser(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.weak_entity();
        window.defer(cx, move |window, cx| {
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.open_in_browser(url, OpenFrom::Link, window, cx);
            });
        });
    }

    fn on_browser_event(
        &mut self,
        _panel: &Entity<BrowserPanel>,
        event: &BrowserEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            BrowserEvent::FocusChanged => self.sync_keyboard(window, cx),
            // Les onglets partent sur le disque à chaque changement : un arrêt brutal ne les perd pas.
            BrowserEvent::TabsChanged => {
                self.save_browser_tabs(cx);
                cx.notify();
            }
            // Le dernier onglet est fermé : le volet se ferme, le clavier revient au pane (comme `⌘⇧B` sur un volet ouvert).
            BrowserEvent::CloseRequested => self.close_browser(window, cx),
            BrowserEvent::OpenConfig => self.open_config_file(window, cx),
            BrowserEvent::OpenMenu { position, spec } => {
                self.open_menu(*position, spec.clone(), window, cx);
            }
            BrowserEvent::Reveal(path) => cx.reveal_path(path),
        }
    }

    fn on_changes_event(
        &mut self,
        panel: &Entity<ChangesPanel>,
        event: &ChangesEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ChangesEvent::FocusChanged => self.sync_keyboard(window, cx),
            ChangesEvent::ListWidthChanged(width) => {
                self.state_mut().ui.changes_list_width = *width;
                self.save();
                // La préférence est globale : les volets des autres projets suivent.
                for other in self.changes_panels.values().filter(|other| *other != panel) {
                    other.update(cx, |other, cx| other.apply_list_width(*width, cx));
                }
            }
            ChangesEvent::ListHiddenChanged(hidden) => {
                self.state_mut().ui.changes_list_hidden = *hidden;
                self.save();
                for other in self.changes_panels.values().filter(|other| *other != panel) {
                    other.update(cx, |other, cx| other.apply_list_hidden(*hidden, cx));
                }
            }
            // Le double-clic d'une ligne suit le chemin du `⌘`-clic d'un pane : Fichiers prend la place de Changes. Un
            // dossier (« Ouvrir dans Fichiers » du menu) y est révélé dans l'arbre.
            ChangesEvent::OpenInFiles(path) => {
                let target = if path.is_dir() {
                    PathTarget::Folder(path.clone())
                } else {
                    PathTarget::File(FileTarget {
                        path: path.clone(),
                        line: None,
                        column: None,
                    })
                };
                self.open_in_files(target, false, window, cx);
            }
            ChangesEvent::ListMenu { target, position } => {
                self.open_changes_menu(panel.clone(), target.clone(), *position, window, cx);
            }
            ChangesEvent::TargetMenu {
                lines,
                current,
                position,
            } => {
                self.open_target_menu(
                    panel.clone(),
                    target_menu(lines, current.as_deref()),
                    *position,
                    window,
                    cx,
                );
            }
            ChangesEvent::CopyToAgent(reference) => {
                self.paste_to_agent(&format!("{reference} "), window, cx);
            }
            ChangesEvent::DiffMenu {
                position,
                reference,
            } => self.open_diff_menu(panel.clone(), reference.as_deref(), *position, window, cx),
        }
    }

    /// Le menu contextuel du diff de Changes (clic droit) : « Copier vers l'agent », allumé quand des lignes nouvelles sont
    /// choisies (`reference`) et qu'une session peut les recevoir. Le clavier revient au diff à la fermeture ; l'entrée
    /// envoie la référence, et `paste_to_agent` donne alors le focus au pane.
    fn open_diff_menu(
        &mut self,
        panel: Entity<ChangesPanel>,
        reference: Option<&str>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lines = diff_menu(reference, self.selected_view().is_some());
        let owner = panel.clone();
        let entries = menu_entries(lines, move |gesture, _window, cx| {
            panel.update(cx, |panel, cx| panel.run_diff_gesture(gesture, cx));
        });
        let spec = MenuSpec {
            title: None,
            entries,
        };
        self.open_menu(position, spec, window, cx);
        if let Some(menu) = self.menu.as_mut() {
            menu.changes_of = Some(owner);
            menu.changes_to_diff = true;
        }
    }

    /// Le menu des cibles de Changes (la puce de l'en-tête) : « working tree » puis un worktree par ligne, le chemin en
    /// texte secondaire, la cible montrée cochée. Choisir une ligne appelle `choose_target` sur le volet.
    fn open_target_menu(
        &mut self,
        panel: Entity<ChangesPanel>,
        items: Vec<TargetItem>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owner = panel.clone();
        let entries = items
            .into_iter()
            .map(|item| {
                let panel = panel.clone();
                let target = item.target;
                let mut entry = MenuItemSpec::new(item.label, move |_this, window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.choose_target(target.clone(), window, cx);
                    });
                })
                .checked(item.checked);
                if let Some(detail) = item.detail {
                    entry = entry.detail(detail);
                }
                MenuEntry::Item(entry)
            })
            .collect();
        let spec = MenuSpec {
            title: None,
            entries,
        };
        self.open_menu(position, spec, window, cx);
        // La liste garde le clavier à la fermeture, quelle que soit la sortie du menu.
        if let Some(menu) = self.menu.as_mut() {
            menu.changes_of = Some(owner);
        }
    }

    /// Le menu contextuel de la liste de Changes (clic droit sur un fichier ou un dossier). Les lignes viennent de
    /// `changes_menu` ; chaque entrée rejoue son geste sur le volet.
    fn open_changes_menu(
        &mut self,
        panel: Entity<ChangesPanel>,
        target: ChangeTarget,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lines = changes_menu(&target);
        let owner = panel.clone();
        let entries = menu_entries(lines, move |gesture, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.run_changes_gesture(gesture, &target, window, cx);
            });
        });
        let spec = MenuSpec {
            title: None,
            entries,
        };
        self.open_menu(position, spec, window, cx);
        // La liste garde le clavier à la fermeture, quelle que soit la sortie du menu.
        if let Some(menu) = self.menu.as_mut() {
            menu.changes_of = Some(owner);
        }
    }

    /// Le projet de contexte a peut-être changé : le volet droit ouvert, quel qu'il soit, suit. Un bouillon
    /// sélectionné n'a pas de projet de volet (`panel_project`) : Terminal, Fichiers, Changes et Projet se ferment, le
    /// Scratchpad et le Browser restent.
    fn sync_right_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Sur une session distante, Fichiers, Changes, Browser et Projet ne se ferment pas : ils montrent « Pas encore
        // disponible » et se taisent (plus de surveillance du disque, page native cachée). Leur contenu revient au retour
        // sur une session locale, par les branches ci-dessous.
        if unavailable_open_panel(self.selected_is_remote(), self.right_panel).is_some() {
            self.sync_files_shown(window, cx);
            self.sync_changes_shown(window, cx);
            self.sync_browser_shown(window, cx);
            self.sync_keyboard(window, cx);
            cx.notify();
            return;
        }
        match self.right_panel {
            Some(RightPanel::Terminal) => self.sync_terminal(window, cx),
            Some(RightPanel::Files) => self.sync_files(window, cx),
            Some(RightPanel::Changes) => self.sync_changes(window, cx),
            Some(RightPanel::Project) => self.sync_project(window, cx),
            // Le navigateur ne suit pas le projet de contexte (P4-5), ni le Scratchpad : sa note est globale (AC-8).
            // Il se remontre au retour d'une session distante, où sa page native était cachée.
            Some(RightPanel::Browser) => self.sync_browser_shown(window, cx),
            Some(RightPanel::Scratchpad) | None => {}
        }
    }

    fn on_files_event(
        &mut self,
        panel: &Entity<FilesPanel>,
        event: &FilesEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            FilesEvent::FocusChanged => self.sync_keyboard(window, cx),
            FilesEvent::TreeCollapsedChanged(collapsed) => {
                self.state_mut().ui.files_tree_collapsed = *collapsed;
                self.save();
                // La préférence est globale : les volets des autres projets suivent.
                for other in self.files_panels.values().filter(|other| *other != panel) {
                    other.update(cx, |other, cx| other.apply_tree_collapsed(*collapsed, cx));
                }
            }
            FilesEvent::TreeWidthChanged(width) => {
                self.state_mut().ui.files_tree_width = *width;
                self.save();
                for other in self.files_panels.values().filter(|other| *other != panel) {
                    other.update(cx, |other, cx| other.apply_tree_width(*width, cx));
                }
            }
            FilesEvent::CopyToAgent(reference) => {
                self.paste_to_agent(&format!("{reference} "), window, cx);
            }
            FilesEvent::TreeMenu { target, position } => {
                self.open_tree_menu(panel.clone(), target.clone(), *position, window, cx);
            }
            FilesEvent::EditorMenu {
                position,
                has_selection,
            } => self.open_editor_menu(panel.clone(), *position, *has_selection, window, cx),
        }
    }

    /// Le menu contextuel de l'arbre du volet Fichiers (clic droit sur une ligne ou dans le vide). Les lignes viennent
    /// de `tree_menu` ; chaque entrée rejoue son geste sur le volet. « Copier vers l'agent » demande une session.
    fn open_tree_menu(
        &mut self,
        panel: Entity<FilesPanel>,
        target: TreeTarget,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Ce que Git dit de l'entrée allume ou éteint « Ajouter au .gitignore » ; la racine n'a rien à en dire.
        let git = match &target {
            TreeTarget::Root => EntryGit {
                tracked: true,
                ignored: false,
            },
            TreeTarget::Entry { path, .. } => panel.read(cx).entry_git(path),
        };
        let lines = tree_menu(&target, self.selected_view().is_some(), git);
        let owner = panel.clone();
        let entries = menu_entries(lines, move |gesture, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.run_tree_gesture(gesture, &target, window, cx);
            });
        });
        let spec = MenuSpec {
            title: None,
            entries,
        };
        self.open_menu(position, spec, window, cx);
        // L'arbre garde le clavier à la fermeture, quelle que soit la sortie du menu.
        if let Some(menu) = self.menu.as_mut() {
            menu.tree_of = Some(owner);
        }
    }

    /// Le menu contextuel de l'éditeur (clic droit dans le texte). Couper et Copier exigent une sélection ; « Copier vers
    /// l'agent », une session. Chaque entrée rejoue son geste sur le volet.
    fn open_editor_menu(
        &mut self,
        panel: Entity<FilesPanel>,
        position: Point<Pixels>,
        has_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lines = editor_menu(has_selection, self.selected_view().is_some());
        let entries = menu_entries(lines, move |gesture, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.run_editor_gesture(gesture, window, cx);
            });
        });
        let spec = MenuSpec {
            title: None,
            entries,
        };
        self.open_menu(position, spec, window, cx);
    }

    /// « Copier vers l'agent » : colle `text`, tel quel, dans le pane actif de la session sélectionnée. Fichiers et Changes
    /// y ajoutent l'espace qui suit leur référence ; le Scratchpad colle sa sélection sans rien de plus. Le volet reste
    /// ouvert ; le pane prend le focus sauf si le volet est agrandi (le clavier reste alors à l'éditeur, spec §4.7).
    fn paste_to_agent(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.selected_name().map(str::to_owned) else {
            hud::log_event("copy to agent skipped reason=no-session");
            return;
        };
        // La galerie cache la session sélectionnée : coller dans un pane non dessiné donnerait le focus à rien. On
        // ramène d'abord la session au centre, puis on colle.
        if self.artifacts_shown {
            self.select(&name, window, cx);
        }
        let Some(view) = self.selected_view() else {
            hud::log_event("copy to agent skipped reason=no-session");
            return;
        };
        let focus = !self.panel_maximized();
        let pasted = view.update(cx, |view, cx| view.paste_to_active(text, focus, window, cx));
        match pasted {
            Some(pane) => hud::log_event(&format!(
                "copy to agent session={} pane=%{} chars={}",
                key_label(&name),
                pane.0,
                text.chars().count()
            )),
            None => hud::log_event("copy to agent skipped reason=no-pane"),
        }
    }

    fn on_terminal_event(
        &mut self,
        view: &Entity<PaneView>,
        event: &PaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PaneEvent::Closed => self.on_terminal_closed(view, window, cx),
            PaneEvent::FocusChanged => self.sync_keyboard(window, cx),
            PaneEvent::OpenPath(target) => self.open_in_files(target.clone(), false, window, cx),
            PaneEvent::OpenUrl(url) => self.defer_open_in_browser(url.clone(), window, cx),
            _ => {}
        }
    }

    /// Le shell d'un projet ou d'une machine est parti (écart E26) : son PTY est retiré, le prochain `` ⌃` `` en lance un neuf. S'il
    /// était affiché, le volet se ferme et le focus revient à la session.
    fn on_terminal_closed(
        &mut self,
        view: &Entity<PaneView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self.shown_terminal().map(|(_, shown)| shown);
        let Some(key) = self.terminal.remove(view) else {
            return;
        };
        hud::log_event(&format!("terminal exited {}", terminal_label(&key)));
        if shown.as_ref() == Some(view) {
            self.close_terminal(window, cx);
        } else {
            cx.notify();
        }
    }

    /// Le badge du Dock compte les sessions qui attendent ou sont en erreur ; il disparaît à zéro.
    fn refresh_badge(&self, cx: &App) {
        dock::set_badge(dock_badge_count(
            self.entries.iter().map(|entry| &entry.summary),
            self.ideation().is_some(),
        ));
        self.publish_fleet(cx);
    }

    /// Publie l'instantané de la flotte au service du compagnon : projets et sessions dans l'ordre de la barre (tous les
    /// projets dépliés, les bouillons en tête), avec l'état, l'instant d'entrée, les agents et les panes de chaque
    /// session. Compagnon éteint, rien n'est construit. Un instantané qui ne dit rien de plus que le précédent n'est
    /// pas republié (`CompanionController::publish`), donc on peut l'appeler à chaque changement possible.
    fn publish_fleet(&self, cx: &App) {
        if !self.companion.is_on() {
            return;
        }
        self.sync_session_servers();
        let sessions = self
            .entries
            .iter()
            .map(|entry| {
                let view = entry.view.as_ref().map(|view| view.read(cx));
                SessionInput {
                    summary: entry.summary.clone(),
                    state_since: entry.state_since,
                    seen: view.is_some_and(SessionView::is_seen),
                    agent_names: match &entry.summary.machine {
                        // Les agents d'une session distante viennent du listage de sa machine, pas du relevé du Mac.
                        Some(_) => self
                            .remote_agents
                            .get(&entry.summary.name)
                            .cloned()
                            .unwrap_or_default(),
                        None => self.servers.as_ref().map_or_else(Vec::new, |servers| {
                            servers.read(cx).agents_of(&entry.summary.name).to_vec()
                        }),
                    },
                    remote_agents: entry
                        .summary
                        .machine
                        .as_deref()
                        .and_then(|connection| self.machine_index(connection))
                        .and_then(|index| self.machines[index].facts.as_ref())
                        .and_then(|facts| facts.agents.clone())
                        .unwrap_or_default(),
                    panes: view.map_or_else(Vec::new, |view| {
                        view.pane_states(cx)
                            .into_iter()
                            .map(|(pane, active, state)| FleetPane {
                                id: pane.0,
                                active,
                                state: state_label(state),
                            })
                            .collect()
                    }),
                }
            })
            .collect();
        let projects: Vec<PathBuf> = self
            .state()
            .ordered_projects()
            .iter()
            .map(|project| project.path.clone())
            .collect();
        let machines: Vec<(String, String)> = self
            .machines
            .iter()
            .map(|machine| {
                (
                    machine.entry.connection.clone(),
                    machine.entry.label.clone(),
                )
            })
            .collect();
        self.companion.publish(compose_fleet(
            mac_name(),
            SystemTime::now(),
            self.ideation().map(|ideation| ideation.label.as_str()),
            &projects,
            &machines,
            &self.config.agents,
            sessions,
        ));
    }

    /// Range dans la chronologie du compagnon le changement d'état d'une session (sans effet compagnon éteint).
    fn report_transition(
        &self,
        session: &str,
        from: ActivityState,
        to: ActivityState,
        at: SystemTime,
    ) {
        self.companion.record_transition(session, from, to, at);
    }

    /// Une session change d'état : la barre, le badge et, au passage en « attend » d'une session non vue, le son.
    fn on_state_changed(
        &mut self,
        view: &Entity<SessionView>,
        previous: ActivityState,
        current: ActivityState,
        cx: &mut Context<Self>,
    ) {
        let ideation_enabled = self.ideation().is_some();
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.view.as_ref() == Some(view))
        else {
            return;
        };
        let now = SystemTime::now();
        entry.enter_state(current, now);
        let name = entry.summary.name.clone();
        let alerting = is_alerting_session(entry.summary.ideation, ideation_enabled);
        let chimes = alerting && should_chime(previous, current, view.read(cx).is_seen());
        if chimes {
            self.chime(&format!("session={}", key_label(&name)));
        }
        self.report_transition(&name, previous, current, now);
        self.refresh_badge(cx);
        // Après le badge : la flotte publiée porte déjà le compte d'après la transition, que la notification affiche.
        self.report_episodes(&name, view, (previous, current), chimes, alerting, cx);
        cx.notify();
    }

    /// Un pane a changé d'état : si plus aucun pane de la session n'attend, l'épisode « attend » est fini. Ce que l'état
    /// agrégé ne dit pas toujours : un pane caché qui attendait pendant que la session était en erreur a ouvert un
    /// épisode (`on_hidden_pane_waiting`) que le passage de la session hors de « attend » ne clôt jamais. La clôture
    /// d'un épisode qui n'existe pas est sans effet.
    fn close_waiting_episode_if_over(&self, view: &Entity<SessionView>, cx: &App) {
        if !self.companion.is_on() {
            return;
        }
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.view.as_ref() == Some(view))
        else {
            return;
        };
        let panes: Vec<(u32, bool, ActivityState)> = view
            .read(cx)
            .pane_states(cx)
            .into_iter()
            .map(|(pane, active, state)| (pane.0, active, state))
            .collect();
        if no_pane_waiting(&panes) {
            self.companion.send(ServiceEvent::EpisodeClosed {
                session: entry.summary.name.clone(),
                kind: EpisodeKind::Waiting,
            });
        }
    }

    /// Dit au compagnon ce que le passage de `states.0` à `states.1` change aux épisodes de la session (spec §3.7) :
    /// un épisode « attend » quand le son retentirait (`chimes`, que le son soit coupé ou non), un épisode « erreur »
    /// au passage en erreur d'une session qui compte au badge, et la clôture de celui que l'état vient de quitter.
    /// Le pane d'un épisode est le plus urgent des panes de la session. Sans effet compagnon éteint.
    fn report_episodes(
        &self,
        session: &str,
        view: &Entity<SessionView>,
        states: (ActivityState, ActivityState),
        chimes: bool,
        alerting: bool,
        cx: &App,
    ) {
        if !self.companion.is_on() {
            return;
        }
        for change in episode_changes(states.0, states.1, chimes, alerting) {
            match change {
                EpisodeChange::Close(kind) => self.companion.send(ServiceEvent::EpisodeClosed {
                    session: session.to_owned(),
                    kind,
                }),
                EpisodeChange::Open(kind) => {
                    let panes: Vec<(u32, bool, ActivityState)> = view
                        .read(cx)
                        .pane_states(cx)
                        .into_iter()
                        .map(|(pane, active, state)| (pane.0, active, state))
                        .collect();
                    if let Some(pane) = urgent_pane(&panes, kind) {
                        self.companion.send(ServiceEvent::EpisodeOpened {
                            session: session.to_owned(),
                            pane,
                            kind,
                        });
                    }
                }
            }
        }
    }

    /// Le son d'un passage en « attend » : coupé par `sound_on_wait = false`. Le son peut ne pas partir (un autre
    /// joue encore) : la ligne du journal s'écrit quand même. `what` dit qui attend (`session=<nom>`, plus
    /// `pane=%<id>` pour un pane caché par le zoom).
    fn chime(&self, what: &str) {
        if self.config.sound_on_wait {
            dock::play_wait_sound();
            hud::log_event(&format!("chime {what}"));
        } else {
            hud::log_event(&format!("chime muted {what}"));
        }
    }

    /// Un pane caché par le zoom attend alors que la session est vue (décision J4-13) : son, comme un passage en
    /// « attend » d'une session non vue.
    fn on_hidden_pane_waiting(
        &mut self,
        view: &Entity<SessionView>,
        pane: PaneId,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.view.as_ref() == Some(view))
        else {
            return;
        };
        if !is_alerting_session(entry.summary.ideation, self.ideation().is_some()) {
            return;
        }
        let name = entry.summary.name.clone();
        self.chime(&format!("session={} pane=%{}", key_label(&name), pane.0));
        // Le son aurait retenti : c'est un épisode « attend » de ce pane, vu ou non.
        self.companion.send(ServiceEvent::EpisodeOpened {
            session: name,
            pane: pane.0,
            kind: EpisodeKind::Waiting,
        });
        cx.notify();
    }

    /// Fait de la session `name` (entrée `index`) la sélection : projet de contexte, `state.json`, relance d'une
    /// session injoignable. Ne touche ni à l'affichage, ni au focus, ni au « vu » : c'est la part commune de `select`
    /// et de `select_behind_gallery`.
    fn adopt_selection(
        &mut self,
        index: usize,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Un bouillon n'a pas de projet : choisir son dossier d'idéation comme projet de contexte le ferait lire
        // partout (`⌘T` après sa fermeture, `⌘O`). Le dernier projet de contexte reste donc celui d'avant.
        // Une session distante n'a pas non plus de projet du Mac : le contexte reste celui d'avant.
        if !self.entries[index].summary.ideation && self.entries[index].summary.machine.is_none() {
            self.selected_project = Some(self.entries[index].summary.project.clone());
            self.project_wins = false;
        }
        self.state_mut().ui.selected_session = Some(name.to_owned());
        // Un choix de Romain l'emporte sur la session distante mémorisée au lancement (`should_reselect`).
        self.chosen_since_launch = true;
        self.save();
        // Une session injoignable relance ses essais, le premier une seconde plus tard. Une série déjà relancée
        // (`attempts` sous le maximum) n'est pas doublée par un second clic. Chaque essai est noté au journal
        // (spec §5.1), celui-ci comme les autres.
        let entry = &mut self.entries[index];
        if let Some(raw) = entry.unreachable.clone()
            && entry.attempts >= REATTACH_ATTEMPTS
        {
            entry.attempts = 1;
            hud::log_event(&format!(
                "reattach session={} attempt=1 reason={raw}",
                key_label(name)
            ));
            self.reattach_later(name, window, cx);
        }
    }

    /// La session cachée sélectionnée vient de finir alors que la galerie est au centre : la voisine prend la
    /// sélection (barre, `state.json`, volets) sans être affichée ni vue, et la galerie garde le centre et le focus.
    /// Passer par `select` effacerait le badge d'activité de la voisine (`set_seen`) et enverrait un aller-retour de
    /// focus (ESC[I / ESC[O) à son programme, alors qu'elle n'a jamais quitté l'ombre de la galerie.
    fn select_behind_gallery(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.entry_index(name) else {
            return;
        };
        // Un menu ouvert ne survit pas au changement de sélection ; la galerie reprend alors le focus qu'il avait.
        if self.menu.take().is_some()
            && let Some(view) = self.artifacts.clone()
        {
            window.focus(&view.focus_handle(cx), cx);
        }
        self.adopt_selection(index, name, window, cx);
        self.sync_branch(cx);
        self.sync_right_panel(window, cx);
        cx.notify();
    }

    /// Sélectionne la session `name` : elle devient la session affichée et reçoit le focus.
    pub(crate) fn select(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.entry_index(name) else {
            return;
        };
        self.menu = None;
        // Choisir une session, la sélectionnée comprise, ramène le centre sur elle.
        self.artifacts_shown = false;
        if self.selected_name() != Some(name)
            && let Some(previous) = self.selected_view()
        {
            previous.update(cx, |view, cx| view.set_displayed(false, window, cx));
        }
        self.adopt_selection(index, name, window, cx);
        // Choisir une session lui donne le clavier, même si le volet Terminal l'avait.
        self.panel_keyboard = false;
        self.show_selected(window, cx);
        self.sync_branch(cx);
        self.sync_right_panel(window, cx);
    }

    /// `⌘1` à `⌘9` : la n-ième session de l'ordre de navigation, les bouillons d'abord, puis les projets dépliés.
    fn select_nth(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let target = {
            let summaries = self.summaries();
            let order = self.navigation(&summaries);
            let order: Vec<&str> = order.iter().map(String::as_str).collect();
            nth_session(&order, index).map(str::to_owned)
        };
        if let Some(name) = target {
            self.select(&name, window, cx);
        }
    }

    /// `⌃⇥` et `⌃⇧⇥` : la session suivante ou précédente des projets dépliés, en boucle.
    fn cycle_session(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let target = {
            let summaries = self.summaries();
            let order = self.navigation(&summaries);
            let order: Vec<&str> = order.iter().map(String::as_str).collect();
            cycle(&order, self.selected_name(), forward).map(str::to_owned)
        };
        if let Some(name) = target {
            self.select(&name, window, cx);
        }
    }

    /// La section `[ideation]` de la config en mémoire : absente, la fonction est masquée.
    pub(crate) fn ideation(&self) -> Option<&IdeationConfig> {
        self.config.ideation.as_ref()
    }

    fn summaries(&self) -> Vec<SessionSummary> {
        self.entries
            .iter()
            .map(|entry| entry.summary.clone())
            .collect()
    }

    /// Les sessions qu'on parcourt au clavier : les bouillons quand leur section est ouverte, puis celles des projets
    /// dépliés, dans l'ordre de la barre.
    fn navigation(&self, summaries: &[SessionSummary]) -> Vec<String> {
        let state = self.state();
        let projects: Vec<PathBuf> = state
            .ordered_projects()
            .into_iter()
            .map(|project| project.path.clone())
            .collect();
        // Les bouillons n'appartiennent à aucune machine : Localhost replié ne les retire pas du cycle.
        let mut order = owned_names(navigation_order(
            &[],
            |_| false,
            ideation_shown(self.ideation().is_some(), state.ui.ideation_collapsed),
            summaries,
        ));
        let local = owned_names(navigation_order(
            &projects,
            |path| state.is_expanded(path),
            false,
            summaries,
        ));
        order.extend(machine_navigation_order(
            local,
            &self.machine_groups(summaries, true),
            self.local_collapsed(),
        ));
        order
    }

    /// Les sessions de chaque machine distante, dans l'ordre de la config : (clé, repliée, clés). `honor_collapse` faux :
    /// aucune machine n'est repliée.
    fn machine_groups(
        &self,
        summaries: &[SessionSummary],
        honor_collapse: bool,
    ) -> Vec<(String, bool, Vec<String>)> {
        self.machines
            .iter()
            .map(|machine| {
                let key = machine.entry.connection.clone();
                let collapsed = honor_collapse && self.state().is_machine_collapsed(&key);
                let keys = summaries
                    .iter()
                    .filter(|summary| summary.machine.as_deref() == Some(key.as_str()))
                    .map(|summary| summary.name.clone())
                    .collect();
                (key, collapsed, keys)
            })
            .collect()
    }

    /// Le nom de Localhost dans la barre (`[machines] local_label`, sinon « Localhost »).
    pub(crate) fn local_machine_label(&self) -> &str {
        self.config.local_machine_label()
    }

    /// Localhost est-il replié ? Sans machine distante il n'a pas d'en-tête à replier : jamais.
    pub(crate) fn local_collapsed(&self) -> bool {
        !self.machines.is_empty() && self.state().is_machine_collapsed(LOCAL_MACHINE)
    }

    /// Toutes les sessions dans l'ordre de la barre, repliement ignoré : le voisin d'une session qui se ferme
    /// se cherche ici, car une session sélectionnée dans un projet replié doit elle aussi passer la main.
    fn full_order(&self, summaries: &[SessionSummary]) -> Vec<String> {
        let projects: Vec<PathBuf> = self
            .state()
            .ordered_projects()
            .into_iter()
            .map(|project| project.path.clone())
            .collect();
        let mut order = owned_names(navigation_order(
            &[],
            |_| false,
            ideation_shown(self.ideation().is_some(), false),
            summaries,
        ));
        let local = owned_names(navigation_order(&projects, |_| true, false, summaries));
        order.extend(machine_navigation_order(
            local,
            &self.machine_groups(summaries, false),
            false,
        ));
        order
    }

    fn on_session_event(
        &mut self,
        view: &Entity<SessionView>,
        event: &SessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SessionEvent::MetaChanged => {
                // Le pane actif ou son programme a changé : la flotte porte le drapeau « actif ».
                self.publish_fleet(cx);
                cx.notify();
            }
            SessionEvent::PanesChanged => {
                self.publish_fleet(cx);
                self.close_waiting_episode_if_over(view, cx);
            }
            SessionEvent::StateChanged { previous, current } => {
                self.on_state_changed(view, *previous, *current, cx);
            }
            SessionEvent::Closed => self.on_session_closed(view, window, cx),
            SessionEvent::FocusChanged => self.sync_keyboard(window, cx),
            SessionEvent::SessionsChanged => {
                // Le client d'une machine distante relit sa machine ; celui du Mac, le tmux du Mac.
                let machine = self
                    .entries
                    .iter()
                    .find(|entry| entry.view.as_ref() == Some(view))
                    .and_then(|entry| entry.summary.machine.clone());
                match machine {
                    Some(connection) => self.refresh_machine(&connection, window, cx),
                    None => self.on_sessions_changed(window, cx),
                }
            }
            SessionEvent::OpenPath(target) => self.open_in_files(target.clone(), false, window, cx),
            SessionEvent::OpenUrl(url) => self.defer_open_in_browser(url.clone(), window, cx),
            SessionEvent::HiddenPaneWaiting { pane } => {
                self.on_hidden_pane_waiting(view, *pane, cx);
            }
        }
    }

    /// Le client d'une vue s'est arrêté : `on_attach_lost` décide, avec la raison de l'arrêt et le temps que le
    /// client a vécu.
    fn on_session_closed(
        &mut self,
        view: &Entity<SessionView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.view.as_ref() == Some(view))
        else {
            return;
        };
        let name = entry.summary.name.clone();
        let (raw, lived) = {
            let view = view.read(cx);
            (
                view.exit_reason().unwrap_or(NO_EXIT_REASON).to_owned(),
                view.connected_for(),
            )
        };
        self.on_attach_lost(&name, raw, Some(lived), window, cx);
    }

    /// Une attache est perdue : un client tombé (`alive_for` est sa durée de vie), ou une attache qui a échoué
    /// (`None`). La session a disparu (l'entrée part), ou vit encore (un nouvel essai, trois au plus, puis
    /// « Session injoignable »). `has_session` lance un process, donc hors du fil d'interface.
    pub(crate) fn on_attach_lost(
        &mut self,
        name: &str,
        raw: String,
        alive_for: Option<Duration>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.entry_index(name).is_none() {
            return;
        }
        // `has-session` est un appel ponctuel (un process, ou un canal bref ssh) : hors du fil d'interface. En local, toute
        // erreur vaut « absente » ; sur une machine, une erreur de ssh n'est pas une absence (`has_session_checked`) :
        // `after_remote_close` en fait une machine perdue, pas une session disparue.
        let Some(check) = self.spawn_for(name, cx, |server, tmux| {
            if server.is_remote() {
                server.has_session_checked(tmux)
            } else {
                Ok(server.has_session(tmux))
            }
        }) else {
            // Sans serveur, ou machine non connectée : sa reconnexion reprend les vues (`resume_machine_views`).
            return;
        };
        let name = name.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let exists = check.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(index) = this.entry_index(&name) else {
                    return;
                };
                let after =
                    match after_remote_close(&exists, this.entries[index].attempts, alive_for) {
                        RemoteAfterClose::Close(after) => after,
                        // Ni présente ni absente : la machine ne répond plus. L'entrée et sa vue restent, la machine se
                        // reconnecte et `resume_machine_views` rattache la vue.
                        RemoteAfterClose::MachineLost => {
                            let reason = exists
                                .err()
                                .map(|error| error.to_string())
                                .unwrap_or_default();
                            hud::log_event(&format!(
                                "session check failed session={}: {}",
                                key_label(&name),
                                log_safe(&reason)
                            ));
                            this.lose_machine(machine_of(&name), &reason, window, cx);
                            return;
                        }
                    };
                match after {
                    AfterClose::Remove => this.remove_entry(&name, window, cx),
                    AfterClose::Retry { attempt } => {
                        this.entries[index].attempts = attempt;
                        hud::log_event(&format!(
                            "reattach session={} attempt={attempt} reason={raw}",
                            key_label(&name)
                        ));
                        this.reattach_later(&name, window, cx);
                    }
                    AfterClose::Unreachable => this.mark_unreachable(&name, raw, window, cx),
                }
            });
        })
        .detach();
    }

    /// Une nouvelle attache, `REATTACH_DELAY` plus tard. La vue reste la même : elle reçoit un nouveau client
    /// de contrôle (`reconnect`) et ses panes gardent leur écran, leur historique et leurs états. Un échec
    /// repasse par `on_attach_lost`, qui compte l'essai.
    fn reattach_later(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.entry_index(name).is_none() {
            return;
        }
        let name = name.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(REATTACH_DELAY).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.entry_index(&name).is_none() {
                    return;
                }
                let displayed = this.selected_name() == Some(name.as_str());
                // Rien à offrir (pas de serveur, machine déconnectée) : la reconnexion de la machine reprendra la vue.
                if this.server_for(&name).is_none() {
                    return;
                }
                // Le bail n'est pris que pour la reconnexion : `attach` prend le sien, une vue ne consomme jamais deux canaux.
                let result = match this.entry_view(&name) {
                    Some(view) => {
                        let Some((lease, _)) = this.long_lived_server_for(&name) else {
                            return;
                        };
                        view.update(cx, |view, cx| view.reconnect(lease, window, cx))
                    }
                    // Sans vue (essais épuisés, ou attache ratée) : une vue neuve, à la taille de la zone si affichée.
                    None => {
                        let size =
                            displayed.then(|| this.central_grid(window, this.stored_gaps(&name)));
                        this.attach(&name, size, Vec::new(), window, cx)
                    }
                };
                match result {
                    Ok(()) => {
                        if let Some(index) = this.entry_index(&name) {
                            this.entries[index].unreachable = None;
                        }
                        if displayed {
                            this.show_selected(window, cx);
                        } else {
                            cx.notify();
                        }
                    }
                    Err(error) => {
                        this.on_attach_lost(&name, error.to_string(), None, window, cx);
                    }
                }
            });
        })
        .detach();
    }

    /// Les essais sont épuisés : l'entrée reste dans la barre sans vue, avec le brut de la dernière chute. Le centre
    /// dit « Session injoignable » ; une nouvelle sélection relance les essais, et la réconciliation retire
    /// l'entrée quand la session disparaît.
    fn mark_unreachable(
        &mut self,
        name: &str,
        raw: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.entry_index(name) else {
            return;
        };
        hud::log_event(&format!(
            "session unreachable session={}: {raw}",
            key_label(name)
        ));
        let now = SystemTime::now();
        let entry = &mut self.entries[index];
        entry.view = None;
        entry._subscription = None;
        let left = entry.enter_state(ActivityState::Idle, now);
        entry.unreachable = Some(raw);
        if let Some(previous) = left {
            self.companion.record_reset(name, previous, now);
        }
        self.refresh_badge(cx);
        if self.selected_name() == Some(name) {
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    /// Retire l'entrée d'une session disparue. Si elle était sélectionnée, la voisine (calculée avant le
    /// retrait, sur toutes les sessions) prend la sélection, sinon l'écran vide revient.
    fn remove_entry(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.entry_index(name).is_none() {
            return;
        }
        let was_selected = self.selected_name() == Some(name);
        // La galerie est au centre : la fin de la session cachée n'est pas un geste de l'utilisateur, la galerie reste.
        let keep_gallery = was_selected && self.artifacts_shown;
        let neighbor = was_selected
            .then(|| {
                let summaries = self.summaries();
                let order = self.full_order(&summaries);
                let order: Vec<&str> = order.iter().map(String::as_str).collect();
                neighbor_after_close(&order, name).map(str::to_owned)
            })
            .flatten();
        // Une attente ou une erreur ouverte se clôt dans la chronologie : la session n'existe plus.
        if let Some(entry) = self.entries.iter().find(|entry| entry.summary.name == name) {
            self.companion
                .record_reset(name, entry.summary.state, SystemTime::now());
        }
        self.entries.retain(|entry| entry.summary.name != name);
        self.refresh_badge(cx);
        if was_selected {
            match neighbor {
                Some(next) => match neighbor_handoff(keep_gallery) {
                    NeighborHandoff::Select => self.select(&next, window, cx),
                    NeighborHandoff::BehindGallery => self.select_behind_gallery(&next, window, cx),
                },
                None => {
                    // Comme `select` : un menu ouvert sur la session retirée ne survit pas, sans quoi il
                    // resterait affiché sans focus (menu souris) ou invisible mais `Some` (menu du `⋮`).
                    self.menu = None;
                    self.state_mut().ui.selected_session = None;
                    self.save();
                    if !keep_gallery {
                        window.focus(&self.focus_handle, cx);
                    }
                    self.sync_branch(cx);
                    self.sync_right_panel(window, cx);
                }
            }
        }
        cx.notify();
    }

    /// Compare les entrées à `list-sessions` et retire celles dont la session a disparu. Une entrée sans vue ne
    /// reçoit plus d'événement : c'est le seul chemin qui la retire.
    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = self.server.clone() else {
            return;
        };
        let listing = cx.background_spawn(async move { server.sessions() });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(listed) = listing.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                // Le listage est celui du tmux du Mac : les machines distantes se réconcilient par `refresh_machine`.
                let gone: Vec<String> = this
                    .entries
                    .iter()
                    .filter(|entry| entry.summary.machine.is_none())
                    .filter(|entry| !listed.iter().any(|s| s.name == entry.summary.name))
                    .map(|entry| entry.summary.name.clone())
                    .collect();
                for name in gone {
                    this.remove_entry(&name, window, cx);
                }
            });
        })
        .detach();
    }

    /// Relit la branche quand le projet de l'écran vide change, hors du fil d'interface (`git` est un process).
    fn sync_branch(&mut self, cx: &mut Context<Self>) {
        let project = self.panel_project();
        if project == self.branch_project {
            return;
        }
        self.branch_project = project.clone();
        self.branch = None;
        self._branch_task = project.map(|project| {
            cx.spawn(async move |this, cx| {
                let path = project.clone();
                let branch = cx
                    .background_spawn(async move { project::current_branch(&path) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    // Un autre projet a pris la place pendant la lecture : cette branche est périmée.
                    if this.branch_project.as_deref() == Some(project.as_path()) {
                        this.branch = branch;
                        cx.notify();
                    }
                });
            })
        });
    }

    /// Recalcule les projets dont le dossier a disparu. Un `stat` par projet : la liste est courte.
    fn refresh_missing(&mut self, cx: &mut Context<Self>) {
        let missing = project::missing_projects(
            self.state()
                .projects
                .iter()
                .map(|project| project.path.as_path()),
        );
        if missing != self.missing {
            self.missing = missing;
            cx.notify();
        }
    }

    /// Vrai quand le dossier du projet n'existe plus : sa ligne est grisée et on n'y crée plus de session.
    pub(crate) fn is_missing(&self, path: &Path) -> bool {
        self.missing.contains(path)
    }

    /// Ouvre le menu `spec` ancré en `position` (coordonnées de la fenêtre) et lui donne le focus.
    pub(crate) fn open_menu(
        &mut self,
        position: Point<Pixels>,
        spec: MenuSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_menu(spec, Placement::Pointer(position), window, cx);
    }

    fn show_menu(
        &mut self,
        spec: MenuSpec,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.menu = Some(OpenMenu {
            spec,
            placement,
            focus,
            tree_of: None,
            changes_of: None,
            changes_to_diff: false,
            submenu: None,
            submenu_switch: None,
            submenu_rows: Rc::default(),
        });
        self.sync_page_cover(window, cx);
        cx.notify();
    }

    /// Le pointeur entre sur une entrée du menu : `entry` est le rang de son sous-menu, `None` pour une autre entrée. Le
    /// panneau change après `SUBMENU_DELAY`, le temps de rejoindre en diagonale celui qui est ouvert.
    pub(crate) fn hover_submenu(&mut self, entry: Option<usize>, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.menu else {
            return;
        };
        if menu.submenu == entry {
            menu.submenu_switch = None;
            return;
        }
        menu.submenu_switch = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SUBMENU_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(menu) = &mut this.menu {
                    menu.submenu = entry;
                    menu.submenu_switch = None;
                    cx.notify();
                }
            });
        }));
    }

    /// Clic sur une entrée à sous-menu : le panneau s'ouvre sans attendre.
    pub(crate) fn open_submenu(&mut self, entry: usize, cx: &mut Context<Self>) {
        if let Some(menu) = &mut self.menu {
            menu.submenu = Some(entry);
            menu.submenu_switch = None;
            cx.notify();
        }
    }

    /// Le pointeur a rejoint le sous-menu ouvert : il reste ouvert.
    pub(crate) fn keep_submenu(&mut self) {
        if let Some(menu) = &mut self.menu {
            menu.submenu_switch = None;
        }
    }

    /// Où est le clavier : le volet droit affiché ou la session. Il suit le focus réel ; passé par un menu, un champ ou la
    /// racine (aucun des deux), il reste où il était. La session apprend ensuite si elle l'a (ESC[I et ESC[O aux
    /// programmes qui les demandent, spec §6.3).
    fn sync_keyboard(&mut self, window: &Window, cx: &mut Context<Self>) {
        let (panel_shown, panel_focused) = if let Some((_, view)) = self.shown_terminal() {
            (true, view.focus_handle(cx).is_focused(window))
        } else if let Some((_, panel)) = self.shown_files() {
            (true, panel.read(cx).contains_focused(window, cx))
        } else if let Some((_, panel)) = self.shown_changes() {
            (true, panel.read(cx).contains_focused(window, cx))
        } else if let Some(panel) = self.shown_browser() {
            (true, panel.read(cx).contains_focused(window, cx))
        } else if let Some(panel) = self.shown_scratchpad() {
            (true, panel.read(cx).contains_focused(window, cx))
        } else if let Some((_, panel)) = self.shown_project() {
            (true, panel.read(cx).contains_focused(window, cx))
        } else {
            (false, false)
        };
        let session_focused = self
            .selected_view()
            .is_some_and(|view| view.read(cx).has_focus(window, cx));
        self.panel_keyboard = panel_keeps_keyboard(
            panel_shown,
            panel_focused,
            session_focused,
            self.panel_keyboard,
        );
        let keyboard = !self.panel_keyboard;
        let views: Vec<Entity<SessionView>> = self
            .entries
            .iter()
            .filter_map(|entry| entry.view.clone())
            .collect();
        for view in views {
            view.update(cx, |view, cx| view.set_keyboard(keyboard, window, cx));
        }
    }

    /// Le focus de ce qui est à l'écran : le shell du volet Terminal ou le volet Fichiers quand il est agrandi (la
    /// colonne principale n'est alors pas peinte, et GPUI ramènerait le focus à la racine) ou qu'il tenait le
    /// clavier, sinon la session sélectionnée.
    fn focus_target(&self, cx: &App) -> Option<FocusHandle> {
        if self.panel_maximized() || self.panel_keyboard {
            if let Some((_, view)) = self.shown_terminal() {
                return Some(view.focus_handle(cx));
            }
            if let Some((_, panel)) = self.shown_files() {
                return Some(panel.focus_handle(cx));
            }
            if let Some((_, panel)) = self.shown_changes() {
                return Some(panel.focus_handle(cx));
            }
            if let Some(panel) = self.shown_browser() {
                return Some(panel.focus_handle(cx));
            }
            if let Some(panel) = self.shown_scratchpad() {
                return Some(panel.focus_handle(cx));
            }
            if let Some((_, panel)) = self.shown_project() {
                return Some(panel.focus_handle(cx));
            }
        }
        if self.artifacts_shown
            && let Some(view) = &self.artifacts
        {
            return Some(view.focus_handle(cx));
        }
        self.selected_view().map(|view| view.focus_handle(cx))
    }

    /// Rend le focus à ce qui est affiché (sans rien, à la racine) : la saisie repart au terminal sans clic.
    fn restore_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.focus_target(cx) {
            Some(handle) => window.focus(&handle, cx),
            None => window.focus(&self.focus_handle, cx),
        }
    }

    /// Ferme le menu et rend le focus à la session affichée (sans elle, à la racine) : la saisie repart au terminal.
    pub(crate) fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.menu.take() else {
            return;
        };
        match (menu.tree_of, menu.changes_of) {
            // Un menu de l'arbre rend le clavier à l'arbre : ↩ et ⌘⌫ y agissent sur la ligne, pas sur l'onglet ouvert.
            // Une entrée qui déplace le focus elle-même (champ en place, pane de l'agent) le fait après ce retour.
            (Some(panel), _) => panel.update(cx, |panel, cx| panel.focus_tree(window, cx)),
            // Un menu de la liste de Changes le rend à la liste : `↑` `↓` et `espace` y agissent sur la ligne.
            // Celui du diff le rend au diff, qui garde `⌘⌥C`.
            (None, Some(panel)) if menu.changes_to_diff => {
                panel.update(cx, |panel, cx| panel.focus_diff(window, cx));
            }
            (None, Some(panel)) => panel.update(cx, |panel, cx| panel.focus_list(window, cx)),
            (None, None) => self.restore_focus(window, cx),
        }
        self.sync_page_cover(window, cx);
        cx.notify();
    }

    /// `⌘O` (et `⌘N`) : le dialogue du Finder, dossiers seulement, avec son bouton « Nouveau dossier ».
    fn open_project_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(window, cx);
        if self.project_dialog_open {
            return;
        }
        self.project_dialog_open = true;
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Ouvrir".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let path = match chosen.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                _ => None,
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.project_dialog_open = false;
                if let Some(path) = path {
                    this.add_project_flow(path, window, cx);
                }
            });
        })
        .detach();
    }

    /// `⌘⇧O` : le dialogue du Finder, fichiers seulement (écart E38 : GPUI n'offre ni fichiers cachés ni dossier de
    /// départ), puis le fichier choisi s'ouvre dans le volet Fichiers, même hors de tout projet. Sans projet de
    /// contexte, ou dialogue déjà ouvert, rien.
    fn open_file_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(window, cx);
        if self.file_dialog_open || self.panel_project().is_none() {
            return;
        }
        self.file_dialog_open = true;
        hud::log_event("file dialog opened");
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Ouvrir".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            // Une annulation rend `Ok(None)` ; un échec du dialogue est journalisé avec son texte.
            let (path, failure) = match chosen.await {
                Ok(Ok(Some(mut paths))) => (paths.pop(), None),
                Ok(Ok(None)) => (None, None),
                Ok(Err(error)) => (None, Some(error.to_string())),
                Err(error) => (None, Some(error.to_string())),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.file_dialog_open = false;
                hud::log_event(&format!("file dialog closed chosen={}", path.is_some()));
                if let Some(failure) = failure {
                    hud::log_event(&format!("file dialog failed: {failure}"));
                }
                if let Some(path) = path {
                    // Un fichier choisi à la main est un choix durable : l'onglet est permanent, pas un aperçu.
                    this.open_in_files(
                        PathTarget::File(FileTarget {
                            path,
                            line: None,
                            column: None,
                        }),
                        true,
                        window,
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Ce que devient un dossier choisi (spec §4.1) : refusé, sélectionné s'il est déjà listé, sinon classé hors du
    /// fil d'interface puis ajouté, initialisé ou soumis à confirmation.
    pub(crate) fn add_project_flow(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !project::path_is_acceptable(&path) {
            alert(
                window,
                cx,
                "Ce chemin contient un saut de ligne ou une tabulation : tmux ne peut pas le recevoir.",
                None,
            );
            return;
        }
        if self.state().project(&path).is_some() {
            self.register_project(path, window, cx);
            return;
        }
        let probe = path.clone();
        let kind = cx.background_spawn(async move { project::classify(&probe) });
        cx.spawn_in(window, async move |this, cx| {
            let kind = kind.await;
            let _ = this.update_in(cx, |this, window, cx| match kind {
                Ok(FolderKind::InsideRepository) => this.register_project(path, window, cx),
                Ok(FolderKind::Empty) => this.init_then_register(path, window, cx),
                Ok(FolderKind::NotEmpty) => this.confirm_init(path, window, cx),
                Err(error) => {
                    hud::log_event(&format!(
                        "project classify failed path={}: {error}",
                        path.display()
                    ));
                    alert(window, cx, "Impossible de lire ce dossier.", None);
                }
            });
        })
        .detach();
    }

    /// Ce que macOS demande d'ouvrir (CLI `idema`, action rapide du Finder) : chaque dossier suit le chemin de `⌘O`. Un
    /// fichier n'est pas un projet : il est écarté, et le journal le dit.
    pub(crate) fn open_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for path in paths {
            if let Some(folder) = project::folder_on_disk(&path) {
                self.add_project_flow(folder, window, cx);
            } else {
                hud::log_event(&format!(
                    "open request ignored path={}: not a folder",
                    path.display()
                ));
            }
        }
    }

    /// Dossier non vide et sans dépôt : on demande avant de toucher au disque. Le bouton « Annuler » est un vrai
    /// bouton d'annulation, donc Échap le choisit.
    fn confirm_init(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let detail = path.to_string_lossy().into_owned();
        let answer = window.prompt(
            PromptLevel::Info,
            "Initialiser un dépôt Git ici ?",
            Some(&detail),
            &[
                PromptButton::new("Initialiser"),
                PromptButton::cancel("Annuler"),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.init_then_register(path, window, cx);
                });
            }
        })
        .detach();
    }

    /// `git init` hors du fil d'interface, puis l'ajout. Un échec ne laisse rien dans la liste.
    fn init_then_register(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let target = path.clone();
        // Le Finder sème des `.DS_Store` partout : le dépôt neuf les ignore d'emblée. Un `.gitignore` qui n'a pas pu
        // s'écrire ne défait pas l'init, il se dit au journal.
        let init = cx.background_spawn(async move {
            project::git_init(&target)?;
            if let Err(error) = project::ignore_ds_store(&target) {
                hud::log_event(&format!(
                    "gitignore write failed path={}: {error}",
                    target.display()
                ));
            }
            Ok::<(), std::io::Error>(())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = init.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(()) => this.register_project(path, window, cx),
                Err(error) => {
                    hud::log_event(&format!("git init failed path={}: {error}", path.display()));
                    alert(
                        window,
                        cx,
                        "Impossible d'initialiser un dépôt Git ici.",
                        None,
                    );
                }
            });
        })
        .detach();
    }

    /// Ajoute le projet s'il manque, le déplie et le prend pour projet sélectionné.
    fn register_project(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let now = rfc3339(SystemTime::now());
        let state = self.state_mut();
        state.add_project(&path, &now);
        state.set_expanded(&path, true);
        self.selected_project = Some(path);
        self.project_wins = true;
        self.save();
        self.sync_servers_projects(cx);
        self.refresh_missing(cx);
        self.sync_branch(cx);
        // Le projet de contexte change : le volet droit ouvert (Fichiers ou Terminal) suit.
        self.sync_right_panel(window, cx);
        cx.notify();
    }

    /// Clic droit sur une ligne de projet : épingler ou désépingler, puis retirer de la liste.
    pub(crate) fn open_project_menu(
        &mut self,
        path: PathBuf,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pinned = self
            .state()
            .project(&path)
            .is_some_and(|project| project.pinned_at.is_some());
        let pin_path = path.clone();
        let spec = MenuSpec {
            title: None,
            entries: vec![
                MenuEntry::Item(MenuItemSpec::new(
                    if pinned { "Désépingler" } else { "Épingler" },
                    move |this, _window, cx| this.set_project_pinned(&pin_path, !pinned, cx),
                )),
                MenuEntry::Separator,
                MenuEntry::Item(MenuItemSpec::new(
                    "Retirer de la liste",
                    move |this, window, cx| this.remove_project(&path, window, cx),
                )),
            ],
        };
        self.open_menu(position, spec, window, cx);
    }

    /// Le menu de la légende « Projets » : l'ouverture d'un projet, comme `⌘O`. `position` est le coin haut-gauche du
    /// menu : la barre le pose sous le bouton « … », pour que celui-ci reste découvert et qu'un second clic le ferme.
    pub(crate) fn open_projects_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let spec = MenuSpec {
            title: None,
            entries: vec![MenuEntry::Item(
                MenuItemSpec::new("Ouvrir un projet…", |this, window, cx| {
                    this.open_project_dialog(window, cx);
                })
                .shortcut("⌘O"),
            )],
        };
        self.open_menu(position, spec, window, cx);
    }

    fn set_project_pinned(&mut self, path: &Path, pinned: bool, cx: &mut Context<Self>) {
        let now = rfc3339(SystemTime::now());
        self.state_mut()
            .set_pinned(path, pinned.then_some(now.as_str()));
        self.save();
        self.publish_fleet(cx);
        cx.notify();
    }

    /// Retire le projet de la liste, jamais du disque. Refusé tant qu'un de ses serveurs lancés par IdemA vit ; ses
    /// sessions ouvertes se ferment d'abord, après confirmation (Échap annule).
    fn remove_project(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let servers = self
            .servers
            .as_ref()
            .map_or(0, |servers| servers.read(cx).live_sessions_of(path));
        if let Some((title, detail)) = removal_refusal(&project::display_name(path), servers) {
            alert(window, cx, title, Some(&detail));
            return;
        }
        let sessions = project_session_names(&self.entries, path);
        if !sessions.is_empty() {
            self.confirm_project_removal(path.to_path_buf(), sessions, window, cx);
            return;
        }
        self.state_mut().remove_project(path);
        self.sync_servers_projects(cx);
        // Le volet Changes du projet part avec lui : on le masque d'abord, pour que sa surveillance s'arrête hors du fil
        // d'interface (`stop_watch`) au lieu de bloquer à la chute de l'entité.
        if let Some(panel) = self.changes_panels.remove(path) {
            panel.update(cx, |panel, cx| panel.set_shown(false, window, cx));
        }
        // Ses onglets partent sur le disque, puis son volet et sa surveillance sont lâchés.
        // Le retrait est un geste explicite : il a lieu même si un texte n'a pas pu partir, mais la bulle le dit.
        if let Some(panel) = self.files_panels.remove(path) {
            let unsaved = panel.update(cx, |panel, cx| panel.save_all("remove", cx));
            if !unsaved.is_empty() {
                alert(
                    window,
                    cx,
                    "Du texte n'a pas pu être enregistré.",
                    Some(&format!(
                        "Le projet est retiré quand même : le texte non enregistré de {} est perdu.",
                        unsaved.join(", ")
                    )),
                );
            }
        }
        if self.selected_project.as_deref() == Some(path) {
            self.selected_project = None;
            self.project_wins = false;
        }
        self.save();
        self.refresh_missing(cx);
        self.sync_branch(cx);
        self.sync_right_panel(window, cx);
        cx.notify();
    }

    /// Le projet a encore des sessions : on demande avant de les fermer, programmes en cours nommés.
    fn confirm_project_removal(
        &mut self,
        path: PathBuf,
        sessions: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let running: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| sessions.contains(&entry.summary.name))
            .filter_map(|entry| entry.view.as_ref())
            .flat_map(|view| view.read(cx).running_programs(cx))
            .collect();
        let detail = removal_detail(sessions.len(), &running);
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Retirer « {} » ?", project::display_name(&path)),
            Some(&detail),
            &[
                PromptButton::new("Fermer et retirer"),
                PromptButton::cancel("Annuler"),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.close_sessions_then_remove(path, sessions, window, cx);
                });
            }
        })
        .detach();
    }

    /// `kill-session` de chaque session hors du fil d'interface, puis leurs lignes partent et le retrait reprend : une
    /// session ouverte entre-temps redemande. Une fermeture ratée garde le projet dans la liste et le dit.
    fn close_sessions_then_remove(
        &mut self,
        path: PathBuf,
        sessions: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.server.clone() else {
            return;
        };
        let killed = cx.background_spawn(async move {
            sessions
                .into_iter()
                .map(|name| {
                    // Une session déjà partie (entrée que la réconciliation n'a pas encore retirée) est fermée.
                    let result = server.kill_session(&name).or_else(|error| {
                        if server.has_session(&name) {
                            Err(error)
                        } else {
                            Ok(())
                        }
                    });
                    (name, result)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let results = killed.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let mut failed = false;
                for (name, result) in results {
                    match result {
                        Ok(()) => {
                            this.forget_closed_session(&name);
                            this.remove_entry(&name, window, cx);
                        }
                        Err(error) => {
                            failed = true;
                            hud::log_event(&format!(
                                "session kill failed session={}: {error}",
                                key_label(&name)
                            ));
                        }
                    }
                }
                if failed {
                    alert(
                        window,
                        cx,
                        "Une session n'a pas pu être fermée.",
                        Some("Le projet reste dans la liste."),
                    );
                } else {
                    this.remove_project(&path, window, cx);
                }
            });
        })
        .detach();
    }

    /// L'entrée du `+` et de `⌘T` : l'agent par défaut du lanceur, sinon le shell seul.
    fn default_choice(&self) -> LaunchChoice {
        self.config
            .default_agent()
            .cloned()
            .map_or(LaunchChoice::Shell, LaunchChoice::Agent)
    }

    /// `⌘T` : avec un bouillon sélectionné, un nouveau bouillon. Sinon l'agent par défaut, dans le projet que
    /// l'écran vide affiche (`context_project` : le dernier geste gagne entre session et projet sélectionnés, sinon le premier projet). Sans projet, `⌘T` ouvre le dialogue de `⌘O` : seul un bouillon naît hors d'un projet.
    fn new_session_here(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_is_ideation() {
            self.new_ideation_session(window, cx);
            return;
        }
        // Une session distante visée (le dernier geste n'est pas un projet) : `⌘T` crée sur sa machine.
        if !self.project_wins
            && let Some(connection) = self
                .selected_entry()
                .and_then(|entry| entry.summary.machine.clone())
        {
            self.new_remote_session_default(&connection, window, cx);
            return;
        }
        match self.context_project() {
            Some(project) => self.create_session(project, self.default_choice(), window, cx),
            None => self.open_project_dialog(window, cx),
        }
    }

    /// Clic sur le `+` d'un projet : l'agent par défaut.
    pub(crate) fn new_session_default(
        &mut self,
        project: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_session(project, self.default_choice(), window, cx);
    }

    /// Clic droit sur le `+` : un sous-menu par harness (coche et `⌘T` sur le défaut), puis « Shell seul ». Sans agent
    /// configuré, le défaut est « Shell seul ».
    pub(crate) fn open_launcher_menu(
        &mut self,
        project: PathBuf,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let default_index = self.config.default_agent().and_then(|default| {
            self.config
                .agents
                .iter()
                .position(|agent| std::ptr::eq(agent, default))
        });
        let mut entries = agent_menu_entries(&self.config.agents, |index, agent| {
            let (target, choice) = (project.clone(), LaunchChoice::Agent(agent.clone()));
            let item = MenuItemSpec::new(agent.name.clone(), move |this, window, cx| {
                this.create_session(target.clone(), choice.clone(), window, cx);
            })
            .checked(default_index == Some(index));
            if default_index == Some(index) {
                item.shortcut("⌘T")
            } else {
                item
            }
        });
        if !entries.is_empty() {
            entries.push(MenuEntry::Separator);
        }
        let mut shell = MenuItemSpec::new(SHELL_ENTRY, move |this, window, cx| {
            this.create_session(project.clone(), LaunchChoice::Shell, window, cx);
        })
        .checked(default_index.is_none());
        if default_index.is_none() {
            shell = shell.shortcut("⌘T");
        }
        entries.push(MenuEntry::Item(shell));
        let spec = MenuSpec {
            title: Some("Nouvelle session d'agent".into()),
            entries,
        };
        self.open_menu(position, spec, window, cx);
    }

    /// Crée une session dans `project` (spec §4.2) : la création tmux se fait hors du fil d'interface, puis la
    /// session est attachée à la taille de la zone centrale, insérée dans la barre et sélectionnée. La commande
    /// de l'agent part quand le pane est prêt (`LaunchGate`), jamais à l'aveugle.
    pub(crate) fn create_session(
        &mut self,
        project: PathBuf,
        choice: LaunchChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.server.clone() else {
            return;
        };
        // Pas de session dans un projet inconnu, ni dans un dossier disparu.
        if self.state().project(&project).is_none() || self.is_missing(&project) {
            return;
        }
        let taken: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| entry.summary.project == project)
            .map(|entry| entry.summary.display_name.clone())
            .chain(
                self.pending_names
                    .iter()
                    .filter(|(pending_project, _)| *pending_project == project)
                    .map(|(_, name)| name.clone()),
            )
            .collect();
        let taken: Vec<&str> = taken.iter().map(String::as_str).collect();
        let display_name = default_session_name(choice.label(), &taken);
        let command = match &choice {
            LaunchChoice::Agent(agent) => Some(agent.command.clone()),
            LaunchChoice::Shell => None,
        };
        let request = SessionRequest {
            project,
            display_name,
            agent: choice.label().to_owned(),
            command,
            remote_name: None,
            machine: None,
        };
        self.spawn_session(server, request, window, cx);
    }

    /// L'agent par défaut d'une machine, d'après la dernière lecture de son `~/.idnma.yml` : celui que l'agent par défaut local
    /// désigne si la machine l'a, sinon son premier. `None` : le shell seul.
    fn remote_default_agent(&self, connection: &str) -> Option<IdnmaAgentInfo> {
        let machine = &self.machines[self.machine_index(connection)?];
        let agents = machine.facts.as_ref()?.agents.as_deref()?;
        default_remote_agent(self.config.default_agent(), agents).cloned()
    }

    /// Clic sur le « + » d'une machine : l'agent par défaut de la machine, sinon le shell seul.
    pub(crate) fn new_remote_session_default(
        &mut self,
        connection: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let agent = self.remote_default_agent(connection);
        self.new_remote_session(connection, agent, window, cx);
    }

    /// Crée une session sur la machine `connection` (spec ssh-tmux §3.6) : `new-session` dans le home de l'utilisateur
    /// distant, à la taille de la zone centrale, sur le canal bref de la machine, hors du fil d'interface. La commande
    /// `idnma run <clé>` est tapée quand le pane est prêt (`LaunchGate`), jamais à l'aveugle. `None` : le shell seul.
    pub(crate) fn new_remote_session(
        &mut self,
        connection: &str,
        agent: Option<IdnmaAgentInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.machine_index(connection) else {
            return;
        };
        let machine = &self.machines[index];
        if machine.status != MachineStatus::Connected {
            return;
        }
        let Some(pool) = machine.pool.as_ref() else {
            return;
        };
        let server = pool.brief();
        let command = agent
            .as_ref()
            .and_then(|agent| idnma_run_command(&agent.key));
        if agent.is_some() && command.is_none() {
            hud::log_event(&format!(
                "remote session create skipped machine={connection}: agent key is not a plain word"
            ));
            return;
        }
        let label = agent.as_ref().map_or_else(
            || SHELL_SESSION_NAME.to_owned(),
            |agent| agent.label.clone().unwrap_or_else(|| agent.key.clone()),
        );
        // Les noms déjà pris parmi les sessions de la machine, ceux d'une création en vol compris (leur projet est vide).
        let taken: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| entry.summary.machine.as_deref() == Some(connection))
            .map(|entry| entry.summary.display_name.clone())
            .chain(
                self.pending_names
                    .iter()
                    .filter(|(project, _)| project.as_os_str().is_empty())
                    .map(|(_, name)| name.clone()),
            )
            .collect();
        let taken: Vec<&str> = taken.iter().map(String::as_str).collect();
        let request = SessionRequest {
            // Une session distante n'a pas de projet du Mac : le dossier de départ est le home de la machine
            // (`spawn_session`).
            project: PathBuf::new(),
            display_name: default_session_name(&label, &taken),
            agent: agent.map_or_else(|| SHELL_SESSION_NAME.to_owned(), |agent| agent.key),
            command,
            remote_name: None,
            machine: Some(connection.to_owned()),
        };
        self.spawn_session(server, request, window, cx);
    }

    /// Clic droit sur le « + » d'une machine : ses agents (relus de son `~/.idnma.yml`, le menu s'ouvre sur la dernière
    /// lecture), puis « Shell seul ».
    pub(crate) fn open_remote_launcher_menu(
        &mut self,
        connection: &str,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.machine_index(connection) else {
            return;
        };
        let agents: Option<Vec<IdnmaAgentInfo>> = self.machines[index]
            .facts
            .as_ref()
            .and_then(|facts| facts.agents.clone());
        let default_key = self.remote_default_agent(connection).map(|agent| agent.key);
        let mut entries = Vec::new();
        for line in remote_menu_lines(agents.as_deref(), default_key.as_deref()) {
            entries.push(match line {
                RemoteMenuLine::Agent {
                    key,
                    text,
                    default,
                    runnable,
                } => {
                    let agent = agents
                        .as_deref()
                        .and_then(|agents| agents.iter().find(|agent| agent.key == key))
                        .cloned();
                    let logo = agent.as_ref().and_then(remote_agent_logo);
                    let target = connection.to_owned();
                    let mut item = MenuItemSpec::new(text, move |this, window, cx| {
                        this.new_remote_session(&target, agent.clone(), window, cx);
                    })
                    .logo(logo)
                    .checked(default)
                    .enabled(runnable);
                    if default {
                        item = item.shortcut("⌘T");
                    }
                    MenuEntry::Item(item)
                }
                RemoteMenuLine::Note(text) => MenuEntry::Note(text.into()),
                RemoteMenuLine::Separator => MenuEntry::Separator,
                RemoteMenuLine::Shell { default } => {
                    let target = connection.to_owned();
                    let mut item = MenuItemSpec::new(SHELL_ENTRY, move |this, window, cx| {
                        this.new_remote_session(&target, None, window, cx);
                    })
                    .checked(default);
                    if default {
                        item = item.shortcut("⌘T");
                    }
                    MenuEntry::Item(item)
                }
            });
        }
        let spec = MenuSpec {
            title: Some("Nouvelle session d'agent".into()),
            entries,
        };
        self.open_menu(position, spec, window, cx);
        self.reread_machine_agents(connection, cx);
    }

    /// Relit le `~/.idnma.yml` de la machine (`cat` sur son canal bref) pour la prochaine ouverture du menu. Une relecture
    /// qui échoue laisse la dernière lecture. Le contenu n'est jamais journalisé.
    fn reread_machine_agents(&mut self, connection: &str, cx: &mut Context<Self>) {
        let Some(index) = self.machine_index(connection) else {
            return;
        };
        let machine = &self.machines[index];
        if machine.status != MachineStatus::Connected {
            return;
        }
        let Some(spec) = machine
            .pool
            .as_ref()
            .and_then(|pool| pool.brief().remote_spec().cloned())
        else {
            return;
        };
        let turn = Arc::clone(&machine.adhoc);
        let read = cx.background_spawn(async move {
            let _held = turn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            read_agents(&spec)
        });
        let target = connection.to_owned();
        cx.spawn(async move |this, cx| {
            let Some(agents) = read.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let Some(index) = this.machine_index(&target) else {
                    return;
                };
                if let Some(facts) = this.machines[index].facts.as_mut()
                    && facts.agents != agents
                {
                    facts.agents = agents;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// `+` de la section « Bouillon de neurones » et `⌘T` sur un bouillon : une session dans le dossier de la section
    /// `[ideation]`, sous un nom Remote Control libre. Sans cette section, rien n'est créé. Le dossier est vérifié ici,
    /// avant tout appel tmux. Un bouillon n'est pas un projet de `state.json` : la garde de `create_session` ne
    /// s'applique pas.
    pub(crate) fn new_ideation_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.ideation().cloned() else {
            hud::log_event("ideation create skipped reason=disabled");
            return;
        };
        let dir = config.dir_path();
        if !dir.is_dir() {
            hud::log_event("ideation create skipped reason=missing-dir");
            alert(
                window,
                cx,
                "Dossier d'idéation introuvable",
                Some(&dir.to_string_lossy()),
            );
            return;
        }
        let Some(server) = self.server.clone() else {
            return;
        };
        let ideation_sessions = || self.entries.iter().filter(|entry| entry.summary.ideation);
        let labels: Vec<&str> = ideation_sessions()
            .map(|entry| entry.summary.display_name.as_str())
            .chain(
                self.pending_names
                    .iter()
                    .filter(|(project, _)| project == &dir)
                    .map(|(_, name)| name.as_str()),
            )
            .collect();
        let display_name = default_session_name(&config.label, &labels);
        let taken = taken_remote_names(
            ideation_sessions().map(|entry| entry.remote_name.as_str()),
            &self.pending_remote_names,
        );
        let remote_name = next_remote_name(&remote_prefix(&dir), &taken);
        let request = SessionRequest {
            project: dir,
            display_name,
            agent: config.label.clone(),
            command: Some(ideation_command(&config, &remote_name)),
            remote_name: Some(remote_name),
            machine: None,
        };
        self.spawn_session(server, request, window, cx);
    }

    /// Le chemin commun d'une création : les noms en attente (pour que ni la relecture ni un second clic ne les
    /// reprennent), la création tmux hors du fil d'interface, puis `on_session_created`.
    fn spawn_session(
        &mut self,
        server: Arc<TmuxServer>,
        request: SessionRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_names
            .push((request.project.clone(), request.display_name.clone()));
        if let Some(remote_name) = &request.remote_name {
            self.pending_remote_names.push(remote_name.clone());
        }
        let grid = self.central_grid(window, Preset::Single.column_gaps());
        let columns = u16::try_from(grid.columns).unwrap_or(u16::MAX).max(1);
        let lines = u16::try_from(grid.lines).unwrap_or(u16::MAX).max(1);
        // Une session distante démarre dans le home de l'utilisateur de la machine (`request.project` est vide).
        let project = match server.remote_spec() {
            Some(spec) if request.project.as_os_str().is_empty() => PathBuf::from(&spec.home),
            _ => request.project.clone(),
        };
        let (display_name, agent, remote_name) = (
            request.display_name.clone(),
            request.agent.clone(),
            request.remote_name.clone(),
        );
        // Sur une machine, la création passe par le canal bref comme les autres appels ponctuels, et pose les deux options de
        // serveur permises : le serveur existe désormais, même si la session est la première de la machine.
        let turn = request
            .machine
            .as_deref()
            .and_then(|connection| self.machine_index(connection))
            .map(|index| Arc::clone(&self.machines[index].adhoc));
        let created = cx.background_spawn(async move {
            let _held = turn.as_ref().map(|turn| {
                turn.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            });
            let created = server.create_session(&NewTmuxSession {
                project: &project,
                display_name: &display_name,
                agent: &agent,
                columns,
                lines,
                remote_name: remote_name.as_deref(),
            });
            if created.is_ok() && server.is_remote() {
                set_extended_keys(&server);
            }
            created
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = created.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.release_pending(&request);
                this.on_session_created(request, result, window, cx);
            });
        })
        .detach();
    }

    /// Retire de l'attente les noms d'une création terminée (réussie ou non).
    fn release_pending(&mut self, request: &SessionRequest) {
        clear_pending(
            &mut self.pending_names,
            &mut self.pending_remote_names,
            &request.project,
            &request.display_name,
            request.remote_name.as_deref(),
        );
    }

    fn on_session_created(
        &mut self,
        request: SessionRequest,
        result: Result<CreatedSession, ServerError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SessionRequest {
            project,
            display_name,
            agent,
            command,
            remote_name,
            machine,
        } = request;
        let ideation = remote_name.is_some();
        let created = match result {
            Ok(created) => created,
            Err(error) => {
                hud::log_event(&format!(
                    "session create failed project={}: {error}",
                    project.display()
                ));
                let Explained { text, detail } = explain_server(&error);
                let detail = match detail {
                    Some(raw) => format!("{text}\n{raw}"),
                    None => text,
                };
                alert(window, cx, "Impossible de créer la session.", Some(&detail));
                // Le dossier a disparu entre deux relectures : le projet se grise tout de suite.
                if !ideation && matches!(error, ServerError::ProjectGone(_)) {
                    self.refresh_missing(cx);
                }
                return;
            }
        };
        // Sur une machine, la clé de la session porte sa connexion ; le dossier du projet n'est pas un projet du Mac.
        let key = match &machine {
            Some(connection) => remote_session_key(connection, &created.name),
            None => created.name.clone(),
        };
        self.entries.push(SessionEntry {
            summary: SessionSummary {
                name: key.clone(),
                project: project.clone(),
                display_name,
                created: created.created,
                state: ActivityState::Idle,
                ideation,
                machine: machine.clone(),
            },
            agent,
            remote_name: remote_name.unwrap_or_default(),
            pane: created.pane,
            preset: Preset::Single,
            auto_layout: true,
            view: None,
            attempts: 0,
            unreachable: None,
            state_since: SystemTime::now(),
            _subscription: None,
        });
        self.entries
            .sort_by(|a, b| session_order(&a.summary, &b.summary));
        let size = Some(self.central_grid(window, Preset::Single.column_gaps()));
        let launches = command
            .map(|command| PaneLaunch {
                pane: created.pane,
                command,
                delay: Duration::ZERO,
                pacer: None,
            })
            .into_iter()
            .collect();
        if let Err(error) = self.attach(&key, size, launches, window, cx) {
            hud::log_event(&format!(
                "attach failed session={}: {error}",
                key_label(&key)
            ));
            // Mêmes essais qu'au lancement (écart E34), puis « Session injoignable » : pas d'écran vide. La commande
            // d'agent n'est pas rejouée au nouvel essai (`reattach_later` attache sans commande) : le pane ouvre un shell.
            self.on_attach_lost(&key, error.to_string(), None, window, cx);
        }
        // La session choisie doit se voir : sa machine ou son projet se déplie, ou la section pour un bouillon (`select`
        // écrit `state.json`).
        if let Some(connection) = &machine {
            self.state_mut().set_machine_collapsed(connection, false);
        } else if ideation {
            self.state_mut().ui.ideation_collapsed = false;
        } else {
            self.state_mut().set_expanded(&project, true);
        }
        self.select(&key, window, cx);
    }

    /// Les entrées « Renommer » et « Fermer la session » d'un menu de session.
    fn session_gesture_entries(name: &str) -> [MenuEntry; 2] {
        let (renamed, closed) = (name.to_owned(), name.to_owned());
        [
            MenuEntry::Item(MenuItemSpec::new("Renommer", move |this, window, cx| {
                this.start_rename(&renamed, window, cx);
            })),
            MenuEntry::Item(MenuItemSpec::new(
                "Fermer la session",
                move |this, window, cx| this.close_session(&closed, window, cx),
            )),
        ]
    }

    /// Clic droit sur une ligne de session.
    pub(crate) fn open_session_menu(
        &mut self,
        name: &str,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let spec = MenuSpec {
            title: None,
            entries: Self::session_gesture_entries(name).into(),
        };
        self.open_menu(position, spec, window, cx);
    }

    /// Le menu de la session affichée (`⋮` et clic droit dans le terminal) : la section « Layout » (vignettes, puis
    /// les panes masqués), lancer un agent dans ce pane, puis renommer et fermer. Les agents ne sont actifs qu'au
    /// prompt ; sinon une note dit pourquoi (écart E22 : pas de titre de menu, des titres de section).
    fn session_actions_spec(&self, cx: &App) -> Option<MenuSpec> {
        let step = Instant::now();
        let entry = self.selected_entry()?;
        let session = entry.summary.name.clone();
        let view = entry.view.as_ref().map(|view| view.read(cx));
        let at_prompt = view.is_some_and(|view| view.at_prompt(cx));
        let mut entries: Vec<MenuEntry> = Vec::new();
        if let Some(view) = view {
            let target = session.clone();
            entries.push(MenuEntry::Heading("Layout".into()));
            entries.push(MenuEntry::Layouts {
                current: view.preset(),
                on_select: Rc::new(move |preset, this, window, cx| {
                    this.apply_layout(&target, preset, window, cx);
                }),
            });
            let hidden = view.hidden_count();
            if hidden > 0 {
                entries.push(MenuEntry::HiddenPanes {
                    count: hidden,
                    dot: hidden_panes_dot(view.hidden_state(cx)),
                });
            }
            entries.push(MenuEntry::Separator);
        }
        if !self.config.agents.is_empty() {
            entries.push(MenuEntry::Heading("Lancer dans ce pane".into()));
            if !at_prompt {
                entries.push(MenuEntry::Note("Un programme tourne déjà".into()));
            }
            entries.extend(agent_menu_entries(&self.config.agents, |_, agent| {
                let (target, chosen) = (session.clone(), agent.clone());
                MenuItemSpec::new(agent.name.clone(), move |this, _window, cx| {
                    this.launch_agent_in_pane(&target, &chosen, cx);
                })
                .enabled(at_prompt)
            }));
            entries.push(MenuEntry::Separator);
        }
        entries.extend(self.reclaim_entries(&session, &entry.summary.project, at_prompt));
        hud::log_event(&format!(
            "session_menu.spec us={}",
            step.elapsed().as_micros()
        ));
        entries.extend(Self::session_gesture_entries(&session));
        Some(MenuSpec {
            title: None,
            entries,
        })
    }

    /// La section « Ramener dans ce pane » : un sous-menu par projet qui a des sessions hors d'IdemA, puis un séparateur.
    /// Vide sans session. La racine du projet du pane vient du dernier balayage ; une session créée depuis le dernier
    /// tour n'y est pas encore, le projet lui-même fait alors office de racine.
    fn reclaim_entries(&self, session: &str, project: &Path, at_prompt: bool) -> Vec<MenuEntry> {
        // Les processus que « Ramener » arrête sont ceux de ce Mac : rien à proposer sur une session distante.
        if !offers_local_tools(session) {
            return Vec::new();
        }
        let current_root = self
            .reclaim
            .roots
            .get(project)
            .map_or(project, PathBuf::as_path);
        let groups = reclaim_groups(
            &self.reclaim.sessions,
            current_root,
            at_prompt,
            &self.config.agents,
        );
        if groups.is_empty() {
            return Vec::new();
        }
        let mut entries = vec![MenuEntry::Heading("Ramener dans ce pane".into())];
        if !at_prompt {
            entries.push(MenuEntry::Note("Un programme tourne déjà".into()));
        }
        for group in groups {
            let items = group
                .items
                .into_iter()
                .map(|item| {
                    let (target, outside) = (session.to_owned(), item.session.clone());
                    MenuItemSpec::new(item.session.name.clone(), move |this, _window, cx| {
                        this.start_reclaim(target.clone(), outside.clone(), cx);
                    })
                    .logo(item.logo)
                    .enabled(item.enabled)
                })
                .collect();
            entries.push(MenuEntry::Submenu {
                label: group.label.into(),
                items,
                openable: true,
            });
        }
        entries.push(MenuEntry::Separator);
        entries
    }

    /// Passe la session `session` au preset `preset` (spec §4.4) : la vue lance l'opération, rejoint ou cache les
    /// panes, et réapplique le preset quand la zone change de taille (écart E17, dans `SessionView`).
    pub(crate) fn apply_layout(
        &mut self,
        session: &str,
        preset: Preset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .entry_index(session)
            .and_then(|index| self.entries[index].view.clone())
        else {
            return;
        };
        view.update(cx, |view, cx| view.apply_preset(preset, window, cx));
    }

    /// Clic droit dans la zone du terminal : le menu de la session, à la souris.
    fn open_pane_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Un bouillon n'a aucun menu dans son terminal (spec §3.4).
        if self.selected_is_ideation() {
            return;
        }
        if let Some(spec) = self.session_actions_spec(cx) {
            self.open_menu(position, spec, window, cx);
        }
    }

    /// Clic sur `⋮` : le même menu, sous le bouton.
    fn open_toolbar_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_is_ideation() {
            return;
        }
        if let Some(spec) = self.session_actions_spec(cx) {
            self.show_menu(spec, Placement::Toolbar, window, cx);
        }
    }

    /// Tape la commande de l'agent dans le pane de `session` s'il est au prompt (AC-18), puis note l'agent dans
    /// `@idema_agent` : le même pane, la même session, rien n'est recréé.
    fn launch_agent_in_pane(&mut self, session: &str, agent: &AgentEntry, cx: &mut Context<Self>) {
        let Some(index) = self.entry_index(session) else {
            return;
        };
        let Some(view) = self.entries[index].view.clone() else {
            return;
        };
        if !view.update(cx, |view, cx| view.launch_in_pane(&agent.command, cx)) {
            return;
        }
        let entry = &mut self.entries[index];
        entry.agent = agent.name.clone();
        // Le pane qui a reçu la commande : l'actif de la session.
        let pane = view.read(cx).active_pane().unwrap_or(entry.pane);
        cx.notify();
        let name = agent.name.clone();
        if let Some(write) = self.spawn_for(session, cx, move |server, _| {
            if let Err(error) = server.set_pane_option(pane, "@idema_agent", &name) {
                hud::log_event(&format!("agent option failed pane=%{}: {error}", pane.0));
            }
        }) {
            write.detach();
        }
    }

    /// « Ramener dans ce pane » : arrête la session `target`, qui tourne hors d'IdemA, puis la reprend dans le pane
    /// actif de `session`. Aucune confirmation : l'identité du processus est relue avant chaque signal
    /// (`stop_session`), et rien n'est signalé si elle a changé. L'arrêt dort entre deux lectures : il part sur le
    /// `background_executor`, et la suite revient sur le fil d'interface (`finish_reclaim`).
    pub(crate) fn start_reclaim(
        &mut self,
        session: String,
        target: OutsideSession,
        cx: &mut Context<Self>,
    ) {
        // « Ramener » ne propose rien sur une session distante : les processus qu'il arrête sont ceux du Mac.
        if !offers_local_tools(&session) {
            return;
        }
        let Some(view) = self
            .entry_index(&session)
            .and_then(|index| self.entries[index].view.clone())
        else {
            return;
        };
        let (at_prompt, active) = {
            let view = view.read(cx);
            (view.at_prompt(cx), view.active_pane())
        };
        let Some(pane) = active else {
            return;
        };
        // Sans commande de reprise, `may_start` refuse : la suite du geste en reçoit toujours une.
        let command = match may_start(self.reclaiming, at_prompt, target.resume.as_deref()) {
            Ok(command) => command.to_owned(),
            Err(bubble) => {
                if let Some(text) = bubble {
                    hud::log_event(&format!("reclaim aborted pid={} reason=busy", target.pid));
                    view.update(cx, |view, cx| view.show_failure(pane, text.to_owned(), cx));
                }
                return;
            }
        };
        let Some(reclaim_dirs) = reclaim_paths(
            std::env::var_os("HOME").as_deref(),
            std::env::var_os("CLAUDE_CONFIG_DIR").as_deref(),
        ) else {
            return;
        };
        // L'entrée du lanceur est choisie avant l'attente : la configuration peut changer pendant ce temps.
        let agent = agent_entry(&target.agent, target.model.as_deref(), &self.config.agents)
            .map(|entry| entry.name.clone());
        // La vue est retenue ici : le nom de la session peut changer pendant l'attente, il ne sert plus de clé.
        let held = view.downgrade();
        self.reclaiming = Some(target.pid);
        // Écrit avant l'arrêt : si l'application tombe pendant l'attente, l'identifiant de la conversation survit.
        hud::log_event(&format!(
            "reclaim stopping pid={} session={}",
            target.pid, target.session_id
        ));
        cx.spawn(async move |this, cx| {
            let stopping = target.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let control = SystemControl {
                        sessions_dir: reclaim_dirs.sessions,
                    };
                    stop_session(&stopping, &control, &STOP_TIMINGS)
                })
                .await;
            // L'espace de travail fermé pendant l'attente : plus personne à prévenir.
            let _ = this.update(cx, |this, cx| {
                let click = ReclaimClick {
                    held,
                    pane,
                    agent,
                    command,
                    target,
                };
                this.finish_reclaim(click, outcome, cx);
            });
        })
        .detach();
    }

    /// La suite d'un arrêt, sur le fil d'interface. La session et le pane ont pu disparaître ou changer de nom pendant
    /// l'attente : la vue retenue au clic reçoit la commande, jamais une autre. Si elle ne peut pas la recevoir, la
    /// commande part dans le presse-papiers et la bulle le dit, là où elle se verra (`show_reclaim_failure`).
    fn finish_reclaim(
        &mut self,
        click: ReclaimClick,
        outcome: StopOutcome,
        cx: &mut Context<Self>,
    ) {
        let ReclaimClick {
            held,
            pane,
            agent,
            command,
            target,
        } = click;
        let command = command.as_str();
        self.reclaiming = None;
        let view = held.upgrade();
        if let NextStep::Tell(text) = after_stop(outcome) {
            let reason = match outcome {
                StopOutcome::Survived => {
                    // L'arrêt n'est pas confirmé : la session est peut-être partie, la commande reste à portée.
                    cx.write_to_clipboard(ClipboardItem::new_string(command.to_owned()));
                    "survived"
                }
                _ => "changed",
            };
            hud::log_event(&format!(
                "reclaim aborted pid={} reason={reason}",
                target.pid
            ));
            self.show_reclaim_failure(view.as_ref(), pane, text, cx);
            return;
        }
        if let StopOutcome::Stopped(signal) = outcome {
            hud::log_event(&format!(
                "reclaim stopped pid={} session={} signal={}",
                target.pid,
                target.session_id,
                signal.name()
            ));
        }
        let typed = view
            .as_ref()
            .is_some_and(|view| view.update(cx, |view, cx| view.reclaim_in(pane, command, cx)));
        if let (true, Some(view)) = (typed, view.as_ref()) {
            self.note_reclaimed_agent(view, pane, agent, cx);
            self.note_reclaimed_resume(pane, &target, cx);
            hud::log_event(&format!("reclaim typed session={}", target.session_id));
            return;
        }
        // Le pane n'est plus au prompt, n'existe plus, ou son client est mort : la session est arrêtée, la commande
        // reste à portée.
        cx.write_to_clipboard(ClipboardItem::new_string(command.to_owned()));
        self.show_reclaim_failure(view.as_ref(), pane, MESSAGE_COPIED, cx);
        hud::log_event(&format!("reclaim not typed session={}", target.session_id));
    }

    /// Montre la bulle d'échec du geste : sur le pane retenu, sinon sur le pane actif de sa session, et si la session a
    /// disparu, sur le pane actif de la session affichée. Une session passée au second plan pendant l'attente la montre
    /// hors de la vue : le presse-papiers et le journal restent alors seuls à le dire.
    fn show_reclaim_failure(
        &self,
        view: Option<&Entity<SessionView>>,
        pane: PaneId,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = view
            && view.update(cx, |view, cx| view.show_failure(pane, text.to_owned(), cx))
        {
            return;
        }
        let shown = self.selected_entry().and_then(|entry| entry.view.clone());
        if let Some(shown) = shown {
            let active = shown.read(cx).active_pane();
            if let Some(active) = active {
                shown.update(cx, |view, cx| {
                    view.show_failure(active, text.to_owned(), cx)
                });
            }
        }
    }

    /// Note l'agent ramené dans `@idema_agent` du pane qui a reçu la commande, comme `launch_agent_in_pane` : le même
    /// pane, rien n'est recréé. L'entrée se retrouve par sa vue, pas par son nom.
    fn note_reclaimed_agent(
        &mut self,
        view: &Entity<SessionView>,
        pane: PaneId,
        agent: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.view.as_ref() == Some(view))
        else {
            return;
        };
        // Sans entrée du lanceur pour cette session, l'option est vidée : l'ancienne valeur (un `glm` qui tournait là)
        // nommerait un agent qui n'est plus celui du pane, et la restauration le relancerait.
        let name = agent.unwrap_or_default();
        self.entries[index].agent = name.clone();
        cx.notify();
        let session = self.entries[index].summary.name.clone();
        if let Some(write) = self.spawn_for(&session, cx, move |server, _| {
            if let Err(error) = server.set_pane_option(pane, "@idema_agent", &name) {
                hud::log_event(&format!("agent option failed pane=%{}: {error}", pane.0));
            }
        }) {
            write.detach();
        }
    }

    /// Note dans `@idema_resume` du pane qui a reçu la commande la conversation ramenée et sa commande de relance
    /// (`<uuid> <idnma run ..>`, un seul espace) : la restauration rejoue celle-là plutôt que l'entrée du lanceur, qui
    /// ne connaît ni le modèle, ni l'effort, ni le remote-control de la session d'origine (défaut D-1). Un échec va au
    /// journal sans la valeur (elle porte un nom libre) : l'erreur de tmux recopie ses arguments, seul son `stderr` passe.
    fn note_reclaimed_resume(&self, pane: PaneId, target: &OutsideSession, cx: &mut Context<Self>) {
        let (Some(relaunch), Some(server)) = (target.relaunch.as_deref(), self.server.clone())
        else {
            return;
        };
        let value = format!("{} {relaunch}", target.session_id);
        cx.background_spawn(async move {
            if let Err(error) = server.set_pane_option(pane, "@idema_resume", &value) {
                let reason = match &error {
                    ServerError::Tmux { stderr, .. } => stderr.as_str(),
                    _ => "unavailable",
                };
                hud::log_event(&format!("resume option failed pane=%{}: {reason}", pane.0));
            }
        })
        .detach();
    }

    /// `⌘W` : ferme la session sélectionnée.
    fn close_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = self
            .selected_entry()
            .map(|entry| entry.summary.name.clone())
        {
            self.close_session(&name, window, cx);
        }
    }

    /// Ferme la session `name`. Si un programme tourne au premier plan, on demande d'abord (Échap annule).
    pub(crate) fn close_session(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.entry_index(name) else {
            return;
        };
        let entry = &self.entries[index];
        let display = entry.summary.display_name.clone();
        let running = entry
            .view
            .as_ref()
            .map(|view| view.read(cx).running_programs(cx))
            .unwrap_or_default();
        if running.is_empty() {
            self.kill_session(name, window, cx);
            return;
        }
        let detail = running_detail(&running);
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Fermer « {display} » ?"),
            Some(&detail),
            &[
                PromptButton::new("Fermer la session"),
                PromptButton::cancel("Annuler"),
            ],
            cx,
        );
        let name = name.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.kill_session(&name, window, cx);
                });
            }
        })
        .detach();
    }

    /// `kill-session` hors du fil d'interface. La ligne part par `SessionEvent::Closed` ; une entrée sans vue ne
    /// recevra pas d'événement et part dès que la commande a réussi.
    fn kill_session(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(killed) = self.spawn_for(name, cx, |server, tmux| server.kill_session(tmux))
        else {
            return;
        };
        let target = name.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let result = killed.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    this.forget_closed_session(&target);
                    let detached = this
                        .entries
                        .iter()
                        .any(|entry| entry.summary.name == target && entry.view.is_none());
                    if detached {
                        this.remove_entry(&target, window, cx);
                    }
                }
                Err(error) => {
                    hud::log_event(&format!(
                        "session kill failed session={}: {error}",
                        key_label(&target)
                    ));
                }
            });
        })
        .detach();
    }

    /// Ouvre le champ de renommage à la place du nom, dans la ligne de la session. La ligne doit se voir : le
    /// projet se déplie et la barre se montre si besoin.
    pub(crate) fn start_rename(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.entry_index(name) else {
            return;
        };
        self.menu = None;
        let initial = self.entries[index].summary.display_name.clone();
        let project = self.entries[index].summary.project.clone();
        let ideation = self.entries[index].summary.ideation;
        let remote = self.entries[index].summary.machine.is_some();
        let state = self.state_mut();
        // Un bouillon se révèle en dépliant sa section : son dossier n'est jamais écrit dans `state.json`.
        let revealed = if ideation {
            let collapsed = state.ui.ideation_collapsed;
            state.ui.ideation_collapsed = false;
            collapsed || !state.ui.sidebar_visible
        } else if remote {
            // Une session distante n'a pas de projet à déplier.
            !state.ui.sidebar_visible
        } else {
            let collapsed = !state.is_expanded(&project);
            state.set_expanded(&project, true);
            collapsed || !state.ui.sidebar_visible
        };
        state.ui.sidebar_visible = true;
        if revealed {
            self.save();
        }
        let field = rename_field::open(&initial, window, cx);
        let subscription = cx.subscribe_in(&field, window, Self::on_rename_event);
        self.renaming = Some(Renaming {
            session: name.to_owned(),
            field,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn on_rename_event(
        &mut self,
        field: &Entity<RenameField>,
        event: &RenameEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Le renommage n'émet pas `Changed` : seul un champ de recherche le fait.
        if matches!(event, RenameEvent::Changed(_))
            || self
                .renaming
                .as_ref()
                .is_none_or(|renaming| renaming.field != *field)
        {
            return;
        }
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        // Entrée et Échap : le champ a encore le focus, il retourne à la session affichée. Une perte du focus a
        // déjà donné le focus ailleurs (clic, menu, autre session) : on n'y touche pas.
        if field.focus_handle(cx).is_focused(window) {
            self.restore_focus(window, cx);
        }
        if let RenameEvent::Confirm(text) = event {
            self.apply_rename(&renaming.session, text.clone(), cx);
        }
        cx.notify();
    }

    /// Écrit `@idema_name` hors du fil d'interface. La ligne change aussitôt ; un échec la remet.
    fn apply_rename(&mut self, name: &str, text: String, cx: &mut Context<Self>) {
        let Some(index) = self.entry_index(name) else {
            return;
        };
        // Un nom vide (ou fait d'espaces) annule ; le même nom ne change rien.
        if text.is_empty() || self.entries[index].summary.display_name == text {
            return;
        }
        let written = text.clone();
        let Some(write) = self.spawn_for(name, cx, move |server, tmux| {
            server.set_session_option(tmux, "@idema_name", &written)
        }) else {
            return;
        };
        let previous =
            std::mem::replace(&mut self.entries[index].summary.display_name, text.clone());
        self.publish_fleet(cx);
        let target = name.to_owned();
        cx.spawn(async move |this, cx| {
            if let Err(error) = write.await {
                hud::log_event(&format!(
                    "rename failed session={}: {error}",
                    key_label(&target)
                ));
                let _ = this.update(cx, |this, cx| {
                    if let Some(index) = this.entry_index(&target)
                        && this.entries[index].summary.display_name == text
                    {
                        this.entries[index].summary.display_name = previous;
                        this.publish_fleet(cx);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// Le champ de renommage de la session `name`, s'il est ouvert : la barre le met à la place du nom.
    pub(crate) fn rename_field_of(&self, name: &str) -> Option<Entity<RenameField>> {
        self.renaming
            .as_ref()
            .filter(|renaming| renaming.session == name)
            .map(|renaming| renaming.field.clone())
    }

    fn increase_font_size(
        &mut self,
        _: &IncreaseFontSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_font_size(FONT_STEP, window, cx);
    }

    fn decrease_font_size(
        &mut self,
        _: &DecreaseFontSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_font_size(-FONT_STEP, window, cx);
    }

    fn reset_font_size(&mut self, _: &ResetFontSize, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_font_zoom(self.config.terminal_font_size, window, cx);
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state_mut();
        state.ui.sidebar_visible = !state.ui.sidebar_visible;
        self.save();
        cx.notify();
    }

    /// Vrai quand le projet `path` est celui que `⌘T` vise devant la session affichée : sa ligne porte alors le
    /// style de sélection.
    pub(crate) fn is_targeted_project(&self, path: &Path) -> bool {
        // Un seul surlignage dans la barre : la ligne Artifacts l'a, tant que la galerie est au centre.
        !self.artifacts_shown
            && is_targeted(
                self.project_wins,
                self.selected_is_ideation(),
                self.selected_project.as_deref(),
                path,
            )
    }

    /// Clic sur le nom d'un projet : le sélectionne (le dernier geste gagne) et le déplie s'il était replié, sans
    /// jamais le replier. La session affichée ne change pas. Un projet dont le dossier manque se déplie sans être
    /// sélectionné : `⌘T` n'y créerait rien. La branche et le volet Terminal suivent le projet visé.
    pub(crate) fn click_project(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state_mut().set_expanded(path, true);
        if !self.is_missing(path) {
            self.selected_project = Some(path.to_path_buf());
            self.project_wins = true;
        }
        self.save();
        self.sync_branch(cx);
        self.sync_right_panel(window, cx);
        cx.notify();
    }

    /// Vrai quand un projet l'emporte sur la session affichée : la barre ne surligne alors que lui.
    pub(crate) fn project_wins(&self) -> bool {
        self.project_wins
    }

    /// Clic sur le chevron d'un projet : le plie ou le déplie, sans rien sélectionner.
    pub(crate) fn toggle_project(&mut self, path: &Path, cx: &mut Context<Self>) {
        let expanded = self.state().is_expanded(path);
        self.state_mut().set_expanded(path, !expanded);
        self.save();
        cx.notify();
    }

    /// Clic sur l'en-tête d'une machine (`LOCAL_MACHINE` pour Localhost) : la plie ou la déplie, sans rien sélectionner.
    pub(crate) fn toggle_machine(&mut self, key: &str, cx: &mut Context<Self>) {
        let collapsed = self.state().is_machine_collapsed(key);
        self.state_mut().set_machine_collapsed(key, !collapsed);
        self.save();
        cx.notify();
    }

    /// « Réessayer » : reconnecte la machine tout de suite, sans attendre la minuterie. Le compte des essais repart de 0.
    pub(crate) fn retry_machine(
        &mut self,
        connection: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.machine_index(connection) else {
            return;
        };
        let machine = &mut self.machines[index];
        machine.attempt = 0;
        // L'essai programmé tombe : celui-ci le remplace.
        machine.retry = None;
        if machine.status != MachineStatus::Connected {
            machine.status = MachineStatus::Connecting;
        }
        self.connect_machine(connection, window, cx);
        cx.notify();
    }

    /// Le menu « ··· » de l'en-tête d'une machine distante : réessayer, ouvrir un terminal (machine connectée seulement) et
    /// ses réglages.
    pub(crate) fn open_machine_menu(
        &mut self,
        connection: String,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let retry = connection.clone();
        let terminal = connection.clone();
        let connected = self
            .machine_index(&connection)
            .is_some_and(|index| self.machines[index].status == MachineStatus::Connected);
        let spec = MenuSpec {
            title: None,
            entries: vec![
                MenuEntry::Item(MenuItemSpec::new("Réessayer", move |this, window, cx| {
                    this.retry_machine(&retry, window, cx);
                })),
                MenuEntry::Item(
                    MenuItemSpec::new("Ouvrir un terminal", move |this, window, cx| {
                        this.open_machine_terminal(&terminal, window, cx);
                    })
                    .enabled(connected),
                ),
                MenuEntry::Item(MenuItemSpec::new(
                    "Réglages de la machine",
                    |this, window, cx| this.open_settings_on(Some(Section::Machines), window, cx),
                )),
            ],
        };
        self.open_menu(position, spec, window, cx);
    }

    /// Clic sur le chevron de la section « Bouillon de neurones » : la plie ou la déplie, comme un projet.
    pub(crate) fn toggle_ideation(&mut self, cx: &mut Context<Self>) {
        let collapsed = self.state().ui.ideation_collapsed;
        self.state_mut().ui.ideation_collapsed = !collapsed;
        self.save();
        cx.notify();
    }

    /// Mesure du démarrage (AC-9), à la racine pour que la fenêtre soit mesurée barre visible ou non. Écrite par
    /// la peinture, car `on_next_frame` passerait avant le dessin. `first window paint` : première peinture,
    /// avant ou après tmux ; `first sidebar paint` : première peinture qui suit l'amorçage réussi. Le tout
    /// sépare le démarrage de GPUI de celui de tmux.
    fn paint_probe(&self) -> Option<impl IntoElement> {
        let window_pending = !self.window_paint_logged.get();
        let sidebar_pending = self.bootstrapped && !self.sidebar_paint_logged.get();
        if !window_pending && !sidebar_pending {
            return None;
        }
        let window_logged = self.window_paint_logged.clone();
        let sidebar_logged = self.sidebar_paint_logged.clone();
        let bootstrapped = self.bootstrapped;
        let sessions = self.entries.len();
        Some(
            canvas(
                |_, _, _| (),
                move |_, _, _: &mut Window, _| {
                    let startup_ms = hud::since_process_start().as_millis();
                    if !window_logged.replace(true) {
                        hud::log_event(&format!("first window paint startup_ms={startup_ms}"));
                    }
                    if bootstrapped && !sidebar_logged.replace(true) {
                        hud::log_event(&format!(
                            "first sidebar paint startup_ms={startup_ms} sessions={sessions}"
                        ));
                    }
                },
            )
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .size(px(0.)),
        )
    }

    /// Le bouton de la barre, dans la bande de titre : 28 × 28 natifs à `left: 94, top: 12`.
    /// La poignée de 8 px (pixels d'écran) centrée sur l'abscisse `edge_x` du bord intérieur. Le curseur de
    /// redimensionnement la survole ; l'appui démarre le glisser (`capture_pointer`), le relâché l'écrit, un
    /// double-clic rend la largeur de la maquette. Le fond de la poignée bloque la souris : le pane dessous ne
    /// reçoit pas l'appui.
    fn resize_handle(
        &self,
        edge: Edge,
        edge_x: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let workspace = cx.entity();
        let dragging = self.edge_drag.is_some_and(|drag| drag.edge == edge);
        canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::BlockMouse),
            move |_, hitbox, window, _| {
                if dragging {
                    window.set_window_cursor_style(CursorStyle::ResizeLeftRight);
                } else {
                    window.set_cursor_style(CursorStyle::ResizeLeftRight, &hitbox);
                }
                let target = workspace.clone();
                let handle = hitbox.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble
                        || event.button != MouseButton::Left
                        || !handle.is_hovered(window)
                    {
                        return;
                    }
                    cx.stop_propagation();
                    if event.click_count >= 2 {
                        target.update(cx, |this, cx| this.reset_edge_width(edge, cx));
                    } else {
                        window.capture_pointer(handle.id);
                        let x = event.position.x;
                        target.update(cx, |this, cx| this.begin_edge_drag(edge, x, window, cx));
                    }
                });
                if dragging {
                    let target = workspace.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase != DispatchPhase::Capture {
                            return;
                        }
                        let x = event.position.x;
                        // Un relâché perdu (hors de la fenêtre) : la souris n'appuie plus, le glisser s'arrête.
                        if event.pressed_button == Some(MouseButton::Left) {
                            target.update(cx, |this, cx| this.drag_edge_to(x, window, cx));
                        } else {
                            target.update(cx, |this, cx| this.end_edge_drag(cx));
                        }
                    });
                    let target = workspace;
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                        if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                            target.update(cx, |this, cx| this.end_edge_drag(cx));
                        }
                    });
                }
            },
        )
        .absolute()
        .left(edge_x - px(HANDLE_WIDTH / 2.))
        .top(px(0.))
        .w(px(HANDLE_WIDTH))
        .h_full()
    }

    fn sidebar_toggle(&self, visible: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let icon = if visible {
            Icon::LeftPanelClose
        } else {
            Icon::LeftPanelOpen
        };
        div()
            .id("sidebar-toggle")
            .group("sidebar-toggle")
            .absolute()
            .left(px(94.))
            .top(px(12.))
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .occlude()
            .cursor_pointer()
            .hover(|style| style.bg(white(HOVER_STRONG)))
            .on_click(cx.listener(|this, _, window, cx| {
                this.toggle_sidebar(&ToggleSidebar, window, cx);
            }))
            .child(
                svg()
                    .path(icon.path())
                    .size(px(18.))
                    .text_color(white(TEXT_MIN))
                    .group_hover("sidebar-toggle", |style| {
                        style.text_color(white(TEXT_HOVER))
                    }),
            )
    }

    /// Le bouton Terminal de la barre d'outils (il n'apparaît qu'avec un projet de contexte).
    fn terminal_toggle_button(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        toolbar_toggle_button(
            theme,
            "terminal-toggle",
            Icon::Terminal,
            self.terminal_open(),
            true,
            |this, window, cx| this.toggle_terminal(window, cx),
            cx,
        )
    }

    /// Le bouton Fichiers de la barre d'outils, en dernier des quatre : même gabarit et mêmes états. Sans projet de
    /// contexte il est éteint (25 %) et ne répond pas au clic.
    fn files_toggle_button(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        toolbar_toggle_button(
            theme,
            "files-toggle",
            Icon::Folder,
            self.files_open(),
            self.panel_project().is_some(),
            |this, window, cx| this.toggle_files(window, cx),
            cx,
        )
        .when(self.selected_is_remote(), unavailable_tooltip)
    }

    /// Le bouton Changes de la barre d'outils, après Terminal : même gabarit et mêmes états. Sans projet de
    /// contexte il est éteint et ne répond pas au clic.
    fn changes_toggle_button(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        toolbar_toggle_button(
            theme,
            "changes-toggle",
            Icon::Difference,
            self.changes_open(),
            self.panel_project().is_some(),
            |this, window, cx| this.toggle_changes(window, cx),
            cx,
        )
        .when(self.selected_is_remote(), unavailable_tooltip)
    }

    /// Le bouton Projet de la barre d'outils, le dernier des boutons de volet, après Fichiers : même gabarit et mêmes états.
    /// Sans projet de contexte il est éteint et ne répond pas au clic. Sa bulle d'aide dit « Projet ».
    fn project_toggle_button(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        toolbar_toggle_button(
            theme,
            "project-toggle",
            Icon::Settings,
            self.project_open(),
            self.panel_project().is_some(),
            |this, window, cx| this.toggle_project_panel(window, cx),
            cx,
        )
        .tooltip(if self.selected_is_remote() {
            |_: &mut Window, cx: &mut App| cx.new(|_| HelpBubble::new(UNAVAILABLE_ON_REMOTE)).into()
        } else {
            |_: &mut Window, cx: &mut App| cx.new(|_| HelpBubble::new(PROJECT_HELP)).into()
        })
    }

    /// Le bouton Scratchpad de la barre d'outils, le premier, avant Terminal : même gabarit et mêmes états. La note est
    /// globale, le bouton est toujours actif, projet de contexte ou non.
    fn scratchpad_toggle_button(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        toolbar_toggle_button(
            theme,
            "scratchpad-toggle",
            Icon::StickyNote,
            self.scratchpad_open(),
            true,
            |this, window, cx| this.toggle_scratchpad(window, cx),
            cx,
        )
    }

    /// Le bouton Browser de la barre d'outils, entre Changes et Fichiers : même gabarit et mêmes états. Le navigateur ne dépend pas du projet de
    /// contexte, le bouton est toujours actif.
    /// Volet fermé, le bouton porte le nombre de serveurs en marche (spec §4.1, P4-14) : une pastille verte de 14 en haut à
    /// droite, rien sous un serveur en marche. Volet ouvert, la page d'accueil les liste, la pastille s'efface.
    fn browser_toggle_button(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let badge = self.shown_badge(cx);
        toolbar_toggle_button(
            theme,
            "browser-toggle",
            Icon::Language,
            self.browser_open(),
            !self.selected_is_remote(),
            |this, window, cx| this.toggle_browser(window, cx),
            cx,
        )
        .when(self.selected_is_remote(), unavailable_tooltip)
        .relative()
        .children(badge.map(|count| servers_badge(count, theme)))
    }

    /// Le nombre que porte la pastille du bouton Browser : celui des serveurs en marche, volet fermé ; rien volet ouvert ni sans
    /// serveur en marche.
    fn shown_badge(&self, cx: &App) -> Option<usize> {
        if self.browser_open() {
            return None;
        }
        let running = self
            .servers
            .as_ref()
            .map_or(0, |servers| servers.read(cx).running_count());
        servers_badge_count(running)
    }

    /// Écrit au journal `servers badge count=N` quand le nombre de la pastille change (0 : elle est absente). Les critères
    /// d'acceptation le lisent : la capture ne dit pas un chiffre de 10 px.
    fn sync_badge_log(&mut self, cx: &App) {
        let count = self.shown_badge(cx).unwrap_or(0);
        if self.logged_badge != Some(count) {
            self.logged_badge = Some(count);
            hud::log_event(&format!("servers badge count={count}"));
        }
    }

    /// Le bouton de zoom de la barre d'outils, présent pendant un zoom : même gabarit que le bouton Terminal, en
    /// état « actif » puisque le zoom l'est. Un clic rétablit la fenêtre. La pastille porte l'état le plus urgent des
    /// panes que le zoom cache, s'il attend ou échoue (écart E18).
    fn zoom_button(
        &self,
        theme: &Theme,
        badge: ActivityState,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("zoom-toggle")
            .group("zoom-toggle")
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(theme.px(28.))
            .rounded(theme.px(6.))
            .occlude()
            .cursor_pointer()
            .bg(white(CONTROL))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if let Some(view) = this.selected_view() {
                        view.update(cx, |view, cx| view.toggle_zoom(window, cx));
                    }
                }),
            )
            .child(
                svg()
                    .path(Icon::ZoomInMap.path())
                    .size(theme.px(18.))
                    .text_color(white(TEXT_FULL)),
            )
            .children(hidden_panes_dot(badge).map(|state| {
                state_dot(theme, state)
                    .absolute()
                    .top(theme.px(-3.))
                    .right(theme.px(-3.))
            }))
    }

    /// Le bouton `⋮` de l'en-tête : 28 × 28, rayon 6, icône 18. Son menu en est l'enfant : sans `position`, l'ancre
    /// est l'origine du bouton. L'enveloppe est un conteneur flex à l'alignement de départ, pour que l'enfant
    /// absolu parte du coin du bouton et non de son centre.
    fn toolbar_menu_button(
        &self,
        theme: &Theme,
        menu: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .relative()
            .flex()
            .flex_none()
            .size(theme.px(28.))
            .child(
                div()
                    .id("session-toolbar-more")
                    .group("session-toolbar-more")
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme.px(6.))
                    .occlude()
                    .cursor_pointer()
                    .hover(|style| style.bg(white(BUTTON_HOVER)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.open_toolbar_menu(window, cx);
                        }),
                    )
                    .child(
                        svg()
                            .path(Icon::MoreVert.path())
                            .size(theme.px(18.))
                            .text_color(white(TEXT_MIN))
                            .group_hover("session-toolbar-more", |style| {
                                style.text_color(white(TEXT_FULL))
                            }),
                    ),
            )
            .children(menu)
    }

    /// L'écran vide (B-23) : contexte du projet et les deux raccourcis qui ouvrent le travail.
    fn empty_screen(&self, theme: &Theme) -> impl IntoElement {
        let project = self.context_project();
        let context = project.map(|path| {
            let mut line = div()
                .flex()
                .items_center()
                .gap(theme.px(6.))
                .px(theme.px(8.))
                .pb(theme.px(12.))
                .text_size(theme.text(11.5))
                .font_family(self.config.terminal_font_family.clone())
                .text_color(white(TEXT_MIN));
            let name = project::display_name(&path);
            match &self.branch {
                Some(branch) => {
                    line = line
                        .child(
                            svg()
                                .path(Icon::ForkRight.path())
                                .flex_none()
                                .size(theme.px(15.))
                                .text_color(white(TEXT_MIN)),
                        )
                        .child(format!("{name} · {branch}"));
                }
                None => line = line.child(name),
            }
            line
        });
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .pb(theme.px(60.))
            .child(
                div()
                    .w(theme.px(260.))
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .children(context)
                    .child(shortcut_row(
                        theme,
                        "empty-new-session",
                        "Nouvelle session",
                        ["⌘", "T"],
                        |window, cx| window.dispatch_action(Box::new(NewSession), cx),
                    ))
                    .child(shortcut_row(
                        theme,
                        "empty-open-project",
                        "Ouvrir un projet",
                        ["⌘", "O"],
                        |window, cx| window.dispatch_action(Box::new(NewProject), cx),
                    ))
                    .child(shortcut_row(
                        theme,
                        "empty-terminal",
                        "Terminal",
                        ["⌃", "`"],
                        |window, cx| window.dispatch_action(Box::new(ToggleTerminal), cx),
                    )),
            )
    }

    /// Le centre d'une session dont les essais de reconnexion sont épuisés : le titre explique, le texte de
    /// tmux s'il est au catalogue, puis le message brut en détail (spec jalon 4, §5.1 et §5.5).
    fn unreachable_screen(&self, raw: &str, theme: &Theme) -> impl IntoElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(theme.px(12.))
            .pb(theme.px(60.))
            .child(
                div()
                    .text_size(theme.text(16.))
                    .child("Session injoignable"),
            )
            .children(tmux_text(raw).map(|text| div().text_size(theme.text(13.)).child(text)))
            .child(
                div()
                    .max_w(theme.px(560.))
                    .text_size(theme.text(12.))
                    .text_color(white(TEXT_MIN))
                    .child(raw.to_owned()),
            )
    }

    /// Le blocage de démarrage : le message de `StartupError` et la commande qui le règle.
    fn startup_failure(&self, error: &StartupError, theme: &Theme) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(TITLE_BAND))
                    .flex_none()
                    .on_mouse_down(MouseButton::Left, drag_window),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(theme.px(12.))
                    .pb(theme.px(60.))
                    .text_size(theme.text(16.))
                    .child(error.message.clone())
                    .children(error.detail.clone().map(|detail| {
                        div()
                            .max_w(theme.px(560.))
                            .text_size(theme.text(12.))
                            .text_color(white(TEXT_MIN))
                            .child(detail)
                    }))
                    .children(error.hint.map(|command| {
                        div()
                            .font_family(self.config.terminal_font_family.clone())
                            .text_color(white(TEXT_PROJECT))
                            .child(command)
                    })),
            )
            .into_any_element()
    }
}

/// Une ligne de raccourci de l'écran vide : libellé à gauche, touches à droite.
fn shortcut_row(
    theme: &Theme,
    id: &'static str,
    label: &'static str,
    keys: [&'static str; 2],
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(theme.px(8.))
        .h(theme.px(30.))
        .px(theme.px(8.))
        .rounded(theme.px(6.))
        .text_color(white(TEXT_PROJECT))
        .cursor_pointer()
        .hover(|style| style.bg(white(HOVER)).text_color(white(TEXT_HOVER)))
        .on_click(move |_, window, cx| on_click(window, cx))
        .child(div().flex_1().child(label))
        .child(div().flex().gap(px(3.)).children(keys.map(|key| {
            div()
                .flex()
                .items_center()
                .justify_center()
                .min_w(theme.px(18.))
                .h(theme.px(18.))
                .px(theme.px(4.))
                .rounded(theme.px(4.))
                .bg(white(HOVER_STRONG))
                .text_size(theme.text(11.))
                .text_color(white(TEXT_MIN))
                .child(key)
        })))
}

/// Un bouton à bascule de la barre d'outils (Terminal, Fichiers) : 28 × 28, rayon 6, icône 18 à 55 % ; au survol fond
/// à 8 % et icône blanche ; volet ouvert, fond à 10 % et icône blanche. `enabled` faux : icône à 25 %, ni survol ni
/// clic.
fn toolbar_toggle_button(
    theme: &Theme,
    id: &'static str,
    icon: Icon,
    open: bool,
    enabled: bool,
    on_press: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> Stateful<Div> {
    let text = if !enabled {
        TEXT_DISABLED
    } else if open {
        TEXT_FULL
    } else {
        TEXT_MIN
    };
    div()
        .id(id)
        .group(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(theme.px(28.))
        .rounded(theme.px(6.))
        .occlude()
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .when(open, |button| button.bg(white(CONTROL)))
                .when(!open, |button| {
                    button.hover(|style| style.bg(white(BUTTON_HOVER)))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        on_press(this, window, cx);
                    }),
                )
        })
        .child(
            svg()
                .path(icon.path())
                .size(theme.px(18.))
                .text_color(white(text))
                .when(enabled, |icon| {
                    icon.group_hover(id, |style| style.text_color(white(TEXT_FULL)))
                }),
        )
}

/// L'infobulle d'un bouton de volet grisé sur une session distante.
fn unavailable_tooltip(button: Stateful<Div>) -> Stateful<Div> {
    button.tooltip(|_, cx| cx.new(|_| HelpBubble::new(UNAVAILABLE_ON_REMOTE)).into())
}

/// La pastille du bouton Browser (spec §4.1) : 14 de haut (14 au moins de large, un second chiffre l'élargit), fond
/// `#30D158`, chiffre 10/700 en noir, ancrée en haut à droite du bouton.
fn servers_badge(count: usize, theme: &Theme) -> Div {
    div()
        .absolute()
        .top(theme.px(-3.))
        .right(theme.px(-3.))
        .flex()
        .items_center()
        .justify_center()
        .min_w(theme.px(14.))
        .h(theme.px(14.))
        .px(theme.px(3.))
        .rounded_full()
        .bg(color(RUNNING))
        // Un filet du fond de la barre autour de la pastille : elle ne se confond pas avec le contour de l'icône dessous.
        .shadow(vec![BoxShadow {
            color: color(WINDOW).into(),
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(2.),
            inset: false,
        }])
        .text_size(theme.text(10.))
        .font_weight(FontWeight::BOLD)
        .text_color(black(1.))
        .child(count.to_string())
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        // Filet : un geste qui retire le menu sans `close_menu` (`self.menu = None`) est vu ici.
        self.sync_page_cover(window, cx);
        // Un menu ancré à la souris vit à la racine ; celui du `⋮` est l'enfant de son bouton.
        let (pointer_menu, toolbar_menu) = match &self.menu {
            Some(menu) if menu.placement == Placement::Toolbar => {
                (None, Some(render_menu(menu, &theme, window, cx)))
            }
            Some(menu) => (Some(render_menu(menu, &theme, window, cx)), None),
            None => (None, None),
        };
        let root = div()
            .size_full()
            .flex()
            .bg(color(WINDOW))
            .font_family(UI_FONT)
            .text_size(theme.text(13.))
            .text_color(white(TEXT_PRIMARY))
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::open_config))
            .on_action(cx.listener(Self::install_integrations))
            .when(
                cx.try_global::<crate::idnma::IdnmaStatus>()
                    .is_none_or(crate::idnma::IdnmaStatus::can_install),
                |root| {
                    root.on_action(
                        cx.listener(|_, _: &crate::menus::InstallIdnma, window, cx| {
                            crate::idnma::install(window, cx);
                        }),
                    )
                },
            )
            .on_action(cx.listener(Self::increase_font_size))
            .on_action(cx.listener(Self::decrease_font_size))
            .on_action(cx.listener(Self::reset_font_size))
            .on_action(cx.listener(|this, _: &NewProject, window, cx| {
                this.open_project_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenFile, window, cx| {
                this.open_file_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewSession, window, cx| {
                this.new_session_here(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseSession, window, cx| {
                // La session sélectionnée est cachée par la galerie : `⌘W` n'y ferait rien de visible, il ne la ferme pas.
                if !this.artifacts_shown {
                    this.close_selected(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                this.toggle_terminal(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleFiles, window, cx| {
                this.toggle_files(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleChanges, window, cx| {
                this.toggle_changes(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleBrowser, window, cx| {
                this.toggle_browser(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleScratchpad, window, cx| {
                this.toggle_scratchpad(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleProject, window, cx| {
                this.toggle_project_panel(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTree, _window, cx| {
                this.toggle_files_tree(cx);
            }))
            .on_action(cx.listener(|this, _: &ProjectSearch, window, cx| {
                this.project_search(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextSession, window, cx| {
                this.cycle_session(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousSession, window, cx| {
                this.cycle_session(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession1, window, cx| {
                this.select_nth(0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession2, window, cx| {
                this.select_nth(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession3, window, cx| {
                this.select_nth(2, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession4, window, cx| {
                this.select_nth(3, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession5, window, cx| {
                this.select_nth(4, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession6, window, cx| {
                this.select_nth(5, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession7, window, cx| {
                this.select_nth(6, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession8, window, cx| {
                this.select_nth(7, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession9, window, cx| {
                this.select_nth(8, window, cx);
            }))
            .children(self.paint_probe());
        if let Some(error) = &self.startup_error {
            return root.child(self.startup_failure(error, &theme));
        }

        self.sync_badge_log(cx);
        // La page d'accueil du navigateur met le groupe de serveurs du projet de contexte en premier.
        if let Some(browser) = self.browser.clone() {
            let context = self.panel_project();
            browser.update(cx, |browser, cx| browser.set_context(context, cx));
        }

        // Un volet ouvert dont le projet de contexte n'a pas (encore) de shell ni de volet : le changement de contexte
        // passe par un chemin qui n'a pas resynchronisé le volet, on le fait ici, après cette image.
        let shown_terminal = self.shown_terminal();
        let shown_files = self.shown_files();
        let shown_changes = self.shown_changes();
        let shown_browser = self.shown_browser();
        let shown_scratchpad = self.shown_scratchpad();
        let shown_project = self.shown_project();
        let notice = self.right_panel_notice();
        if self.right_panel.is_some()
            && shown_terminal.is_none()
            && shown_files.is_none()
            && shown_changes.is_none()
            && shown_browser.is_none()
            && shown_scratchpad.is_none()
            && shown_project.is_none()
            && notice.is_none()
        {
            let workspace = cx.entity();
            window.defer(cx, move |window, cx| {
                workspace.update(cx, |this, cx| this.sync_right_panel(window, cx));
            });
        }
        let maximized = (shown_terminal.is_some()
            || shown_files.is_some()
            || shown_changes.is_some()
            || shown_browser.is_some()
            || shown_scratchpad.is_some()
            || shown_project.is_some()
            || notice.is_some())
            && self.panel_maximized();
        let sidebar_visible = self.state().ui.sidebar_visible;
        let inset = if sidebar_visible {
            theme.px(10.)
        } else {
            px(COLLAPSED_HEADER_INSET)
        };
        // Les boutons de la barre d'outils, dans l'ordre de `toolbar_items` : Scratchpad, Terminal (avec un projet),
        // Changes, Browser, Fichiers, Projet, séparateur, `⋮`. Un bouillon n'a que Scratchpad et Browser. Changes, Fichiers
        // et Projet sont éteints sans projet de contexte ; Scratchpad en tête.
        let mut toolbar_menu = toolbar_menu;
        // La galerie n'a pas de barre d'outils de session (ni de bande : voir `session_header`).
        let toolbar: Vec<AnyElement> = if self.artifacts_shown {
            Vec::new()
        } else {
            toolbar_items(
                self.selected_is_ideation(),
                // Le Terminal a un shell sur une machine distante, sans projet de ce Mac.
                self.panel_project().is_some() || self.selected_is_remote(),
                self.selected_entry().is_some(),
            )
            .into_iter()
            .map(|item| match item {
                ToolbarItem::Scratchpad => {
                    self.scratchpad_toggle_button(&theme, cx).into_any_element()
                }
                ToolbarItem::Terminal => self.terminal_toggle_button(&theme, cx).into_any_element(),
                ToolbarItem::Changes => self.changes_toggle_button(&theme, cx).into_any_element(),
                ToolbarItem::Browser => self.browser_toggle_button(&theme, cx).into_any_element(),
                ToolbarItem::Files => self.files_toggle_button(&theme, cx).into_any_element(),
                ToolbarItem::Project => self.project_toggle_button(&theme, cx).into_any_element(),
                ToolbarItem::Separator => div()
                    .flex_none()
                    .w(px(1.))
                    .h(theme.px(16.))
                    .mx(theme.px(4.))
                    .bg(white(CONTROL))
                    .into_any_element(),
                ToolbarItem::Menu => self
                    .toolbar_menu_button(&theme, toolbar_menu.take(), cx)
                    .into_any_element(),
            })
            .collect()
        };
        // La galerie porte son propre en-tête dans la bande de 48 (titre et boutons sur la ligne des feux, comme la
        // maquette) : la bande de la session n'existe pas, et l'en-tête de la galerie déplace la fenêtre lui-même.
        let gallery_header_inset = (!sidebar_visible).then_some(inset);
        if self.artifacts_shown
            && let Some(view) = &self.artifacts
        {
            view.update(cx, |view, _| view.set_header_inset(gallery_header_inset));
        }
        let session_header = (!self.artifacts_shown).then(|| {
            div()
                .h(theme.px(48.))
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .pl(inset)
                .pr(theme.px(10.))
                .on_mouse_down(MouseButton::Left, drag_window)
                .gap(theme.px(2.))
                .children(
                    self.selected_view()
                        .and_then(|view| view.read(cx).zoom_badge(cx))
                        .map(|badge| self.zoom_button(&theme, badge, cx)),
                )
                .children(toolbar)
        });
        let main = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            // L'en-tête déplace la fenêtre ; à droite, la barre d'outils de la session sélectionnée.
            .children(session_header)
            .child(match self.selected_view() {
                _ if self.artifacts_shown && self.artifacts.is_some() => div()
                    .flex_1()
                    .min_h_0()
                    .children(self.artifacts.clone())
                    .into_any_element(),
                Some(view) => div()
                    .flex_1()
                    .min_h_0()
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|_, event: &MouseDownEvent, window, cx| {
                            // Le pane cliqué prend le focus par son propre gestionnaire, dont l'événement part
                            // dans la file d'effets : le menu s'ouvre après, pour décrire ce pane devenu l'actif.
                            let (workspace, position) = (cx.entity(), event.position);
                            window.defer(cx, move |window, cx| {
                                workspace.update(cx, |this, cx| {
                                    this.open_pane_menu(position, window, cx);
                                });
                            });
                        }),
                    )
                    .child(view)
                    .into_any_element(),
                None => match self
                    .selected_entry()
                    .and_then(|entry| entry.unreachable.as_deref())
                {
                    Some(raw) => self.unreachable_screen(raw, &theme).into_any_element(),
                    None => self.empty_screen(&theme).into_any_element(),
                },
            });
        // Agrandi, le volet prend la place de la colonne principale ; barre repliée, son en-tête laisse la place aux
        // feux et au bouton de la barre.
        let panel_inset = if maximized && !sidebar_visible {
            px(COLLAPSED_HEADER_INSET)
        } else {
            theme.px(10.)
        };
        let (sidebar_width, panel_width) = {
            let (sidebar, panel) = self.edge_widths(window);
            (theme.px(sidebar), theme.px(panel))
        };
        let has_panel = shown_terminal.is_some()
            || shown_files.is_some()
            || shown_changes.is_some()
            || shown_browser.is_some()
            || shown_scratchpad.is_some()
            || shown_project.is_some()
            || notice.is_some();
        let panel = match (
            shown_terminal,
            shown_files,
            shown_changes,
            shown_browser,
            shown_scratchpad,
            shown_project,
        ) {
            (Some((key, view)), _, _, _, _, _) => Some(
                render_panel(
                    self.terminal_title(&key),
                    view,
                    maximized,
                    panel_inset,
                    panel_width,
                    &theme,
                    cx,
                )
                .into_any_element(),
            ),
            (None, Some((project, files)), _, _, _, _) => Some(
                render_files_panel(
                    project::display_name(&project),
                    files,
                    maximized,
                    panel_inset,
                    panel_width,
                    &theme,
                    cx,
                )
                .into_any_element(),
            ),
            (None, None, Some((_, changes)), _, _, _) => Some(
                render_changes_panel(changes, maximized, panel_inset, panel_width, &theme, cx)
                    .into_any_element(),
            ),
            (None, None, None, Some(browser), _, _) => Some(
                // Agrandi, le volet prend ce que la barre latérale laisse : c'est la largeur dont les onglets disposent.
                render_browser_panel(
                    browser,
                    maximized,
                    panel_inset,
                    if maximized {
                        window.viewport_size().width - sidebar_width
                    } else {
                        panel_width
                    },
                    &theme,
                    cx,
                )
                .into_any_element(),
            ),
            (None, None, None, None, Some(scratchpad), _) => Some(
                render_scratchpad_panel(
                    scratchpad,
                    maximized,
                    panel_inset,
                    panel_width,
                    &theme,
                    |this, window, cx| this.focus_scratchpad(window, cx),
                    |this, window, cx| this.toggle_scratchpad_maximized(window, cx),
                    |this, window, cx| this.close_scratchpad(window, cx),
                    cx,
                )
                .into_any_element(),
            ),
            (None, None, None, None, None, Some((project, panel))) => Some(
                render_project_panel(
                    project::display_name(&project),
                    panel,
                    maximized,
                    panel_inset,
                    panel_width,
                    &theme,
                    cx,
                )
                .into_any_element(),
            ),
            (None, None, None, None, None, None) => notice.map(|(panel, text)| {
                render_notice_panel(panel, text, maximized, panel_inset, panel_width, &theme, cx)
                    .into_any_element()
            }),
        };
        let sidebar = sidebar_visible.then(|| render_sidebar(self, &theme, sidebar_width, cx));
        // Les poignées : pas de poignée à la barre masquée ni au volet agrandi.
        let sidebar_handle =
            sidebar_visible.then(|| self.resize_handle(Edge::Sidebar, sidebar_width, cx));
        let panel_handle = (has_panel && !maximized).then(|| {
            let edge_x = window.viewport_size().width - panel_width;
            self.resize_handle(Edge::Panel, edge_x, cx)
        });
        root.relative()
            .children(sidebar)
            .when(!maximized, |root| root.child(main))
            .children(panel)
            .children(sidebar_handle)
            .children(panel_handle)
            .child(self.sidebar_toggle(sidebar_visible, cx))
            .children(pointer_menu)
    }
}

/// Le volet droit `panel` dont le corps est remplacé par `text`, au centre : « Pas encore disponible » pour un volet de
/// dossier sur une session distante, « Ouverture… » le temps que le shell d'une machine s'ouvre. Même en-tête que le volet
/// qu'il remplace (pastille, agrandir, fermer).
fn render_notice_panel(
    panel: RightPanel,
    text: &'static str,
    maximized: bool,
    header_inset: Pixels,
    width: Pixels,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let (icon, label) = match panel {
        RightPanel::Terminal => (Icon::Terminal, "Terminal"),
        RightPanel::Files => (Icon::Folder, "Fichiers"),
        RightPanel::Changes => (Icon::Difference, "Changes"),
        RightPanel::Browser => (Icon::Language, "Browser"),
        RightPanel::Project => (Icon::Settings, "Projet"),
        RightPanel::Scratchpad => (Icon::StickyNote, "Scratchpad"),
    };
    let header = panel_header(
        theme,
        icon,
        label,
        String::new(),
        ("notice-maximize", "notice-close"),
        maximized,
        header_inset,
        |_, _, _| {},
        |this, window, cx| this.toggle_notice_maximized(window, cx),
        |this, window, cx| this.close_notice_panel(window, cx),
        cx,
    );
    let body = div()
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .px(theme.px(24.))
        .text_size(theme.text(13.))
        .text_color(white(TEXT_MIN))
        .child(text);
    panel_shell("notice-panel", maximized, width)
        .child(header)
        .child(body)
}

/// Le détail de l'alerte de fermeture, accordé au nombre de programmes (« Il sera arrêté. » au singulier).
fn running_detail(running: &[String]) -> String {
    let verdict = if running.len() == 1 {
        "Il sera arrêté."
    } else {
        "Ils seront arrêtés."
    };
    format!("En cours : {}. {verdict}", running.join(", "))
}

/// Les noms des sessions que le retrait du projet `path` ferme. Un bouillon n'est jamais de la partie, même quand son
/// dossier d'idéation a été ajouté comme projet : il appartient à sa section, pas à ce projet.
fn project_session_names(entries: &[SessionEntry], path: &Path) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| !entry.summary.ideation && entry.summary.project == path)
        .map(|entry| entry.summary.name.clone())
        .collect()
}

/// Pourquoi le retrait d'un projet est refusé (titre et détail de l'alerte), ou `None` s'il est permis : ses serveurs lancés
/// par IdemA. Sans le projet dans la liste, leurs lignes ne s'afficheraient plus et ils tourneraient sans qu'on puisse les
/// arrêter (P4-R7). `servers` est le nombre de sessions serveur vivantes du projet. Ses sessions, elles, se ferment avec
/// lui après confirmation (`removal_detail`).
fn removal_refusal(name: &str, servers: usize) -> Option<(&'static str, String)> {
    let detail = match servers {
        0 => return None,
        1 => format!("Le projet « {name} » a encore un serveur lancé par IdemA."),
        count => format!("Le projet « {name} » a encore {count} serveurs lancés par IdemA."),
    };
    Some(("Arrête d'abord ses serveurs", detail))
}

/// Le détail de la confirmation du retrait : combien de sessions se ferment, puis les programmes en cours s'il y en a.
fn removal_detail(sessions: usize, running: &[String]) -> String {
    let closing = if sessions == 1 {
        "Sa session sera fermée.".to_string()
    } else {
        format!("Ses {sessions} sessions seront fermées.")
    };
    if running.is_empty() {
        closing
    } else {
        format!("{closing} {}", running_detail(running))
    }
}

/// Les entrées de lanceur d'un menu : un sous-menu par harness, puis les agents sans harness. `item` construit l'entrée
/// de l'agent de rang `index` ; le logo s'y ajoute ici.
fn agent_menu_entries(
    agents: &[AgentEntry],
    item: impl Fn(usize, &AgentEntry) -> MenuItemSpec,
) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    for group in harness_groups(agents) {
        let items = group.agents.iter().map(|&index| {
            let agent = &agents[index];
            item(index, agent).logo(logo_path(&agent.icon_name()))
        });
        match group.harness {
            Some(harness) => entries.push(MenuEntry::Submenu {
                label: harness.into(),
                items: items.collect(),
                openable: false,
            }),
            None => entries.extend(items.map(MenuEntry::Item)),
        }
    }
    entries
}

/// Traduit les lignes d'un menu de gestes en entrées du menu d'IdemA : le libellé, le raccourci et l'état viennent de la
/// ligne ; choisir l'entrée appelle `run` avec son geste.
fn menu_entries<G: Copy + 'static>(
    lines: Vec<MenuLine<G>>,
    run: impl Fn(G, &mut Window, &mut App) + 'static,
) -> Vec<MenuEntry> {
    let run = Rc::new(run);
    lines
        .into_iter()
        .map(|line| match line {
            MenuLine::Separator => MenuEntry::Separator,
            MenuLine::Item(item) => {
                let (run, gesture) = (Rc::clone(&run), item.gesture);
                let mut entry = MenuItemSpec::new(item.label, move |_this, window, cx| {
                    run(gesture, window, cx);
                })
                .enabled(item.enabled);
                if let Some(shortcut) = item.shortcut {
                    entry = entry.shortcut(shortcut);
                }
                MenuEntry::Item(entry)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use idema_tmux::layout::Preset;

    use super::{
        Edge, RIGHT_PANEL_MIN_WIDTH, RightPanel, attach_grid, clamp_width, edge_widths_for,
        font_zoom_for, next_right_panel, other_holders, panel_keeps_keyboard, set_edge_width,
        stepped_font_size, zoom_after_reload,
    };
    use crate::config::ReloadEffects;
    use crate::state::UiState;

    #[test]
    fn a_file_is_released_by_every_other_panel_that_holds_it() {
        use std::path::Path;
        let panels = [
            (Path::new("/work/b"), true),
            (Path::new("/work/a"), true),
            (Path::new("/work/c"), false),
            (Path::new("/work/current"), true),
        ];
        // Le projet courant ne se libère pas lui-même ; un volet qui n'a pas le fichier n'est pas touché.
        assert_eq!(
            other_holders(Path::new("/work/current"), panels),
            [Path::new("/work/a"), Path::new("/work/b")]
        );
    }

    #[test]
    fn a_file_held_only_by_the_current_panel_is_released_by_nobody() {
        use std::path::Path;
        let panels = [
            (Path::new("/work/current"), true),
            (Path::new("/work/other"), false),
        ];
        assert!(other_holders(Path::new("/work/current"), panels).is_empty());
    }

    #[test]
    fn a_project_with_live_servers_cannot_be_removed() {
        assert_eq!(super::removal_refusal("alpha", 0), None);
        // Un serveur lancé par IdemA retient le projet : sans lui, sa ligne ne se verrait plus nulle part.
        let (title, detail) = super::removal_refusal("alpha", 1).unwrap();
        assert_eq!(title, "Arrête d'abord ses serveurs");
        assert_eq!(
            detail,
            "Le projet « alpha » a encore un serveur lancé par IdemA."
        );
        let (_, detail) = super::removal_refusal("alpha", 3).unwrap();
        assert_eq!(
            detail,
            "Le projet « alpha » a encore 3 serveurs lancés par IdemA."
        );
    }

    #[test]
    fn the_removal_confirmation_counts_sessions_and_names_running_programs() {
        assert_eq!(super::removal_detail(1, &[]), "Sa session sera fermée.");
        assert_eq!(
            super::removal_detail(3, &["claude".to_string()]),
            "Ses 3 sessions seront fermées. En cours : claude. Il sera arrêté."
        );
    }

    #[test]
    fn a_font_step_moves_one_point() {
        assert_eq!(stepped_font_size(16., 1.), 17.);
        assert_eq!(stepped_font_size(16., -1.), 15.);
    }

    #[test]
    fn a_font_step_stops_at_the_bounds() {
        assert_eq!(stepped_font_size(32., 1.), 32.);
        assert_eq!(stepped_font_size(10., -1.), 10.);
    }

    #[test]
    fn a_font_step_never_moves_against_the_key_pressed() {
        // Config valide mais hors des bornes du zoom : un pas va dans le sens de la touche.
        assert_eq!(stepped_font_size(40., 1.), 40.);
        assert_eq!(stepped_font_size(40., -1.), 39.);
        assert_eq!(stepped_font_size(8., -1.), 8.);
        assert_eq!(stepped_font_size(8., 1.), 9.);
    }

    #[test]
    fn a_size_equal_to_the_config_is_no_zoom() {
        assert_eq!(font_zoom_for(16., 16.), None);
        assert_eq!(font_zoom_for(17., 16.), Some(17.));
    }

    #[test]
    fn a_config_font_change_drops_the_zoom() {
        let changed = ReloadEffects {
            theme: false,
            terminal_font: true,
        };
        let untouched = ReloadEffects {
            theme: true,
            terminal_font: false,
        };
        assert_eq!(zoom_after_reload(Some(20.), changed), None);
        assert_eq!(zoom_after_reload(Some(20.), untouched), Some(20.));
        assert_eq!(zoom_after_reload(None, untouched), None);
    }

    #[test]
    fn widths_stay_between_seventy_and_one_hundred_fifty_percent() {
        assert!((clamp_width(248., 100., f32::MAX) - 173.6).abs() < 1e-3);
        assert!((clamp_width(248., 999., f32::MAX) - 372.).abs() < 1e-3);
        assert_eq!(clamp_width(248., 300., f32::MAX), 300.);
    }

    fn ui_with(sidebar: Option<f32>, panel: Option<f32>, visible: bool) -> UiState {
        UiState {
            sidebar_visible: visible,
            sidebar_width: sidebar,
            panel_width: panel,
            ..UiState::default()
        }
    }

    const EVERY_PANEL: [RightPanel; 6] = [
        RightPanel::Terminal,
        RightPanel::Files,
        RightPanel::Changes,
        RightPanel::Browser,
        RightPanel::Scratchpad,
        RightPanel::Project,
    ];

    #[test]
    fn every_right_panel_shows_the_width_given_to_any_of_them() {
        for dragged in EVERY_PANEL {
            let mut ui = UiState::default();
            set_edge_width(&mut ui, Edge::Panel, Some(dragged), Some(611.));
            assert_eq!(
                edge_widths_for(2000., 280., &ui, true).1,
                611.,
                "{dragged:?}"
            );
            // Le double-clic rend à tous la largeur par défaut, depuis n'importe quel volet.
            set_edge_width(&mut ui, Edge::Panel, Some(dragged), None);
            assert_eq!(
                edge_widths_for(2000., 280., &ui, true).1,
                420.,
                "{dragged:?}"
            );
        }
    }

    #[test]
    fn the_right_panel_stays_between_the_browser_minimum_and_half_the_window() {
        let narrow = ui_with(None, Some(1.), true);
        assert_eq!(
            edge_widths_for(2000., 280., &narrow, true).1,
            RIGHT_PANEL_MIN_WIDTH
        );
        let wide = ui_with(None, Some(9999.), true);
        assert_eq!(edge_widths_for(2000., 280., &wide, true).1, 1000.);
        assert_eq!(edge_widths_for(3000., 280., &wide, true).1, 1500.);
        // Le plafond suit la fenêtre sans réécrire la largeur gardée.
        assert_eq!(wide.panel_width, Some(9999.));
    }

    #[test]
    fn the_floor_beats_half_the_window_and_the_center_columns() {
        let ui = ui_with(None, None, true);
        assert_eq!(
            edge_widths_for(500., 280., &ui, true),
            (248., RIGHT_PANEL_MIN_WIDTH)
        );
    }

    #[test]
    fn set_edge_width_leaves_the_sidebar_and_a_closed_panel_alone() {
        let mut ui = UiState::default();
        set_edge_width(&mut ui, Edge::Panel, Some(RightPanel::Files), Some(444.));
        assert_eq!(ui.sidebar_width, None);
        set_edge_width(&mut ui, Edge::Sidebar, None, Some(300.));
        assert_eq!((ui.sidebar_width, ui.panel_width), (Some(300.), Some(444.)));
        // Sans volet, un bord de volet n'écrit rien.
        set_edge_width(&mut ui, Edge::Panel, None, Some(999.));
        assert_eq!(ui.panel_width, Some(444.));
    }

    #[test]
    fn no_panel_beside_leaves_room_to_the_sidebar() {
        // Volet fermé ou agrandi : la barre garde toute sa largeur gardée, là où un volet à côté la rogne.
        let ui = ui_with(Some(372.), Some(630.), true);
        let (sidebar, panel) = edge_widths_for(700., 280., &ui, false);
        assert_eq!(sidebar, 372.);
        assert_eq!(panel, 0.);
        assert_eq!(edge_widths_for(700., 280., &ui, true).0, 248.);
    }

    #[test]
    fn opening_one_right_panel_closes_the_other() {
        use RightPanel::{Files, Terminal};
        assert_eq!(next_right_panel(None, Files), Some(Files));
        assert_eq!(next_right_panel(Some(Terminal), Files), Some(Files));
        assert_eq!(next_right_panel(Some(Files), Files), None);
        assert_eq!(next_right_panel(None, Terminal), Some(Terminal));
        assert_eq!(next_right_panel(Some(Files), Terminal), Some(Terminal));
        assert_eq!(next_right_panel(Some(Terminal), Terminal), None);
    }

    #[test]
    fn changes_takes_the_place_of_either_panel_and_gives_it_back() {
        use RightPanel::{Changes, Files, Terminal};
        assert_eq!(next_right_panel(None, Changes), Some(Changes));
        assert_eq!(next_right_panel(Some(Files), Changes), Some(Changes));
        assert_eq!(next_right_panel(Some(Terminal), Changes), Some(Changes));
        assert_eq!(next_right_panel(Some(Changes), Changes), None);
        assert_eq!(next_right_panel(Some(Changes), Files), Some(Files));
        assert_eq!(next_right_panel(Some(Changes), Terminal), Some(Terminal));
    }

    #[test]
    fn browser_takes_the_place_of_any_panel_and_toggles_off() {
        for current in [
            None,
            Some(RightPanel::Terminal),
            Some(RightPanel::Files),
            Some(RightPanel::Changes),
        ] {
            assert_eq!(
                next_right_panel(current, RightPanel::Browser),
                Some(RightPanel::Browser)
            );
        }
        assert_eq!(
            next_right_panel(Some(RightPanel::Browser), RightPanel::Browser),
            None
        );
    }

    #[test]
    fn toggling_the_scratchpad_twice_closes_it() {
        use RightPanel::{Browser, Changes, Files, Scratchpad, Terminal};
        assert_eq!(next_right_panel(None, Scratchpad), Some(Scratchpad));
        assert_eq!(next_right_panel(Some(Scratchpad), Scratchpad), None);
        for current in [Terminal, Files, Changes, Browser] {
            assert_eq!(
                next_right_panel(Some(current), Scratchpad),
                Some(Scratchpad)
            );
        }
    }

    #[test]
    fn toggling_the_project_panel_twice_closes_it() {
        use RightPanel::{Browser, Changes, Files, Project, Scratchpad, Terminal};
        assert_eq!(next_right_panel(None, Project), Some(Project));
        assert_eq!(next_right_panel(Some(Project), Project), None);
        for current in [Terminal, Files, Changes, Browser, Scratchpad] {
            assert_eq!(next_right_panel(Some(current), Project), Some(Project));
        }
    }

    #[test]
    fn a_menu_covers_the_shown_browser_page() {
        assert!(page_covered(true, true));
        // Sans menu, ou sans volet Browser à l'écran, il n'y a rien à recouvrir.
        assert!(!page_covered(false, true));
        assert!(!page_covered(true, false));
        assert!(!page_covered(false, false));
    }

    #[test]
    fn the_neighbor_never_goes_through_select_while_the_gallery_covers_the_center() {
        // La galerie au centre : la voisine prend la sélection sans être affichée ni vue.
        assert_eq!(neighbor_handoff(true), NeighborHandoff::BehindGallery);
        // Hors galerie, le chemin d'origine : `select`.
        assert_eq!(neighbor_handoff(false), NeighborHandoff::Select);
    }

    #[test]
    fn the_default_width_gives_way_to_half_the_window_but_never_to_the_center() {
        let ui = ui_with(None, None, true);
        assert_eq!(edge_widths_for(1600., 280., &ui, true), (248., 420.));
        // Le centre tombe sous 40 colonnes plutôt que de rogner la largeur par défaut.
        assert_eq!(edge_widths_for(900., 280., &ui, true), (248., 420.));
        // Sous 840 de large, la moitié de la fenêtre passe devant, jusqu'au plancher.
        assert_eq!(edge_widths_for(760., 280., &ui, true).1, 380.);
        assert_eq!(
            edge_widths_for(700., 280., &ui, true).1,
            RIGHT_PANEL_MIN_WIDTH
        );
    }

    #[test]
    fn wide_stored_widths_are_trimmed_on_a_narrow_window_and_restored_on_a_wide_one() {
        let ui = ui_with(Some(372.), Some(630.), true);
        // Étroite : jamais sous la largeur par défaut, la barre revient à sa maquette.
        assert_eq!(edge_widths_for(900., 280., &ui, true), (248., 420.));
        assert_eq!(edge_widths_for(1000., 280., &ui, true), (248., 472.));
        // Intermédiaire : la moitié de la fenêtre borne le volet, la barre prend le reste.
        assert_eq!(edge_widths_for(1200., 280., &ui, true), (320., 600.));
        // Large : les largeurs gardées reviennent entières.
        assert_eq!(edge_widths_for(2000., 280., &ui, true), (372., 630.));
    }

    #[test]
    fn a_hidden_sidebar_a_closed_or_maximized_panel_leave_room_to_the_other() {
        // Barre masquée : largeur 0, et seule la moitié de la fenêtre borne le volet.
        let hidden = ui_with(Some(372.), Some(630.), false);
        assert_eq!(edge_widths_for(1000., 280., &hidden, true), (0., 500.));
        // Volet fermé ou agrandi (aucun volet à côté) : il ne limite pas la barre.
        let wide = ui_with(Some(372.), Some(630.), true);
        assert_eq!(edge_widths_for(700., 280., &wide, false).0, 372.);
        assert_eq!(edge_widths_for(700., 280., &wide, true).0, 248.);
    }

    #[test]
    fn a_stored_width_is_clamped_to_the_window_without_being_rewritten() {
        // Le volet de 630 gardé dans `state.json` tient dans 500 quand la fenêtre n'en laisse pas plus. La fonction
        // est pure : la valeur gardée n'est pas réécrite ici, le rendu ne fait que lire (vérifié en direct, step 4).
        assert_eq!(clamp_width(420., 630., 500.), 500.);
        // Fenêtre trop étroite : la largeur de la maquette reste le plancher du plafond, jamais en dessous.
        assert_eq!(clamp_width(420., 630., 100.), 420.);
        assert_eq!(clamp_width(420., 420., 100.), 420.);
        // Une largeur gardée plus étroite que la maquette n'est pas relevée.
        assert_eq!(clamp_width(420., 300., 100.), 300.);
    }

    #[test]
    fn the_attach_size_leaves_room_for_the_gutters_of_the_stored_preset() {
        let cell = (10., 24.);
        let grid = |preset: Preset| attach_grid(1000., 480., cell, preset.column_gaps());
        let single = grid(Preset::Single);
        let columns_3 = grid(Preset::Columns3);
        // Les colonnes et les deux gouttières de `columns-3` tiennent dans la zone, sans une colonne de plus.
        let extra = 24. - 10.;
        let used = |columns: usize| columns as f32 * cell.0 + 2. * extra;
        assert!(used(columns_3.columns) <= 1000.);
        assert!(used(columns_3.columns + 1) > 1000.);
        assert!(columns_3.columns < single.columns);
        assert_eq!(single.lines, columns_3.lines);
        assert!(columns_3.columns < grid(Preset::Columns2).columns);
        assert_eq!(grid(Preset::Rows2), single);
    }

    #[test]
    fn the_keyboard_stays_in_the_panel_through_a_menu_and_leaves_it_for_the_session() {
        // Le volet prend le focus : il a le clavier.
        assert!(panel_keeps_keyboard(true, true, false, false));
        // Un menu ou un champ de renommage prend le focus (ni volet ni session) : le clavier reste au volet.
        assert!(panel_keeps_keyboard(true, false, false, true));
        // La session prend le focus : le clavier la rejoint.
        assert!(!panel_keeps_keyboard(true, false, true, true));
        // Sans mémoire du volet, un menu ne le lui donne pas.
        assert!(!panel_keeps_keyboard(true, false, false, false));
        // Volet fermé ou masqué : le clavier n'y est plus, quoi qu'il ait été.
        assert!(!panel_keeps_keyboard(false, false, false, true));
    }

    use super::*;

    #[test]
    fn the_close_warning_agrees_with_the_number_of_programs() {
        assert_eq!(
            running_detail(&["claude".to_owned()]),
            "En cours : claude. Il sera arrêté."
        );
        assert_eq!(
            running_detail(&["claude".to_owned(), "vim".to_owned()]),
            "En cours : claude, vim. Ils seront arrêtés."
        );
    }

    fn listed_session(name: &str, created: u128) -> ListedSession {
        ListedSession {
            name: name.into(),
            project: "/p".into(),
            created,
            layout: "single".into(),
            ideation: false,
            remote_name: String::new(),
            display_name: name.into(),
        }
    }

    fn listed_pane(session: &str, id: u32, active: bool, agent: &str) -> ListedPane {
        ListedPane {
            session: session.into(),
            pane: PaneId(id),
            active,
            agent: agent.into(),
            idnma_agent: String::new(),
        }
    }

    #[test]
    fn entries_follow_the_bar_order_and_track_the_active_pane() {
        let sessions = [listed_session("late", 9), listed_session("early", 1)];
        let panes = [
            listed_pane("early", 4, false, "Shell"),
            listed_pane("early", 5, true, "Claude"),
            listed_pane("late", 7, false, "Pi"),
        ];
        let entries = session_entries(&sessions, &panes);
        let names: Vec<&str> = entries
            .iter()
            .map(|entry| entry.summary.name.as_str())
            .collect();
        assert_eq!(names, ["early", "late"]);
        // Le pane actif gagne, avec son agent ; sans pane actif, le premier.
        assert_eq!(
            (entries[0].pane, entries[0].agent.as_str()),
            (PaneId(5), "Claude")
        );
        assert_eq!(
            (entries[1].pane, entries[1].agent.as_str()),
            (PaneId(7), "Pi")
        );
    }

    #[test]
    fn an_entry_remembers_when_it_entered_its_state() {
        let mut entries = session_entries(
            &[listed_session("a", 1)],
            &[listed_pane("a", 1, true, "Shell")],
        );
        let entry = &mut entries[0];
        let start = entry.state_since;
        let later = start + Duration::from_secs(30);

        // Le même état : rien ne bouge, l'instant d'entrée reste celui d'origine.
        assert_eq!(entry.enter_state(ActivityState::Idle, later), None);
        assert_eq!(entry.state_since, start);

        // Un autre état : l'ancien est rendu, l'instant devient celui du changement.
        assert_eq!(
            entry.enter_state(ActivityState::Waiting, later),
            Some(ActivityState::Idle)
        );
        assert_eq!(entry.summary.state, ActivityState::Waiting);
        assert_eq!(entry.state_since, later);
    }

    #[test]
    fn the_layout_option_gives_the_starting_preset_and_defaults_to_single() {
        assert_eq!(preset_of("columns-2"), Preset::Columns2);
        assert_eq!(preset_of("grid-3x2"), Preset::Grid3x2);
        assert_eq!(preset_of(""), Preset::Single);
        assert_eq!(preset_of("unknown"), Preset::Single);
        let mut session = listed_session("split", 1);
        session.layout = "main-left".into();
        let entries = session_entries(&[session], &[listed_pane("split", 1, true, "Shell")]);
        assert_eq!(entries[0].preset, Preset::MainLeft);
    }

    #[test]
    fn session_entries_keep_the_ideation_marks() {
        let mut ideation_session = listed_session("ideation-session", 1);
        ideation_session.ideation = true;
        ideation_session.remote_name = "erom-ideation-3".into();
        let sessions = [ideation_session, listed_session("plain", 2)];
        let panes = [
            listed_pane("ideation-session", 1, true, "Fable"),
            listed_pane("plain", 2, true, "Shell"),
        ];
        let entries = session_entries(&sessions, &panes);
        let find = |name: &str| entries.iter().find(|e| e.summary.name == name).unwrap();
        assert!(find("ideation-session").summary.ideation);
        assert_eq!(find("ideation-session").remote_name, "erom-ideation-3");
        assert!(!find("plain").summary.ideation);
        assert_eq!(find("plain").remote_name, "");
    }

    const IDEATION_TEST_DIR: &str = "/Users/Shared/erom-ideation";

    #[test]
    fn ideation_sessions_are_never_adopted_as_projects() {
        let mut ideation_session = listed_session("ideation-session", 1);
        ideation_session.ideation = true;
        ideation_session.project = IDEATION_TEST_DIR.into();
        let mut unset = listed_session("unset", 3);
        unset.project = String::new();
        let sessions = [ideation_session, listed_session("plain", 2), unset];
        assert_eq!(adoptable_projects(&sessions), ["/p"]);
    }

    #[test]
    fn a_finished_creation_clears_only_its_own_pending_names() {
        let ideation = PathBuf::from(IDEATION_TEST_DIR);
        let mut names = vec![
            (ideation.clone(), "Fable".to_owned()),
            (ideation.clone(), "Fable 2".to_owned()),
            (PathBuf::from("/p"), "Fable".to_owned()),
        ];
        let mut remotes = vec!["erom-ideation-2".to_owned(), "erom-ideation-3".to_owned()];
        clear_pending(
            &mut names,
            &mut remotes,
            &ideation,
            "Fable",
            Some("erom-ideation-2"),
        );
        assert_eq!(
            names,
            [
                (ideation.clone(), "Fable 2".to_owned()),
                (PathBuf::from("/p"), "Fable".to_owned())
            ]
        );
        assert_eq!(remotes, ["erom-ideation-3"]);
        // Une session de projet n'a pas de nom Remote Control : les noms des bouillons restent.
        clear_pending(&mut names, &mut remotes, Path::new("/p"), "Fable", None);
        assert_eq!(names, [(ideation, "Fable 2".to_owned())]);
        assert_eq!(remotes, ["erom-ideation-3"]);
    }

    fn saved_pane(
        command: Option<&str>,
        conversation: Option<&str>,
    ) -> crate::restore::snapshot::SavedPane {
        crate::restore::snapshot::SavedPane {
            cwd: "/p".into(),
            agent: "Shell".into(),
            hidden_at: None,
            active: false,
            conversation: conversation.map(str::to_owned),
            command: command.map(str::to_owned),
        }
    }

    #[test]
    fn restored_launches_pair_ids_with_commands_and_continue_the_queue() {
        const UUID: &str = "aaaa1111-bbbb-4ccc-8ddd-eeeeffff0000";
        let saved = SavedSession {
            name: "idema-a".into(),
            project: "/p".into(),
            display_name: "Claude".into(),
            created: 1,
            layout: "single".into(),
            ideation: false,
            remote_name: String::new(),
            panes: vec![
                saved_pane(Some("idnma run claude"), Some(UUID)),
                saved_pane(None, None),
                saved_pane(Some("idnma run codex"), None),
            ],
        };
        let plan = plan_session(&saved, |_| true).unwrap();
        let ids = vec![PaneId(7), PaneId(8), PaneId(9)];
        let pacer = LaunchPacer::new(Instant::now());
        let (launches, resumed, fresh) = restored_launches(&plan, ids, 4, &pacer);
        // Le shell nu (pane 8) n'a pas de lancement ; les deux autres suivent les 4 déjà placés.
        let placed: Vec<(PaneId, Duration)> = launches.iter().map(|l| (l.pane, l.delay)).collect();
        assert_eq!(
            placed,
            [(PaneId(7), launch_delay(4)), (PaneId(9), launch_delay(5))]
        );
        assert_eq!(
            launches[0].command,
            format!("idnma run claude --resume {UUID}")
        );
        assert_eq!(launches[1].command, "idnma run codex");
        assert_eq!((resumed, fresh), (1, 1));
    }

    #[test]
    fn removing_the_ideation_folder_as_a_project_closes_no_ideation_session() {
        let mut ideation_session = listed_session("ideation-session", 1);
        ideation_session.ideation = true;
        ideation_session.project = "/p".into();
        let sessions = [ideation_session, listed_session("plain", 2)];
        let panes = [
            listed_pane("ideation-session", 1, true, "Fable"),
            listed_pane("plain", 2, true, "Shell"),
        ];
        let entries = session_entries(&sessions, &panes);
        assert_eq!(project_session_names(&entries, Path::new("/p")), ["plain"]);
    }

    #[test]
    fn no_project_row_is_targeted_while_an_ideation_session_is_selected() {
        let project = Path::new("/q");
        assert!(is_targeted(true, false, Some(project), project));
        assert!(!is_targeted(true, true, Some(project), project));
        assert!(!is_targeted(false, false, Some(project), project));
        assert!(!is_targeted(
            true,
            false,
            Some(project),
            Path::new("/other")
        ));
    }

    #[test]
    fn two_pending_ideation_sessions_get_distinct_remote_names() {
        let living = ["erom-ideation-2", ""];
        let none: [String; 0] = [];
        let first = {
            let taken = taken_remote_names(living.iter().copied(), &none);
            crate::ideation::next_remote_name("erom-ideation-", &taken)
        };
        assert_eq!(first, "erom-ideation-3");
        // Le premier clic n'a pas fini : son nom attend dans `pending`, le second clic prend le suivant.
        let pending = [first];
        let taken = taken_remote_names(living.iter().copied(), &pending);
        assert_eq!(
            crate::ideation::next_remote_name("erom-ideation-", &taken),
            "erom-ideation-4"
        );
    }

    #[test]
    fn a_session_without_a_pane_has_no_entry() {
        let entries = session_entries(&[listed_session("orphan", 1)], &[]);
        assert!(entries.is_empty());
    }

    #[test]
    fn missing_tmux_suggests_an_install_and_an_old_one_an_upgrade() {
        let missing = StartupError::from(ServerError::NotFound);
        assert_eq!(missing.hint, Some("brew install tmux"));
        let outdated = StartupError::from(ServerError::Unsupported("tmux 3.5".into()));
        assert_eq!(outdated.hint, Some("brew upgrade tmux"));
        let other = StartupError::from(ServerError::Tmux {
            args: "list-panes".into(),
            stderr: "boom".into(),
        });
        assert_eq!(other.hint, None);
    }

    #[test]
    fn a_startup_failure_explains_itself_and_keeps_the_raw_message_below() {
        let unreachable = StartupError::from(ServerError::Tmux {
            args: "list-panes".into(),
            stderr: "error connecting to /private/tmp/tmux-501/idema (No such file or directory)"
                .into(),
        });
        assert_eq!(unreachable.message, "Le serveur tmux est injoignable.");
        assert!(unreachable.detail.unwrap().contains("error connecting to"));
        let missing = StartupError::from(ServerError::NotFound);
        assert_eq!(
            (missing.message.as_str(), missing.detail),
            ("tmux introuvable", None)
        );
    }

    #[test]
    fn the_launcher_entries_nest_agents_under_their_harness() {
        let agent = |name: &str, harness: Option<&str>| AgentEntry {
            name: name.into(),
            harness: harness.map(str::to_owned),
            command: "c".into(),
            default: false,
            icon: None,
        };
        let agents = [
            agent("Sonnet", Some("Claude Code")),
            agent("Solo", None),
            agent("Opus", Some("Claude Code")),
            agent("Argon", Some("Antigravity")),
        ];
        let entries = agent_menu_entries(&agents, |_, agent| {
            MenuItemSpec::new(agent.name.clone(), |_, _, _| {})
        });
        let shape: Vec<(String, Vec<String>)> = entries
            .iter()
            .map(|entry| match entry {
                MenuEntry::Submenu {
                    label,
                    items,
                    openable,
                } => {
                    // Un harness du lanceur sans agent actif reste fermé.
                    assert!(!openable);
                    (
                        label.to_string(),
                        items.iter().map(|item| item.label.to_string()).collect(),
                    )
                }
                MenuEntry::Item(item) => (item.label.to_string(), Vec::new()),
                _ => panic!("a launcher only holds agents"),
            })
            .collect();
        let expected = [
            ("Antigravity", vec!["Argon"]),
            ("Claude Code", vec!["Opus", "Sonnet"]),
            ("Solo", vec![]),
        ]
        .map(|(label, items)| {
            (
                label.to_owned(),
                items.into_iter().map(str::to_owned).collect::<Vec<_>>(),
            )
        });
        assert_eq!(shape, expected);
    }

    #[test]
    fn a_gesture_menu_keeps_its_labels_shortcuts_states_and_separators_in_order() {
        use crate::files::gestures::EditorGesture;
        let lines = crate::files::gestures::editor_menu(false, true);
        let expected = lines.clone();
        let entries = menu_entries(lines, |_: EditorGesture, _, _| {});
        assert_eq!(entries.len(), expected.len());
        for (entry, line) in entries.iter().zip(&expected) {
            match (entry, line) {
                (MenuEntry::Separator, MenuLine::Separator) => {}
                (MenuEntry::Item(entry), MenuLine::Item(item)) => {
                    assert_eq!(entry.label.as_ref(), item.label);
                    assert_eq!(entry.shortcut.as_deref(), item.shortcut);
                    assert_eq!(entry.enabled, item.enabled);
                }
                _ => panic!("a menu line changed kind in translation"),
            }
        }
    }

    #[test]
    fn two_machines_with_one_session_name_keep_two_entries() {
        use crate::machines::sessions::qualify_listing;
        let mut sessions = Vec::new();
        let mut panes = Vec::new();
        for connection in ["caserne-claude", "dell-lan"] {
            let (listed, own) = qualify_listing(
                connection,
                vec![listed_session("testvpc", 1)],
                vec![listed_pane("testvpc", 0, true, "")],
            );
            sessions.extend(listed);
            panes.extend(own);
        }
        sessions.push(listed_session("testvpc", 1));
        panes.push(listed_pane("testvpc", 0, true, "Shell"));
        let entries = session_entries(&sessions, &panes);
        assert_eq!(entries.len(), 3);
        let mut machines: Vec<Option<&str>> = entries
            .iter()
            .map(|entry| entry.summary.machine.as_deref())
            .collect();
        machines.sort();
        assert_eq!(machines, [None, Some("caserne-claude"), Some("dell-lan")]);
        // Les trois clés diffèrent : chacune est celle d'une seule entrée.
        let mut names: Vec<&str> = entries
            .iter()
            .map(|entry| entry.summary.name.as_str())
            .collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn pane_inputs_ignore_remote_entries() {
        use crate::machines::sessions::qualify_listing;
        let (mut sessions, mut panes) = qualify_listing(
            "dell-lan",
            vec![listed_session("testvpc", 1)],
            vec![listed_pane("testvpc", 7, true, "")],
        );
        sessions.push(listed_session("idema-a", 2));
        panes.push(listed_pane("idema-a", 1, true, "Shell"));
        let entries = session_entries(&sessions, &panes);
        assert_eq!(entries.len(), 2);
        // Les panes lus pour les serveurs de dev sont ceux du tmux du Mac : `%7` d'une machine n'y figure pas.
        let names: Vec<String> = local_session_infos(&entries)
            .into_iter()
            .map(|info| info.name)
            .collect();
        assert_eq!(names, ["idema-a"]);
    }

    #[test]
    fn a_remote_key_never_enters_the_restore_snapshot() {
        use crate::machines::key::remote_session_key;
        let saved = |name: &str| SavedSession {
            name: name.into(),
            project: PathBuf::from("/p"),
            display_name: name.into(),
            created: 1,
            layout: String::new(),
            ideation: false,
            remote_name: String::new(),
            panes: Vec::new(),
        };
        let kept = local_sessions_only(vec![
            saved("idema-a"),
            saved(&remote_session_key("dell-lan", "testvpc")),
        ]);
        let names: Vec<&str> = kept.iter().map(|session| session.name.as_str()).collect();
        assert_eq!(names, ["idema-a"]);
    }
}
