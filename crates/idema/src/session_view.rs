//! La vue d'une session : possède le client de contrôle, route ses événements vers les panes, suit les fenêtres
//! tmux (layout, pane actif, panes cachés), place les panes, tient la taille demandée à tmux, répare le layout
//! et agrège l'état. Tout ce qui est propre à un pane (saisie, émulateur, signaux, lancement, HUD) est dans
//! `pane_view`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedSender};
use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton,
    MouseDownEvent, Pixels, Subscription, Task, Window, actions, canvas, div, prelude::*, px,
};
use idema_term::activity::{ActivityState, LaunchPacer, most_urgent};
use idema_term::emulator::GridSize;
use idema_term::pane_session::size_watch_pane;
use idema_tmux::client::ControlClient;
use idema_tmux::command::{BORDER_WATCH, PaneWatch, border_reset, border_subscription};
use idema_tmux::layout::{Direction, PaneRect, Preset, WindowLayout, neighbor, preset_cell};
use idema_tmux::protocol::{ControlEvent, PaneId};
use idema_tmux::server::{ServerError, TmuxServer};
use idema_tmux::windows::{
    LayoutRepair, PaneRoster, QueryStep, SessionWindows, UNZOOM_SETTLE, WindowsQuery, layout_repair,
};

use crate::config::SHELL_SESSION_NAME;
use crate::file_open::PathTarget;
use crate::gutter::{Frame, pane_frame, separator_frame, session_columns};
use crate::hud;
use crate::pane_view::{LaunchRequest, PaneEvent, PaneView, RESIZE_SETTLE, SessionRef};
use crate::signpost;
use crate::terminal_element::{TerminalFont, cell_metrics, content_bounds, grid_for};
use crate::theme::{INACTIVE_PANE, PANE_SEPARATOR, Theme, WINDOW, color, white};

actions!(
    session,
    [
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
        ToggleZoom
    ]
);

/// Délai après le dernier changement de la zone centrale avant de demander la nouvelle taille à tmux (spec §3.3).
const SIZE_REQUEST_DELAY: Duration = Duration::from_millis(50);

// La relecture d'un pane qui a changé de taille ne part qu'une fois la taille posée par tmux : son silence doit
// être plus long que le délai de la demande, sans quoi la relecture capturerait l'ancienne taille.
const _: () = assert!(RESIZE_SETTLE.as_millis() > SIZE_REQUEST_DELAY.as_millis());

/// Silence des notifications de layout avant que la session ne se répare (spec §4.4, écart E28).
const LAYOUT_SETTLE: Duration = Duration::from_millis(300);

/// Temps pendant lequel un focus donné par Idema prime sur le pane actif que tmux rapporte : `select-pane` est un
/// autre process, sa notification arrive après le geste.
const FOCUS_PENDING_MAX: Duration = Duration::from_secs(1);

/// Pas de la minuterie : chaque pane fait tomber ses états à délai et rafraîchit son HUD ; les panes absents des
/// layouts depuis plus de `PANE_GONE_GRACE` partent.
const TICK: Duration = Duration::from_secs(1);

/// Nombre de pas entre deux lignes de mesure du journal (5 s) et deux vidages du tampon.
const LOG_EVERY_TICKS: u64 = 5;

/// Valeur de `echo_pane` tant qu'aucun pane n'est actif : aucun identifiant de pane tmux ne l'atteint.
const NO_PANE: u32 = u32::MAX;

/// `refresh-client -C` : la taille de la zone centrale, demandée à tmux pour la session affichée (spec §3.3).
fn client_size_command(grid: GridSize) -> String {
    format!("refresh-client -C {}x{}", grid.columns, grid.lines)
}

/// La commande d'agent d'un pane, et le délai avant lequel elle ne part pas : une restauration étale ses lancements.
#[derive(Debug, Clone)]
pub struct PaneLaunch {
    pub pane: PaneId,
    pub command: String,
    pub delay: Duration,
    /// Le rythme commun des lancements d'une restauration (`None` pour une session neuve).
    pub pacer: Option<LaunchPacer>,
}

/// Ce que l'espace de travail sait d'une session au moment de l'attacher.
#[derive(Debug, Clone)]
pub struct SessionEntryInfo {
    pub name: String,
    pub project: PathBuf,
    /// Le preset de `@idema_layout` ; `Single` quand l'option manque.
    pub preset: Preset,
}

/// Ce que la vue signale à l'espace de travail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Le client de contrôle s'est arrêté (`%exit`, flux fermé) : la session a peut-être disparu.
    Closed,
    /// Le pane actif, son titre ou sa commande au premier plan a changé : la barre se repeint.
    MetaChanged,
    /// L'état de la session (le plus urgent de ses panes, cachés compris) a changé : pastilles, badge et son
    /// suivent.
    StateChanged {
        previous: ActivityState,
        current: ActivityState,
    },
    /// L'état d'un de ses panes a changé, que celui de la session change ou non : le compagnon republie la flotte, qui
    /// porte l'état de chaque pane.
    PanesChanged,
    /// Le focus GPUI d'un de ses panes a changé : l'espace de travail recalcule qui tient le clavier.
    FocusChanged,
    /// Un pane caché par le zoom passe en « attend » alors que la session est vue : le son part quand même
    /// (décision J4-13).
    HiddenPaneWaiting { pane: PaneId },
    /// `%sessions-changed` : une session est née ou morte sur le serveur. Chaque client le reçoit : l'espace de
    /// travail regroupe les notifications de toutes les vues.
    SessionsChanged,
    /// `⌘`-clic sur un chemin d'un de ses panes : l'espace de travail l'ouvre dans le volet Fichiers.
    OpenPath(PathTarget),
    /// `⌘`-clic sur une adresse web ou un document local d'un de ses panes : l'espace de travail l'ouvre dans le
    /// navigateur.
    OpenUrl(String),
}

/// Ce que la dernière peinture a mesuré de la zone centrale.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Zone {
    /// La zone entière, marges comprises.
    area: Bounds<Pixels>,
    /// La zone utile : la zone moins les marges de la maquette.
    content: Bounds<Pixels>,
    cell: (Pixels, Pixels),
    /// Les lignes de cellules qui tiennent dans la zone utile.
    lines: usize,
}

impl Zone {
    /// La taille à demander à tmux : les colonnes laissent la place des `gaps` gouttières (`gutter.rs`).
    fn grid(&self, gaps: u16) -> GridSize {
        let cell = (f32::from(self.cell.0), f32::from(self.cell.1));
        GridSize {
            columns: session_columns(f32::from(self.content.size.width), gaps, cell),
            lines: self.lines,
        }
    }
}

/// Les gouttières à réserver dans la taille demandée : celles du preset visé par une opération en cours, sinon
/// celles du layout de la fenêtre visible, sinon (fenêtres pas encore lues) celles du preset de la session.
fn requested_gaps(operation: Option<u16>, layout: Option<&WindowLayout>, preset: Preset) -> u16 {
    operation
        .or_else(|| layout.map(WindowLayout::column_gaps))
        .unwrap_or_else(|| preset.column_gaps())
}

/// Un événement du client de contrôle, avec la génération du client qui l'a émis et son instant d'arrivée.
type Incoming = (u64, Instant, ControlEvent);

/// Un pane suivi : sa vue et l'abonnement aux événements de cette vue.
struct PaneSlot {
    id: PaneId,
    view: Entity<PaneView>,
    _subscription: Subscription,
}

/// Une opération de layout d'Idema, lancée hors du fil d'interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Preset(Preset),
    Restore,
}

/// Ce qui identifie une réparation : la même, avec le même nombre de panes visibles et cachés dans la même fenêtre,
/// ne repart pas après un échec ou sans effet. Sans fenêtre visible (`Restore`), le nombre de panes cachés fait la
/// différence : une restauration qui a rendu un pane change la clé, la suivante peut partir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RepairKey {
    repair: LayoutRepair,
    visible: usize,
    hidden: usize,
    window: (u16, u16),
}

#[derive(Debug, PartialEq, Eq)]
enum RepairStep {
    /// Tout est en ordre : la dernière réparation tentée est oubliée.
    InOrder,
    /// La même réparation a déjà été tentée sans rien changer : on n'y revient pas.
    Stuck,
    /// La fenêtre est zoomée : rien ne se pose avant le dézoom, dont le `%layout-change` réarme la réparation
    /// (écart E30).
    Deferred,
    Run(Op, RepairKey),
}

/// Vrai si les panes du layout ont la géométrie du preset pour la taille de la fenêtre (ou si la fenêtre est trop
/// petite pour le preset : il n'y a rien à poser).
fn matches_preset(layout: &WindowLayout, preset: Preset) -> bool {
    let Some(ideal) = preset_cell(preset, layout.width(), layout.height()) else {
        return true;
    };
    let ideal = WindowLayout {
        root: ideal,
        active: None,
    };
    let geometry = |layout: &WindowLayout| -> Vec<(u16, u16, u16, u16)> {
        layout
            .panes()
            .iter()
            .map(|rect| (rect.x, rect.y, rect.width, rect.height))
            .collect()
    };
    geometry(layout) == geometry(&ideal)
}

