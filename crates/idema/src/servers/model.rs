//! L'entité `Servers` (spec P4 §5) : elle tient le `Registry`, vide le canal du `Scanner` dans une tâche GPUI et offre
//! les gestes démarrer, arrêter et relancer. Rien d'ici ne bloque le fil principal : `lsof`, `ps`, tmux et les signaux
//! tournent sur des fils d'arrière-plan (`run_start`, `run_stop`).
//!
//! La logique vit dans `ServersCore`, sans fenêtre ni GPUI, pour se tester avec un faux lanceur : `begin_*` décide sur
//! le fil principal et rend un travail, `run_*` l'exécute (bloquant), `finish_*` en range le résultat.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use futures::StreamExt;
use futures::channel::mpsc::unbounded;
use gpui::{Context, EventEmitter, Task};
use idema_tmux::protocol::PaneId;
use idema_tmux::server::{
    ListedPane, ListedServerSession, NewServerSession, ServerError, TmuxServer,
};
use url::Url;

use super::launch::{ConfigEntry, ProjectConfig, read_project_config};
use super::registry::ProjectGroup;
use super::registry::{
    Origin, PaneOwner, PaneRoot, PaneRoots, Registry, RowKind, RowState, ServerKey, ServerRow,
};
use super::scan::{OutputTarget, ProbeScope, Process, SCAN_PERIOD, ScanResult, Scanner, Snapshot};
pub use super::session_agents::session_agents;
use super::stop::{
    STOP_GRACE, StopOutcome, StopPlan, StopTarget, current_processes, execute_session_stop,
    execute_stop, listener_of, plan_stop, stop_target,
};
use crate::browser::address::same_address;
use crate::config::{AgentEntry, SHELL_ENTRY, SHELL_SESSION_NAME};
use crate::hud;

/// Après un arrêt, le port doit se libérer en moins de ce délai pour qu'une relance démarre.
const FREE_WAIT: Duration = Duration::from_secs(5);
/// Le port est sondé à ce rythme pendant l'attente.
const FREE_POLL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------------------------------------------------
// Le lanceur

/// Ce qu'il faut pour lancer un serveur dans une session cachée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    pub project: PathBuf,
    pub entry: String,
    pub cwd: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    /// `PORT=<port>` quand le port est connu, puis l'`env` de l'entrée.
    pub env: Vec<(String, String)>,
}

/// Ce que `Servers` demande à tmux et au système : un faux en test, `TmuxServer` en vrai.
pub trait ServerLauncher: Send + Sync + 'static {
    /// Crée la session serveur et rend son nom.
    fn create(&self, request: &LaunchRequest) -> Result<String, String>;
    fn kill(&self, session: &str) -> Result<(), String>;
    /// Les sessions serveur, mortes comprises.
    fn sessions(&self) -> Result<Vec<ListedServerSession>, String>;
    /// Le PID qui écoute sur ce port (`lsof`), `Ok(None)` si personne.
    fn listener_of(&self, port: u16) -> Result<Option<i32>, String>;
}

impl ServerLauncher for TmuxServer {
    fn create(&self, request: &LaunchRequest) -> Result<String, String> {
        self.create_server_session(&NewServerSession {
            project: &request.project,
            entry: &request.entry,
            cwd: &request.cwd,
            program: &request.program,
            args: &request.args,
            env: &request.env,
        })
        .map(|created| created.name)
        .map_err(|error: ServerError| error.to_string())
    }

    fn kill(&self, session: &str) -> Result<(), String> {
        self.kill_session(session)
            .map_err(|error: ServerError| error.to_string())
    }

    fn sessions(&self) -> Result<Vec<ListedServerSession>, String> {
        self.server_sessions()
            .map_err(|error: ServerError| error.to_string())
    }

    fn listener_of(&self, port: u16) -> Result<Option<i32>, String> {
        listener_of(port)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Les racines des panes

/// Une session de la barre, telle que l'espace de travail la connaît.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub project: PathBuf,
    pub display_name: String,
}

/// Ce que l'espace de travail sait et que tmux ne dit pas : de quoi bâtir les racines des panes.
#[derive(Debug, Clone, Default)]
pub struct PaneInputs {
    pub sessions: Vec<SessionInfo>,
    /// Les lanceurs de la configuration en mémoire : `@idema_agent` désigne l'un d'eux, dont la commande sert à
    /// reconnaître l'agent. Le fichier de configuration n'est jamais relu ici.
    pub agents: Vec<AgentEntry>,
    /// Le shell local du volet Terminal, par projet.
    pub terminal_shells: Vec<(PathBuf, i32)>,
}

/// Les racines des panes à un instant, avec les sessions serveur listées au même moment.
#[derive(Debug, Clone)]
pub struct PaneObservation {
    pub roots: PaneRoots,
    /// Les lanceurs de la configuration au moment de l'observation : leurs commandes reconnaissent les agents des
    /// sessions dans l'arbre des processus (`session_agents`).
    pub launchers: Vec<AgentEntry>,
    pub server_sessions: Vec<ListedServerSession>,
    /// Pris avant la lecture de tmux : une observation plus ancienne que la création d'une session serveur ne peut
    /// pas la décrire.
    pub taken_at: Instant,
}

/// Le pane est un shell nu : `@idema_agent` vide, ou l'entrée « Shell seul ».
pub(crate) fn is_bare_shell(agent: &str) -> bool {
    agent.is_empty() || agent == SHELL_SESSION_NAME || agent == SHELL_ENTRY
}

/// L'entrée du dernier `idnma run <clé>` du pane (`@idnma_agent`, posé par idnma), qui a pu être tapé à la main après le
/// lanceur. Entre plusieurs entrées de cette clé : celle qui a lancé le pane si elle en est (`launched`), puis un homonyme
/// de celle-ci (`launched` est le premier de ce nom, deux harness peuvent le partager), sinon la plus courte, `idnma run
/// glm` plutôt que `idnma run glm --effort high`.
pub(crate) fn idnma_entry<'a>(
    key: &str,
    launched: Option<&'a AgentEntry>,
    agents: &'a [AgentEntry],
) -> Option<&'a AgentEntry> {
    if key.is_empty() {
        return None;
    }
    let runs_key = |entry: &AgentEntry| {
        let mut words = entry.command.split_whitespace();
        words
            .next()
            .and_then(|program| Path::new(program).file_name())
            .is_some_and(|program| program == "idnma")
            && words.next() == Some("run")
            && words.next() == Some(key)
    };
    if let Some(entry) = launched.filter(|entry| runs_key(entry)) {
        return Some(entry);
    }
    let shortest = |name: Option<&str>| {
        agents
            .iter()
            .filter(|entry| runs_key(entry) && name.is_none_or(|name| entry.name == name))
            .min_by_key(|entry| entry.command.split_whitespace().count())
    };
    launched
        .and_then(|entry| shortest(Some(&entry.name)))
        .or_else(|| shortest(None))
}

/// Les racines : un pane vivant par session de la barre (`pane_pids` et `panes`), la session serveur de chaque
/// serveur démarré (morte comprise, avec `dead`) et le shell local du volet Terminal.
pub fn build_pane_roots(
    inputs: &PaneInputs,
    pane_pids: &[(String, PaneId, i32)],
    panes: &[ListedPane],
    server_sessions: &[ListedServerSession],
) -> PaneRoots {
    let mut roots = Vec::new();
    for (session, pane, pid) in pane_pids {
        let Some(info) = inputs.sessions.iter().find(|info| &info.name == session) else {
            continue;
        };
        let listed = panes
            .iter()
            .find(|listed| &listed.session == session && listed.pane == *pane);
        let agent = listed.map_or("", |listed| listed.agent.as_str());
        let has_agent = !is_bare_shell(agent);
        let launched = has_agent
            .then(|| inputs.agents.iter().find(|entry| entry.name == agent))
            .flatten();
        let idnma_agent = listed.map_or("", |listed| listed.idnma_agent.as_str());
        let agent_command = idnma_entry(idnma_agent, launched, &inputs.agents)
            .or(launched)
            .map(|entry| entry.command.clone());
        roots.push(PaneRoot {
            pid: *pid,
            owner: PaneOwner::Session {
                session: session.clone(),
                display_name: info.display_name.clone(),
                project: info.project.clone(),
            },
            agent_command,
            dead: false,
        });
    }
    for session in server_sessions {
        roots.push(PaneRoot {
            pid: session.pane_pid,
            owner: PaneOwner::ServerSession {
                key: ServerKey {
                    project: session.project.clone(),
                    name: session.entry.clone(),
                },
            },
            agent_command: None,
            dead: session.dead,
        });
    }
    for (project, pid) in &inputs.terminal_shells {
        roots.push(PaneRoot {
            pid: *pid,
            owner: PaneOwner::TerminalPanel {
                project: project.clone(),
            },
            agent_command: None,
            dead: false,
        });
    }
    PaneRoots { panes: roots }
}

/// Lit tmux (bloquant : hors du fil principal) et bâtit l'observation. Une lecture qui échoue donne l'erreur : l'état
/// précédent reste en place.
pub fn observe_panes(tmux: &TmuxServer, inputs: &PaneInputs) -> Result<PaneObservation, String> {
    let taken_at = Instant::now();
    let pane_pids = tmux.pane_pids().map_err(|error| error.to_string())?;
    let panes = tmux.panes().map_err(|error| error.to_string())?;
    let server_sessions = tmux.server_sessions().map_err(|error| error.to_string())?;
    Ok(PaneObservation {
        roots: build_pane_roots(inputs, &pane_pids, &panes, &server_sessions),
        launchers: inputs.agents.clone(),
        server_sessions,
        taken_at,
    })
}

// ---------------------------------------------------------------------------------------------------------------------
// Les fichiers de configuration d'un projet

/// Les fichiers dont dépend la configuration des serveurs d'un projet (spec §5.1).
const WATCHED_FILES: [&str; 5] = [
    ".claude/launch.json",
    "package.json",
    "bun.lock",
    "bun.lockb",
    "pnpm-lock.yaml",
];

/// Date et taille de chaque fichier surveillé, comme `FileWatch` de la config : un changement de l'un des deux suffit.
type FileStamps = Vec<Option<(SystemTime, u64)>>;