/// Ce que la réparation fait maintenant. `reapply` : la taille de la fenêtre vient d'être posée (écart E17), tmux
/// a pu redistribuer les cellules sans parts égales, le preset courant est reposé, mais seulement quand le nombre de
/// panes visibles est celui du preset : au-delà de six panes (écart E28) la session s'affiche telle que tmux la rend.
/// Zoomée, la fenêtre n'est pas touchée : poser un layout dézoomerait (écart E30).
fn repair_step(
    windows: &SessionWindows,
    preset: Preset,
    reapply: bool,
    attempted: Option<RepairKey>,
) -> RepairStep {
    if windows.zoomed_pane().is_some() {
        return RepairStep::Deferred;
    }
    let layout = windows.visible_layout();
    let (op, repair) = match layout_repair(windows, preset) {
        LayoutRepair::Restore => (Op::Restore, LayoutRepair::Restore),
        LayoutRepair::Apply(target) => (Op::Preset(target), LayoutRepair::Apply(target)),
        LayoutRepair::Nothing => match layout {
            Some(layout)
                if reapply
                    && layout.panes().len() == preset.panes()
                    && !matches_preset(layout, preset) =>
            {
                (Op::Preset(preset), LayoutRepair::Apply(preset))
            }
            _ => return RepairStep::InOrder,
        },
    };
    let key = RepairKey {
        repair,
        visible: layout.map_or(0, |layout| layout.panes().len()),
        hidden: windows.hidden_panes().len(),
        window: layout.map_or((0, 0), |layout| (layout.width(), layout.height())),
    };
    if attempted == Some(key) {
        RepairStep::Stuck
    } else {
        RepairStep::Run(op, key)
    }
}

/// Où va un événement du client de contrôle.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// `%exit` : la session se ferme.
    Exit,
    /// Le pane nommé (sortie, pause, abonnement).
    Pane(PaneId),
    /// Une réponse : la relecture des fenêtres, puis chaque pane (relectures d'écran).
    Reply,
    /// Les fenêtres de la session : layout, ajout, fermeture, fenêtre courante, pane actif.
    Windows,
    /// `pane-border-status` d'une fenêtre a changé : Idema l'éteint s'il ne l'est pas (AC-38).
    Border,
    /// `%sessions-changed` : l'espace de travail relit les sessions.
    Sessions,
    Ignore,
}

fn route(event: &ControlEvent) -> Route {
    match event {
        ControlEvent::Exit { .. } => Route::Exit,
        ControlEvent::Output { pane, .. } | ControlEvent::Pause { pane } => Route::Pane(*pane),
        ControlEvent::SubscriptionChanged { name, .. } if name == BORDER_WATCH => Route::Border,
        ControlEvent::SubscriptionChanged { name, .. } => PaneWatch::parse(name)
            .map(|(_, pane)| pane)
            .or_else(|| size_watch_pane(name))
            .map_or(Route::Ignore, Route::Pane),
        ControlEvent::Reply { .. } => Route::Reply,
        ControlEvent::SessionsChanged => Route::Sessions,
        ControlEvent::LayoutChange { .. }
        | ControlEvent::WindowAdd { .. }
        | ControlEvent::WindowClose { .. }
        | ControlEvent::SessionChanged { .. }
        | ControlEvent::SessionWindowChanged { .. }
        | ControlEvent::WindowPaneChanged { .. } => Route::Windows,
        _ => Route::Ignore,
    }
}

/// Le pane actif de la session. Celui de tmux, sauf : un focus que l'utilisateur vient de donner et que tmux n'a pas
/// encore rapporté, et une opération de layout d'Idema en cours (le focus d'Idema ne suit pas tmux pendant qu'il
/// déplace des panes). Toujours un pane visible ; à défaut, le premier.
fn resolve_active(
    current: Option<PaneId>,
    tmux_active: Option<PaneId>,
    pending: Option<PaneId>,
    op_pending: bool,
    visible: &[PaneId],
) -> Option<PaneId> {
    let shown = |pane: &PaneId| visible.contains(pane);
    pending
        .filter(shown)
        .or_else(|| current.filter(shown).filter(|_| op_pending))
        .or_else(|| tmux_active.filter(shown))
        .or_else(|| visible.first().copied())
}

/// Un focus donné par Idema vaut encore tant que tmux ne l'a pas rapporté, que le pane est visible et que le délai
/// n'est pas écoulé.
fn pending_focus_alive(
    pane: PaneId,
    elapsed: Duration,
    tmux_active: Option<PaneId>,
    visible: &[PaneId],
) -> bool {
    elapsed < FOCUS_PENDING_MAX && tmux_active != Some(pane) && visible.contains(&pane)
}

/// Le son d'un pane caché par le zoom : il part quand le pane passe en « attend », dans une session vue. Une session
/// non vue sonne déjà pour son état agrégé (décision J4-13).
fn hidden_pane_chimes(
    seen: bool,
    zoom_hidden: bool,
    previous: ActivityState,
    current: ActivityState,
) -> bool {
    seen && zoom_hidden && current == ActivityState::Waiting && previous != ActivityState::Waiting
}

/// Les drapeaux d'un pane, calculés pour toute la session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneFlags {
    /// Vu : la session est vue et le pane est affiché. Un pane caché, ou caché par le zoom, n'est jamais vu
    /// (écart E18).
    seen: bool,
    /// Le programme du pane est au premier plan : session affichée, fenêtre active, pane actif (mode 1004).
    focused: bool,
    active: bool,
}

fn pane_flags(
    pane: PaneId,
    displayed: &[PaneId],
    active: Option<PaneId>,
    session_seen: bool,
    window_focused: bool,
) -> PaneFlags {
    let is_active = active == Some(pane);
    PaneFlags {
        seen: session_seen && displayed.contains(&pane),
        focused: window_focused && is_active,
        active: is_active,
    }
}

/// La grille d'un pane d'après son rectangle : au moins 2 colonnes sur 1 ligne, ce que l'émulateur accepte.
fn grid_of(rect: &PaneRect) -> GridSize {
    GridSize {
        columns: usize::from(rect.width).max(2),
        lines: usize::from(rect.height).max(1),
    }
}

/// Place un `div` absolu au `frame`, dans la zone utile.
fn place(frame: Frame) -> gpui::Div {
    div()
        .absolute()
        .left(px(frame.x))
        .top(px(frame.y))
        .w(px(frame.width))
        .h(px(frame.height))
}

/// Attache un client de contrôle à la session. Les événements partent vers `sender` avec la `generation` du
/// client : ceux d'un client remplacé par une reconnexion sont ignorés. Le repère `echo` s'ouvre ici, sur le fil de
/// lecture, pour la sortie du pane actif (lu dans `echo_pane`).
fn connect(
    server: &TmuxServer,
    session: &str,
    generation: u64,
    sender: UnboundedSender<Incoming>,
    echo_pane: Arc<AtomicU32>,
) -> Result<ControlClient, ServerError> {
    // Le rappel du client tourne sur son fil de lecture et ne doit jamais bloquer (son `Drop` joint ce fil) :
    // un canal sans borne, vidé côté interface. Une erreur d'envoi veut dire que la vue est fermée.
    // L'instant d'arrivée est pris ici, sur le fil de lecture : c'est le départ de la latence d'écho.
    ControlClient::spawn(server, session, move |event| {
        // Même instant pour le repère Instruments : il ouvre l'intervalle `echo` que `record_frame` ferme.
        // Le pane actif est seul à être peint : la sortie des autres n'ouvre pas d'intervalle.
        if matches!(&event, ControlEvent::Output { pane, .. } if pane.0 == echo_pane.load(Ordering::Relaxed))
        {
            signpost::echo_arrived();
        }
        let _ = sender.unbounded_send((generation, Instant::now(), event));
    })
}

/// Envoie une relecture des fenêtres sur `client` et rend la requête, qui lira la réponse.
fn start_query(client: &mut ControlClient, counter: &mut u64) -> WindowsQuery {
    *counter += 1;
    let query = WindowsQuery::new(&format!("{}-{}", std::process::id(), counter));
    // Une écriture qui échoue veut dire que le client est mort : son fil de lecture le signale par `Exit`.
    let _ = client.send(&query.command_line());
    query
}

pub struct SessionView {
    server: Arc<TmuxServer>,
    /// Nom tmux de la session et dossier de son projet : `apply_preset` crée les shells neufs dans le projet.
    name: String,
    project: PathBuf,
    /// Le client de contrôle de la session : la vue le garde en vie, les panes y écrivent par un `ClientWriter`.
    client: ControlClient,
    /// Génération du client courant : les événements d'un client remplacé sont ignorés.
    client_generation: u64,
    /// Depuis quand le client courant est attaché (`spawn` ou dernier `reconnect`) : un client qui tombe aussitôt
    /// ne compte pas pour un essai réussi (écart E33).
    connected_at: Instant,
    /// La raison du dernier `Exit` du client courant, `client stream closed` compris.
    exit_reason: Option<String>,
    /// Vrai depuis le `Exit` du client courant jusqu'à la reconnexion : un `%exit` sans raison laisse `exit_reason`
    /// vide, mais le client est mort et rien de ce qu'on lui écrit n'arrive.
    client_exited: bool,
    /// L'entrée du canal d'événements, gardée pour le client d'une reconnexion.
    sender: UnboundedSender<Incoming>,
    /// Identifiant du pane actif, lu par le rappel du client pour le repère `echo` (`NO_PANE` sans pane actif).
    echo_pane: Arc<AtomicU32>,
    /// Les panes suivis : ceux de toutes les fenêtres de la session, cachés compris.
    slots: Vec<PaneSlot>,
    windows: SessionWindows,
    roster: PaneRoster,
    /// La relecture des fenêtres en vol, s'il y en a une.
    query: Option<WindowsQuery>,
    query_counter: u64,
    /// Vrai après la première réponse de la relecture : avant, rien n'est dessiné ni suivi.
    windows_loaded: bool,
    /// Les commandes d'agent à taper, une par pane, quand le pane qui porte l'identifiant apparaît.
    launches: Vec<PaneLaunch>,
    active: Option<PaneId>,
    /// Un focus donné par Idema, que tmux n'a pas encore rapporté.
    pending_focus: Option<(PaneId, Instant)>,
    preset: Preset,
    /// Une opération de layout est en vol : une seule à la fois.
    op_pending: bool,
    /// La dernière réparation tentée, pour ne pas la reprendre telle quelle.
    attempted_repair: Option<RepairKey>,
    /// Compteur qui annule les minuteries de réparation dépassées par un nouveau changement.
    repair_generation: u64,
    /// La taille de la fenêtre vient d'être posée : le preset courant est reposé une fois le layout stable.
    reapply_preset: bool,
    /// Taille demandée à tmux : celle de la zone centrale, une fois la session affichée (spec §3.3).
    requested: Option<GridSize>,
    /// Les gouttières du preset visé par l'opération de layout en cours : le layout de la fenêtre ne les montre
    /// qu'une fois l'opération finie et les fenêtres relues.
    operation_gaps: Option<u16>,
    /// Compteur qui annule les demandes de taille dépassées par un nouveau changement de la zone.
    size_generation: u64,
    zone: Option<Zone>,
    /// Vrai pour la seule session montrée au centre : elle seule suit la zone, rapporte le focus (mode 1004)
    /// et écrit au journal.
    displayed: bool,
    /// Le clavier est du côté de la session et non du volet Terminal : sans lui, ses panes ne sont plus « au premier
    /// plan » et leur programme apprend la perte du focus (ESC[O, spec §6.3). Posé par l'espace de travail.
    keyboard: bool,
    /// La session est vue (centre de la fenêtre active) : ses panes visibles le sont aussi.
    seen: bool,
    /// Dernier état agrégé publié : `StateChanged` ne part qu'à un changement.
    last_state: ActivityState,
    ticks: u64,
    /// Reçoit le focus quand aucun pane n'existe encore ; la racine de la vue le suit.
    focus_handle: FocusHandle,
    _event_task: Task<()>,
    _tick_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl SessionView {
    /// Attache un client de contrôle à la session, puis crée la vue. `Some(grid)` attache la session affichée à la
    /// taille de la zone centrale ; `None` attache une session de fond, qui garde la taille de ses panes
    /// jusqu'à son affichage (spec §3.3). Les panes naissent de la première relecture des fenêtres. `launches` donne,
    /// par pane, la commande d'agent à taper quand il est prêt.
    pub fn spawn(
        server: Arc<TmuxServer>,
        session: &SessionEntryInfo,
        launches: Vec<PaneLaunch>,
        size: Option<GridSize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self>, ServerError> {
        let (sender, mut receiver) = mpsc::unbounded();
        let echo_pane = Arc::new(AtomicU32::new(NO_PANE));
        let step = Instant::now();
        let mut client = connect(&server, &session.name, 0, sender.clone(), echo_pane.clone())?;
        hud::log_step("attach.control_client", step);
        // La taille d'abord : la relecture qui suit capture déjà les panes à cette taille. Une session de fond garde
        // la sienne jusqu'à son affichage.
        if let Some(grid) = size {
            let _ = client.send(&client_size_command(grid));
        }
        let mut query_counter = 0;
        let query = start_query(&mut client, &mut query_counter);
        let _ = client.send(&border_subscription());
        let name = session.name.clone();
        let project = session.project.clone();
        let preset = session.preset;
        let step = Instant::now();

        let view = Ok(cx.new(|cx: &mut Context<Self>| {
            let event_task = cx.spawn_in(window, async move |this, cx| {
                while let Some((generation, arrival, event)) = receiver.next().await {
                    if this
                        .update_in(cx, |view, window, cx| {
                            view.on_event(generation, event, arrival, window, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let tick_task = cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    if this
                        .update_in(cx, |view, window, cx| view.on_tick(window, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            // ESC[I et ESC[O : le programme du pane les demande par le mode 1004.
            // Seul le pane affiché les reçoit : sinon chaque activation réveillerait toutes les sessions de fond.
            let subscriptions = vec![cx.observe_window_activation(window, |view, window, cx| {
                view.apply_flags(window, cx);
                cx.notify();
            })];
            Self {
                server,
                name,
                project,
                client,
                client_generation: 0,
                connected_at: Instant::now(),
                exit_reason: None,
                client_exited: false,
                sender,
                echo_pane,
                slots: Vec::new(),
                windows: SessionWindows::new(),
                roster: PaneRoster::new(),
                query: Some(query),
                query_counter,
                windows_loaded: false,
                launches,
                active: None,
                pending_focus: None,
                preset,
                op_pending: false,
                attempted_repair: None,
                repair_generation: 0,
                // Attachée à la taille de la zone (`-C` déjà envoyé) : `sync_size` ne la renverra pas, donc le preset est
                // reposé ici une fois le layout stable (écart E17), sans quoi un relancement garde le partage de tmux.
                reapply_preset: size.is_some(),
                requested: size,
                operation_gaps: None,
                size_generation: 0,
                zone: None,
                displayed: false,
                keyboard: true,
                seen: false,
                last_state: ActivityState::Idle,
                ticks: 0,
                focus_handle: cx.focus_handle(),
                _event_task: event_task,
                _tick_task: tick_task,
                _subscriptions: subscriptions,
            }
        }));
        hud::log_step("attach.view", step);
        view
    }

    /// Le pane actif : celui qui a le focus de tmux.
    pub fn active_pane(&self) -> Option<PaneId> {
        self.active
    }

    fn active_view(&self) -> Option<&Entity<PaneView>> {
        let active = self.active?;
        self.slots
            .iter()
            .find(|slot| slot.id == active)
            .map(|slot| &slot.view)
    }

    pub fn preset(&self) -> Preset {
        self.preset
    }

    /// Les panes suivis, cachés compris : leur identifiant tmux, s'ils sont le pane actif, et leur état propre. C'est
    /// ce que le compagnon montre de la session, au-delà de l'état le plus urgent.
    pub fn pane_states(&self, cx: &App) -> Vec<(PaneId, bool, ActivityState)> {
        self.slots
            .iter()
            .map(|slot| {
                (
                    slot.id,
                    self.active == Some(slot.id),
                    slot.view.read(cx).state(),
                )
            })
            .collect()
    }

    /// Le nombre de panes cachés (la fenêtre cachée), pour « N panes masqués ».
    pub fn hidden_count(&self) -> usize {
        self.windows.hidden_panes().len()
    }

    /// Le plus urgent des états des panes cachés : la pastille de « N panes masqués » (écart E18).
    pub fn hidden_state(&self, cx: &App) -> ActivityState {
        let hidden: Vec<PaneId> = self
            .windows
            .hidden_panes()
            .iter()
            .map(|rect| rect.pane)
            .collect();
        most_urgent(
            self.slots
                .iter()
                .filter(|slot| hidden.contains(&slot.id))
                .map(|slot| slot.view.read(cx).state()),
        )
    }

    /// Vrai si la session est vue (centre de la fenêtre active).
    pub fn is_seen(&self) -> bool {
        self.seen
    }

    /// Le shell du pane actif est au prompt (marqueurs OSC 133, sinon commande au premier plan). Faux sans pane.
    pub fn at_prompt(&self, cx: &App) -> bool {
        self.active_view()
            .is_some_and(|view| view.read(cx).at_prompt())
    }

    /// Les programmes encore en cours au premier plan de tous les panes, cachés compris, pour confirmer une
    /// fermeture (spec §4.2).
    pub fn running_programs(&self, cx: &App) -> Vec<String> {
        self.slots
            .iter()
            .flat_map(|slot| slot.view.read(cx).running_programs())
            .collect()
    }

    /// Envoie la commande d'un agent dans le pane actif s'il est au prompt (spec §4.3). Faux, sans rien taper,
    /// quand un programme tourne.
    pub fn launch_in_pane(&mut self, command: &str, cx: &mut Context<Self>) -> bool {
        let Some(view) = self.active_view().cloned() else {
            return false;
        };
        view.update(cx, |pane, _| pane.launch_in_pane(command))
    }

    /// Tape la commande de reprise d'une session ramenée dans le pane `pane`, s'il existe encore et s'il est au prompt.
    /// Faux, sans rien taper, sinon, dont quand le client de contrôle est mort : rien de ce qu'on lui écrit n'arrive, et
    /// annoncer une frappe serait faux.
    pub fn reclaim_in(&mut self, pane: PaneId, command: &str, cx: &mut Context<Self>) -> bool {
        if self.client_exited {
            return false;
        }
        let Some(view) = self.live_pane_view(pane) else {
            return false;
        };
        view.update(cx, |pane, _| pane.reclaim_in_pane(command))
    }

    /// Montre une bulle d'échec sur le pane `pane`, ou sur le pane actif s'il a disparu. Faux quand la session n'a plus
    /// aucun pane : la bulle n'a nulle part où s'afficher.
    pub fn show_failure(&mut self, pane: PaneId, text: String, cx: &mut Context<Self>) -> bool {
        let Some(view) = self
            .live_pane_view(pane)
            .or_else(|| self.active_view().cloned())
        else {
            return false;
        };
        view.update(cx, |pane, cx| pane.show_failure(text, cx));
        true
    }

    /// La vue du pane `id`, s'il est encore dans un layout. Un pane fermé garde sa vue pendant `PANE_GONE_GRACE` : sa
    /// seule présence dans `slots` ne prouve pas qu'il existe, et ce qu'on lui écrit se perdrait.
    fn live_pane_view(&self, id: PaneId) -> Option<Entity<PaneView>> {
        self.windows
            .all_panes()
            .iter()
            .any(|rect| rect.pane == id)
            .then(|| self.pane_view(id))
            .flatten()
    }

    /// La session est vue ou ne l'est plus. Vue, « attend » et « erreur » de ses panes affichés retombent ; ceux
    /// d'un pane caché, par la fenêtre cachée ou par le zoom, restent (écart E18).
    pub fn set_seen(&mut self, seen: bool, cx: &mut Context<Self>) {
        self.seen = seen;
        let displayed = self.displayed_ids();
        for slot in &self.slots {
            let pane_seen = seen && displayed.contains(&slot.id);
            slot.view
                .update(cx, |pane, cx| pane.set_seen(pane_seen, cx));
        }
    }

    /// Montre ou retire la vue du centre. Seule la session affichée suit la zone centrale : une session de fond
    /// est redimensionnée au moment où elle s'affiche (spec §3.3).
    pub fn set_displayed(&mut self, displayed: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.displayed == displayed {
            return;
        }
        self.displayed = displayed;
        self.apply_flags(window, cx);
        if displayed {
            self.sync_size(window, cx);
        }
        cx.notify();
    }

    /// Le clavier est de nouveau du côté de la session, ou l'a quittée pour le volet Terminal : ses panes l'apprennent
    /// par ESC[I et ESC[O (mode 1004), comme pour un changement de fenêtre active.
    pub fn set_keyboard(&mut self, keyboard: bool, window: &Window, cx: &mut Context<Self>) {
        if self.keyboard == keyboard {
            return;
        }
        self.keyboard = keyboard;
        self.apply_flags(window, cx);
        cx.notify();
    }

    /// Les identifiants des panes visibles, dans l'ordre du layout.
    fn visible_ids(&self) -> Vec<PaneId> {
        self.windows
            .visible_panes()
            .iter()
            .map(|rect| rect.pane)
            .collect()
    }

    /// Les identifiants des panes affichés : le pane zoomé seul pendant un zoom, sinon les panes visibles.
    fn displayed_ids(&self) -> Vec<PaneId> {
        self.windows
            .displayed_panes()
            .iter()
            .map(|rect| rect.pane)
            .collect()
    }

    /// Les panes visibles que le zoom cache ; vide sans zoom.
    fn zoom_hidden_ids(&self) -> Vec<PaneId> {
        let Some(zoomed) = self.windows.zoomed_pane() else {
            return Vec::new();
        };
        self.visible_ids()
            .into_iter()
            .filter(|pane| *pane != zoomed)
            .collect()
    }

    /// Le plus urgent des états des panes que le zoom cache : `None` sans zoom. La pastille de l'icône de zoom.
    pub fn zoom_badge(&self, cx: &App) -> Option<ActivityState> {
        self.windows.zoomed_pane()?;
        let hidden = self.zoom_hidden_ids();
        Some(most_urgent(
            self.slots
                .iter()
                .filter(|slot| hidden.contains(&slot.id))
                .map(|slot| slot.view.read(cx).state()),
        ))
    }

    fn pane_view(&self, id: PaneId) -> Option<Entity<PaneView>> {
        self.slots
            .iter()
            .find(|slot| slot.id == id)
            .map(|slot| slot.view.clone())
    }

    /// Vrai si le focus de la fenêtre est dans la session : sur un de ses panes, ou sur la poignée d'attente.
    pub(crate) fn has_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.is_focused(window)
            || self
                .slots
                .iter()
                .any(|slot| slot.view.read(cx).focus_handle_ref().is_focused(window))
    }

    /// Donne le focus au pane actif (à la poignée d'attente sans pane).
    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.active_view().map_or_else(
            || self.focus_handle.clone(),
            |view| view.read(cx).focus_handle_ref().clone(),
        );
        window.focus(&handle, cx);
    }

    /// Colle `text` dans le pane actif comme le ferait un collage, lui donne le focus si `focus`, et rend son
    /// identifiant. `None` quand la session n'a pas de pane.
    pub fn paste_to_active(
        &mut self,
        text: &str,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneId> {
        let view = self.active_view()?.clone();
        let pane = self.active?;
        view.update(cx, |pane, _| pane.paste_text(text));
        if focus {
            self.focus_active(window, cx);
        }
        Some(pane)
    }

    /// Pousse à chaque pane ses drapeaux : vu (session vue et pane affiché), au premier plan (session affichée,
    /// clavier de son côté, fenêtre active, pane actif), actif.
    fn apply_flags(&mut self, window: &Window, cx: &mut Context<Self>) {
        let displayed = self.displayed_ids();
        let window_focused = self.displayed && self.keyboard && window.is_window_active();
        for slot in &self.slots {
            let flags = pane_flags(slot.id, &displayed, self.active, self.seen, window_focused);
            slot.view.update(cx, |pane, cx| {
                pane.set_seen(flags.seen, cx);
                pane.set_focused(flags.focused, cx);
                pane.set_active(flags.active, cx);
            });
        }
    }

    /// Change le pane actif. Vrai si c'est un autre : le repère `echo` le suit, la barre se repeint.
    fn set_active(&mut self, active: Option<PaneId>, cx: &mut Context<Self>) -> bool {
        if self.active == active {
            return false;
        }
        self.active = active;
        self.echo_pane
            .store(active.map_or(NO_PANE, |pane| pane.0), Ordering::Relaxed);
        cx.emit(SessionEvent::MetaChanged);
        true
    }

    /// La zone entière a été peinte aux `bounds` : retient ses marges, sa cellule et sa grille. Un changement
    /// de grille est demandé à tmux. Sans changement de la zone, rien n'est notifié.
    fn set_area(&mut self, bounds: Bounds<Pixels>, window: &Window, cx: &mut Context<Self>) {
        let theme = *cx.global::<Theme>();
        let terminal_font = cx.global::<TerminalFont>().clone();
        let cell = cell_metrics(window, &terminal_font);
        let content = content_bounds(bounds, &theme);
        let zone = Zone {
            area: bounds,
            content,
            cell,
            lines: grid_for(content, cell).lines,
        };
        if self.zone == Some(zone) {
            return;
        }
        self.zone = Some(zone);
        self.sync_size(window, cx);
        cx.notify();
    }

    /// Les gouttières entre panes côte à côte que la taille demandée doit réserver : celles du preset visé pendant
    /// une opération, sinon celles du layout complet de la fenêtre visible (jamais d'un layout réduit au pane
    /// zoomé). Avant la première lecture des fenêtres, celles du preset de la session.
    fn gaps(&self) -> u16 {
        requested_gaps(
            self.operation_gaps,
            self.windows.visible_layout(),
            self.preset,
        )
    }

    /// Si la session est affichée et que la grille de la zone diffère de la taille demandée, la retient et la
    /// demande à tmux 50 ms après le dernier changement. La grille laisse la place des gouttières (`gaps`). Chaque
    /// pane suit ensuite la taille de son rectangle du layout (`sync_panes`), pas la grille entière. Une fois la
    /// taille posée, le preset est reposé quand le layout s'est stabilisé (écart E17).
    fn sync_size(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(zone) = self.zone else {
            return;
        };
        let grid = zone.grid(self.gaps());
        // Le test porte sur la taille demandée, pas sur celle d'un émulateur, que tmux peut garder inchangée (pane
        // d'une ligne de moins sous une bordure) : sinon chaque peinture redemanderait la taille.
        if !self.displayed || self.requested == Some(grid) {
            return;
        }
        self.requested = Some(grid);
        self.size_generation += 1;
        let generation = self.size_generation;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SIZE_REQUEST_DELAY).await;
            let _ = this.update_in(cx, |view, window, cx| {
                if view.size_generation == generation
                    && let Some(grid) = view.requested
                {
                    let _ = view.client.send(&client_size_command(grid));
                    view.reapply_preset = true;
                    view.schedule_repair(window, cx);
                }
            });
        })
        .detach();
    }

    /// Une fois par seconde : les panes absents des layouts depuis trop longtemps partent, chaque pane avance, et
    /// toutes les 5 s le pane actif écrit la mesure au journal.
    fn on_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ticks += 1;
        let now = Instant::now();
        // Une première relecture en échec ne laisse rien d'autre pour la relancer : aucune notification n'arrive.
        if !self.windows_loaded && self.query.is_none() {
            self.send_windows_query();
        }
        self.sync_panes(window, cx);
        for slot in &self.slots {
            slot.view.update(cx, |pane, cx| pane.on_tick(now, cx));
        }
        if self.displayed
            && self.ticks.is_multiple_of(LOG_EVERY_TICKS)
            && let Some(view) = self.active_view().cloned()
        {
            view.update(cx, |pane, cx| pane.log_measure(cx));
        }
    }

    /// Route un événement du client vers le ou les panes qu'il concerne, ou vers le suivi des fenêtres. `Exit`
    /// ferme la session. Un événement d'un client remplacé par une reconnexion est ignoré.
    fn on_event(
        &mut self,
        generation: u64,
        event: ControlEvent,
        arrival: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.client_generation {
            return;
        }
        match route(&event) {
            Route::Exit => {
                // La raison reste ici, l'espace de travail la lit par `exit_reason`.
                if let ControlEvent::Exit { reason } = &event {
                    self.exit_reason.clone_from(reason);
                }
                self.client_exited = true;
                cx.emit(SessionEvent::Closed);
            }
            Route::Pane(id) => {
                if let Some(view) = self.pane_view(id) {
                    view.update(cx, |pane, cx| pane.on_event(&event, arrival, cx));
                }
            }
            Route::Reply => {
                // Les lignes de la réponse servent la relecture des fenêtres et, toujours, chaque pane : chacun ne
                // lit que ses propres marqueurs de relecture.
                if let ControlEvent::Reply { ok, lines, .. } = &event
                    && let Some(query) = self.query.as_mut()
                {
                    match query.advance(*ok, lines) {
                        QueryStep::Pending => {}
                        QueryStep::Done(windows) => {
                            self.query = None;
                            // La relecture qui suit une opération montre son layout : le preset visé n'a plus à parler.
                            if !self.op_pending {
                                self.operation_gaps = None;
                            }
                            self.windows.replace(windows);
                            self.windows_loaded = true;
                            self.on_windows_changed(window, cx);
                        }
                        QueryStep::Failed(text) => {
                            // La relecture est finie : la suivante part au prochain besoin (`on_windows_changed`) ou,
                            // tant que rien n'est chargé, au pas suivant de la minuterie.
                            self.query = None;
                            hud::log_event(&format!(
                                "windows query failed session={}: {text}",
                                self.name
                            ));
                        }
                    }
                }
                for slot in &self.slots {
                    slot.view
                        .update(cx, |pane, cx| pane.on_event(&event, arrival, cx));
                }
            }
            Route::Windows => {
                if self.windows.on_notification(&event, Instant::now()) {
                    self.on_windows_changed(window, cx);
                }
                if self.windows.has_held_layout() {
                    self.release_held_layout_later(window, cx);
                }
            }
            Route::Border => {
                if let ControlEvent::SubscriptionChanged { value, .. } = &event
                    && let Some(line) = border_reset(value)
                {
                    // Une écriture qui échoue veut dire que le client est mort : `Exit` le signale.
                    let _ = self.client.send(&line);
                    let (window, was) = value.split_once(' ').unwrap_or((value, ""));
                    hud::log_event(&format!(
                        "border reset session={} window={window} was={was}",
                        self.name
                    ));
                }
            }
            Route::Sessions => cx.emit(SessionEvent::SessionsChanged),
            Route::Ignore => {}
        }
    }

    /// Un layout non zoomé est retenu (`UNZOOM_SETTLE`) : revient l'appliquer à l'échéance si aucun layout de la
    /// fenêtre ne l'a remplacé entre-temps. Un réveil de trop ne fait rien : `release_due` vérifie l'échéance.
    fn release_held_layout_later(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(UNZOOM_SETTLE).await;
            let _ = this.update_in(cx, |view, window, cx| {
                if view.windows.release_due(Instant::now()) {
                    view.on_windows_changed(window, cx);
                }
            });
        })
        .detach();
    }

    /// Les fenêtres ont changé : une relecture si une fenêtre est mal connue, la taille demandée suit les
    /// gouttières du layout, les panes se calent sur les layouts, la réparation s'arme.
    fn on_windows_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.windows.needs_query() && self.query.is_none() {
            self.send_windows_query();
        }
        // Le layout a pu changer de nombre de colonnes : la taille demandée suit ses gouttières.
        self.sync_size(window, cx);
        self.sync_panes(window, cx);
        self.schedule_repair(window, cx);
        cx.notify();
    }

    /// Relit les fenêtres. Une relecture déjà en vol est remplacée : sa réponse pourrait dater d'avant le geste qui
    /// nous amène ici.
    fn send_windows_query(&mut self) {
        self.query = Some(start_query(&mut self.client, &mut self.query_counter));
    }

    /// Cale les panes suivis sur les layouts : un pane ajouté reçoit sa vue (à la taille de son rectangle), un pane
    /// retiré la perd avec ses abonnements ; chaque pane suit la taille de son rectangle, cachés compris ; le pane
    /// actif, le focus et les drapeaux suivent. Rien avant la première réponse de la relecture.
    fn sync_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.windows_loaded {
            return;
        }
        // Le pane zoomé a la taille de la fenêtre, les autres celle de leur rectangle du layout complet.
        let displayed = self.windows.displayed_panes();
        let rects: Vec<PaneRect> = self
            .windows
            .all_panes()
            .into_iter()
            .map(|rect| {
                displayed
                    .iter()
                    .find(|shown| shown.pane == rect.pane)
                    .copied()
                    .unwrap_or(rect)
            })
            .collect();
        let present: Vec<PaneId> = rects.iter().map(|rect| rect.pane).collect();
        let had_focus = self.has_focus(window, cx);
        let change = self.roster.update(&present, Instant::now());

        let mut focus_lost = false;
        for removed in change.removed {
            if let Some(at) = self.slots.iter().position(|slot| slot.id == removed) {
                let slot = self.slots.remove(at);
                focus_lost |= slot.view.read(cx).focus_handle_ref().is_focused(window);
                slot.view.update(cx, |pane, _| pane.release());
            }
        }
        for added in change.added {
            let Some(rect) = rects.iter().find(|rect| rect.pane == added) else {
                continue;
            };
            let launch = self
                .launches
                .iter()
                .position(|launch| launch.pane == added)
                .map(|at| self.launches.remove(at))
                .map(|launch| LaunchRequest {
                    command: launch.command,
                    delay: launch.delay,
                    pacer: launch.pacer,
                });
            let writer = self.client.writer();
            let name = self.name.clone();
            let project = self.project.clone();
            let size = grid_of(rect);
            let session = SessionRef {
                name: &name,
                project: &project,
            };
            let view = cx.new(|cx| PaneView::new(writer, added, size, launch, session, window, cx));
            let subscription = cx.subscribe_in(&view, window, Self::on_pane_event);
            self.slots.push(PaneSlot {
                id: added,
                view,
                _subscription: subscription,
            });
        }
        for rect in &rects {
            if let Some(view) = self.pane_view(rect.pane) {
                let size = grid_of(rect);
                view.update(cx, |pane, cx| pane.fit(size, cx));
            }
        }

        let visible = self.visible_ids();
        let tmux_active = self.windows.active_pane();
        if let Some((pane, since)) = self.pending_focus
            && !pending_focus_alive(pane, since.elapsed(), tmux_active, &visible)
        {
            self.pending_focus = None;
        }
        let wanted = resolve_active(
            self.active,
            tmux_active,
            self.pending_focus.map(|(pane, _)| pane),
            self.op_pending,
            &visible,
        );
        let changed = self.set_active(wanted, cx);
        self.apply_flags(window, cx);
        // Le focus suit le pane actif quand il était dans la session (ou sur le pane qui vient de disparaître) ;
        // ailleurs (un menu, un champ de la barre) il n'est pas volé.
        if self.displayed && (focus_lost || (changed && had_focus)) {
            self.focus_active(window, cx);
        }
        self.refresh_state(cx);
        cx.notify();
    }

    /// L'état de la session est le plus urgent de tous ses panes, cachés compris (spec §5.3).
    fn refresh_state(&mut self, cx: &mut Context<Self>) {
        let current = most_urgent(self.slots.iter().map(|slot| slot.view.read(cx).state()));
        if current != self.last_state {
            let previous = std::mem::replace(&mut self.last_state, current);
            cx.emit(SessionEvent::StateChanged { previous, current });
        }
    }

    /// Ce qu'un pane signale : la méta remonte telle quelle, l'état se réagrège, un appui sur un pane inactif lui
    /// donne le focus.
    fn on_pane_event(
        &mut self,
        pane: &Entity<PaneView>,
        event: &PaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PaneEvent::MetaChanged => cx.emit(SessionEvent::MetaChanged),
            PaneEvent::StateChanged { previous, current } => {
                self.refresh_state(cx);
                cx.emit(SessionEvent::PanesChanged);
                if let Some(id) = pane.read(cx).pane()
                    && hidden_pane_chimes(
                        self.seen,
                        self.zoom_hidden_ids().contains(&id),
                        *previous,
                        *current,
                    )
                {
                    cx.emit(SessionEvent::HiddenPaneWaiting { pane: id });
                }
            }
            PaneEvent::FocusRequested => {
                if let Some(id) = pane.read(cx).pane() {
                    self.focus_pane(id, window, cx);
                }
            }
            PaneEvent::FocusChanged => cx.emit(SessionEvent::FocusChanged),
            PaneEvent::OpenPath(target) => cx.emit(SessionEvent::OpenPath(target.clone())),
            PaneEvent::OpenUrl(url) => cx.emit(SessionEvent::OpenUrl(url.clone())),
            // Seul le shell local du volet Terminal l'émet.
            PaneEvent::Closed => {}
        }
    }