fn stamp_project(project: &Path) -> FileStamps {
    WATCHED_FILES
        .iter()
        .map(|file| {
            let meta = std::fs::metadata(project.join(file)).ok()?;
            Some((meta.modified().ok()?, meta.len()))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------------
// Les travaux

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMode {
    /// Le geste « Démarrer » : refusé quand la ligne tourne déjà.
    Gesture,
    /// La fin d'une relance : le port vient d'être libéré, la ligne montre encore l'ancien processus jusqu'au relevé
    /// suivant. Le contrôle en direct du port reste fait.
    Restart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartRefusal {
    UnknownEntry,
    /// Un démarrage de cette entrée est déjà en route, ou la ligne est en démarrage.
    InProgress,
    /// Le port est en écoute (P4-10).
    PortBusy,
}

/// Un démarrage décidé : ce que le fil d'arrière-plan exécute.
#[derive(Debug, Clone)]
pub struct StartJob {
    pub key: ServerKey,
    pub request: LaunchRequest,
    /// Le port à vérifier en direct juste avant de créer la session.
    pub port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    Started {
        session: String,
        /// Après la fermeture des anciennes sessions et la création de la nouvelle.
        at: Instant,
        /// La session telle que tmux la liste juste après sa création (son PID de pane), si la relecture a réussi.
        listed: Option<ListedServerSession>,
    },
    PortBusy {
        pid: i32,
    },
    Failed {
        reason: String,
    },
}

/// La cause d'un démarrage qui n'a pas pu créer sa session, telle que la ligne la montre : le dossier qui manque se dit en
/// français, le reste est le message de tmux, sur sa première ligne.
fn failure_text(reason: &str) -> String {
    if let Some(path) = reason.strip_prefix("no such directory: ") {
        return format!("dossier introuvable : {path}");
    }
    reason.lines().next().unwrap_or(reason).trim().to_owned()
}

/// Exécute un démarrage (bloquant). Le port est relu en direct (P4-10 : un serveur lancé par un agent il y a une
/// seconde n'est pas encore dans le registre), les sessions serveur précédentes de cette entrée sont fermées (une
/// session en échec reste jusqu'ici pour que son journal se relise), puis la nouvelle est créée.
pub fn run_start(job: &StartJob, launcher: &dyn ServerLauncher) -> StartOutcome {
    if let Some(port) = job.port {
        match launcher.listener_of(port) {
            Ok(Some(pid)) => return StartOutcome::PortBusy { pid },
            Ok(None) => {}
            // Un relevé raté ne dit rien du port : on démarre, un port pris fera échouer le serveur lui-même.
            Err(error) => hud::log_event(&format!(
                "server start port check failed port={port} reason={error}"
            )),
        }
    }
    // Sans fermer l'ancienne session on ne crée pas la nouvelle : deux sessions d'une entrée brouilleraient le
    // rattachement (laquelle est « la » session du serveur ?) et le journal.
    let sessions = match launcher.sessions() {
        Ok(sessions) => sessions,
        Err(error) => {
            return StartOutcome::Failed {
                reason: format!("cannot list server sessions: {error}"),
            };
        }
    };
    for session in sessions
        .iter()
        .filter(|session| session.project == job.key.project && session.entry == job.key.name)
    {
        if let Err(error) = launcher.kill(&session.name) {
            // Une session qui a disparu entre le listage et la fermeture (sa commande s'est terminée, un autre
            // client l'a fermée) est fermée : on relit pour le savoir, et sans réponse on ne crée rien.
            let gone = launcher
                .sessions()
                .is_ok_and(|now| now.iter().all(|listed| listed.name != session.name));
            if !gone {
                return StartOutcome::Failed {
                    reason: format!("cannot close session {}: {error}", session.name),
                };
            }
        }
    }
    match launcher.create(&job.request) {
        Ok(session) => {
            let at = Instant::now();
            // Le PID du pane sert à l'arrêt dans les 2 s qui suivent, avant la prochaine observation.
            let listed = match launcher.sessions() {
                Ok(sessions) => sessions.into_iter().find(|listed| listed.name == session),
                Err(error) => {
                    // Sans cette ligne, un arrêt dans les 2 s ne retrouverait pas la session : le journal le dit.
                    hud::log_event(&format!(
                        "server session relist failed session={session} reason={error}"
                    ));
                    None
                }
            };
            StartOutcome::Started {
                session,
                at,
                listed,
            }
        }
        Err(reason) => StartOutcome::Failed { reason },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopDecline {
    UnknownRow,
    NotRunning,
    /// Un arrêt de cette ligne est déjà en route.
    InProgress,
    /// Rien ne peut être arrêté sans toucher un shell de pane ou l'agent, ou aucun relevé n'est encore arrivé.
    NoTarget,
}

/// Le port d'une ligne en marche et le processus qui l'écoute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listening {
    pub port: u16,
    pub pid: i32,
}

/// Un arrêt décidé : le plan relevé au moment du geste.
#[derive(Debug, Clone)]
pub struct StopJob {
    pub kind: RowKind,
    pub name: String,
    pub project: PathBuf,
    /// Le port que la ligne montre et le PID qui l'écoute ; `None` pour l'arrêt d'une session serveur dont aucun port ne
    /// répond (démarrage en cours ou en échec, P4-R7) : le plan est alors tout le sous-arbre du pane de la session.
    pub listening: Option<Listening>,
    pub plan: StopPlan,
    /// Les sessions serveur qui se ferment quand le serveur est arrêté.
    pub sessions_to_close: Vec<String>,
}

/// Exécute un arrêt (bloquant, jusqu'à `grace` plus une courte attente) puis ferme la session serveur d'un serveur
/// arrêté. Un arrêt refusé ou remplacé laisse la session telle quelle.
pub fn run_stop(
    job: &StopJob,
    launcher: &dyn ServerLauncher,
    grace: Duration,
    listener_of: impl Fn(u16) -> Result<Option<i32>, String>,
    processes_now: impl Fn() -> Option<HashMap<i32, Process>>,
) -> StopOutcome {
    let outcome = match job.listening {
        Some(Listening { port, pid }) => {
            execute_stop(&job.plan, port, pid, grace, listener_of, processes_now)
        }
        None => execute_session_stop(&job.plan, grace, processes_now),
    };
    if matches!(outcome, StopOutcome::Freed | StopOutcome::Killed) {
        let mut close_failed = false;
        for session in &job.sessions_to_close {
            if let Err(error) = launcher.kill(session) {
                close_failed = true;
                hud::log_event(&format!(
                    "server session close failed session={session} reason={error}"
                ));
            }
        }
        // Un arrêt par la session n'a pas de port à relire : un plan vide (le processus est déjà parti) y compte pour
        // réussi sans que rien ait été signalé. La session doit alors avoir disparu, sinon la ligne garde son état.
        if job.listening.is_none() && !session_closed(job, launcher, close_failed) {
            hud::log_event(&format!(
                "server stop refused reason=session-still-listed name={} project={}",
                job.name,
                job.project.display()
            ));
            return StopOutcome::Refused { pids: Vec::new() };
        }
    }
    outcome
}

/// Les sessions de l'arrêt ont-elles disparu de tmux ? Le relistage tranche (une fermeture en erreur peut vouloir dire
/// « déjà partie ») ; sans relistage, une fermeture réussie fait foi et une fermeture en erreur ne prouve rien.
fn session_closed(job: &StopJob, launcher: &dyn ServerLauncher, close_failed: bool) -> bool {
    match launcher.sessions() {
        Ok(listed) => !listed
            .iter()
            .any(|session| job.sessions_to_close.contains(&session.name)),
        Err(_) => !close_failed,
    }
}

/// Ce que devient une relance une fois l'arrêt rendu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartStep {
    /// Le port est libre : on démarre.
    Start,
    /// On ne démarre pas, et pourquoi (pour le journal).
    Abort { reason: &'static str },
}

/// La première moitié d'une relance (bloquante) : arrête, puis attend que le port se libère. Rien n'est démarré si un
/// autre processus a repris le port (`Replaced`), si l'arrêt est refusé, ou si le port reste tenu après l'attente.
pub fn run_restart_stop(
    job: &StopJob,
    launcher: &dyn ServerLauncher,
    grace: Duration,
    free_wait: Duration,
    free_poll: Duration,
    listener_of: impl Fn(u16) -> Result<Option<i32>, String>,
    processes_now: impl Fn() -> Option<HashMap<i32, Process>>,
) -> (StopOutcome, RestartStep) {
    let outcome = run_stop(job, launcher, grace, &listener_of, processes_now);
    let step = match &outcome {
        StopOutcome::Freed | StopOutcome::Killed => {
            // Une relance part d'une ligne en marche, donc d'un port ; sans port, rien à attendre.
            if job.listening.is_none_or(|listening| {
                wait_until_free(listening.port, free_wait, free_poll, &listener_of)
            }) {
                RestartStep::Start
            } else {
                RestartStep::Abort {
                    reason: "port-busy",
                }
            }
        }
        StopOutcome::Replaced { .. } => RestartStep::Abort { reason: "replaced" },
        StopOutcome::Refused { .. } => RestartStep::Abort { reason: "refused" },
    };
    (outcome, step)
}

/// Attend que plus rien n'écoute sur le port, au plus `timeout`, en sondant toutes les `poll`. Un relevé qui échoue
/// ne dit rien : on continue d'attendre. Vrai quand le port est libre.
pub fn wait_until_free(
    port: u16,
    timeout: Duration,
    poll: Duration,
    listener_of: impl Fn(u16) -> Result<Option<i32>, String>,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if matches!(listener_of(port), Ok(None)) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(poll.min(deadline - now));
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Le journal

/// D'où l'onglet journal d'un serveur tient ses octets (spec §4.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalSource {
    /// Le pane de la session tmux que `Servers` a créée pour ce serveur.
    Session { name: String, pane: PaneId },
    /// Un fichier ordinaire où va la sortie standard du processus : on en suit la fin.
    File(PathBuf),
    /// La sortie va ailleurs (`/dev/null`, un tube, un terminal), ou on ne sait pas où.
    Unavailable { target: Option<String> },
    /// Le serveur n'est pas en marche et sa session n'existe plus (arrêté par IdemA ou ailleurs) : il n'y a rien à relire.
    Stopped,
}

impl JournalSource {
    /// Le journal se lit-il ? C'est aussi la condition de l'action « journal » sur une ligne.
    pub fn is_available(&self) -> bool {
        !matches!(self, Self::Unavailable { .. } | Self::Stopped)
    }

    /// Le texte d'un serveur arrêté, sans session à relire (`JournalSource::Stopped`).
    pub const STOPPED_TEXT: &'static str = "Ce serveur est arrêté : aucune sortie à afficher.";

    /// Ce que dit l'onglet quand le journal manque (spec §4.5).
    pub fn unavailable_text(target: Option<&str>) -> String {
        match target {
            Some(target) => format!("Journal indisponible : ce serveur écrit dans {target}"),
            None => "Journal indisponible : la sortie de ce serveur est inconnue".to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Le cœur, sans GPUI

#[derive(Default)]
pub struct ServersCore {
    registry: Registry,
    projects: Vec<PathBuf>,
    configs: HashMap<PathBuf, ProjectConfig>,
    stamps: HashMap<PathBuf, FileStamps>,
    snapshot: Option<Snapshot>,
    roots: PaneRoots,
    roots_taken_at: Option<Instant>,
    server_sessions: Vec<ListedServerSession>,
    /// Par entrée : l'instant où sa session serveur a été créée. Un relevé ou des racines plus anciens sont écartés
    /// pour cette entrée, jusqu'à ce qu'un relevé et des racines plus récents soient arrivés.
    started: HashMap<ServerKey, Instant>,
    /// La session créée par un démarrage, tant qu'aucune observation postérieure ne la décrit : sans elle, un arrêt
    /// demandé avant la prochaine observation ne retrouverait pas la session à fermer.
    fresh: HashMap<ServerKey, (Instant, ListedServerSession)>,
    starting: HashSet<ServerKey>,
    stopping: HashSet<RowKind>,
    /// Les PID de pane des sessions serveur qu'IdemA vient de fermer, avec l'instant : une observation plus ancienne les
    /// liste encore, et la ligne repasserait un instant en « démarrage ».
    closed: HashMap<i32, Instant>,
    reattach_logged: bool,
    /// Les lanceurs de la dernière observation.
    launchers: Vec<AgentEntry>,
    /// Par session de la barre, les agents qui tournent dans ses panes au dernier relevé (`session_agents`).
    session_agents: HashMap<String, Vec<String>>,
}

impl ServersCore {
    pub fn new() -> Self {
        Self {
            registry: Registry::new(),
            ..Self::default()
        }
    }

    /// Les projets de la barre, dans son ordre. Vrai quand la liste a changé. La configuration d'un projet nouveau
    /// se lit ici (deux petits fichiers).
    pub fn set_projects(&mut self, projects: Vec<PathBuf>) -> bool {
        let mut seen = HashSet::new();
        let projects: Vec<PathBuf> = projects
            .into_iter()
            .filter(|project| seen.insert(project.clone()))
            .collect();
        if projects == self.projects {
            return false;
        }
        self.configs.retain(|project, _| projects.contains(project));
        self.stamps.retain(|project, _| projects.contains(project));
        for project in &projects {
            if !self.configs.contains_key(project) {
                // L'empreinte est prise avant la lecture : un changement entre les deux se voit au pas suivant.
                self.stamps.insert(project.clone(), stamp_project(project));
                self.configs
                    .insert(project.clone(), read_project_config(project));
            }
        }
        self.projects = projects;
        self.push_projects();
        true
    }

    /// Relit la configuration des projets dont un fichier surveillé a changé. Vrai quand l'un d'eux a été relu.
    pub fn poll_project_files(&mut self) -> bool {
        let changed: Vec<PathBuf> = self
            .projects
            .iter()
            .filter(|project| self.stamps.get(*project) != Some(&stamp_project(project)))
            .cloned()
            .collect();
        for project in &changed {
            self.reread_project(project);
        }
        !changed.is_empty()
    }

    /// Relit la configuration d'un projet connu.
    pub fn reread_project(&mut self, project: &Path) {
        if !self.projects.iter().any(|known| known == project) {
            return;
        }
        self.stamps
            .insert(project.to_path_buf(), stamp_project(project));
        self.configs
            .insert(project.to_path_buf(), read_project_config(project));
        hud::log_event(&format!(
            "servers config reread project={}",
            project.display()
        ));
        self.push_projects();
    }

    fn push_projects(&mut self) {
        let ordered = self
            .projects
            .iter()
            .filter_map(|project| {
                self.configs
                    .get(project)
                    .map(|config| (project.clone(), config.clone()))
            })
            .collect();
        self.registry.set_projects(ordered);
    }

    /// Les racines des panes et les sessions serveur listées au même moment.
    pub fn set_observation(&mut self, observation: PaneObservation) {
        let live = observation
            .server_sessions
            .iter()
            .filter(|session| !session.dead)
            .count();
        if !self.reattach_logged {
            self.reattach_logged = true;
            if live > 0 {
                // Les serveurs d'une instance précédente : leurs lignes passent en marche au relevé suivant (AC-93).
                hud::log_event(&format!("servers reattached count={live}"));
            }
        }
        let taken_at = observation.taken_at;
        // Une observation prise après la fermeture ne liste plus la session : l'oubli n'a plus d'objet. Avant, elle la liste encore.
        self.closed.retain(|_, closed_at| taken_at < *closed_at);
        let mut observation = observation;
        let closed = &self.closed;
        observation.roots.panes.retain(|pane| {
            !(matches!(pane.owner, PaneOwner::ServerSession { .. })
                && closed.contains_key(&pane.pid))
        });
        observation
            .server_sessions
            .retain(|session| !closed.contains_key(&session.pane_pid));
        self.roots = observation.roots;
        self.launchers = observation.launchers;
        self.roots_taken_at = Some(taken_at);
        self.server_sessions = observation.server_sessions;
        // Une observation postérieure à la création décrit la session elle-même ; une plus ancienne l'ignore.
        self.fresh.retain(|_, (at, _)| taken_at < *at);
        for (key, (_, listed)) in &self.fresh {
            self.server_sessions
                .retain(|session| !(session.project == key.project && session.entry == key.name));
            self.server_sessions.push(listed.clone());
        }
    }

    /// Les sessions serveur que l'arrêt vient de fermer ne comptent plus : ni leurs racines, ni leur liste, jusqu'à une
    /// observation prise après la fermeture.
    fn forget_sessions(&mut self, names: &[String], now: Instant) {
        for session in self
            .server_sessions
            .iter()
            .filter(|session| names.contains(&session.name))
        {
            self.closed.insert(session.pane_pid, now);
        }
        let closed = &self.closed;
        self.server_sessions
            .retain(|session| !names.contains(&session.name));
        self.fresh
            .retain(|_, (_, listed)| !names.contains(&listed.name));
        self.roots.panes.retain(|pane| {
            !(matches!(pane.owner, PaneOwner::ServerSession { .. })
                && closed.contains_key(&pane.pid))
        });
    }

    /// Combien de sessions serveur d'IdemA vivent pour ce projet (le retrait du projet les attend : sans lui, leurs
    /// serveurs ne se verraient plus nulle part, P4-R7).
    pub fn live_sessions_of(&self, project: &Path) -> usize {
        self.server_sessions
            .iter()
            .filter(|session| !session.dead && session.project == project)
            .count()
    }

    /// Les racines pour le registre : une racine morte d'une session serveur qu'un démarrage vient de remplacer est
    /// celle de l'ancienne session, pas de la nouvelle (elle ferait échouer le démarrage tout juste lancé).
    fn effective_roots(&self) -> PaneRoots {
        let mut panes: Vec<PaneRoot> = self
            .roots
            .panes
            .iter()
            .filter(|root| {
                let PaneOwner::ServerSession { key } = &root.owner else {
                    return true;
                };
                // Des racines lues avant la création ne décrivent que l'ancienne session, morte ou fermée depuis.
                let stale = self.started.get(key).is_some_and(|started| {
                    self.roots_taken_at
                        .is_none_or(|taken_at| taken_at < *started)
                });
                !stale
            })
            .cloned()
            .collect();
        for (key, (_, listed)) in &self.fresh {
            if self.started.contains_key(key) {
                panes.push(PaneRoot {
                    pid: listed.pane_pid,
                    owner: PaneOwner::ServerSession { key: key.clone() },
                    agent_command: None,
                    dead: listed.dead,
                });
            }
        }
        PaneRoots { panes }
    }

    /// Ce que la sonde HTTP du fil de relevé vise (spec §5.2) : les racines de pane que le registre retient, vivantes,
    /// les projets et les ports que leurs entrées déclarent.
    pub fn probe_scope(&self) -> ProbeScope {
        ProbeScope {
            pane_pids: self
                .effective_roots()
                .panes
                .iter()
                .filter(|pane| !pane.dead)
                .map(|pane| pane.pid)
                .collect(),
            projects: self.projects.clone(),
            declared_ports: self
                .configs
                .values()
                .flat_map(|config| config.entries.iter().filter_map(|entry| entry.port))
                .collect(),
            // Relus par le fil de relevé à chaque passage (`scan_once`).
            launchd_jobs: HashSet::new(),
        }
    }

    /// Fusionne un relevé. Vrai quand une ligne a changé. Un relevé pris avant la création d'une session serveur est
    /// écarté en entier (il montre l'ancien processus de la ligne, ou rien) ; il suffit d'attendre le suivant.
    pub fn apply_scan(&mut self, snapshot: Snapshot, now: Instant) -> bool {
        let stale = self
            .started
            .values()
            .any(|started| snapshot.taken_at < *started);
        if stale {
            return false;
        }
        // Un relevé et des racines plus récents que le démarrage : la garde n'a plus lieu d'être.
        let roots_taken_at = self.roots_taken_at;
        self.started
            .retain(|_, started| roots_taken_at.is_none_or(|taken_at| taken_at < *started));
        let before = self.flat_rows();
        let roots = self.effective_roots();
        self.registry.apply_scan(&snapshot, &roots, now);
        let agents = session_agents(&roots, &snapshot.processes, &self.launchers);
        let agents_changed = agents != self.session_agents;
        self.session_agents = agents;
        self.snapshot = Some(snapshot);
        before != self.flat_rows() || agents_changed
    }

    /// La table de processus du dernier relevé, si elle date de `max_age` au plus avant `now` ; `None` avant le premier
    /// relevé ou quand le fil de relevé n'aboutit plus (la capture de l'instantané saute alors son tour).
    pub fn recent_processes(
        &self,
        now: Instant,
        max_age: Duration,
    ) -> Option<&HashMap<i32, Process>> {
        let snapshot = self.snapshot.as_ref()?;
        (now.saturating_duration_since(snapshot.taken_at) <= max_age).then_some(&snapshot.processes)
    }

    /// Les agents qui tournent dans la session, dans l'ordre de ses panes ; vide avant le premier relevé.
    pub fn agents_of(&self, session: &str) -> &[String] {
        self.session_agents.get(session).map_or(&[], Vec::as_slice)
    }

    fn flat_rows(&self) -> Vec<ServerRow> {
        self.registry
            .rows(None)
            .into_iter()
            .flat_map(|group| group.rows.into_iter().chain(group.detected))
            .collect()
    }

    pub fn groups(&self, context: Option<&Path>) -> Vec<ProjectGroup> {
        self.registry.rows(context)
    }

    pub fn running_count(&self) -> usize {
        self.registry.running_count()
    }

    /// Le nombre de serveurs repérés hors de la configuration qui écoutent encore (la ligne qui s'éteint n'en est plus).
    pub fn detected_running(&self) -> usize {
        self.flat_rows()
            .iter()
            .filter(|row| {
                matches!(row.kind, RowKind::Detected { .. })
                    && matches!(
                        row.state,
                        RowState::Running { .. } | RowState::StopRefused { .. }
                    )
            })
            .count()
    }

    pub fn remembered_dev_ports(&self) -> HashMap<PathBuf, u16> {
        self.registry.remembered_dev_ports()
    }

    pub fn restore_dev_ports(&mut self, ports: HashMap<PathBuf, u16>) {
        self.registry.restore_dev_ports(ports);
    }

    pub fn row_of(&self, kind: &RowKind) -> Option<ServerRow> {
        self.flat_rows().into_iter().find(|row| &row.kind == kind)
    }

    /// La ligne de configuration que cette adresse ouvre : la même adresse au fragment près, ou le port d'une entrée
    /// sur la boucle locale.
    pub fn row_for_url(&self, url: &str) -> Option<ServerRow> {
        let parsed = Url::parse(url).ok();
        let local_port = parsed.as_ref().and_then(|parsed| {
            let local = matches!(
                parsed.host_str(),
                Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
            );
            local.then(|| parsed.port_or_known_default()).flatten()
        });
        self.flat_rows()
            .into_iter()
            .filter(|row| matches!(row.kind, RowKind::Config(_)))
            .find(|row| {
                (!row.url.is_empty() && same_address(&row.url, url))
                    || (local_port.is_some() && row.port == local_port)
            })
    }

    /// La ligne que désigne un onglet journal : une entrée de configuration par son projet et son nom, un serveur repéré
    /// par son projet, son nom et son port (deux serveurs repérés de même nom se distinguent par le port).
    pub fn find_row(&self, project: &Path, name: &str, port: Option<u16>) -> Option<ServerRow> {
        self.flat_rows().into_iter().find(|row| {
            row.project == project
                && row.name == name
                && match row.kind {
                    RowKind::Config(_) => true,
                    RowKind::Detected { .. } => row.port == port,
                }
        })
    }

    /// Où se lit la sortie de cette ligne (spec §4.5) : le pane de sa session serveur tant qu'elle vient d'IdemA (ou
    /// qu'elle n'est pas en marche : le journal d'un démarrage raté se relit), sinon la fin du fichier où va sa sortie
    /// standard, sinon rien.
    pub fn journal_source(&self, row: &ServerRow) -> JournalSource {
        let from_idema = matches!(row.origin, None | Some(Origin::Idema));
        if from_idema
            && let RowKind::Config(key) = &row.kind
            && let Some(session) = self.server_session_of(key)
        {
            return JournalSource::Session {
                name: session.name,
                pane: session.pane,
            };
        }
        match &row.output {
            Some(OutputTarget::File(path)) => JournalSource::File(path.clone()),
            Some(OutputTarget::Other(target)) => JournalSource::Unavailable {
                target: Some(target.clone()),
            },
            None if matches!(row.state, RowState::Stopped | RowState::Failed) => {
                JournalSource::Stopped
            }
            None => JournalSource::Unavailable { target: None },
        }
    }

    /// La session serveur de l'entrée : la vivante d'abord, sinon la plus récemment listée (morte comprise).
    pub fn server_session_of(&self, key: &ServerKey) -> Option<ListedServerSession> {
        let of_entry = |session: &&ListedServerSession| {
            session.project == key.project && session.entry == key.name
        };
        self.server_sessions
            .iter()
            .filter(of_entry)
            .rfind(|session| !session.dead)
            .or_else(|| self.server_sessions.iter().rfind(of_entry))
            .cloned()
    }

    fn entry_of(&self, key: &ServerKey) -> Option<&ConfigEntry> {
        self.configs
            .get(&key.project)?
            .entries
            .iter()
            .find(|entry| entry.name == key.name)
    }

    /// Décide d'un démarrage (P4-10). Refuse une entrée inconnue, un démarrage déjà en route, une ligne en démarrage
    /// et, pour le geste, un serveur qui tourne ou dont le port est en écoute (journal
    /// `server start refused reason=port-busy`).
    pub fn begin_start(
        &mut self,
        key: &ServerKey,
        mode: StartMode,
    ) -> Result<StartJob, StartRefusal> {
        let entry = self
            .entry_of(key)
            .cloned()
            .ok_or(StartRefusal::UnknownEntry)?;
        let kind = RowKind::Config(key.clone());
        if self.starting.contains(key) {
            return Err(StartRefusal::InProgress);
        }
        // Un arrêt en route est celui d'une relance : seule la relance démarre à sa suite.
        if mode == StartMode::Gesture && self.stopping.contains(&kind) {
            return Err(StartRefusal::InProgress);
        }
        let row = self.row_of(&kind);
        if row
            .as_ref()
            .is_some_and(|row| row.state == RowState::Starting)
        {
            return Err(StartRefusal::InProgress);
        }
        if mode == StartMode::Gesture
            && let Some(row) = &row
        {
            match row.state {
                RowState::Running { .. } | RowState::StopRefused { .. } => {
                    hud::log_event(&format!(
                        "server start refused reason=port-busy name={} project={}",
                        key.name,
                        key.project.display()
                    ));
                    return Err(StartRefusal::PortBusy);
                }
                // `Starting` est refusé plus haut, pour les deux modes.
                RowState::Starting | RowState::Stopped | RowState::Failed => {}
            }
        }
        let mut env = Vec::new();
        if let Some(port) = entry.port
            && !entry.env.iter().any(|(name, _)| name == "PORT")
        {
            env.push(("PORT".to_string(), port.to_string()));
        }
        env.extend(entry.env.iter().cloned());
        self.starting.insert(key.clone());
        Ok(StartJob {
            key: key.clone(),
            request: LaunchRequest {
                project: key.project.clone(),
                entry: key.name.clone(),
                cwd: entry.cwd.clone(),
                program: entry.program.clone(),
                args: entry.args.clone(),
                env,
            },
            port: entry.port.or_else(|| row.and_then(|row| row.port)),
        })
    }

    /// Range le résultat d'un démarrage : la ligne passe en démarrage ou en échec, et la garde contre les relevés
    /// d'avant la création s'arme. Vrai quand la ligne a changé de façon visible.
    pub fn finish_start(&mut self, job: &StartJob, outcome: &StartOutcome) -> bool {
        self.starting.remove(&job.key);
        let name = &job.key.name;
        let project = job.key.project.display();
        match outcome {
            StartOutcome::Started {
                session,
                at,
                listed,
            } => {
                self.started.insert(job.key.clone(), *at);
                if let Some(listed) = listed {
                    self.server_sessions.retain(|known| {
                        !(known.project == job.key.project && known.entry == job.key.name)
                    });
                    self.server_sessions.push(listed.clone());
                    self.fresh.insert(job.key.clone(), (*at, listed.clone()));
                }
                self.registry.mark_starting(&job.key, *at);
                hud::log_event(&format!(
                    "server started name={name} project={project} session={session}"
                ));
                true
            }
            StartOutcome::PortBusy { pid } => {
                hud::log_event(&format!(
                    "server start refused reason=port-busy name={name} project={project} pid={pid}"
                ));
                false
            }
            StartOutcome::Failed { reason } => {
                self.registry
                    .mark_failed(&job.key, Some(failure_text(reason)));
                hud::log_event(&format!(
                    "server start failed name={name} project={project} reason={reason}"
                ));
                true
            }
        }
    }

    /// Range l'arrêt d'une relance et rend le démarrage qui suit, s'il a lieu. Le démarrage de la relance passe même si
    /// la ligne montre encore l'ancien processus (le port vient d'être confirmé libre).
    pub fn finish_restart_stop(
        &mut self,
        job: &StopJob,
        outcome: &StopOutcome,
        step: &RestartStep,
    ) -> Option<StartJob> {
        self.finish_stop(job, outcome);
        let RowKind::Config(key) = &job.kind else {
            return None;
        };
        match step {
            RestartStep::Start => self.begin_start(key, StartMode::Restart).ok(),
            RestartStep::Abort { reason } => {
                hud::log_event(&format!(
                    "server restart aborted reason={reason} name={} project={}",
                    job.name,
                    job.project.display()
                ));
                None
            }
        }
    }

    /// Décide d'un arrêt : relève la cible et le plan sur le dernier relevé. Un arrêt impossible sans toucher le shell
    /// d'un pane ou l'agent marque la ligne « arrêt refusé ».
    pub fn begin_stop(&mut self, kind: &RowKind) -> Result<StopJob, StopDecline> {
        let row = self.row_of(kind).ok_or(StopDecline::UnknownRow)?;
        let (RowState::Running { port, .. } | RowState::StopRefused { port, .. }) = row.state
        else {
            return self.begin_session_stop(kind, &row);
        };
        if self.stopping.contains(kind) {
            return Err(StopDecline::InProgress);
        }
        let Some(snapshot) = &self.snapshot else {
            return Err(StopDecline::NoTarget);
        };
        let listener = row.listener_pid.ok_or(StopDecline::NotRunning)?;
        let roots = self.effective_roots();
        let target = stop_target(&row, &snapshot.processes, &roots, &self.server_sessions);
        let plan = target
            .as_ref()
            .map(|target| plan_stop(target, &snapshot.processes))
            .filter(|plan| !plan.pids.is_empty());
        let (Some(target), Some(plan)) = (target, plan) else {
            hud::log_event(&format!(
                "server stop refused reason=no-target name={} project={}",
                row.name,
                row.project.display()
            ));
            self.registry.mark_stop_refused(kind);
            return Err(StopDecline::NoTarget);
        };
        let sessions_to_close = match target {
            StopTarget::ServerSession { pane_pid } => self
                .server_sessions
                .iter()
                .filter(|session| !session.dead && session.pane_pid == pane_pid)
                .map(|session| session.name.clone())
                .collect(),
            _ => Vec::new(),
        };
        self.stopping.insert(kind.clone());
        Ok(StopJob {
            kind: kind.clone(),
            name: row.name,
            project: row.project,
            listening: Some(Listening {
                port,
                pid: listener,
            }),
            plan,
            sessions_to_close,
        })
    }

    /// L'arrêt d'une ligne qui n'est pas en marche mais dont la session serveur d'IdemA vit : un démarrage en cours ou en
    /// échec qui n'a pas ouvert le port de l'entrée (P4-R7 : elle doit pouvoir s'arrêter d'un clic). La cible est le
    /// sous-arbre du pane de la session, comme pour un serveur d'IdemA en marche ; le pane est celui que tmux liste, jamais
    /// un ancêtre. Le plan peut être vide (le processus est déjà parti) : la session se ferme quand même.
    fn begin_session_stop(
        &mut self,
        kind: &RowKind,
        row: &ServerRow,
    ) -> Result<StopJob, StopDecline> {
        let RowKind::Config(key) = kind else {
            return Err(StopDecline::NotRunning);
        };
        let sessions: Vec<&ListedServerSession> = self
            .server_sessions
            .iter()
            .filter(|session| {
                !session.dead && session.project == key.project && session.entry == key.name
            })
            .collect();
        if !row.session_alive || sessions.is_empty() {
            return Err(StopDecline::NotRunning);
        }
        if self.stopping.contains(kind) {
            return Err(StopDecline::InProgress);
        }
        let Some(snapshot) = &self.snapshot else {
            return Err(StopDecline::NoTarget);
        };
        let mut plan = StopPlan {
            pids: Vec::new(),
            group: None,
            commands: HashMap::new(),
        };
        for session in &sessions {
            let one = plan_stop(
                &StopTarget::ServerSession {
                    pane_pid: session.pane_pid,
                },
                &snapshot.processes,
            );
            plan.pids.extend(one.pids);
            plan.commands.extend(one.commands);
            // Un seul groupe à la fois : avec plusieurs sessions, aucun `killpg` (les PID suffisent).
            plan.group = if sessions.len() == 1 { one.group } else { None };
        }
        let sessions_to_close = sessions
            .iter()
            .map(|session| session.name.clone())
            .collect();
        self.stopping.insert(kind.clone());
        Ok(StopJob {
            kind: kind.clone(),
            name: row.name.clone(),
            project: row.project.clone(),
            listening: None,
            plan,
            sessions_to_close,
        })
    }

    /// Range le résultat d'un arrêt. `Replaced` ne marque pas « arrêt refusé » : un autre processus tient le port,
    /// l'ancien est parti, et c'est le relevé qui le dira. `Refused` marque la ligne.
    pub fn finish_stop(&mut self, job: &StopJob, outcome: &StopOutcome) {
        self.stopping.remove(&job.kind);
        let name = &job.name;
        let project = job.project.display();
        match outcome {
            StopOutcome::Freed | StopOutcome::Killed => {
                hud::log_event(&format!(
                    "server stopped name={name} project={project} outcome={}",
                    if *outcome == StopOutcome::Freed {
                        "freed"
                    } else {
                        "killed"
                    }
                ));
                self.forget_sessions(&job.sessions_to_close, Instant::now());
                if let RowKind::Config(key) = &job.kind {
                    self.registry.mark_stopped(key);
                }
            }
            StopOutcome::Replaced { by } => hud::log_event(&format!(
                "server stop replaced name={name} project={project} by={by}"
            )),
            StopOutcome::Refused { pids } => {
                self.registry.mark_stop_refused(&job.kind);
                let list: Vec<String> = pids.iter().map(i32::to_string).collect();
                hud::log_event(&format!(
                    "server stop refused name={name} project={project} pids={}",
                    list.join(",")
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// L'entité

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServersEvent {
    /// Une ligne, un groupe ou la pastille ont changé (un démarrage qui échoue aussi : la ligne porte sa cause).
    Changed,
}

pub struct Servers {
    core: ServersCore,
    launcher: Arc<dyn ServerLauncher>,
    scanner: Option<Scanner>,
    /// Le nombre de serveurs en marche au dernier changement (`running_count`).
    running: usize,
    _drain: Task<()>,
}

impl EventEmitter<ServersEvent> for Servers {}

impl Servers {
    pub fn new(tmux: Arc<TmuxServer>, cx: &mut Context<Self>) -> Self {
        Self::with_launcher(tmux, cx)
    }

    /// Comme `new`, avec le lanceur de son choix.
    pub fn with_launcher(launcher: Arc<dyn ServerLauncher>, cx: &mut Context<Self>) -> Self {
        let (sender, receiver) = mpsc::channel::<ScanResult>();
        let scanner = Scanner::spawn(SCAN_PERIOD, sender);
        // Le canal du fil de relevé est bloquant : un fil le passe à un canal que la tâche GPUI attend sans bloquer.
        // Il s'arrête quand le `Scanner` tombe (le canal se ferme).
        let (forward, mut results) = unbounded::<ScanResult>();
        std::thread::Builder::new()
            .name("idema-servers-forward".to_string())
            .spawn(move || {
                while let Ok(result) = receiver.recv() {
                    if forward.unbounded_send(result).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn idema-servers-forward");
        let drain = cx.spawn(async move |this, cx| {
            while let Some(result) = results.next().await {
                let alive = this.update(cx, |this, cx| this.on_scan(result, cx));
                if alive.is_err() {
                    break;
                }
            }
        });
        Self {
            core: ServersCore::new(),
            launcher,
            scanner: Some(scanner),
            running: 0,
            _drain: drain,
        }
    }

    /// Une ligne, un groupe ou la pastille ont changé : le nombre de serveurs en marche est recalculé (une fois, ici, et
    /// non à chaque image de la fenêtre qui le lit), puis les observateurs sont prévenus.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.running = self.core.running_count();
        cx.emit(ServersEvent::Changed);
        cx.notify();
    }

    /// Un relevé réussi remplace l'état ; un relevé raté garde le dernier (le fil l'a déjà écrit au journal).
    fn on_scan(&mut self, result: ScanResult, cx: &mut Context<Self>) {
        let mut changed = self.core.poll_project_files();
        if let Ok(snapshot) = result {
            changed |= self.core.apply_scan(snapshot, Instant::now());
            // Une ligne par relevé (niveau debug) : les critères d'acceptation en lisent l'horodatage pour dire en
            // combien de temps un serveur lancé hors d'IdemA apparaît sur la page d'accueil.
            hud::log_debug(&format!(
                "servers scanned running={} detected={}",
                self.core.running_count(),
                self.core.detected_running()
            ));
        }
        if changed {
            self.changed(cx);
        }
        // Une configuration relue peut déclarer un autre port.
        self.push_probe_scope();
    }

    /// Les projets de la barre. Une liste inchangée ne coûte rien.
    pub fn set_projects(&mut self, projects: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.core.set_projects(projects) {
            self.push_probe_scope();
            self.changed(cx);
        }
    }

    /// Les racines des panes, poussées toutes les 2 s par l'espace de travail (hors fil principal).
    pub fn set_pane_roots(&mut self, observation: PaneObservation, _cx: &mut Context<Self>) {
        self.core.set_observation(observation);
        self.push_probe_scope();
    }

    /// Le fil de relevé ne sonde que ce que le registre peut montrer : les racines, les projets et leurs ports déclarés
    /// d'à présent.
    fn push_probe_scope(&self) {
        if let Some(scanner) = &self.scanner {
            scanner.set_probe_scope(self.core.probe_scope());
        }
    }

    pub fn groups(&self, context: Option<&Path>) -> Vec<ProjectGroup> {
        self.core.groups(context)
    }

    /// Les sessions serveur d'IdemA vivantes d'un projet (voir `ServersCore::live_sessions_of`).
    pub fn live_sessions_of(&self, project: &Path) -> usize {
        self.core.live_sessions_of(project)
    }

    /// Les agents qui tournent dans une session de la barre (voir `ServersCore::agents_of`).
    pub fn agents_of(&self, session: &str) -> &[String] {
        self.core.agents_of(session)
    }

    /// La table de processus du dernier relevé (voir `ServersCore::recent_processes`), clonée pour la tâche de capture.
    pub fn recent_processes(&self, max_age: Duration) -> Option<HashMap<i32, Process>> {
        self.core.recent_processes(Instant::now(), max_age).cloned()
    }

    /// Le nombre de serveurs en marche, tel que le dernier changement l'a laissé (la pastille du bouton Browser).
    pub fn running_count(&self) -> usize {
        self.running
    }

    pub fn row_for_url(&self, url: &str) -> Option<ServerRow> {
        self.core.row_for_url(url)
    }

    pub fn row_of(&self, kind: &RowKind) -> Option<ServerRow> {
        self.core.row_of(kind)
    }

    /// La ligne qu'un onglet journal désigne (voir `ServersCore::find_row`).
    pub fn find_row(&self, project: &Path, name: &str, port: Option<u16>) -> Option<ServerRow> {
        self.core.find_row(project, name, port)
    }

    /// D'où le journal de cette ligne se lit.
    pub fn journal_source(&self, row: &ServerRow) -> JournalSource {
        self.core.journal_source(row)
    }

    pub fn remembered_dev_ports(&self) -> HashMap<PathBuf, u16> {
        self.core.remembered_dev_ports()
    }

    pub fn restore_dev_ports(&mut self, ports: HashMap<PathBuf, u16>) {
        self.core.restore_dev_ports(ports);
    }

    fn scan_now(&self) {
        // Un démarrage vient d'ajouter la racine de sa session serveur : le relevé demandé la sonde déjà.
        self.push_probe_scope();
        if let Some(scanner) = &self.scanner {
            scanner.scan_now();
        }
    }

    /// Démarre une entrée de configuration. Refusé quand son port est en écoute (P4-10).
    pub fn start(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        if let Ok(job) = self.core.begin_start(key, StartMode::Gesture) {
            self.run_start_job(job, cx);
        }
    }

    fn run_start_job(&mut self, job: StartJob, cx: &mut Context<Self>) {
        let launcher = Arc::clone(&self.launcher);
        cx.spawn(async move |this, cx| {
            let work = job.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move { run_start(&work, launcher.as_ref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let visible = this.core.finish_start(&job, &outcome);
                this.scan_now();
                if visible {
                    this.changed(cx);
                }
            });
        })
        .detach();
    }

    /// Arrête une ligne en marche (spec §5.4). L'arrêt tourne hors du fil principal.
    pub fn stop(&mut self, kind: &RowKind, cx: &mut Context<Self>) {
        let job = match self.core.begin_stop(kind) {
            Ok(job) => job,
            Err(StopDecline::NoTarget) => {
                // La ligne vient de passer en « arrêt refusé ».
                self.changed(cx);
                return;
            }
            Err(_) => return,
        };
        let launcher = Arc::clone(&self.launcher);
        cx.spawn(async move |this, cx| {
            let work = job.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    run_stop(
                        &work,
                        launcher.as_ref(),
                        STOP_GRACE,
                        listener_of,
                        current_processes,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.core.finish_stop(&job, &outcome);
                // Freed, Killed, Replaced ou Refused : le relevé suivant dit ce qui tient le port.
                this.scan_now();
                this.changed(cx);
            });
        })
        .detach();
    }

    /// Relance une entrée de configuration : arrête, attend que le port se libère, puis démarre par IdemA. Une ligne
    /// arrêtée ou en échec démarre simplement. Les lignes repérées ne se relancent pas (P4-12).
    pub fn restart(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        let kind = RowKind::Config(key.clone());
        let Some(row) = self.core.row_of(&kind) else {
            return;
        };
        match row.state {
            RowState::Starting => {}
            RowState::Stopped | RowState::Failed => self.start(key, cx),
            RowState::Running { .. } | RowState::StopRefused { .. } => {
                let Ok(job) = self.core.begin_stop(&kind) else {
                    self.changed(cx);
                    return;
                };
                self.run_restart_job(job, cx);
            }
        }
    }

    fn run_restart_job(&mut self, job: StopJob, cx: &mut Context<Self>) {
        let launcher = Arc::clone(&self.launcher);
        cx.spawn(async move |this, cx| {
            let work = job.clone();
            let (outcome, step) = cx
                .background_executor()
                .spawn(async move {
                    run_restart_stop(
                        &work,
                        launcher.as_ref(),
                        STOP_GRACE,
                        FREE_WAIT,
                        FREE_POLL,
                        listener_of,
                        current_processes,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match this.core.finish_restart_stop(&job, &outcome, &step) {
                    Some(start) => this.run_start_job(start, cx),
                    None => this.scan_now(),
                }
                this.changed(cx);
            });
        })
        .detach();
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        // Le `Scanner` attend son fil au `Drop` (jusqu'à un `lsof` interrompu) : pas sur le fil principal.
        if let Some(scanner) = self.scanner.take() {
            std::thread::spawn(move || drop(scanner));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::servers::scan::Listener;
    use std::sync::Mutex;

    /// Un faux lanceur : il note ce qu'on lui demande et rend ce qu'on lui a préparé.
    #[derive(Default)]
    struct FakeLauncher {
        sessions: Mutex<Vec<ListedServerSession>>,
        created: Mutex<Vec<LaunchRequest>>,
        killed: Mutex<Vec<String>>,
        /// Les réponses successives de `listener_of` ; la dernière se répète.
        listeners: Mutex<Vec<Result<Option<i32>, String>>>,
        create_error: Mutex<Option<String>>,
        sessions_error: Mutex<bool>,
        kill_error: Mutex<bool>,
        /// Avec `kill_error` : la session a disparu entre le listage et la fermeture, le relistage ne la voit plus.
        kill_vanishes: Mutex<bool>,
    }

    impl FakeLauncher {
        fn with_sessions(sessions: Vec<ListedServerSession>) -> Self {
            let launcher = Self::default();
            *launcher.sessions.lock().unwrap() = sessions;
            launcher
        }

        fn listening(self, answers: Vec<Result<Option<i32>, String>>) -> Self {
            *self.listeners.lock().unwrap() = answers;
            self
        }
    }

    impl ServerLauncher for FakeLauncher {
        fn create(&self, request: &LaunchRequest) -> Result<String, String> {
            if let Some(error) = self.create_error.lock().unwrap().clone() {
                return Err(error);
            }
            self.created.lock().unwrap().push(request.clone());
            // Comme tmux : la session créée se liste aussitôt, avec le PID de son pane.
            self.sessions.lock().unwrap().push(ListedServerSession {
                name: "idema-srv-new".to_string(),
                project: request.project.clone(),
                entry: request.entry.clone(),
                pane: PaneId(9),
                pane_pid: 700,
                dead: false,
            });
            Ok("idema-srv-new".to_string())
        }

        fn kill(&self, session: &str) -> Result<(), String> {
            if *self.kill_error.lock().unwrap() {
                if *self.kill_vanishes.lock().unwrap() {
                    self.sessions
                        .lock()
                        .unwrap()
                        .retain(|listed| listed.name != session);
                }
                return Err("no such session".to_string());
            }
            self.killed.lock().unwrap().push(session.to_string());
            // Comme tmux : la session fermée ne se liste plus.
            self.sessions
                .lock()
                .unwrap()
                .retain(|listed| listed.name != session);
            Ok(())
        }

        fn sessions(&self) -> Result<Vec<ListedServerSession>, String> {
            if *self.sessions_error.lock().unwrap() {
                return Err("tmux unreachable".to_string());
            }
            Ok(self.sessions.lock().unwrap().clone())
        }

        fn listener_of(&self, _port: u16) -> Result<Option<i32>, String> {
            let mut answers = self.listeners.lock().unwrap();
            if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers.first().cloned().unwrap_or(Ok(None))
            }
        }
    }

    #[test]
    fn recent_processes_are_none_before_a_scan_and_once_it_is_too_old() {
        let base = Instant::now();
        let mut core = ServersCore::new();
        let limit = Duration::from_secs(10);
        assert!(core.recent_processes(base, limit).is_none());

        core.apply_scan(snapshot(base, &[(10, 1, "zsh")], &[]), base);

        let processes = core
            .recent_processes(base + Duration::from_secs(4), limit)
            .expect("a scan of 4 s ago is recent");
        assert_eq!(processes.get(&10).map(|p| p.command.as_str()), Some("zsh"));
        assert!(
            core.recent_processes(base + limit, limit).is_some(),
            "the limit itself is still recent"
        );
        assert!(
            core.recent_processes(base + limit + Duration::from_secs(1), limit)
                .is_none(),
            "a stalled scanner leaves no table to trust"
        );
    }

    fn temp_project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("idema-servers-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        dir
    }

    fn write_launch(project: &Path, text: &str) {
        std::fs::write(project.join(".claude/launch.json"), text).unwrap();
    }

    const WEB_ONLY: &str = r#"{"version":"0.0.1","configurations":[
        {"name":"web","runtimeExecutable":"python3","runtimeArgs":["-m","http.server","8811"],"port":8811}]}"#;

    fn key(project: &Path, name: &str) -> ServerKey {
        ServerKey {
            project: project.to_path_buf(),
            name: name.to_string(),
        }
    }

    fn snapshot(at: Instant, processes: &[(i32, i32, &str)], listeners: &[(i32, u16)]) -> Snapshot {
        Snapshot {
            processes: processes
                .iter()
                .map(|&(pid, ppid, command)| {
                    (
                        pid,
                        Process {
                            pid,
                            ppid,
                            pgid: pid,
                            command: command.to_string(),
                        },
                    )
                })
                .collect(),
            listeners: listeners
                .iter()
                .map(|&(pid, port)| Listener { pid, port })
                .collect(),
            cwd: HashMap::new(),
            stdout: HashMap::new(),
            http: listeners.iter().copied().collect(),
            taken_at: at,
        }
    }

    fn server_session(
        name: &str,
        project: &Path,
        entry: &str,
        pane_pid: i32,
        dead: bool,
    ) -> ListedServerSession {
        ListedServerSession {
            name: name.to_string(),
            project: project.to_path_buf(),
            entry: entry.to_string(),
            pane: PaneId(1),
            pane_pid,
            dead,
        }
    }

    fn observation(sessions: Vec<ListedServerSession>, taken_at: Instant) -> PaneObservation {
        let roots = build_pane_roots(&PaneInputs::default(), &[], &[], &sessions);
        PaneObservation {
            roots,
            launchers: Vec::new(),
            server_sessions: sessions,
            taken_at,
        }
    }

    #[test]
    fn the_probe_scope_holds_live_panes_projects_and_declared_ports() {
        let project = temp_project("scope");
        write_launch(&project, WEB_ONLY);
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        core.set_observation(observation(
            vec![
                server_session("idema-srv-1", &project, "web", 700, false),
                server_session("idema-srv-2", &project, "api", 701, true),
            ],
            Instant::now(),
        ));
        let scope = core.probe_scope();
        // Une racine morte n'attribue plus rien (son PID peut avoir été réutilisé) : elle n'ouvre pas la sonde.
        assert_eq!(scope.pane_pids, HashSet::from([700]));
        assert_eq!(scope.projects, [project]);
        assert_eq!(scope.declared_ports, HashSet::from([8811]));
    }

    fn state_of(core: &ServersCore, key: &ServerKey) -> RowState {
        core.row_of(&RowKind::Config(key.clone()))
            .expect("the row exists")
            .state
    }

    #[test]
    fn start_is_refused_when_the_port_listens() {
        let project = temp_project("busy");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);

        // Le registre voit déjà le port en écoute : le geste est refusé avant tout lancement.
        let listening = snapshot(
            base,
            &[(500, 1, "python3 -m http.server 8811")],
            &[(500, 8811)],
        );
        core.apply_scan(listening, base);
        assert!(matches!(
            state_of(&core, &web),
            RowState::Running { port: 8811, .. }
        ));
        assert_eq!(
            core.begin_start(&web, StartMode::Gesture).unwrap_err(),
            StartRefusal::PortBusy
        );

        // Une ligne arrêtée dont le port s'est ouvert depuis le dernier relevé : le contrôle en direct refuse.
        let mut quiet = ServersCore::new();
        quiet.set_projects(vec![project.clone()]);
        quiet.apply_scan(snapshot(base, &[], &[]), base);
        let job = quiet.begin_start(&web, StartMode::Gesture).unwrap();
        let launcher = FakeLauncher::default().listening(vec![Ok(Some(77))]);
        assert_eq!(
            run_start(&job, &launcher),
            StartOutcome::PortBusy { pid: 77 }
        );
        assert!(launcher.created.lock().unwrap().is_empty());
        assert!(launcher.killed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_launch_json_change_rereads_the_project() {
        let project = temp_project("reread");
        write_launch(&project, WEB_ONLY);
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        assert_eq!(core.groups(None)[0].rows.len(), 1);
        assert!(!core.poll_project_files(), "rien n'a changé");

        write_launch(
            &project,
            r#"{"configurations":[
                {"name":"web","runtimeExecutable":"python3","runtimeArgs":["-m","http.server","8811"],"port":8811},
                {"name":"api","runtimeExecutable":"python3","runtimeArgs":["-m","http.server","8812"],"port":8812}]}"#,
        );
        assert!(core.poll_project_files());
        let names: Vec<String> = core.groups(None)[0]
            .rows
            .iter()
            .map(|row| row.name.clone())
            .collect();
        assert_eq!(names, ["web", "api"]);

        // Le fichier devient illisible : la ligne d'erreur remplace les entrées, sans panique.
        write_launch(&project, "{ pas du json");
        assert!(core.poll_project_files());
        let group = &core.groups(None)[0];
        assert!(group.rows.is_empty());
        assert_eq!(group.errors.len(), 1);
    }

    #[test]
    fn a_new_project_is_read_once_and_an_unchanged_list_costs_nothing() {
        let project = temp_project("known");
        write_launch(&project, WEB_ONLY);
        let mut core = ServersCore::new();
        assert!(core.set_projects(vec![project.clone(), project.clone()]));
        assert!(!core.set_projects(vec![project.clone()]));
        assert_eq!(core.groups(None).len(), 1);
        assert!(core.set_projects(vec![]));
        assert!(core.groups(None).is_empty());
    }

    #[test]
    fn restart_waits_for_the_port_then_starts() {
        // L'attente : le port est tenu trois sondages, puis libre.
        let calls = Mutex::new(0);
        let free = wait_until_free(
            8811,
            Duration::from_secs(5),
            Duration::from_millis(1),
            |_| {
                let mut calls = calls.lock().unwrap();
                *calls += 1;
                Ok((*calls <= 3).then_some(42))
            },
        );
        assert!(free);
        assert_eq!(*calls.lock().unwrap(), 4);
        // Un port qui ne se libère pas : l'attente rend la main à l'échéance, la relance n'a pas lieu.
        assert!(!wait_until_free(
            8811,
            Duration::from_millis(30),
            Duration::from_millis(5),
            |_| Ok(Some(42))
        ));
        // Un relevé qui échoue ne vaut pas « libre ».
        assert!(!wait_until_free(
            8811,
            Duration::from_millis(30),
            Duration::from_millis(5),
            |_| Err("lsof absent".to_string())
        ));

        // Le démarrage qui suit : la ligne montre encore l'ancien processus, seule la relance passe.
        let project = temp_project("restart");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        core.apply_scan(
            snapshot(
                base,
                &[(500, 1, "python3 -m http.server 8811")],
                &[(500, 8811)],
            ),
            base,
        );
        assert_eq!(
            core.begin_start(&web, StartMode::Gesture).unwrap_err(),
            StartRefusal::PortBusy
        );
        let job = core.begin_start(&web, StartMode::Restart).unwrap();
        let launcher = FakeLauncher::default().listening(vec![Ok(None)]);
        let outcome = run_start(&job, &launcher);
        assert!(matches!(outcome, StartOutcome::Started { .. }));
        assert_eq!(launcher.created.lock().unwrap().len(), 1);
        assert!(core.finish_start(&job, &outcome));
        assert_eq!(state_of(&core, &web), RowState::Starting);
    }

    #[test]
    fn a_restart_does_not_start_while_the_new_listener_holds_the_port() {
        let project = temp_project("restart-busy");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let mut core = ServersCore::new();
        core.set_projects(vec![project]);
        let job = core.begin_start(&web, StartMode::Restart).unwrap();
        let launcher = FakeLauncher::default().listening(vec![Ok(Some(901))]);
        assert_eq!(
            run_start(&job, &launcher),
            StartOutcome::PortBusy { pid: 901 }
        );
        assert!(launcher.created.lock().unwrap().is_empty());
    }

    #[test]
    fn servers_of_a_previous_instance_are_reattached() {
        let project = temp_project("reattach");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        // La session serveur d'une instance précédente vit encore dans le tmux d'IdemA.
        core.set_observation(observation(
            vec![server_session("idema-srv-old", &project, "web", 500, false)],
            base,
        ));
        core.apply_scan(
            snapshot(
                base + Duration::from_secs(1),
                &[(500, 1, "python3 -m http.server 8811")],
                &[(500, 8811)],
            ),
            base + Duration::from_secs(1),
        );
        let row = core.row_of(&RowKind::Config(web.clone())).unwrap();
        assert_eq!(
            row.state,
            RowState::Running {
                pid: 500,
                port: 8811
            }
        );
        assert_eq!(row.origin, Some(Origin::Idema));

        // L'arrêt vise la session serveur, qui se ferme avec le serveur.
        let job = core.begin_stop(&RowKind::Config(web)).unwrap();
        assert_eq!(job.sessions_to_close, ["idema-srv-old"]);
        assert_eq!(job.listening.map(|listening| listening.pid), Some(500));
        assert_eq!(job.listening.map(|listening| listening.port), Some(8811));
    }

    #[test]
    fn a_stopped_server_closes_its_session_and_a_refused_one_keeps_it() {
        let project = temp_project("close");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        core.set_observation(observation(
            vec![server_session("idema-srv-old", &project, "web", 500, false)],
            base,
        ));
        core.apply_scan(
            snapshot(
                base,
                &[(500, 1, "python3 -m http.server 8811")],
                &[(500, 8811)],
            ),
            base,
        );
        let job = core.begin_stop(&RowKind::Config(web.clone())).unwrap();
        // Une seconde demande d'arrêt pendant le premier est ignorée.
        assert_eq!(
            core.begin_stop(&RowKind::Config(web)).unwrap_err(),
            StopDecline::InProgress
        );

        // Un relevé qui échoue avant tout signal refuse l'arrêt : la session reste.
        let launcher = FakeLauncher::default();
        let refused = run_stop(
            &job,
            &launcher,
            Duration::from_millis(10),
            |_| Err("lsof absent".to_string()),
            || None,
        );
        assert!(matches!(refused, StopOutcome::Refused { .. }));
        assert!(launcher.killed.lock().unwrap().is_empty());

        // Le port est déjà libre : rien à signaler, la session se ferme.
        let freed = run_stop(
            &job,
            &launcher,
            Duration::from_millis(10),
            |_| Ok(None),
            || None,
        );
        assert_eq!(freed, StopOutcome::Freed);
        assert_eq!(*launcher.killed.lock().unwrap(), ["idema-srv-old"]);
    }

    #[test]
    fn a_replaced_stop_rescans_without_marking_the_row_refused() {
        let project = temp_project("replaced");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let kind = RowKind::Config(web.clone());
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project]);
        core.apply_scan(
            snapshot(
                base,
                &[(500, 1, "python3 -m http.server 8811")],
                &[(500, 8811)],
            ),
            base,
        );

        let job = core.begin_stop(&kind).unwrap();
        core.finish_stop(&job, &StopOutcome::Replaced { by: 900 });
        assert_eq!(
            state_of(&core, &web),
            RowState::Running {
                pid: 500,
                port: 8811
            }
        );

        let job = core.begin_stop(&kind).unwrap();
        core.finish_stop(&job, &StopOutcome::Refused { pids: vec![500] });
        assert_eq!(
            state_of(&core, &web),
            RowState::StopRefused {
                pid: 500,
                port: 8811
            }
        );
    }

    #[test]
    fn a_start_replaces_the_previous_session_and_ignores_older_scans() {
        let project = temp_project("guard");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let base = Instant::now();
        let at = |seconds: u64| base + Duration::from_secs(seconds);
        let old = server_session("idema-srv-old", &project, "web", 0, true);

        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        // La session précédente est morte : la ligne est en échec.
        core.set_observation(observation(vec![old.clone()], at(1)));
        core.registry.mark_starting(&web, at(0));
        core.apply_scan(snapshot(at(1), &[], &[]), at(1));
        assert_eq!(state_of(&core, &web), RowState::Failed);

        // Le geste ferme l'ancienne session avant de créer la nouvelle.
        let job = core.begin_start(&web, StartMode::Gesture).unwrap();
        let launcher = FakeLauncher::with_sessions(vec![old.clone()]);
        let outcome = run_start(&job, &launcher);
        assert!(matches!(outcome, StartOutcome::Started { .. }));
        assert_eq!(*launcher.killed.lock().unwrap(), ["idema-srv-old"]);
        assert_eq!(launcher.created.lock().unwrap().len(), 1);
        // L'instant de démarrage est posé à la main pour que le test ne dépende pas de l'horloge.
        let started = StartOutcome::Started {
            session: "idema-srv-new".to_string(),
            at: at(10),
            listed: None,
        };
        core.finish_start(&job, &started);
        assert_eq!(state_of(&core, &web), RowState::Starting);

        // Un relevé pris avant le démarrage est écarté.
        assert!(!core.apply_scan(snapshot(at(9), &[], &[]), at(11)));
        assert_eq!(state_of(&core, &web), RowState::Starting);
        // Un relevé récent avec les racines d'avant (la vieille session morte) ne fait pas échouer la ligne neuve.
        core.apply_scan(snapshot(at(11), &[], &[]), at(11));
        assert_eq!(state_of(&core, &web), RowState::Starting);
        // Les racines suivantes décrivent la nouvelle session, vivante : la ligne attend toujours son port.
        core.set_observation(observation(
            vec![server_session("idema-srv-new", &project, "web", 700, false)],
            at(12),
        ));
        core.apply_scan(snapshot(at(12), &[], &[]), at(12));
        assert_eq!(state_of(&core, &web), RowState::Starting);
        // La nouvelle session meurt aussitôt : preuve positive, la ligne échoue.
        core.set_observation(observation(
            vec![server_session("idema-srv-new", &project, "web", 0, true)],
            at(13),
        ));
        core.apply_scan(snapshot(at(13), &[], &[]), at(13));
        assert_eq!(state_of(&core, &web), RowState::Failed);
    }

    #[test]
    fn a_failed_creation_marks_the_row_failed() {
        let project = temp_project("create-failed");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let mut core = ServersCore::new();
        core.set_projects(vec![project]);
        let job = core.begin_start(&web, StartMode::Gesture).unwrap();
        let launcher = FakeLauncher::default();
        *launcher.create_error.lock().unwrap() = Some("no such directory".to_string());
        let outcome = run_start(&job, &launcher);
        assert!(matches!(outcome, StartOutcome::Failed { .. }));
        assert!(core.finish_start(&job, &outcome));
        assert_eq!(state_of(&core, &web), RowState::Failed);
        // Le démarrage terminé, un autre peut suivre.
        assert!(core.begin_start(&web, StartMode::Gesture).is_ok());
        // Mais pas deux à la fois.
        assert_eq!(
            core.begin_start(&web, StartMode::Gesture).unwrap_err(),
            StartRefusal::InProgress
        );
    }

    #[test]
    fn a_failed_creation_shows_its_cause_on_the_row() {
        let project = temp_project("create-cause");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        let kind = RowKind::Config(web.clone());
        let launcher = FakeLauncher::default();

        // Le dossier de l'entrée a disparu : la cause se dit en français, sur la ligne.
        let job = core.begin_start(&web, StartMode::Gesture).unwrap();
        *launcher.create_error.lock().unwrap() =
            Some("no such directory: /work/disparu".to_string());
        let outcome = run_start(&job, &launcher);
        core.finish_start(&job, &outcome);
        let row = core.row_of(&kind).unwrap();
        assert_eq!(row.state, RowState::Failed);
        assert_eq!(
            row.failure.as_deref(),
            Some("dossier introuvable : /work/disparu")
        );

        // Un message de tmux passe tel quel, sur sa première ligne.
        let job = core.begin_start(&web, StartMode::Gesture).unwrap();
        *launcher.create_error.lock().unwrap() =
            Some("tmux new-session : duplicate session\nsecond line".to_string());
        let outcome = run_start(&job, &launcher);
        core.finish_start(&job, &outcome);
        assert_eq!(
            core.row_of(&kind).unwrap().failure.as_deref(),
            Some("tmux new-session : duplicate session")
        );
    }

    /// Une entrée `web` (port déclaré 8811) dont la session serveur vit et dont le serveur écoute ailleurs, sur 5173.
    fn web_listening_elsewhere(name: &str) -> (ServersCore, ServerKey, PathBuf, Instant) {
        let project = temp_project(name);
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        core.set_observation(observation(
            vec![server_session("idema-srv-1", &project, "web", 700, false)],
            base,
        ));
        core.apply_scan(
            snapshot(
                base + Duration::from_secs(1),
                &[
                    (700, 1, "node vite"),
                    (701, 700, "node worker"),
                    // Un arbre d'agent à côté de celui du serveur : l'arrêt n'y touche jamais.
                    (600, 1, "zsh"),
                    (610, 600, "bun idnma run codex"),
                    (620, 610, "codex"),
                ],
                &[(700, 5173)],
            ),
            base + Duration::from_secs(1),
        );
        (core, web, project, base)
    }

    #[test]
    fn a_row_whose_session_lives_without_its_port_stops_through_the_session() {
        let (mut core, web, project, base) = web_listening_elsewhere("session-stop");
        let kind = RowKind::Config(web.clone());
        let row = core.row_of(&kind).unwrap();
        assert_eq!(row.state, RowState::Starting);
        assert!(row.session_alive);
        assert_eq!(core.live_sessions_of(&project), 1);

        // L'arrêt vise tout le sous-arbre du pane de la session, sans port ni processus en écoute attendus.
        let job = core.begin_stop(&kind).unwrap();
        assert_eq!(job.listening, None);
        assert_eq!(job.plan.pids, [700, 701]);
        assert_eq!(job.plan.group, Some(700));
        assert!(
            job.plan.commands.keys().all(|pid| [700, 701].contains(pid)),
            "the plan names a process outside the server tree"
        );
        assert_eq!(job.sessions_to_close, ["idema-srv-1"]);
        // Un second geste pendant le premier est ignoré.
        assert_eq!(core.begin_stop(&kind).unwrap_err(), StopDecline::InProgress);

        // L'arrêt réussi ferme la session : la ligne est arrêtée, et un listage plus ancien que la fermeture ne la fait
        // pas revivre.
        core.finish_stop(&job, &StopOutcome::Killed);
        assert_eq!(core.live_sessions_of(&project), 0);
        let still_listed = observation(
            vec![server_session("idema-srv-1", &project, "web", 700, false)],
            base,
        );
        core.set_observation(still_listed);
        core.apply_scan(
            snapshot(
                base + Duration::from_secs(2),
                &[(700, 1, "node vite")],
                &[(700, 5173)],
            ),
            base + Duration::from_secs(2),
        );
        let row = core.row_of(&kind).unwrap();
        assert_eq!(row.state, RowState::Stopped);
        assert!(!row.session_alive);
        assert_eq!(core.live_sessions_of(&project), 0);
    }

    #[test]
    fn a_refused_session_stop_keeps_the_session_and_its_row() {
        let (mut core, web, project, _base) = web_listening_elsewhere("session-refused");
        let kind = RowKind::Config(web);
        let job = core.begin_stop(&kind).unwrap();
        core.finish_stop(&job, &StopOutcome::Refused { pids: vec![700] });
        assert_eq!(core.live_sessions_of(&project), 1);
        assert_eq!(core.row_of(&kind).unwrap().state, RowState::Starting);
        // Le geste peut être refait.
        assert!(core.begin_stop(&kind).is_ok());
    }

    #[test]
    fn a_session_stop_with_an_empty_plan_succeeds_only_once_the_session_is_gone() {
        let (mut core, web, project, _base) = web_listening_elsewhere("session-empty");
        let kind = RowKind::Config(web);
        let empty = StopPlan {
            pids: Vec::new(),
            group: None,
            commands: HashMap::new(),
        };
        let listed = || vec![server_session("idema-srv-1", &project, "web", 700, false)];

        // Le processus est déjà parti mais tmux ne ferme pas la session : rien n'a été arrêté, la ligne le dit.
        let mut job = core.begin_stop(&kind).unwrap();
        job.plan = empty.clone();
        let launcher = FakeLauncher::with_sessions(listed());
        *launcher.kill_error.lock().unwrap() = true;
        let outcome = run_stop(
            &job,
            &launcher,
            Duration::from_millis(10),
            |_| Ok(None),
            || None,
        );
        assert_eq!(outcome, StopOutcome::Refused { pids: Vec::new() });
        core.finish_stop(&job, &outcome);
        assert_eq!(core.live_sessions_of(&project), 1);
        assert_eq!(core.row_of(&kind).unwrap().state, RowState::Starting);

        // Une fermeture en erreur parce que la session a disparu entre-temps est un arrêt réussi.
        let mut job = core.begin_stop(&kind).unwrap();
        job.plan = empty.clone();
        let launcher = FakeLauncher::with_sessions(listed());
        *launcher.kill_error.lock().unwrap() = true;
        *launcher.kill_vanishes.lock().unwrap() = true;
        let outcome = run_stop(
            &job,
            &launcher,
            Duration::from_millis(10),
            |_| Ok(None),
            || None,
        );
        assert_eq!(outcome, StopOutcome::Freed);

        // Une fermeture réussie, session relistée plus : arrêt réussi.
        let launcher = FakeLauncher::with_sessions(listed());
        let outcome = run_stop(
            &job,
            &launcher,
            Duration::from_millis(10),
            |_| Ok(None),
            || None,
        );
        assert_eq!(outcome, StopOutcome::Freed);
        assert_eq!(*launcher.killed.lock().unwrap(), ["idema-srv-1"]);
    }

    #[test]
    fn a_stopped_row_without_a_live_session_has_nothing_to_stop() {
        let project = temp_project("nothing-to-stop");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        core.apply_scan(snapshot(base, &[], &[]), base);
        let kind = RowKind::Config(web);
        assert_eq!(core.begin_stop(&kind).unwrap_err(), StopDecline::NotRunning);

        // Une session morte (démarrage en échec) n'est pas à arrêter non plus.
        core.set_observation(observation(
            vec![server_session("idema-srv-1", &project, "web", 0, true)],
            base,
        ));
        core.apply_scan(
            snapshot(base + Duration::from_secs(1), &[], &[]),
            base + Duration::from_secs(1),
        );
        assert_eq!(core.begin_stop(&kind).unwrap_err(), StopDecline::NotRunning);
        assert_eq!(core.live_sessions_of(&project), 0);
    }

    #[test]
    fn a_project_counts_only_its_own_live_server_sessions() {
        let project = temp_project("count-mine");
        let other = temp_project("count-other");
        write_launch(&project, WEB_ONLY);
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone(), other.clone()]);
        core.set_observation(observation(
            vec![
                server_session("idema-srv-1", &project, "web", 700, false),
                server_session("idema-srv-2", &project, "api", 710, false),
                server_session("idema-srv-3", &project, "old", 0, true),
                server_session("idema-srv-4", &other, "web", 720, false),
            ],
            base,
        ));
        assert_eq!(core.live_sessions_of(&project), 2);
        assert_eq!(core.live_sessions_of(&other), 1);
        assert_eq!(core.live_sessions_of(Path::new("/nowhere")), 0);
    }

    #[test]
    fn a_start_passes_the_port_the_entry_env_and_its_working_directory() {
        let project = temp_project("env");
        write_launch(
            &project,
            r#"{"configurations":[
                {"name":"web","runtimeExecutable":"bun","runtimeArgs":["run","dev"],"port":8811,
                 "cwd":"${workspaceFolder}/site","env":{"MODE":"test"}},
                {"name":"own","runtimeExecutable":"bun","port":8812,"env":{"PORT":"9000"}}]}"#,
        );
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        let job = core
            .begin_start(&key(&project, "web"), StartMode::Gesture)
            .unwrap();
        assert_eq!(
            job.request.env,
            [
                ("PORT".to_string(), "8811".to_string()),
                ("MODE".to_string(), "test".to_string())
            ]
        );
        assert_eq!(job.request.cwd, project.join("site"));
        assert_eq!(job.request.program, "bun");
        assert_eq!(job.request.args, ["run", "dev"]);
        assert_eq!(job.port, Some(8811));

        // Un `PORT` de l'entrée passe avant celui d'IdemA.
        let own = core
            .begin_start(&key(&project, "own"), StartMode::Gesture)
            .unwrap();
        assert_eq!(own.request.env, [("PORT".to_string(), "9000".to_string())]);
    }

    #[test]
    fn an_entry_without_a_port_gets_no_port_variable() {
        let project = temp_project("dev");
        std::fs::write(
            project.join("package.json"),
            r#"{"scripts":{"dev":"next dev"}}"#,
        )
        .unwrap();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        let job = core
            .begin_start(&key(&project, "dev"), StartMode::Gesture)
            .unwrap();
        assert!(job.request.env.is_empty());
        assert_eq!(job.port, None);
    }

    #[test]
    fn an_unknown_entry_does_not_start() {
        let project = temp_project("unknown");
        write_launch(&project, WEB_ONLY);
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        assert_eq!(
            core.begin_start(&key(&project, "nope"), StartMode::Gesture)
                .unwrap_err(),
            StartRefusal::UnknownEntry
        );
    }

    #[test]
    fn a_page_address_finds_the_entry_that_serves_it() {
        let project = temp_project("url");
        write_launch(&project, WEB_ONLY);
        let mut core = ServersCore::new();
        core.set_projects(vec![project]);
        for url in [
            "http://localhost:8811",
            "http://localhost:8811/#top",
            "http://127.0.0.1:8811/page",
        ] {
            assert_eq!(
                core.row_for_url(url).map(|row| row.name),
                Some("web".into())
            );
        }
        assert!(core.row_for_url("http://localhost:8899/").is_none());
        assert!(core.row_for_url("https://example.org/").is_none());
        assert!(core.row_for_url("pas une adresse").is_none());
    }

    #[test]
    fn pane_roots_follow_sessions_agents_server_sessions_and_the_terminal() {
        let inputs = PaneInputs {
            sessions: vec![
                SessionInfo {
                    name: "idema-a".into(),
                    project: PathBuf::from("/p/alpha"),
                    display_name: "Claude".into(),
                },
                SessionInfo {
                    name: "idema-b".into(),
                    project: PathBuf::from("/p/alpha"),
                    display_name: "Shell".into(),
                },
            ],
            agents: vec![AgentEntry {
                name: "Claude".into(),
                harness: None,
                command: "idnma run claude".into(),
                default: true,
                icon: None,
            }],
            terminal_shells: vec![(PathBuf::from("/p/alpha"), 321)],
        };
        let pane_pids = vec![
            ("idema-a".to_string(), PaneId(1), 100),
            ("idema-b".to_string(), PaneId(2), 200),
            // Une session que l'espace de travail ne connaît pas encore, et un pane de session serveur : écartés ici.
            ("idema-c".to_string(), PaneId(3), 300),
            ("idema-srv-1".to_string(), PaneId(4), 400),
        ];
        let panes = vec![
            ListedPane {
                session: "idema-a".into(),
                pane: PaneId(1),
                active: true,
                agent: "Claude".into(),
                idnma_agent: String::new(),
            },
            ListedPane {
                session: "idema-b".into(),
                pane: PaneId(2),
                active: true,
                agent: "Shell".into(),
                idnma_agent: String::new(),
            },
        ];
        let project = PathBuf::from("/p/alpha");
        let server_sessions = vec![
            server_session("idema-srv-1", &project, "web", 400, false),
            server_session("idema-srv-2", &project, "api", 0, true),
        ];
        let roots = build_pane_roots(&inputs, &pane_pids, &panes, &server_sessions);

        let session = |pid: i32| {
            roots
                .panes
                .iter()
                .find(|root| root.pid == pid && matches!(root.owner, PaneOwner::Session { .. }))
        };
        let claude = session(100).expect("the agent session");
        assert!(!claude.dead);
        assert_eq!(claude.agent_command.as_deref(), Some("idnma run claude"));
        let shell = session(200).expect("the bare shell session");
        assert_eq!(shell.agent_command, None);
        assert!(session(300).is_none());

        let servers: Vec<(&ServerKey, i32, bool)> = roots
            .panes
            .iter()
            .filter_map(|root| match &root.owner {
                PaneOwner::ServerSession { key } => Some((key, root.pid, root.dead)),
                _ => None,
            })
            .collect();
        assert_eq!(
            servers,
            [
                (&key(&project, "web"), 400, false),
                (&key(&project, "api"), 0, true)
            ]
        );
        assert!(roots.panes.iter().any(|root| root.pid == 321
            && root.owner
                == PaneOwner::TerminalPanel {
                    project: project.clone()
                }));
    }

    #[test]
    fn a_homonym_under_another_harness_keeps_its_own_command() {
        let entry = |name: &str, command: &str| AgentEntry {
            name: name.into(),
            harness: None,
            command: command.into(),
            default: false,
            icon: None,
        };
        let agents = [
            entry("Deepseek", "idnma run deepseek"),
            entry("Qwen", "idnma run opencode --engine qwen"),
            entry("Deepseek", "idnma run opencode --engine deepseek"),
        ];
        // `launched`, trouvé par le nom, est la première entrée de ce nom, sous un autre harness que celle qui tourne.
        let found = idnma_entry("opencode", Some(&agents[0]), &agents);
        assert_eq!(
            found.map(|entry| entry.command.as_str()),
            Some("idnma run opencode --engine deepseek")
        );
    }

    #[test]
    fn the_last_idnma_run_of_a_pane_names_its_agent_while_an_agent_runs() {
        let entry = |name: &str, command: &str| AgentEntry {
            name: name.into(),
            harness: None,
            command: command.into(),
            default: false,
            icon: None,
        };
        let inputs = PaneInputs {
            sessions: ["idema-a", "idema-b", "idema-c"]
                .map(|name| SessionInfo {
                    name: name.into(),
                    project: PathBuf::from("/p/alpha"),
                    display_name: name.into(),
                })
                .to_vec(),
            agents: vec![
                entry("GLM", "idnma run glm"),
                entry("GLM vif", "idnma run glm --effort high"),
                entry("Kimi", "~/.bun/bin/idnma run kimi"),
            ],
            terminal_shells: Vec::new(),
        };
        let pane_pids = vec![
            ("idema-a".to_string(), PaneId(1), 100),
            ("idema-b".to_string(), PaneId(2), 200),
            ("idema-c".to_string(), PaneId(3), 300),
        ];
        let listed = |session: &str, pane: u32, agent: &str, idnma_agent: &str| ListedPane {
            session: session.into(),
            pane: PaneId(pane),
            active: true,
            agent: agent.into(),
            idnma_agent: idnma_agent.into(),
        };
        let panes = vec![
            // Un shell seul où l'utilisateur a tapé `idnma run kimi` à la main.
            listed("idema-a", 1, SHELL_ENTRY, "kimi"),
            // Lancé « GLM vif » : idnma ne pose que la clé, l'entrée du lanceur reste la plus précise.
            listed("idema-b", 2, "GLM vif", "glm"),
            // Kimi est sorti : l'option reste posée sur le pane, seul le shell tourne.
            listed("idema-c", 3, "Kimi", "kimi"),
        ];
        let roots = build_pane_roots(&inputs, &pane_pids, &panes, &[]);
        let processes: HashMap<i32, Process> = [
            (100, 1, "-zsh"),
            (
                110,
                100,
                "claude --model kimi-k3:cloud[1m] --name kimi-junon-x9z1",
            ),
            (200, 1, "-zsh"),
            (
                210,
                200,
                "claude --model glm-5.3-flash:cloud[1m] --effort high --name glm-hera-ab12",
            ),
            (300, 1, "-zsh"),
        ]
        .into_iter()
        .map(|(pid, ppid, command)| {
            let process = Process {
                pid,
                ppid,
                pgid: pid,
                command: command.into(),
            };
            (pid, process)
        })
        .collect();
        let agents = session_agents(&roots, &processes, &inputs.agents);
        assert_eq!(agents["idema-a"], ["Kimi"]);
        assert_eq!(agents["idema-b"], ["GLM vif"]);
        assert!(!agents.contains_key("idema-c"));
    }

    #[test]
    fn the_real_launcher_replaces_the_session_of_an_entry_and_the_observation_sees_it() {
        let dir = std::env::temp_dir().join(format!("idema-t6-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let project = dir.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let socket = dir.join("sock").to_string_lossy().into_owned();
        let mut inherited: Vec<(String, String)> = std::env::vars()
            .filter(|(key, _)| key != "SHELL" && key != "ZDOTDIR")
            .collect();
        inherited.push(("SHELL".into(), "/bin/sh".into()));
        let boot = crate::bootstrap::bootstrap(&dir, &socket, None, inherited, None)
            .expect("tmux bootstraps on a fresh socket");
        let server = boot.server;

        let web = key(&project, "web");
        let job = StartJob {
            key: web.clone(),
            request: LaunchRequest {
                project: project.clone(),
                entry: "web".into(),
                cwd: project.clone(),
                program: "sleep".into(),
                args: vec!["30".into()],
                env: vec![("PORT".into(), "48123".into())],
            },
            port: Some(48123),
        };
        let StartOutcome::Started { session: first, .. } = run_start(&job, server.as_ref()) else {
            panic!("the first start creates a session");
        };
        let StartOutcome::Started {
            session: second, ..
        } = run_start(&job, server.as_ref())
        else {
            panic!("the second start creates a session");
        };
        assert_ne!(first, second);

        // Une seule session vivante pour l'entrée : la première a été fermée avant la seconde.
        let observed = observe_panes(&server, &PaneInputs::default()).expect("tmux answers");
        let names: Vec<&str> = observed
            .server_sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect();
        assert_eq!(names, [second.as_str()]);
        assert!(observed.server_sessions[0].pane_pid > 0);
        assert!(observed.roots.panes.iter().any(|root| {
            !root.dead && root.owner == PaneOwner::ServerSession { key: web.clone() }
        }));

        // Fermer la session la retire des observations.
        ServerLauncher::kill(server.as_ref(), &second).unwrap();
        let observed = observe_panes(&server, &PaneInputs::default()).expect("tmux answers");
        assert!(observed.server_sessions.is_empty());
        let _ = server.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Un moteur dont la ligne `web` tourne (pid 500, port 8811), et l'arrêt qui la vise, sans PID à signaler : un test
    /// ne signale jamais un vrai processus.
    fn running_web(name: &str) -> (ServersCore, ServerKey, StopJob) {
        let project = temp_project(name);
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project]);
        core.apply_scan(
            snapshot(
                base,
                &[(500, 1, "python3 -m http.server 8811")],
                &[(500, 8811)],
            ),
            base,
        );
        let mut job = core.begin_stop(&RowKind::Config(web.clone())).unwrap();
        job.plan.pids.clear();
        job.plan.group = None;
        (core, web, job)
    }

    /// Les réponses successives de `listener_of` ; la dernière se répète.
    fn scripted(
        answers: Vec<Result<Option<i32>, String>>,
    ) -> impl Fn(u16) -> Result<Option<i32>, String> {
        let queue = Mutex::new(answers);
        move |_| {
            let mut queue = queue.lock().unwrap();
            if queue.len() > 1 {
                queue.remove(0)
            } else {
                queue.first().cloned().unwrap_or(Ok(None))
            }
        }
    }

    const GRACE: Duration = Duration::from_millis(10);
    const WAIT: Duration = Duration::from_millis(30);
    const POLL: Duration = Duration::from_millis(5);

    #[test]
    fn a_restart_stops_without_starting_when_another_process_takes_the_port() {
        let (mut core, web, job) = running_web("restart-replaced");
        let launcher = FakeLauncher::default();
        let (outcome, step) = run_restart_stop(
            &job,
            &launcher,
            GRACE,
            WAIT,
            POLL,
            scripted(vec![Ok(Some(500)), Ok(Some(900))]),
            || None,
        );
        assert_eq!(outcome, StopOutcome::Replaced { by: 900 });
        assert_eq!(step, RestartStep::Abort { reason: "replaced" });
        assert!(core.finish_restart_stop(&job, &outcome, &step).is_none());
        assert!(matches!(state_of(&core, &web), RowState::Running { .. }));
        assert!(launcher.created.lock().unwrap().is_empty());
    }

    #[test]
    fn a_refused_restart_marks_the_row_and_starts_nothing() {
        let (mut core, web, job) = running_web("restart-refused");
        let launcher = FakeLauncher::default();
        // Le processus qui écoute n'est plus celui du geste : rien n'est signalé, l'arrêt est refusé.
        let (outcome, step) = run_restart_stop(
            &job,
            &launcher,
            GRACE,
            WAIT,
            POLL,
            scripted(vec![Ok(Some(901))]),
            || None,
        );
        assert!(matches!(outcome, StopOutcome::Refused { .. }));
        assert_eq!(step, RestartStep::Abort { reason: "refused" });
        assert!(core.finish_restart_stop(&job, &outcome, &step).is_none());
        assert!(matches!(
            state_of(&core, &web),
            RowState::StopRefused { .. }
        ));
        assert!(launcher.created.lock().unwrap().is_empty());
    }

    #[test]
    fn a_restart_does_not_start_while_the_freed_port_is_held_again() {
        let (mut core, _, job) = running_web("restart-held");
        let launcher = FakeLauncher::default();
        let (outcome, step) = run_restart_stop(
            &job,
            &launcher,
            GRACE,
            WAIT,
            POLL,
            scripted(vec![Ok(Some(500)), Ok(None), Ok(Some(77))]),
            || None,
        );
        assert_eq!(outcome, StopOutcome::Freed);
        assert_eq!(
            step,
            RestartStep::Abort {
                reason: "port-busy"
            }
        );
        assert!(core.finish_restart_stop(&job, &outcome, &step).is_none());
        assert!(launcher.created.lock().unwrap().is_empty());
    }

    #[test]
    fn a_freed_port_lets_the_restart_start_the_entry() {
        let (mut core, web, job) = running_web("restart-start");
        let launcher = FakeLauncher::default();
        let (outcome, step) = run_restart_stop(
            &job,
            &launcher,
            GRACE,
            WAIT,
            POLL,
            scripted(vec![Ok(Some(500)), Ok(None)]),
            || None,
        );
        assert_eq!(outcome, StopOutcome::Freed);
        assert_eq!(step, RestartStep::Start);
        // La ligne montre encore l'ancien processus : la relance démarre quand même, le geste ne l'aurait pas pu.
        assert!(matches!(state_of(&core, &web), RowState::Running { .. }));
        let start = core
            .finish_restart_stop(&job, &outcome, &step)
            .expect("the restart goes on");
        let started = run_start(&start, &launcher);
        assert!(matches!(started, StartOutcome::Started { .. }));
        assert_eq!(launcher.created.lock().unwrap().len(), 1);
        core.finish_start(&start, &started);
        assert_eq!(state_of(&core, &web), RowState::Starting);
    }

    #[test]
    fn a_start_click_cannot_race_a_restart_or_a_starting_row() {
        let (mut core, web, job) = running_web("race");
        // Pendant l'arrêt d'une relance, le geste « Démarrer » est refusé même si le relevé montre la ligne arrêtée.
        let base = Instant::now();
        core.apply_scan(snapshot(base + Duration::from_secs(1), &[], &[]), base);
        assert_eq!(state_of(&core, &web), RowState::Stopped);
        assert_eq!(
            core.begin_start(&web, StartMode::Gesture).unwrap_err(),
            StartRefusal::InProgress
        );
        // La relance elle-même peut démarrer une fois l'arrêt rendu.
        core.finish_stop(&job, &StopOutcome::Freed);
        let start = core.begin_start(&web, StartMode::Restart).unwrap();
        core.finish_start(
            &start,
            &StartOutcome::Started {
                session: "idema-srv-new".into(),
                at: base + Duration::from_secs(2),
                listed: None,
            },
        );
        // Une ligne en démarrage refuse les deux modes.
        for mode in [StartMode::Gesture, StartMode::Restart] {
            assert_eq!(
                core.begin_start(&web, mode).unwrap_err(),
                StartRefusal::InProgress
            );
        }
    }

    #[test]
    fn a_start_fails_rather_than_leave_the_previous_session_open() {
        let project = temp_project("keep-old");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        let job = core.begin_start(&web, StartMode::Gesture).unwrap();

        let old = server_session("idema-srv-old", &project, "web", 0, true);
        let launcher = FakeLauncher::with_sessions(vec![old.clone()]);
        *launcher.kill_error.lock().unwrap() = true;
        assert!(matches!(
            run_start(&job, &launcher),
            StartOutcome::Failed { .. }
        ));
        assert!(launcher.created.lock().unwrap().is_empty());

        // La fermeture échoue parce que la session a disparu entre le listage et elle : c'est fermé, on démarre.
        let launcher = FakeLauncher::with_sessions(vec![old.clone()]);
        *launcher.kill_error.lock().unwrap() = true;
        *launcher.kill_vanishes.lock().unwrap() = true;
        assert!(matches!(
            run_start(&job, &launcher),
            StartOutcome::Started { .. }
        ));
        assert_eq!(launcher.created.lock().unwrap().len(), 1);

        let launcher = FakeLauncher::with_sessions(vec![old]);
        *launcher.sessions_error.lock().unwrap() = true;
        assert!(matches!(
            run_start(&job, &launcher),
            StartOutcome::Failed { .. }
        ));
        assert!(launcher.created.lock().unwrap().is_empty());
    }

    #[test]
    fn a_server_started_a_moment_ago_can_be_stopped_before_the_next_observation() {
        let project = temp_project("fresh");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let base = Instant::now();
        let at = |seconds: u64| base + Duration::from_secs(seconds);
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        core.set_observation(observation(vec![], at(0)));

        let job = core.begin_start(&web, StartMode::Gesture).unwrap();
        let launcher = FakeLauncher::default();
        let StartOutcome::Started {
            session, listed, ..
        } = run_start(&job, &launcher)
        else {
            panic!("the start creates a session");
        };
        assert_eq!(listed.as_ref().map(|listed| listed.pane_pid), Some(700));
        core.finish_start(
            &job,
            &StartOutcome::Started {
                session,
                at: at(10),
                listed,
            },
        );

        // Le serveur écoute déjà ; la dernière observation (avant la création) ne connaît pas la session.
        core.set_observation(observation(vec![], at(9)));
        core.apply_scan(
            snapshot(
                at(11),
                &[(700, 1, "python3 -m http.server 8811")],
                &[(700, 8811)],
            ),
            at(11),
        );
        let row = core.row_of(&RowKind::Config(web.clone())).unwrap();
        assert_eq!(row.origin, Some(Origin::Idema));
        let stop = core.begin_stop(&RowKind::Config(web.clone())).unwrap();
        assert_eq!(stop.sessions_to_close, ["idema-srv-new"]);

        // Une observation postérieure prend le relais et décrit la session elle-même.
        core.set_observation(observation(
            vec![server_session("idema-srv-new", &project, "web", 700, false)],
            at(12),
        ));
        assert!(core.fresh.is_empty());
    }

    /// Un serveur repéré dans `project` (dossier courant dans le projet), qui écoute sur `port` avec cette sortie.
    fn detected_snapshot(
        at: Instant,
        project: &Path,
        servers: &[(i32, u16, Option<OutputTarget>)],
    ) -> Snapshot {
        let processes: Vec<(i32, i32, String)> = servers
            .iter()
            .map(|(pid, port, _)| (*pid, 1, format!("python3 -m http.server {port}")))
            .collect();
        let borrowed: Vec<(i32, i32, &str)> = processes
            .iter()
            .map(|(pid, ppid, command)| (*pid, *ppid, command.as_str()))
            .collect();
        let listeners: Vec<(i32, u16)> =
            servers.iter().map(|(pid, port, _)| (*pid, *port)).collect();
        let mut taken = snapshot(at, &borrowed, &listeners);
        for (pid, _, output) in servers {
            taken.cwd.insert(*pid, project.to_path_buf());
            if let Some(output) = output {
                taken.stdout.insert(*pid, output.clone());
            }
        }
        taken
    }

    #[test]
    fn the_journal_of_a_server_started_by_idema_is_the_pane_of_its_session() {
        let project = temp_project("journal-session");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        core.set_observation(observation(
            vec![server_session("idema-srv-1", &project, "web", 700, false)],
            base,
        ));
        core.apply_scan(
            snapshot(
                base,
                &[(700, 1, "python3 -m http.server 8811")],
                &[(700, 8811)],
            ),
            base,
        );
        let row = core.row_of(&RowKind::Config(web)).unwrap();
        assert_eq!(row.origin, Some(Origin::Idema));
        assert_eq!(
            core.journal_source(&row),
            JournalSource::Session {
                name: "idema-srv-1".to_string(),
                pane: PaneId(1)
            }
        );
    }

    #[test]
    fn a_failed_start_keeps_its_dead_session_as_its_journal() {
        let project = temp_project("journal-dead");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        core.set_observation(observation(
            vec![
                server_session("idema-srv-old", &project, "web", 0, true),
                server_session("idema-srv-new", &project, "web", 0, true),
            ],
            base,
        ));
        core.apply_scan(snapshot(base, &[], &[]), base);
        let row = core.row_of(&RowKind::Config(web)).unwrap();
        assert_eq!(row.origin, None);
        // Deux sessions mortes : la dernière listée est la plus récente.
        assert!(matches!(
            core.journal_source(&row),
            JournalSource::Session { name, .. } if name == "idema-srv-new"
        ));
    }

    #[test]
    fn the_live_session_of_an_entry_beats_a_dead_one() {
        let project = temp_project("journal-live");
        write_launch(&project, WEB_ONLY);
        let web = key(&project, "web");
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        core.set_observation(observation(
            vec![
                server_session("idema-srv-live", &project, "web", 700, false),
                server_session("idema-srv-dead", &project, "web", 0, true),
            ],
            Instant::now(),
        ));
        assert_eq!(
            core.server_session_of(&web).map(|session| session.name),
            Some("idema-srv-live".to_string())
        );
        assert_eq!(
            core.server_session_of(&key(&project, "api")),
            None,
            "an entry without a session has none"
        );
    }

    #[test]
    fn the_journal_of_a_detected_server_is_the_file_its_output_goes_to() {
        let project = temp_project("journal-file");
        write_launch(&project, WEB_ONLY);
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        let log = PathBuf::from("/tmp/idema-ac89.log");
        core.apply_scan(
            detected_snapshot(
                base,
                &project,
                &[
                    (900, 8814, Some(OutputTarget::File(log.clone()))),
                    (
                        901,
                        8815,
                        Some(OutputTarget::Other("/dev/null".to_string())),
                    ),
                    (902, 8816, None),
                ],
            ),
            base,
        );
        let source = |port: u16| {
            let row = core
                .find_row(&project, "python3 -m http.server", Some(port))
                .unwrap();
            core.journal_source(&row)
        };
        assert_eq!(source(8814), JournalSource::File(log));
        assert_eq!(
            source(8815),
            JournalSource::Unavailable {
                target: Some("/dev/null".to_string())
            }
        );
        assert_eq!(source(8816), JournalSource::Unavailable { target: None });
        assert!(!source(8815).is_available() && !source(8816).is_available());
    }

    #[test]
    fn a_detected_server_is_found_by_its_port_and_an_entry_by_its_name_alone() {
        let project = temp_project("find-row");
        write_launch(&project, WEB_ONLY);
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        core.apply_scan(
            detected_snapshot(base, &project, &[(900, 8814, None), (901, 8815, None)]),
            base,
        );
        let tool = "python3 -m http.server";
        assert_eq!(core.detected_running(), 2);
        assert_eq!(
            core.find_row(&project, tool, Some(8815))
                .map(|row| row.port),
            Some(Some(8815))
        );
        assert!(core.find_row(&project, tool, Some(8999)).is_none());
        assert!(core.find_row(&project, tool, None).is_none());
        // Une entrée de configuration se retrouve par son nom, quel que soit le port que l'onglet a retenu.
        assert!(core.find_row(&project, "web", Some(1)).is_some());
        assert!(core.find_row(&project, "web", None).is_some());
        assert!(core.find_row(Path::new("/ailleurs"), "web", None).is_none());
    }

    #[test]
    fn a_stopped_entry_without_a_session_says_it_is_stopped_while_a_running_stranger_stays_unknown()
    {
        let project = temp_project("journal-stopped");
        write_launch(&project, WEB_ONLY);
        let base = Instant::now();
        let mut core = ServersCore::new();
        core.set_projects(vec![project.clone()]);
        core.set_observation(observation(vec![], base));
        core.apply_scan(snapshot(base, &[], &[]), base);
        let web = core.row_of(&RowKind::Config(key(&project, "web"))).unwrap();
        assert_eq!(web.state, RowState::Stopped);
        let source = core.journal_source(&web);
        assert_eq!(source, JournalSource::Stopped);
        assert!(!source.is_available());
        // Un serveur lancé ailleurs dont on ne sait pas où va la sortie garde le texte « inconnue ».
        core.apply_scan(
            detected_snapshot(base, &project, &[(900, 8814, None)]),
            base,
        );
        let stranger = core
            .find_row(&project, "python3 -m http.server", Some(8814))
            .unwrap();
        assert_eq!(
            core.journal_source(&stranger),
            JournalSource::Unavailable { target: None }
        );
    }

    #[test]
    fn the_unavailable_text_names_the_target_or_says_it_is_unknown() {
        assert_eq!(
            JournalSource::unavailable_text(Some("/dev/null")),
            "Journal indisponible : ce serveur écrit dans /dev/null"
        );
        assert!(JournalSource::unavailable_text(None).starts_with("Journal indisponible"));
    }
}