    /// Donne le focus à `pane`, s'il est visible : pane actif local, focus GPUI, puis `select-pane` hors du fil
    /// d'interface (`pane_active` de tmux suit, donc la méta et la relance du pane actif).
    pub fn focus_pane(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible_ids().contains(&pane) {
            return;
        }
        if self.active != Some(pane) {
            self.pending_focus = Some((pane, Instant::now()));
            self.set_active(Some(pane), cx);
            self.apply_flags(window, cx);
            let server = self.server.clone();
            let selected = cx.background_spawn(async move { server.select_pane(pane) });
            let name = self.name.clone();
            cx.spawn_in(window, async move |this, cx| {
                if let Err(error) = selected.await {
                    hud::log_event(&format!(
                        "select-pane failed session={name} pane=%{}: {error}",
                        pane.0
                    ));
                    let _ = this.update_in(cx, |view, window, cx| {
                        if view.pending_focus.map(|(pending, _)| pending) == Some(pane) {
                            view.pending_focus = None;
                        }
                        view.sync_panes(window, cx);
                    });
                }
            })
            .detach();
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    fn focus_direction(
        &mut self,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.active else {
            return;
        };
        if let Some(target) = neighbor(&self.windows.visible_panes(), active, direction) {
            self.focus_pane(target, window, cx);
        }
    }

    /// ⌘⇧↩ : zoome le pane actif, ou rétablit la fenêtre zoomée, par le zoom de tmux (`resize-pane -Z`) hors du fil
    /// d'interface. Idema suit le `%layout-change` qui en résulte.
    pub fn toggle_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.active else {
            return;
        };
        let server = self.server.clone();
        let toggled = cx.background_spawn(async move { server.toggle_zoom(pane) });
        let name = self.name.clone();
        cx.spawn_in(window, async move |_, _| {
            if let Err(error) = toggled.await {
                hud::log_event(&format!(
                    "resize-pane -Z failed session={name} pane=%{}: {error}",
                    pane.0
                ));
            }
        })
        .detach();
    }

    fn zoom_action(&mut self, _: &ToggleZoom, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_zoom(window, cx);
    }

    fn focus_left(&mut self, _: &FocusPaneLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_direction(Direction::Left, window, cx);
    }

    fn focus_right(&mut self, _: &FocusPaneRight, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_direction(Direction::Right, window, cx);
    }

    fn focus_up(&mut self, _: &FocusPaneUp, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_direction(Direction::Up, window, cx);
    }

    fn focus_down(&mut self, _: &FocusPaneDown, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_direction(Direction::Down, window, cx);
    }

    /// Passe la session au preset (spec §4.4). Refusé si une opération de layout est en vol.
    pub fn apply_preset(&mut self, preset: Preset, window: &mut Window, cx: &mut Context<Self>) {
        self.run_op(Op::Preset(preset), window, cx);
    }

    /// Lance une opération de layout hors du fil d'interface. `server.apply_preset` ne cache jamais le pane actif.
    fn run_op(&mut self, op: Op, window: &mut Window, cx: &mut Context<Self>) {
        if self.op_pending {
            return;
        }
        self.op_pending = true;
        if let Op::Preset(preset) = op {
            // La fenêtre se rétrécit avant que le layout ne change : tmux partage ensuite la largeur sans les gouttières.
            self.operation_gaps = Some(preset.column_gaps());
            self.sync_size(window, cx);
        }
        let server = self.server.clone();
        let name = self.name.clone();
        let project = self.project.to_string_lossy().into_owned();
        let focused = self.active;
        let done = cx.background_spawn(async move {
            match op {
                Op::Preset(preset) => server
                    .apply_preset(&name, &project, preset, focused, SHELL_SESSION_NAME)
                    .map(|created| created.len()),
                Op::Restore => server.restore_visible(&name).map(usize::from),
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = done.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.on_op_done(op, result, window, cx);
            });
        })
        .detach();
    }

    /// Une opération de layout est finie : journal, relecture des fenêtres (retirer `@idema_stash` au rétablissement
    /// d'un pane caché seul n'émet aucune notification), réparation réarmée.
    fn on_op_done(
        &mut self,
        op: Op,
        result: Result<usize, ServerError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.op_pending = false;
        match (op, result) {
            (Op::Preset(preset), Ok(created)) => {
                self.preset = preset;
                hud::log_event(&format!(
                    "layout session={} preset={} created={created}",
                    self.name,
                    preset.id()
                ));
            }
            (Op::Preset(preset), Err(error)) => hud::log_event(&format!(
                "layout failed session={} preset={}: {error}",
                self.name,
                preset.id()
            )),
            (Op::Restore, Ok(restored)) => {
                // `restore_visible` écrit `@idema_layout single` quand il rejoint un pane : la vue le suit.
                if restored > 0 {
                    self.preset = Preset::Single;
                }
                hud::log_event(&format!(
                    "restore session={} restored={restored}",
                    self.name
                ));
            }
            (Op::Restore, Err(error)) => {
                hud::log_event(&format!("restore failed session={}: {error}", self.name));
            }
        }
        self.send_windows_query();
        self.schedule_repair(window, cx);
        self.sync_panes(window, cx);
        cx.notify();
    }

    /// Arme la réparation `LAYOUT_SETTLE` après le dernier changement : un nouveau changement repousse l'échéance.
    fn schedule_repair(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.repair_generation += 1;
        let generation = self.repair_generation;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(LAYOUT_SETTLE).await;
            let _ = this.update_in(cx, |view, window, cx| {
                if view.repair_generation == generation {
                    view.repair(window, cx);
                }
            });
        })
        .detach();
    }

    /// Répare le layout (spec §4.4, écarts E17 et E28) : après la fermeture d'un pane, une opération en erreur ou
    /// des panes ajoutés hors d'Idema. Jamais pendant une opération, ni tant que les fenêtres sont mal connues, ni
    /// pendant un zoom (écart E30) : `reapply_preset` est gardé pour le dézoom.
    fn repair(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.op_pending
            || !self.windows_loaded
            || self.query.is_some()
            || self.windows.needs_query()
        {
            return;
        }
        let step = repair_step(
            &self.windows,
            self.preset,
            self.reapply_preset,
            self.attempted_repair,
        );
        // Différé par le zoom : `reapply_preset` est gardé pour le dézoom.
        if step != RepairStep::Deferred {
            self.reapply_preset = false;
        }
        match step {
            RepairStep::InOrder => self.attempted_repair = None,
            RepairStep::Stuck | RepairStep::Deferred => {}
            RepairStep::Run(op, key) => {
                self.attempted_repair = Some(key);
                self.run_op(op, window, cx);
            }
        }
    }

    /// Un nouveau client de contrôle pour la session vivante (reconnexion en place) : les panes gardent leur
    /// émulateur, leur historique et leurs états ; chacun reprend son flux (`rebind`), ses abonnements et une
    /// relecture, les fenêtres sont relues et la taille est redemandée si la session est affichée.
    pub fn reconnect(
        &mut self,
        server: &TmuxServer,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), ServerError> {
        let generation = self.client_generation + 1;
        let mut client = connect(
            server,
            &self.name,
            generation,
            self.sender.clone(),
            self.echo_pane.clone(),
        )?;
        if self.displayed
            && let Some(grid) = self.requested
        {
            let _ = client.send(&client_size_command(grid));
        }
        let writer = client.writer();
        for slot in &self.slots {
            slot.view.update(cx, |pane, _| pane.rebind(writer.clone()));
        }
        self.query = Some(start_query(&mut client, &mut self.query_counter));
        let _ = client.send(&border_subscription());
        // L'ancien client est détaché ici ; ses événements en retard portent l'ancienne génération.
        self.client = client;
        self.client_generation = generation;
        self.connected_at = Instant::now();
        self.exit_reason = None;
        self.client_exited = false;
        cx.notify();
        Ok(())
    }

    /// La raison du dernier arrêt du client de contrôle, quand tmux ou le flux en a donné une.
    pub fn exit_reason(&self) -> Option<&str> {
        self.exit_reason.as_deref()
    }

    /// Depuis combien de temps le client courant est attaché.
    pub fn connected_for(&self) -> Duration {
        self.connected_at.elapsed()
    }

    /// Un clic hors de tout pane (marges, séparateurs) donne quand même le focus au pane actif.
    fn on_mouse_down(&mut self, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_active(window, cx);
        // La poignée de la racine ne reprend pas le focus derrière nous.
        window.prevent_default();
    }
}

impl EventEmitter<SessionEvent> for SessionView {}

impl Focusable for SessionView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_view().map_or_else(
            || self.focus_handle.clone(),
            |view| view.read(cx).focus_handle_ref().clone(),
        )
    }
}

impl Render for SessionView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let session = cx.entity();
        // Les panes se posent dans la zone utile, à la taille de leur rectangle de cellules. Avant la première
        // mesure, la zone tient dans un point ; avant la première réponse de la relecture, rien n'est dessiné.
        let (origin, extent, cell) = self.zone.map_or(
            ((px(0.), px(0.)), (px(0.), px(0.)), (px(0.), px(0.))),
            |Zone {
                 area,
                 content,
                 cell,
                 lines,
             }| {
                let origin = content.origin - area.origin;
                (
                    (origin.x, origin.y),
                    (content.size.width, cell.1 * lines as f32),
                    cell,
                )
            },
        );
        let cell = (f32::from(cell.0), f32::from(cell.1));
        let mut area = div()
            .absolute()
            .left(origin.0)
            .top(origin.1)
            .w(extent.0)
            .h(extent.1)
            .overflow_hidden();
        if self.windows_loaded
            && cell.0 > 0.
            && let Some(layout) = self.windows.visible_layout()
        {
            // Zoomé, un seul pane occupe toute la zone, sans trait ni atténuation ; les gouttières restent celles du
            // layout complet, la fenêtre gardant sa taille.
            let zoomed = self.windows.zoomed_pane().is_some();
            let panes: Vec<(PaneRect, u16)> = if zoomed {
                self.windows
                    .displayed_panes()
                    .into_iter()
                    .map(|rect| (rect, 0))
                    .collect()
            } else {
                layout.gapped_panes()
            };
            let dimmed = panes.len() > 1;
            for (rect, gaps_before) in &panes {
                let Some(slot) = self.slots.iter().find(|slot| slot.id == rect.pane) else {
                    continue;
                };
                let inactive = dimmed && self.active != Some(rect.pane);
                area = area.child(
                    place(pane_frame(rect, *gaps_before, cell))
                        .overflow_hidden()
                        .when(inactive, |frame| frame.opacity(INACTIVE_PANE))
                        .child(slot.view.clone()),
                );
            }
            let separators = if zoomed {
                Vec::new()
            } else {
                layout.gapped_separators()
            };
            for separator in separators {
                area =
                    area.child(place(separator_frame(&separator, cell)).bg(white(PANE_SEPARATOR)));
            }
        }
        div()
            .relative()
            .size_full()
            .bg(color(WINDOW))
            .track_focus(&self.focus_handle)
            // Ces liaisons passent avant `on_key_down` de la pane : le programme ne reçoit jamais `⌘⌥` + flèche,
            // même dans une session à un seul pane (§6.3).
            .on_action(cx.listener(Self::focus_left))
            .on_action(cx.listener(Self::focus_right))
            .on_action(cx.listener(Self::focus_up))
            .on_action(cx.listener(Self::focus_down))
            .on_action(cx.listener(Self::zoom_action))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .child(
                // Mesure la zone entière : ses bornes sont rendues à la session avant la peinture des panes.
                canvas(
                    move |bounds, window, cx| {
                        session.update(cx, |view, cx| view.set_area(bounds, window, cx));
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(area)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idema_tmux::layout::parse_layout;
    use idema_tmux::protocol::{SessionId, WindowId};
    use idema_tmux::windows::WindowState;

    fn pane_json(pane: u32, x: u16, y: u16, w: u16, h: u16, active: bool) -> String {
        let flag = if active { r#""a":true,"# } else { "" };
        format!(r#"{{"t":"p","w":{w},"h":{h},"x":{x},"y":{y},{flag}"i":{pane},"I":"%{pane}"}}"#)
    }

    fn layout_of(root: &str) -> WindowLayout {
        parse_layout(&format!(r#"{{"V":2,"L":{root}}}"#)).expect("a valid layout")
    }

    fn single_layout(pane: u32) -> WindowLayout {
        layout_of(&pane_json(pane, 0, 0, 120, 40, true))
    }

    fn columns_layout(left: u32, right: u32, left_width: u16) -> WindowLayout {
        let right_width = 119 - left_width;
        layout_of(&format!(
            r#"{{"t":"h","w":120,"h":40,"x":0,"y":0,"c":[{},{}]}}"#,
            pane_json(left, 0, 0, left_width, 40, true),
            pane_json(right, left_width + 1, 0, right_width, 40, false)
        ))
    }

    fn visible_window(layout: WindowLayout) -> SessionWindows {
        let mut windows = SessionWindows::new();
        windows.replace(vec![WindowState {
            id: WindowId(0),
            layout: Some(layout),
            zoomed: None,
            current: true,
            stash: Some(false),
        }]);
        windows
    }

    /// Une fenêtre visible zoomée sur `zoomed` : le layout reste le complet.
    fn zoomed_window(layout: WindowLayout, zoomed: u32) -> SessionWindows {
        let mut windows = SessionWindows::new();
        windows.replace(vec![WindowState {
            id: WindowId(0),
            layout: Some(layout),
            zoomed: Some(PaneId(zoomed)),
            current: true,
            stash: Some(false),
        }]);
        windows
    }

    fn rect(pane: u32, x: u16, y: u16, width: u16, height: u16) -> PaneRect {
        PaneRect {
            pane: PaneId(pane),
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_session_with_two_panes_and_a_single_preset_is_repaired_to_the_first_two_pane_preset() {
        let windows = visible_window(columns_layout(1, 2, 60));
        let step = repair_step(&windows, Preset::Single, false, None);
        assert!(matches!(
            step,
            RepairStep::Run(Op::Preset(Preset::Columns2), _)
        ));
    }

    #[test]
    fn a_session_whose_panes_match_its_preset_needs_no_repair() {
        let windows = visible_window(columns_layout(1, 2, 60));
        assert_eq!(
            repair_step(&windows, Preset::Columns2, false, None),
            RepairStep::InOrder
        );
    }

    #[test]
    fn the_same_repair_is_not_tried_twice_in_a_row() {
        let windows = visible_window(columns_layout(1, 2, 60));
        let RepairStep::Run(_, key) = repair_step(&windows, Preset::Single, false, None) else {
            panic!("the first call must repair");
        };
        assert_eq!(
            repair_step(&windows, Preset::Single, false, Some(key)),
            RepairStep::Stuck
        );
    }

    #[test]
    fn a_failed_repair_is_tried_again_once_the_window_has_another_size() {
        let windows = visible_window(columns_layout(1, 2, 60));
        let RepairStep::Run(_, key) = repair_step(&windows, Preset::Single, false, None) else {
            panic!("the first call must repair");
        };
        let resized = visible_window(layout_of(&format!(
            r#"{{"t":"h","w":100,"h":40,"x":0,"y":0,"c":[{},{}]}}"#,
            pane_json(1, 0, 0, 50, 40, true),
            pane_json(2, 51, 0, 49, 40, false)
        )));
        assert!(matches!(
            repair_step(&resized, Preset::Single, false, Some(key)),
            RepairStep::Run(..)
        ));
    }

    #[test]
    fn a_visible_window_that_died_is_restored() {
        let mut windows = SessionWindows::new();
        windows.replace(vec![WindowState {
            id: WindowId(1),
            layout: Some(single_layout(3)),
            zoomed: None,
            current: true,
            stash: Some(true),
        }]);
        assert!(matches!(
            repair_step(&windows, Preset::Columns2, false, None),
            RepairStep::Run(Op::Restore, _)
        ));
    }

    #[test]
    fn a_resized_window_gets_its_preset_back_only_when_tmux_shared_the_cells_unevenly() {
        // tmux a redistribué 119 colonnes en 61 / 58 au lieu de 60 / 59.
        let uneven = visible_window(columns_layout(1, 2, 61));
        let even = visible_window(columns_layout(1, 2, 60));
        assert!(matches!(
            repair_step(&uneven, Preset::Columns2, true, None),
            RepairStep::Run(Op::Preset(Preset::Columns2), _)
        ));
        assert_eq!(
            repair_step(&even, Preset::Columns2, true, None),
            RepairStep::InOrder
        );
        // Sans taille nouvelle, un layout inégal n'est pas touché : il a pu être fait à la main.
        assert_eq!(
            repair_step(&uneven, Preset::Columns2, false, None),
            RepairStep::InOrder
        );
    }

    #[test]
    fn a_layout_matches_the_geometry_of_its_own_preset_and_of_no_other() {
        let even = columns_layout(1, 2, 60);
        assert!(matches_preset(&even, Preset::Columns2));
        assert!(!matches_preset(&even, Preset::Rows2));
        // Une colonne de plus à gauche : ce n'est plus la répartition du preset.
        assert!(!matches_preset(&columns_layout(1, 2, 61), Preset::Columns2));
    }

    /// Un layout de sept panes en colonnes égales, au-delà de ce que le menu sait poser.
    fn seven_panes_layout() -> WindowLayout {
        let panes: Vec<String> = (0..7u32)
            .map(|index| pane_json(10 + index, (index * 17) as u16, 0, 16, 40, index == 0))
            .collect();
        layout_of(&format!(
            r#"{{"t":"h","w":120,"h":40,"x":0,"y":0,"c":[{}]}}"#,
            panes.join(",")
        ))
    }

    #[test]
    fn a_session_of_more_than_six_panes_is_left_as_tmux_shows_it_even_after_a_resize() {
        let windows = visible_window(seven_panes_layout());
        for reapply in [false, true] {
            assert_eq!(
                repair_step(&windows, Preset::Single, reapply, None),
                RepairStep::InOrder,
                "reapply={reapply}"
            );
        }
    }

    #[test]
    fn a_resize_reposes_the_preset_only_when_the_pane_count_is_the_presets() {
        // Deux panes et un preset à un pane : c'est la réparation qui range, pas la repose.
        let two = visible_window(columns_layout(1, 2, 60));
        assert!(matches!(
            repair_step(&two, Preset::Single, true, None),
            RepairStep::Run(Op::Preset(Preset::Columns2), _)
        ));
        // Trois panes alignés, preset à trois panes d'une autre forme : il est reposé.
        let three = visible_window(layout_of(&format!(
            r#"{{"t":"h","w":120,"h":40,"x":0,"y":0,"c":[{},{},{}]}}"#,
            pane_json(1, 0, 0, 40, 40, true),
            pane_json(2, 41, 0, 39, 40, false),
            pane_json(3, 81, 0, 39, 40, false)
        )));
        assert!(matches!(
            repair_step(&three, Preset::MainLeft, true, None),
            RepairStep::Run(Op::Preset(Preset::MainLeft), _)
        ));
    }

    /// Une fenêtre cachée seule et courante, avec `hidden` panes : la visible a disparu.
    fn only_hidden_windows(hidden: u32) -> SessionWindows {
        let layout = if hidden == 1 {
            single_layout(20)
        } else {
            columns_layout(20, 21, 60)
        };
        let mut windows = SessionWindows::new();
        windows.replace(vec![WindowState {
            id: WindowId(1),
            layout: Some(layout),
            zoomed: None,
            current: true,
            stash: Some(true),
        }]);
        windows
    }

    #[test]
    fn a_second_restore_runs_once_the_first_brought_a_pane_back_and_a_repeat_stays_blocked() {
        let RepairStep::Run(Op::Restore, first) =
            repair_step(&only_hidden_windows(2), Preset::Columns2, false, None)
        else {
            panic!("the first restore must run");
        };
        // Le pane rejoint est mort avant tout passage « en ordre » : il reste un pane caché, la restauration repart.
        assert!(matches!(
            repair_step(&only_hidden_windows(1), Preset::Single, false, Some(first)),
            RepairStep::Run(Op::Restore, _)
        ));
        // Une restauration sans effet (même nombre de panes cachés) ne boucle pas.
        assert_eq!(
            repair_step(
                &only_hidden_windows(2),
                Preset::Columns2,
                false,
                Some(first)
            ),
            RepairStep::Stuck
        );
    }

    #[test]
    fn events_go_to_the_pane_they_name_or_to_the_window_tracking() {
        assert_eq!(
            route(&ControlEvent::Output {
                pane: PaneId(4),
                data: Vec::new()
            }),
            Route::Pane(PaneId(4))
        );
        for name in [
            "idema-cmd-5",
            "idema-path-5",
            "idema-agent-5",
            "idema-size-5",
        ] {
            assert_eq!(
                route(&ControlEvent::SubscriptionChanged {
                    name: name.into(),
                    value: String::new()
                }),
                Route::Pane(PaneId(5)),
                "{name}"
            );
        }
        assert_eq!(
            route(&ControlEvent::SubscriptionChanged {
                name: BORDER_WATCH.into(),
                value: "@3 top".into()
            }),
            Route::Border
        );
        assert_eq!(
            route(&ControlEvent::SubscriptionChanged {
                name: "other".into(),
                value: String::new()
            }),
            Route::Ignore
        );
        for event in [
            ControlEvent::LayoutChange {
                window: WindowId(1),
                rest: String::new(),
            },
            ControlEvent::WindowAdd {
                window: WindowId(1),
            },
            ControlEvent::WindowClose {
                window: WindowId(1),
            },
            ControlEvent::SessionChanged {
                session: SessionId(1),
            },
            ControlEvent::SessionWindowChanged {
                session: SessionId(1),
                window: WindowId(1),
            },
            ControlEvent::WindowPaneChanged {
                window: WindowId(1),
                pane: PaneId(2),
            },
        ] {
            assert_eq!(route(&event), Route::Windows, "{event:?}");
        }
        assert_eq!(
            route(&ControlEvent::Reply {
                number: 1,
                ok: true,
                lines: Vec::new()
            }),
            Route::Reply
        );
        assert_eq!(route(&ControlEvent::Exit { reason: None }), Route::Exit);
        assert_eq!(route(&ControlEvent::SessionsChanged), Route::Sessions);
    }

    #[test]
    fn tmux_decides_the_active_pane_unless_idema_moved_the_focus_or_is_rearranging() {
        let visible = [PaneId(1), PaneId(2)];
        // Cas ordinaire : le pane actif de tmux.
        assert_eq!(
            resolve_active(Some(PaneId(1)), Some(PaneId(2)), None, false, &visible),
            Some(PaneId(2))
        );
        // Un focus donné par Idema prime tant que tmux ne l'a pas rapporté.
        assert_eq!(
            resolve_active(
                Some(PaneId(1)),
                Some(PaneId(1)),
                Some(PaneId(2)),
                false,
                &visible
            ),
            Some(PaneId(2))
        );
        // Pendant une opération de layout, le focus d'Idema ne suit pas tmux.
        assert_eq!(
            resolve_active(Some(PaneId(1)), Some(PaneId(2)), None, true, &visible),
            Some(PaneId(1))
        );
        // Un pane devenu invisible ne reste pas actif.
        assert_eq!(
            resolve_active(Some(PaneId(9)), Some(PaneId(9)), None, true, &visible),
            Some(PaneId(1))
        );
        assert_eq!(resolve_active(None, None, None, false, &[]), None);
    }

    #[test]
    fn a_requested_focus_ends_when_tmux_agrees_when_the_pane_hides_or_after_the_delay() {
        let visible = [PaneId(1), PaneId(2)];
        let soon = Duration::from_millis(100);
        assert!(pending_focus_alive(
            PaneId(2),
            soon,
            Some(PaneId(1)),
            &visible
        ));
        assert!(!pending_focus_alive(
            PaneId(2),
            soon,
            Some(PaneId(2)),
            &visible
        ));
        assert!(!pending_focus_alive(
            PaneId(3),
            soon,
            Some(PaneId(1)),
            &visible
        ));
        assert!(!pending_focus_alive(
            PaneId(2),
            FOCUS_PENDING_MAX,
            Some(PaneId(1)),
            &visible
        ));
    }

    #[test]
    fn a_hidden_pane_is_never_seen_and_only_the_active_pane_is_at_the_foreground() {
        let visible = [PaneId(1), PaneId(2)];
        let active = Some(PaneId(2));
        assert_eq!(
            pane_flags(PaneId(2), &visible, active, true, true),
            PaneFlags {
                seen: true,
                focused: true,
                active: true
            }
        );
        assert_eq!(
            pane_flags(PaneId(1), &visible, active, true, true),
            PaneFlags {
                seen: true,
                focused: false,
                active: false
            }
        );
        // Caché : jamais vu, même quand la session l'est.
        assert!(!pane_flags(PaneId(7), &visible, active, true, true).seen);
        // Session non vue ou fenêtre inactive : plus rien n'est vu ni au premier plan.
        let background = pane_flags(PaneId(2), &visible, active, false, false);
        assert!(!background.seen && !background.focused && background.active);
    }

    #[test]
    fn a_pane_rectangle_becomes_a_grid_the_emulator_accepts() {
        assert_eq!(
            grid_of(&rect(1, 0, 0, 60, 40)),
            GridSize {
                columns: 60,
                lines: 40
            }
        );
        assert_eq!(
            grid_of(&rect(1, 0, 0, 1, 0)),
            GridSize {
                columns: 2,
                lines: 1
            }
        );
    }

    #[test]
    fn the_requested_gaps_follow_the_operation_then_the_layout_then_the_stored_preset() {
        let two_columns = columns_layout(1, 2, 60);
        let single = single_layout(1);
        // Une opération vers un preset à trois colonnes prime sur le layout encore à deux colonnes.
        assert_eq!(
            requested_gaps(Some(2), Some(&two_columns), Preset::Single),
            2
        );
        assert_eq!(
            requested_gaps(None, Some(&two_columns), Preset::Columns3),
            two_columns.column_gaps()
        );
        // Avant la première lecture des fenêtres, la taille d'attache (celle du preset) n'est pas contredite.
        assert_eq!(
            requested_gaps(None, None, Preset::Columns3),
            Preset::Columns3.column_gaps()
        );
        assert_eq!(requested_gaps(None, Some(&single), Preset::Columns3), 0);
    }

    #[test]
    fn a_pane_hidden_by_the_zoom_is_never_seen() {
        // Zoomé sur %1 : seul il est affiché, %2 reste dans le layout complet mais n'est pas vu.
        let displayed = [PaneId(1)];
        let active = Some(PaneId(1));
        for session_seen in [true, false] {
            assert!(!pane_flags(PaneId(2), &displayed, active, session_seen, true).seen);
        }
        assert!(pane_flags(PaneId(1), &displayed, active, true, true).seen);
    }

    /// Un `%layout-change` de la fenêtre @0, tel que tmux 3.8 l'envoie pour un layout à deux colonnes zoomé ou non.
    fn resize_event(zoomed: bool) -> ControlEvent {
        let full = format!(
            r#"{{"V":2,"L":{{"t":"h","w":120,"h":40,"x":0,"y":0,"c":[{},{}]}}}}"#,
            pane_json(1, 0, 0, 60, 40, true),
            pane_json(2, 61, 0, 59, 40, false)
        );
        let (visible, flags) = if zoomed {
            (
                format!(r#"{{"V":2,"L":{}}}"#, pane_json(1, 0, 0, 120, 40, true)),
                "*Z",
            )
        } else {
            (full.clone(), "*")
        };
        ControlEvent::LayoutChange {
            window: WindowId(0),
            rest: format!("{full} {visible} {flags}"),
        }
    }

    /// Les drapeaux « vu » des panes %1 (zoomé) et %2 (caché par le zoom) dans une session vue, comme `apply_flags`.
    fn seen_flags(windows: &SessionWindows) -> (bool, bool) {
        let displayed: Vec<PaneId> = windows
            .displayed_panes()
            .iter()
            .map(|rect| rect.pane)
            .collect();
        let seen = |pane| pane_flags(PaneId(pane), &displayed, Some(PaneId(1)), true, true).seen;
        (seen(1), seen(2))
    }

    #[test]
    fn a_resize_burst_while_zoomed_never_shows_the_hidden_pane() {
        // `refresh-client -C` sur une fenêtre zoomée : un layout non zoomé, puis deux zoomés (sonde tmux 3.8). Chaque
        // événement est suivi d'une synchronisation des panes : le pane %2, caché par le zoom, ne doit à aucun
        // moment compter comme vu (J4-13), et « attend » ou « erreur » de son agent ne tombe donc pas.
        let mut windows = zoomed_window(columns_layout(1, 2, 60), 1);
        let start = Instant::now();
        for (at, zoomed) in [false, true, true].into_iter().enumerate() {
            let now = start + Duration::from_millis(at as u64);
            windows.on_notification(&resize_event(zoomed), now);
            assert_eq!(
                seen_flags(&windows),
                (true, false),
                "after event {at} of the burst"
            );
        }
        assert_eq!(windows.zoomed_pane(), Some(PaneId(1)));
    }

    #[test]
    fn a_resize_while_zoomed_defers_the_preset_until_unzoom() {
        let uneven = columns_layout(1, 2, 61);
        let zoomed = zoomed_window(uneven.clone(), 1);
        assert_eq!(
            repair_step(&zoomed, Preset::Columns2, true, None),
            RepairStep::Deferred
        );
        assert!(matches!(
            repair_step(&visible_window(uneven), Preset::Columns2, true, None),
            RepairStep::Run(Op::Preset(Preset::Columns2), _)
        ));
    }

    #[test]
    fn only_a_zoom_hidden_pane_entering_waiting_in_a_seen_session_chimes() {
        use ActivityState::{Error, Idle, Running, Waiting};
        assert!(hidden_pane_chimes(true, true, Running, Waiting));
        assert!(hidden_pane_chimes(true, true, Idle, Waiting));
        // Session non vue : le son de l'état agrégé s'en charge.
        assert!(!hidden_pane_chimes(false, true, Running, Waiting));
        // Pane affiché : la session vue le voit, aucun son.
        assert!(!hidden_pane_chimes(true, false, Running, Waiting));
        // Pas un passage en « attend ».
        assert!(!hidden_pane_chimes(true, true, Waiting, Waiting));
        assert!(!hidden_pane_chimes(true, true, Waiting, Idle));
        assert!(!hidden_pane_chimes(true, true, Running, Error));
    }
}
