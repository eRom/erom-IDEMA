//! État des lignes de serveur (spec P4 §5.1 à §5.3, §6) : fusionne la configuration des projets, les relevés de ports
//! et les démarrages faits par IdemA. Le module est pur : aucune entrée-sortie, aucune horloge hors du `now` reçu.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::launch::{ConfigEntry, EntryError, EntrySource, ProjectConfig};
use super::scan::{Listener, NODE_INSPECTOR_PORT, OutputTarget, Snapshot};

/// Une ligne repérée disparaît ce délai après la disparition de son port (spec §5.2).
pub const DETECTED_GRACE: Duration = Duration::from_secs(5);
/// Au-delà, un démarrage qui n'a ouvert aucun port est un échec (spec §5.3).
pub const START_TIMEOUT: Duration = Duration::from_secs(60);
/// La cause montrée sur la ligne quand le délai a échoué le démarrage.
fn timeout_failure() -> String {
    format!(
        "le port attendu n'a pas répondu en {} s",
        START_TIMEOUT.as_secs()
    )
}

/// Une entrée de configuration : son projet et son nom.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerKey {
    pub project: PathBuf,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowKind {
    Config(ServerKey),
    Detected { pid: i32, port: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Stopped,
    Starting,
    Running { pid: i32, port: u16 },
    Failed,
    StopRefused { pid: i32, port: u16 },
}

/// Qui a lancé le processus qui écoute (spec §5.2, §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// « lancé par <nom de la session> » : `session` est le nom tmux.
    Session {
        session: String,
        display_name: String,
    },
    /// « lancé par le volet Terminal »
    TerminalPanel,
    /// « lancé par IdemA » (une session serveur)
    Idema,
    /// « lancé dans <projet> » : pas de pane ancêtre, mais le dossier courant est dans le projet de la ligne.
    Project,
    /// « lancé hors d'IdemA · <commande> »
    Outside { command: String },
    /// « port pris par <commande> » : le port de l'entrée est tenu par le serveur d'un autre projet.
    PortTakenBy { command: String },
}

/// Un port en écoute dans l'arbre de la session serveur d'une entrée, qui répond en HTTP, mais n'est pas celui que la
/// ligne montre (P4-R7 : rien ne tourne caché). L'entrée déclare `3000`, le serveur ouvre `5173`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExtraPort {
    pub pid: i32,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRow {
    pub kind: RowKind,
    pub project: PathBuf,
    pub name: String,
    pub command: String,
    pub port: Option<u16>,
    /// `entry.url`, sinon `http://localhost:<port>` ; vide quand ni l'un ni l'autre n'existe (`dev` sans port).
    pub url: String,
    pub state: RowState,
    /// `Some` seulement quand un processus écoute (`Running`, `StopRefused`).
    pub origin: Option<Origin>,
    /// Le PID qui écoute, tant que la ligne est en marche.
    pub listener_pid: Option<i32>,
    pub output: Option<OutputTarget>,
    /// La session serveur d'IdemA de cette entrée est vivante (son pane n'est pas sorti), quel que soit l'état de la
    /// ligne : c'est elle qu'on arrête quand aucun port ne se montre (P4-R7).
    pub session_alive: bool,
    /// Les ports HTTP de la session serveur que la ligne ne montre pas, tant qu'elle n'est pas en marche : un démarrage
    /// qui ouvre un autre port que celui de l'entrée reste visible (et compté dans la pastille).
    pub extra_ports: Vec<ExtraPort>,
    /// La cause d'un échec au démarrage, quand IdemA la connaît (`Failed` seulement).
    pub failure: Option<String>,
}

/// Les racines des panes d'IdemA, pour rattacher un processus à une session.
#[derive(Debug, Clone, Default)]
pub struct PaneRoots {
    pub panes: Vec<PaneRoot>,
}

#[derive(Debug, Clone)]
pub struct PaneRoot {
    pub pid: i32,
    pub owner: PaneOwner,
    /// La commande de lancement de l'agent du pane, ex. `idnma run claude` : l'entrée du dernier `idnma run` du pane
    /// (`@idnma_agent`), sinon celle du lanceur (`@idema_agent`). Son programme sert à reconnaître l'agent parmi les
    /// processus d'un arrêt. `None` quand le pane n'a pas d'agent ou que la commande est inconnue.
    pub agent_command: Option<String>,
    /// Le processus du pane est sorti mais tmux garde le pane (`remain-on-exit`) : preuve positive d'un serveur mort.
    /// Une racine morte n'attribue plus aucun processus (son PID peut avoir été réutilisé).
    pub dead: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneOwner {
    Session {
        session: String,
        display_name: String,
        project: PathBuf,
    },
    TerminalPanel {
        project: PathBuf,
    },
    ServerSession {
        key: ServerKey,
    },
}

#[derive(Debug, Clone)]
pub struct ProjectGroup {
    pub project: PathBuf,
    /// Les entrées de configuration, dans l'ordre du fichier.
    pub rows: Vec<ServerRow>,
    /// Les serveurs repérés hors de la configuration, par port croissant.
    pub detected: Vec<ServerRow>,
    pub errors: Vec<EntryError>,
}

/// Les outils reconnus dans la ligne de commande d'un serveur repéré (spec §5.2), dans l'ordre de la recherche.
const TOOLS: [&str; 12] = [
    "next dev",
    "vite",
    "wrangler dev",
    "bun --hot",
    "astro dev",
    "nuxt dev",
    "remix dev",
    "python3 -m http.server",
    "php -S",
    "rails server",
    "hugo server",
    "jekyll serve",
];
/// Longueur maximale du nom d'un serveur repéré quand aucun outil n'est reconnu.
const NAME_LIMIT: usize = 40;

/// Le processus qui écoute sur une entrée de configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Live {
    pid: i32,
    port: u16,
    origin: Origin,
    output: Option<OutputTarget>,
}

#[derive(Debug, Clone, Copy, Default)]
enum Phase {
    #[default]
    Idle,
    Starting {
        since: Instant,
    },
    Failed,
}

/// Ce que le registre retient d'une entrée de configuration entre deux relevés.
#[derive(Debug, Default)]
struct ConfigStatus {
    phase: Phase,
    live: Option<Live>,
    /// Le couple (pid, port) dont l'arrêt a été refusé : il ne vaut que pour ce processus.
    refused: Option<(i32, u16)>,
    /// Le pane de la session serveur de l'entrée est vivant, au dernier relevé.
    session_alive: bool,
    /// Le PID du pane de la dernière session serveur vue vivante : une session déjà vue ne redevient pas « en démarrage » toute seule
    /// (un arrêt en cours la voit encore un instant).
    seen_session: Option<i32>,
    /// Les ports HTTP de cette session que `live` ne montre pas, au dernier relevé.
    extras: Vec<ExtraPort>,
    /// La cause de l'échec, tant que la phase est `Failed`.
    failure: Option<String>,
}

/// Une ligne repérée hors de la configuration.
#[derive(Debug, Clone)]
struct DetectedRow {
    pid: i32,
    port: u16,
    project: PathBuf,
    name: String,
    command: String,
    origin: Origin,
    output: Option<OutputTarget>,
    /// Le premier relevé qui ne l'a plus vue ; la ligne reste `DETECTED_GRACE` après.
    vanished_since: Option<Instant>,
    refused: bool,
}

impl DetectedRow {
    fn to_row(&self) -> ServerRow {
        let (state, origin, listener_pid) = if self.vanished_since.is_some() {
            (RowState::Stopped, None, None)
        } else {
            let state = if self.refused {
                RowState::StopRefused {
                    pid: self.pid,
                    port: self.port,
                }
            } else {
                RowState::Running {
                    pid: self.pid,
                    port: self.port,
                }
            };
            (state, Some(self.origin.clone()), Some(self.pid))
        };
        ServerRow {
            kind: RowKind::Detected {
                pid: self.pid,
                port: self.port,
            },
            project: self.project.clone(),
            name: self.name.clone(),
            command: self.command.clone(),
            port: Some(self.port),
            url: format!("http://localhost:{}", self.port),
            state,
            origin,
            listener_pid,
            output: self.output.clone(),
            session_alive: false,
            extra_ports: Vec::new(),
            failure: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Registry {
    projects: Vec<(PathBuf, ProjectConfig)>,
    status: HashMap<ServerKey, ConfigStatus>,
    /// Par port : un port n'a qu'un serveur à la fois.
    detected: BTreeMap<u16, DetectedRow>,
    dev_ports: HashMap<PathBuf, u16>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Les projets connus, dans l'ordre de la barre latérale. Les lignes d'une entrée nouvelle sont `Stopped` jusqu'au
    /// prochain relevé.
    pub fn set_projects(&mut self, projects: Vec<(PathBuf, ProjectConfig)>) {
        self.projects = projects;
    }

    /// Fusionne un relevé : les entrées de configuration (par port, ou pour `dev` par leur arbre ou leur script), les
    /// serveurs repérés, les démarrages en cours et les ports qui disparaissent.
    pub fn apply_scan(&mut self, snapshot: &Snapshot, roots: &PaneRoots, now: Instant) {
        let outcome = Analysis::new(snapshot, roots, &self.projects).run(&self.dev_ports);
        self.dev_ports.extend(outcome.remembered);

        let keys = self.config_keys();
        self.status.retain(|key, _| keys.contains(key));
        for key in keys {
            let status = self.status.entry(key.clone()).or_default();
            let session_pane = live_server_pane(roots, &key);
            status.session_alive = session_pane.is_some();
            let reattached = session_pane.is_some() && status.seen_session != session_pane;
            if session_pane.is_some() {
                status.seen_session = session_pane;
            }
            status.extras = outcome
                .extras
                .iter()
                .filter(|(owner, _)| *owner == key)
                .map(|(_, extra)| *extra)
                .collect();
            match outcome.live.get(&key) {
                Some(live) => {
                    status.phase = Phase::Idle;
                    status.failure = None;
                    status.refused = status
                        .refused
                        .filter(|&refused| refused == (live.pid, live.port));
                    status.live = Some(live.clone());
                }
                None => {
                    status.live = None;
                    status.refused = None;
                    // Une session serveur vivante qu'aucun démarrage d'IdemA n'explique (une instance précédente d'IdemA
                    // l'a lancée, P4-R7) est en démarrage : la ligne ne la montre pas « arrêtée ».
                    if matches!(status.phase, Phase::Idle) && reattached {
                        status.phase = Phase::Starting { since: now };
                    }
                    if let Phase::Starting { since } = status.phase {
                        // Seule la preuve positive d'un pane mort échoue tout de suite : une racine absente ne dit rien
                        // (course avec le listage de tmux), et le délai couvre une session fermée à la main.
                        let pane_dead = has_dead_server_pane(roots, &key);
                        let timed_out = now.saturating_duration_since(since) >= START_TIMEOUT;
                        if pane_dead || timed_out {
                            status.phase = Phase::Failed;
                            status.failure = timed_out.then(timeout_failure);
                        }
                    }
                }
            }
        }

        // Un port pris par une entrée n'est plus un serveur repéré : sa vieille ligne repérée s'efface sans délai.
        let mut previous = std::mem::take(&mut self.detected);
        previous.retain(|port, _| !outcome.claimed.contains(port));
        for mut row in outcome.detected {
            // Même port, autre PID : la ligne est remplacée (origine, PID, refus d'arrêt), pas prolongée.
            if let Some(old) = previous.remove(&row.port)
                && old.pid == row.pid
            {
                row.refused = old.refused;
            }
            self.detected.insert(row.port, row);
        }
        for (port, mut gone) in previous {
            match gone.vanished_since {
                None => {
                    gone.vanished_since = Some(now);
                    gone.refused = false;
                    self.detected.insert(port, gone);
                }
                Some(since) if now.saturating_duration_since(since) < DETECTED_GRACE => {
                    self.detected.insert(port, gone);
                }
                Some(_) => {}
            }
        }
    }

    /// IdemA vient de créer la session serveur de cette entrée.
    pub fn mark_starting(&mut self, key: &ServerKey, now: Instant) {
        let status = self.status.entry(key.clone()).or_default();
        status.phase = Phase::Starting { since: now };
        status.live = None;
        status.refused = None;
        status.failure = None;
    }

    /// Le démarrage a échoué ; `reason` est la cause à montrer sur la ligne, quand on la connaît.
    pub fn mark_failed(&mut self, key: &ServerKey, reason: Option<String>) {
        let status = self.status.entry(key.clone()).or_default();
        status.phase = Phase::Failed;
        status.live = None;
        status.refused = None;
        status.failure = reason;
    }

    /// IdemA vient d'arrêter le serveur de cette entrée (ou sa session sans port) : la ligne est arrêtée, ni en
    /// démarrage ni en échec, jusqu'au prochain démarrage.
    pub fn mark_stopped(&mut self, key: &ServerKey) {
        if let Some(status) = self.status.get_mut(key) {
            status.phase = Phase::Idle;
            status.failure = None;
            status.extras.clear();
            status.session_alive = false;
        }
    }

    /// L'arrêt n'a pas libéré le port : la ligne reste en marche, mais pour ce seul processus. Sans effet sur une ligne
    /// qui n'est pas en marche.
    pub fn mark_stop_refused(&mut self, kind: &RowKind) {
        match kind {
            RowKind::Config(key) => {
                if let Some(status) = self.status.get_mut(key)
                    && let Some((pid, port)) =
                        status.live.as_ref().map(|live| (live.pid, live.port))
                {
                    status.refused = Some((pid, port));
                }
            }
            RowKind::Detected { pid, port } => {
                if let Some(row) = self.detected.get_mut(port)
                    && row.pid == *pid
                    && row.vanished_since.is_none()
                {
                    row.refused = true;
                }
            }
        }
    }

    /// Un groupe par projet qui a une ligne, un serveur repéré ou une erreur : le projet de contexte d'abord, les
    /// autres dans l'ordre de la barre latérale. Les repérés d'un projet inconnu restent en mémoire mais ne s'affichent pas.
    pub fn rows(&self, context_project: Option<&Path>) -> Vec<ProjectGroup> {
        let mut order: Vec<usize> = (0..self.projects.len()).collect();
        if let Some(context) = context_project
            && let Some(position) = order
                .iter()
                .position(|&index| self.projects[index].0 == context)
        {
            let first = order.remove(position);
            order.insert(0, first);
        }

        order
            .into_iter()
            .filter_map(|index| {
                let (project, config) = &self.projects[index];
                let rows: Vec<ServerRow> = config
                    .entries
                    .iter()
                    .map(|entry| self.config_row(project, entry))
                    .collect();
                let detected: Vec<ServerRow> = self
                    .detected
                    .values()
                    .filter(|row| row.project == *project)
                    .map(DetectedRow::to_row)
                    .collect();
                if rows.is_empty() && detected.is_empty() && config.errors.is_empty() {
                    return None;
                }
                Some(ProjectGroup {
                    project: project.clone(),
                    rows,
                    detected,
                    errors: config.errors.clone(),
                })
            })
            .collect()
    }

    /// Le nombre de serveurs en marche ou dont l'arrêt a été refusé (la pastille de la barre latérale) : un processus
    /// qui tient le port de deux entrées compte pour un.
    pub fn running_count(&self) -> usize {
        self.rows(None)
            .iter()
            .flat_map(|group| group.rows.iter().chain(&group.detected))
            .flat_map(|row| {
                let own = match row.state {
                    RowState::Running { pid, port } | RowState::StopRefused { pid, port } => {
                        Some((pid, port))
                    }
                    _ => None,
                };
                // Les ports HTTP qu'une session ouvre sans que sa ligne les montre comptent aussi : ils tournent.
                own.into_iter()
                    .chain(row.extra_ports.iter().map(|extra| (extra.pid, extra.port)))
            })
            .collect::<HashSet<_>>()
            .len()
    }

    /// Le port retenu de l'entrée `dev` de chaque projet, pour `state.json`.
    pub fn remembered_dev_ports(&self) -> HashMap<PathBuf, u16> {
        self.dev_ports.clone()
    }

    pub fn restore_dev_ports(&mut self, ports: HashMap<PathBuf, u16>) {
        self.dev_ports = ports;
    }

    fn config_keys(&self) -> HashSet<ServerKey> {
        self.projects
            .iter()
            .flat_map(|(project, config)| {
                config.entries.iter().map(|entry| ServerKey {
                    project: project.clone(),
                    name: entry.name.clone(),
                })
            })
            .collect()
    }

    fn config_row(&self, project: &Path, entry: &ConfigEntry) -> ServerRow {
        let key = ServerKey {
            project: project.to_path_buf(),
            name: entry.name.clone(),
        };
        let status = self.status.get(&key);
        let live = status.and_then(|status| status.live.as_ref());
        let port = match live {
            Some(live) => Some(live.port),
            None => entry.port.or_else(|| self.dev_ports.get(project).copied()),
        };
        let state = match live {
            Some(live) => {
                let refused = status.and_then(|status| status.refused);
                if refused == Some((live.pid, live.port)) {
                    RowState::StopRefused {
                        pid: live.pid,
                        port: live.port,
                    }
                } else {
                    RowState::Running {
                        pid: live.pid,
                        port: live.port,
                    }
                }
            }
            None => match status.map(|status| status.phase).unwrap_or_default() {
                Phase::Idle => RowState::Stopped,
                Phase::Starting { .. } => RowState::Starting,
                Phase::Failed => RowState::Failed,
            },
        };
        // L'adresse de l'entrée, sauf si elle est vide : un `url` vide du fichier compte pour absent.
        let url = entry
            .url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string)
            .or_else(|| port.map(|port| format!("http://localhost:{port}")))
            .unwrap_or_default();
        let command = std::iter::once(entry.program.as_str())
            .chain(entry.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        // Les ports en plus se montrent tant que le serveur en marche de la ligne n'est pas celui de la session d'IdemA :
        // sinon ce sont les ports internes d'un outil (un serveur de rendu, un relais), qui ne sont pas des serveurs à
        // compter. Quand un processus étranger tient le port déclaré, la session d'IdemA vit sans que la ligne la montre :
        // ses ports se montrent, rien ne tourne caché.
        let extra_ports = match (live, status) {
            (None, Some(status)) => status.extras.clone(),
            (Some(live), Some(status)) if live.origin != Origin::Idema => status.extras.clone(),
            _ => Vec::new(),
        };
        let failure = match state {
            RowState::Failed => status.and_then(|status| status.failure.clone()),
            _ => None,
        };
        ServerRow {
            kind: RowKind::Config(key),
            project: project.to_path_buf(),
            name: entry.name.clone(),
            command,
            port,
            url,
            state,
            origin: live.map(|live| live.origin.clone()),
            listener_pid: live.map(|live| live.pid),
            output: live.and_then(|live| live.output.clone()),
            session_alive: status.is_some_and(|status| status.session_alive),
            extra_ports,
            failure,
        }
    }
}

/// Le PID du pane de la session serveur vivante de `key` (son processus tourne), s'il y en a une.
fn live_server_pane(roots: &PaneRoots, key: &ServerKey) -> Option<i32> {
    roots.panes.iter().find_map(|pane| {
        (!pane.dead
            && matches!(&pane.owner, PaneOwner::ServerSession { key: owner } if owner == key))
        .then_some(pane.pid)
    })
}

/// Vrai si tmux liste le pane de la session serveur de `key` et que son processus est sorti.
fn has_dead_server_pane(roots: &PaneRoots, key: &ServerKey) -> bool {
    roots.panes.iter().any(|pane| {
        pane.dead && matches!(&pane.owner, PaneOwner::ServerSession { key: owner } if owner == key)
    })
}

/// Un port en écoute, rattaché à son processus, à son dossier courant et au pane d'IdemA dont il descend.
struct Located<'a> {
    pid: i32,
    command: &'a str,
    cwd: Option<&'a Path>,
    pane: Option<&'a PaneRoot>,
    /// La commande du processus, puis celles de ses ancêtres, du plus proche au plus lointain, sans le pane ni launchd.
    lineage: Vec<&'a str>,
}

impl Located<'_> {
    fn server_session(&self) -> Option<&ServerKey> {
        match self.pane.map(|pane| &pane.owner) {
            Some(PaneOwner::ServerSession { key }) => Some(key),
            _ => None,
        }
    }

    /// La commande du processus, telle qu'on la montre (voir `normalize_command`).
    fn shown_command(&self) -> String {
        normalize_command(self.command)
    }
}

/// Un serveur repéré avant de savoir si une entrée `dev` le réclame.
struct Candidate<'a> {
    row: DetectedRow,
    lineage: Vec<&'a str>,
}

#[derive(Default)]
struct Outcome {
    live: HashMap<ServerKey, Live>,
    detected: Vec<DetectedRow>,
    /// Les ports que ce relevé attribue à une entrée.
    claimed: HashSet<u16>,
    remembered: Vec<(PathBuf, u16)>,
    /// Les ports HTTP de la session serveur de chaque entrée, qui n'appartiennent pas à une autre entrée.
    extras: Vec<(ServerKey, ExtraPort)>,
}

/// Le classement pur d'un relevé : qui écoute où, pour quelle entrée ou quel projet.
struct Analysis<'a> {
    snapshot: &'a Snapshot,
    panes: HashMap<i32, &'a PaneRoot>,
    projects: &'a [(PathBuf, ProjectConfig)],
}

impl<'a> Analysis<'a> {
    fn new(
        snapshot: &'a Snapshot,
        roots: &'a PaneRoots,
        projects: &'a [(PathBuf, ProjectConfig)],
    ) -> Self {
        Self {
            snapshot,
            panes: roots
                .panes
                .iter()
                .filter(|pane| !pane.dead)
                .map(|pane| (pane.pid, pane))
                .collect(),
            projects,
        }
    }

    fn run(&self, dev_ports: &HashMap<PathBuf, u16>) -> Outcome {
        let located: BTreeMap<u16, Located<'a>> = self
            .chosen_listeners()
            .into_iter()
            .map(|(port, listener)| (port, self.locate(listener)))
            .collect();
        let entries: Vec<(ServerKey, &ConfigEntry)> = self
            .projects
            .iter()
            .flat_map(|(project, config)| {
                config.entries.iter().map(|entry| {
                    (
                        ServerKey {
                            project: project.clone(),
                            name: entry.name.clone(),
                        },
                        entry,
                    )
                })
            })
            .collect();

        let mut outcome = Outcome::default();
        self.claim_declared_ports(&entries, &located, &mut outcome);
        let mut candidates = self.detect(&entries, &located, &mut outcome);
        self.attach_dev_entries(&entries, &located, dev_ports, &mut candidates, &mut outcome);
        outcome.detected = candidates
            .into_iter()
            .map(|candidate| candidate.row)
            .collect();
        // Le port que la ligne montre n'est pas un port en plus.
        outcome.extras.retain(|(key, extra)| {
            outcome
                .live
                .get(key)
                .is_none_or(|live| live.port != extra.port)
        });
        outcome
    }

    /// Un seul processus par port : celui qui répond en HTTP d'abord, puis le plus petit PID. Un serveur qui forke
    /// partage son port avec ses enfants, et le choix reste le même d'un relevé à l'autre.
    fn chosen_listeners(&self) -> BTreeMap<u16, Listener> {
        let rank = |listener: Listener| {
            (
                !self.snapshot.http.contains(&(listener.pid, listener.port)),
                listener.pid,
            )
        };
        let mut chosen: BTreeMap<u16, Listener> = BTreeMap::new();
        for &listener in &self.snapshot.listeners {
            chosen
                .entry(listener.port)
                .and_modify(|current| {
                    if rank(listener) < rank(*current) {
                        *current = listener;
                    }
                })
                .or_insert(listener);
        }
        chosen
    }

    fn locate(&self, listener: Listener) -> Located<'a> {
        let snapshot = self.snapshot;
        let mut lineage = Vec::new();
        let mut pane = self.panes.get(&listener.pid).copied();
        let mut visited = HashSet::new();
        let mut current = listener.pid;
        while visited.insert(current) {
            let Some(process) = snapshot.processes.get(&current) else {
                break;
            };
            lineage.push(process.command.as_str());
            // Le processus en écoute peut être la racine du pane (une commande lancée par `exec`).
            if pane.is_some() || process.ppid <= 1 {
                break;
            }
            if let Some(root) = self.panes.get(&process.ppid) {
                pane = Some(root);
                break;
            }
            current = process.ppid;
        }
        Located {
            pid: listener.pid,
            command: lineage.first().copied().unwrap_or(""),
            cwd: snapshot.cwd.get(&listener.pid).map(PathBuf::as_path),
            pane,
            lineage,
        }
    }

    /// Le projet connu le plus profond qui contient `path`.
    fn deepest_project(&self, path: &Path) -> Option<&'a Path> {
        self.projects
            .iter()
            .map(|(project, _)| project.as_path())
            .filter(|project| path.starts_with(project))
            .max_by_key(|project| project.components().count())
    }

    fn origin(&self, found: &Located, project: &Path) -> Origin {
        match found.pane.map(|pane| &pane.owner) {
            Some(PaneOwner::Session {
                session,
                display_name,
                ..
            }) => Origin::Session {
                session: session.clone(),
                display_name: display_name.clone(),
            },
            Some(PaneOwner::TerminalPanel { .. }) => Origin::TerminalPanel,
            Some(PaneOwner::ServerSession { .. }) => Origin::Idema,
            // Pas de pane ancêtre : le dossier courant dit « dans <projet> » seulement pour le projet de la ligne.
            None if found.cwd.is_some_and(|cwd| cwd.starts_with(project)) => Origin::Project,
            None => Origin::Outside {
                command: found.shown_command(),
            },
        }
    }

    fn live(&self, found: &Located, port: u16, origin: Origin) -> Live {
        Live {
            pid: found.pid,
            port,
            origin,
            output: self.snapshot.stdout.get(&found.pid).cloned(),
        }
    }

    /// Un port qui porte le `port` d'une entrée la met en marche, quel que soit le lanceur.
    fn claim_declared_ports(
        &self,
        entries: &[(ServerKey, &ConfigEntry)],
        located: &BTreeMap<u16, Located<'a>>,
        outcome: &mut Outcome,
    ) {
        let mut claims: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
        for (index, (_, entry)) in entries.iter().enumerate() {
            if let Some(port) = entry.port {
                claims.entry(port).or_default().push(index);
            }
        }
        for (port, claimants) in claims {
            outcome.claimed.insert(port);
            let Some(found) = located.get(&port) else {
                continue;
            };
            let winner = Self::pick_winner(entries, &claimants, found);
            for index in claimants {
                let key = &entries[index].0;
                let origin = if index == winner {
                    self.origin(found, &key.project)
                } else {
                    Origin::PortTakenBy {
                        command: found.shown_command(),
                    }
                };
                outcome
                    .live
                    .insert(key.clone(), self.live(found, port, origin));
            }
        }
    }

    /// Entre plusieurs entrées sur un port : celle dont la session serveur tient le port, sinon celle dont le projet
    /// contient le dossier courant du processus (le plus profond), sinon la première de la barre latérale.
    fn pick_winner(
        entries: &[(ServerKey, &ConfigEntry)],
        claimants: &[usize],
        found: &Located,
    ) -> usize {
        if let Some(owner) = found.server_session()
            && let Some(&index) = claimants.iter().find(|&&index| entries[index].0 == *owner)
        {
            return index;
        }
        if let Some(cwd) = found.cwd
            && let Some(index) = claimants
                .iter()
                .copied()
                .filter(|&index| cwd.starts_with(&entries[index].0.project))
                .min_by_key(|&index| {
                    (
                        Reverse(entries[index].0.project.components().count()),
                        index,
                    )
                })
        {
            return index;
        }
        claimants[0]
    }

    /// Les ports non déclarés qui sont des serveurs (spec §5.2).
    fn detect(
        &self,
        entries: &[(ServerKey, &ConfigEntry)],
        located: &BTreeMap<u16, Located<'a>>,
        outcome: &mut Outcome,
    ) -> Vec<Candidate<'a>> {
        let mut candidates = Vec::new();
        for (&port, found) in located {
            if outcome.claimed.contains(&port) || port == NODE_INSPECTOR_PORT {
                continue;
            }
            // Un port de l'arbre d'une session serveur appartient à son entrée, pas à la liste des repérés.
            if let Some(owner) = found.server_session()
                && entries.iter().any(|(key, _)| key == owner)
            {
                outcome.claimed.insert(port);
                // Un port HTTP que la ligne de l'entrée ne montrera peut-être pas : on le retient pour qu'il se voie.
                if self.snapshot.http.contains(&(found.pid, port)) {
                    outcome.extras.push((
                        owner.clone(),
                        ExtraPort {
                            pid: found.pid,
                            port,
                        },
                    ));
                }
                continue;
            }
            if !self.snapshot.http.contains(&(found.pid, port)) {
                continue;
            }
            let project = match found.pane.map(|pane| &pane.owner) {
                Some(PaneOwner::Session { project, .. })
                | Some(PaneOwner::TerminalPanel { project }) => project.clone(),
                Some(PaneOwner::ServerSession { key }) => key.project.clone(),
                None => match found.cwd.and_then(|cwd| self.deepest_project(cwd)) {
                    Some(project) => project.to_path_buf(),
                    None => continue,
                },
            };
            let origin = self.origin(found, &project);
            candidates.push(Candidate {
                row: DetectedRow {
                    pid: found.pid,
                    port,
                    name: tool_name(&found.lineage)
                        .map_or_else(|| abbreviate(&found.shown_command()), str::to_string),
                    command: found.shown_command(),
                    origin,
                    output: self.snapshot.stdout.get(&found.pid).cloned(),
                    project,
                    vanished_since: None,
                    refused: false,
                },
                lineage: found.lineage.clone(),
            });
        }
        candidates
    }

    /// L'entrée `dev` de `package.json` n'a pas de port : elle est en marche quand l'arbre de sa session serveur écoute
    /// (et son premier port est retenu), ou quand un serveur repéré de son projet porte le texte de son script ou son
    /// port retenu, qu'elle absorbe alors.
    fn attach_dev_entries(
        &self,
        entries: &[(ServerKey, &ConfigEntry)],
        located: &BTreeMap<u16, Located<'a>>,
        dev_ports: &HashMap<PathBuf, u16>,
        candidates: &mut Vec<Candidate<'a>>,
        outcome: &mut Outcome,
    ) {
        for (key, entry) in entries {
            let EntrySource::PackageDev { script } = &entry.source else {
                continue;
            };
            let remembered = dev_ports.get(&key.project).copied();

            let mut tree: Vec<(u16, &Located)> = located
                .iter()
                .filter(|(port, found)| {
                    **port != NODE_INSPECTOR_PORT && found.server_session() == Some(key)
                })
                .map(|(&port, found)| (port, found))
                .collect();
            tree.sort_by_key(|(port, found)| {
                (
                    Some(*port) != remembered,
                    !self.snapshot.http.contains(&(found.pid, *port)),
                    *port,
                )
            });
            if let Some(&(port, found)) = tree.first() {
                outcome.claimed.insert(port);
                outcome.remembered.push((key.project.clone(), port));
                outcome
                    .live
                    .insert(key.clone(), self.live(found, port, Origin::Idema));
                continue;
            }

            let script = script.trim();
            // Le script se retrouve aussi sous la forme montrée de la commande : `python3 -m http.server 8813` tapé dans un
            // pane tourne sous le chemin complet du Python des outils Xcode.
            let shown_script = normalize_command(script);
            let (mut matching, kept): (Vec<_>, Vec<_>) = std::mem::take(candidates)
                .into_iter()
                .partition(|candidate| {
                    candidate.row.project == key.project
                        && (remembered == Some(candidate.row.port)
                            || (!script.is_empty()
                                // Le script se lit en mots entiers : `vite` n'est pas dans `vitest --ui`.
                                && candidate.lineage.iter().any(|command| {
                                    contains_tool(command, script)
                                        || contains_tool(&normalize_command(command), &shown_script)
                                })))
                });
            *candidates = kept;
            matching.sort_by_key(|candidate| {
                (Some(candidate.row.port) != remembered, candidate.row.port)
            });
            for candidate in &matching {
                outcome.claimed.insert(candidate.row.port);
            }
            if let Some(candidate) = matching.into_iter().next() {
                outcome.live.insert(
                    key.clone(),
                    Live {
                        pid: candidate.row.pid,
                        port: candidate.row.port,
                        origin: candidate.row.origin,
                        output: candidate.row.output,
                    },
                );
            }
        }
    }
}

/// L'outil reconnu dans la première commande de la lignée qui en porte un.
fn tool_name(lineage: &[&str]) -> Option<&'static str> {
    lineage.iter().find_map(|command| {
        let command = normalize_command(command);
        TOOLS
            .iter()
            .copied()
            .find(|tool| contains_tool(&command, tool))
    })
}

/// La commande d'un processus telle qu'on la montre et qu'on la compare : son programme sans son dossier
/// (`/opt/bin/node app.js` devient `node app.js`), et tout interpréteur Python 3 devient `python3` (`python3.12`, et le
/// `Python` du paquet des outils Xcode, que le `python3` de `/usr/bin` lance sous son chemin complet).
fn normalize_command(command: &str) -> String {
    let command = command.trim();
    let (program, rest) = command
        .split_once(char::is_whitespace)
        .map_or((command, ""), |(program, rest)| (program, rest));
    let base = program.rsplit('/').next().unwrap_or(program);
    let lower = base.to_ascii_lowercase();
    let base = if lower == "python" || lower.starts_with("python3") {
        "python3"
    } else {
        base
    };
    if rest.is_empty() {
        base.to_string()
    } else {
        format!("{base} {rest}")
    }
}

/// Vrai si `tool` figure dans `command` comme mot entier : `vite` n'est pas dans `vitest`.
fn contains_tool(command: &str, tool: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    command.match_indices(tool).any(|(start, _)| {
        let before = command[..start].chars().next_back();
        let after = command[start + tool.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// La commande, coupée à `NAME_LIMIT` caractères (le dernier est « … » quand elle est coupée).
fn abbreviate(command: &str) -> String {
    let command = command.trim();
    if command.chars().count() <= NAME_LIMIT {
        return command.to_string();
    }
    let head: String = command.chars().take(NAME_LIMIT - 1).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::super::launch::{ConfigEntry, EntrySource};
    use super::super::scan::{Listener, Process};
    use super::*;

    /// Un relevé construit à la main : on y déclare des processus, des ports en écoute, des dossiers courants.
    #[derive(Default)]
    struct Scene {
        processes: HashMap<i32, Process>,
        listeners: Vec<Listener>,
        cwd: HashMap<i32, PathBuf>,
        stdout: HashMap<i32, OutputTarget>,
        http: HashSet<(i32, u16)>,
    }

    impl Scene {
        fn process(mut self, pid: i32, ppid: i32, command: &str) -> Self {
            self.processes.insert(
                pid,
                Process {
                    pid,
                    ppid,
                    pgid: pid,
                    command: command.to_string(),
                },
            );
            self
        }

        fn listen(mut self, pid: i32, port: u16, http: bool) -> Self {
            self.listeners.push(Listener { pid, port });
            if http {
                self.http.insert((pid, port));
            }
            self
        }

        fn cwd(mut self, pid: i32, cwd: &str) -> Self {
            self.cwd.insert(pid, PathBuf::from(cwd));
            self
        }

        fn stdout(mut self, pid: i32, target: OutputTarget) -> Self {
            self.stdout.insert(pid, target);
            self
        }

        fn snapshot(&self, taken_at: Instant) -> Snapshot {
            Snapshot {
                processes: self.processes.clone(),
                listeners: self.listeners.clone(),
                cwd: self.cwd.clone(),
                stdout: self.stdout.clone(),
                http: self.http.clone(),
                taken_at,
            }
        }
    }

    fn path(text: &str) -> PathBuf {
        PathBuf::from(text)
    }

    fn key(project: &str, name: &str) -> ServerKey {
        ServerKey {
            project: path(project),
            name: name.to_string(),
        }
    }

    fn python_entry(project: &str, name: &str, port: u16) -> ConfigEntry {
        ConfigEntry {
            name: name.to_string(),
            program: "python3".to_string(),
            args: vec![
                "-m".to_string(),
                "http.server".to_string(),
                port.to_string(),
            ],
            cwd: path(project),
            env: Vec::new(),
            port: Some(port),
            url: None,
            source: EntrySource::LaunchJson,
        }
    }

    fn dev_entry(project: &str, script: &str) -> ConfigEntry {
        ConfigEntry {
            name: "dev".to_string(),
            program: "bun".to_string(),
            args: vec!["run".to_string(), "dev".to_string()],
            cwd: path(project),
            env: Vec::new(),
            port: None,
            url: None,
            source: EntrySource::PackageDev {
                script: script.to_string(),
            },
        }
    }

    fn config(entries: Vec<ConfigEntry>) -> ProjectConfig {
        ProjectConfig {
            entries,
            errors: Vec::new(),
        }
    }

    /// `alpha` : `web` sur 8811 et `api` sur 8812 ; `beta` : l'entrée `dev` de `package.json` (spec §8).
    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry.set_projects(vec![
            (
                path("/p/alpha"),
                config(vec![
                    python_entry("/p/alpha", "web", 8811),
                    python_entry("/p/alpha", "api", 8812),
                ]),
            ),
            (
                path("/p/beta"),
                config(vec![dev_entry("/p/beta", "python3 -m http.server 8813")]),
            ),
        ]);
        registry
    }

    fn session_pane(pid: i32, project: &str) -> PaneRoot {
        PaneRoot {
            pid,
            owner: PaneOwner::Session {
                session: format!("idema-{pid}"),
                display_name: format!("Session {pid}"),
                project: path(project),
            },
            agent_command: None,
            dead: false,
        }
    }

    fn terminal_pane(pid: i32, project: &str) -> PaneRoot {
        PaneRoot {
            pid,
            owner: PaneOwner::TerminalPanel {
                project: path(project),
            },
            agent_command: None,
            dead: false,
        }
    }

    fn server_pane(pid: i32, project: &str, name: &str) -> PaneRoot {
        PaneRoot {
            pid,
            owner: PaneOwner::ServerSession {
                key: key(project, name),
            },
            agent_command: None,
            dead: false,
        }
    }

    /// Le pane est listé par tmux, mais son processus est sorti.
    fn dead(mut pane: PaneRoot) -> PaneRoot {
        pane.dead = true;
        pane
    }

    fn roots(panes: Vec<PaneRoot>) -> PaneRoots {
        PaneRoots { panes }
    }

    /// Applique un relevé pris à `base + seconds`.
    fn scan(
        registry: &mut Registry,
        base: Instant,
        seconds: u64,
        scene: &Scene,
        panes: &PaneRoots,
    ) {
        let at = base + Duration::from_secs(seconds);
        registry.apply_scan(&scene.snapshot(at), panes, at);
    }

    fn group<'a>(groups: &'a [ProjectGroup], project: &str) -> &'a ProjectGroup {
        groups
            .iter()
            .find(|group| group.project == path(project))
            .unwrap_or_else(|| panic!("pas de groupe pour {project}"))
    }

    fn row<'a>(group: &'a ProjectGroup, name: &str) -> &'a ServerRow {
        group
            .rows
            .iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("pas de ligne {name}"))
    }

    #[test]
    fn a_config_port_runs_whoever_launched_it() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(500, 1, "python3 -m http.server 8811")
            .listen(500, 8811, true)
            .cwd(500, "/elsewhere")
            .stdout(500, OutputTarget::Other("pipe".to_string()));
        scan(&mut registry, base, 0, &scene, &roots(vec![]));

        let groups = registry.rows(None);
        let alpha = group(&groups, "/p/alpha");
        let web = row(alpha, "web");
        assert_eq!(
            web.state,
            RowState::Running {
                pid: 500,
                port: 8811
            }
        );
        assert_eq!(
            web.origin,
            Some(Origin::Outside {
                command: "python3 -m http.server 8811".to_string()
            })
        );
        assert_eq!(web.listener_pid, Some(500));
        assert_eq!(web.port, Some(8811));
        assert_eq!(web.url, "http://localhost:8811");
        assert_eq!(web.command, "python3 -m http.server 8811");
        assert_eq!(web.output, Some(OutputTarget::Other("pipe".to_string())));
        assert_eq!(row(alpha, "api").state, RowState::Stopped);
        assert_eq!(row(alpha, "api").origin, None);
        assert!(alpha.detected.is_empty());
    }

    #[test]
    fn a_config_port_without_http_still_runs_its_row() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(500, 1, "some-db")
            .listen(500, 8812, false);
        scan(&mut registry, base, 0, &scene, &roots(vec![]));

        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "api").state,
            RowState::Running {
                pid: 500,
                port: 8812
            }
        );
    }

    #[test]
    fn a_detected_server_under_a_session_pane_takes_its_project_and_session() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(10, 1, "-zsh")
            .process(11, 10, "claude")
            .process(12, 11, "node server.js")
            .listen(12, 8820, true)
            .cwd(12, "/somewhere/else");
        // Le pane appartient à une session de beta : le dossier courant ne compte pas.
        scan(
            &mut registry,
            base,
            0,
            &scene,
            &roots(vec![session_pane(10, "/p/beta")]),
        );

        let groups = registry.rows(None);
        let beta = group(&groups, "/p/beta");
        assert_eq!(beta.detected.len(), 1);
        let server = &beta.detected[0];
        assert_eq!(
            server.kind,
            RowKind::Detected {
                pid: 12,
                port: 8820
            }
        );
        assert_eq!(server.project, path("/p/beta"));
        assert_eq!(server.name, "node server.js");
        assert_eq!(server.url, "http://localhost:8820");
        assert_eq!(
            server.state,
            RowState::Running {
                pid: 12,
                port: 8820
            }
        );
        assert_eq!(
            server.origin,
            Some(Origin::Session {
                session: "idema-10".to_string(),
                display_name: "Session 10".to_string()
            })
        );
        assert!(group(&groups, "/p/alpha").detected.is_empty());
    }

    #[test]
    fn a_server_reparented_to_launchd_goes_to_the_project_of_its_cwd() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(30, 1, "python3 -m http.server 8821")
            .listen(30, 8821, true)
            .cwd(30, "/p/alpha/public");
        scan(&mut registry, base, 0, &scene, &roots(vec![]));

        let groups = registry.rows(None);
        let alpha = group(&groups, "/p/alpha");
        assert_eq!(alpha.detected.len(), 1);
        assert_eq!(alpha.detected[0].origin, Some(Origin::Project));
        assert_eq!(alpha.detected[0].name, "python3 -m http.server");
        assert!(group(&groups, "/p/beta").detected.is_empty());
    }

    #[test]
    fn the_deepest_project_holding_the_cwd_wins() {
        let base = Instant::now();
        let mut registry = registry();
        registry.set_projects(vec![
            (path("/p/alpha"), config(vec![])),
            (path("/p/alpha/inner"), config(vec![])),
        ]);
        let scene = Scene::default()
            .process(30, 1, "node dev.js")
            .listen(30, 8821, true)
            .cwd(30, "/p/alpha/inner/src");
        scan(&mut registry, base, 0, &scene, &roots(vec![]));

        let groups = registry.rows(None);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].project, path("/p/alpha/inner"));
    }

    #[test]
    fn a_server_under_the_terminal_panel_says_so() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(40, 1, "-zsh")
            .process(41, 40, "npx serve")
            .process(42, 41, "node serve.js")
            .listen(42, 8822, true);
        scan(
            &mut registry,
            base,
            0,
            &scene,
            &roots(vec![terminal_pane(40, "/p/alpha")]),
        );

        let groups = registry.rows(None);
        let alpha = group(&groups, "/p/alpha");
        assert_eq!(alpha.detected.len(), 1);
        assert_eq!(alpha.detected[0].origin, Some(Origin::TerminalPanel));
    }

    #[test]
    fn a_raw_socket_or_the_node_inspector_is_not_a_server() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(10, 1, "-zsh")
            .process(11, 10, "nc -l 8815")
            .process(12, 10, "node --inspect app.js")
            .listen(11, 8815, false)
            .listen(12, 9229, true)
            .cwd(11, "/p/alpha")
            .cwd(12, "/p/alpha");
        scan(
            &mut registry,
            base,
            0,
            &scene,
            &roots(vec![session_pane(10, "/p/alpha")]),
        );

        let groups = registry.rows(None);
        assert!(groups.iter().all(|group| group.detected.is_empty()));
    }

    #[test]
    fn a_port_outside_any_pane_and_project_is_ignored() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(50, 1, "python3 -m http.server 8830")
            .process(51, 1, "controlcenter")
            .listen(50, 8830, true)
            .listen(51, 7000, true)
            .cwd(50, "/Users/someone/else");
        scan(&mut registry, base, 0, &scene, &roots(vec![]));

        let groups = registry.rows(None);
        assert!(groups.iter().all(|group| group.detected.is_empty()));
        assert_eq!(registry.running_count(), 0);
    }

    #[test]
    fn two_entries_on_one_port_go_to_the_project_of_the_cwd() {
        let base = Instant::now();
        let mut registry = registry();
        registry.set_projects(vec![
            (
                path("/p/alpha"),
                config(vec![python_entry("/p/alpha", "web", 8811)]),
            ),
            (
                path("/p/gamma"),
                config(vec![python_entry("/p/gamma", "site", 8811)]),
            ),
        ]);
        let scene = Scene::default()
            .process(500, 1, "python3 -m http.server 8811")
            .listen(500, 8811, true)
            .cwd(500, "/p/gamma");
        scan(&mut registry, base, 0, &scene, &roots(vec![]));

        let groups = registry.rows(None);
        let site = row(group(&groups, "/p/gamma"), "site");
        assert_eq!(
            site.state,
            RowState::Running {
                pid: 500,
                port: 8811
            }
        );
        assert_eq!(site.origin, Some(Origin::Project));
        let web = row(group(&groups, "/p/alpha"), "web");
        assert_eq!(
            web.state,
            RowState::Running {
                pid: 500,
                port: 8811
            }
        );
        assert_eq!(
            web.origin,
            Some(Origin::PortTakenBy {
                command: "python3 -m http.server 8811".to_string()
            })
        );
        assert!(groups.iter().all(|group| group.detected.is_empty()));

        // Le dossier courant dans l'autre projet : les rôles s'inversent.
        let scene = Scene::default()
            .process(500, 1, "python3 -m http.server 8811")
            .listen(500, 8811, true)
            .cwd(500, "/p/alpha");
        scan(&mut registry, base, 2, &scene, &roots(vec![]));
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").origin,
            Some(Origin::Project)
        );
        assert!(matches!(
            row(group(&groups, "/p/gamma"), "site").origin,
            Some(Origin::PortTakenBy { .. })
        ));
    }

    #[test]
    fn a_server_session_owns_a_port_two_entries_declare() {
        let base = Instant::now();
        let mut registry = registry();
        registry.set_projects(vec![
            (
                path("/p/alpha"),
                config(vec![python_entry("/p/alpha", "web", 8811)]),
            ),
            (
                path("/p/gamma"),
                config(vec![python_entry("/p/gamma", "site", 8811)]),
            ),
        ]);
        // Le dossier courant dit alpha, mais la session serveur est celle de gamma/site.
        let scene = Scene::default()
            .process(60, 1, "-zsh")
            .process(61, 60, "python3 -m http.server 8811")
            .listen(61, 8811, true)
            .cwd(61, "/p/alpha");
        scan(
            &mut registry,
            base,
            0,
            &scene,
            &roots(vec![server_pane(60, "/p/gamma", "site")]),
        );

        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/gamma"), "site").origin,
            Some(Origin::Idema)
        );
        assert!(matches!(
            row(group(&groups, "/p/alpha"), "web").origin,
            Some(Origin::PortTakenBy { .. })
        ));
    }

    #[test]
    fn the_dev_entry_runs_when_its_script_is_running_by_hand() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(699, 1, "sh -c python3 -m http.server 8813")
            .process(700, 699, "python3 -m http.server 8813")
            .listen(700, 8813, true)
            .cwd(700, "/p/beta");
        scan(&mut registry, base, 0, &scene, &roots(vec![]));

        let groups = registry.rows(None);
        let beta = group(&groups, "/p/beta");
        let dev = row(beta, "dev");
        assert_eq!(
            dev.state,
            RowState::Running {
                pid: 700,
                port: 8813
            }
        );
        assert_eq!(dev.origin, Some(Origin::Project));
        assert_eq!(dev.port, Some(8813));
        assert_eq!(dev.url, "http://localhost:8813");
        assert!(beta.detected.is_empty(), "pas de repéré en double");
        // Un serveur lancé à la main ne fait pas retenir son port : seul un démarrage d'IdemA le fait.
        assert!(registry.remembered_dev_ports().is_empty());
    }

    #[test]
    fn the_dev_entry_runs_when_its_script_runs_under_the_full_path_of_the_xcode_python() {
        let base = Instant::now();
        let mut registry = registry();
        // `python3` de /usr/bin lance le Python des outils Xcode sous son chemin complet : le texte du script n'y figure
        // plus tel quel (AC-90 dans la VM).
        let scene = Scene::default()
            .process(10, 1, "-zsh")
            .process(
                700,
                10,
                "/Library/Developer/CommandLineTools/Library/Frameworks/Python3.framework/Versions/3.9/Resources/Python.app/Contents/MacOS/Python -m http.server 8813",
            )
            .listen(700, 8813, true)
            .cwd(700, "/p/beta");
        scan(
            &mut registry,
            base,
            0,
            &scene,
            &roots(vec![session_pane(10, "/p/beta")]),
        );
        let groups = registry.rows(None);
        let beta = group(&groups, "/p/beta");
        assert_eq!(
            row(beta, "dev").state,
            RowState::Running {
                pid: 700,
                port: 8813
            }
        );
        assert!(beta.detected.is_empty(), "pas de repéré en double");
    }

    #[test]
    fn the_dev_entry_runs_when_only_an_ancestor_carries_its_script() {
        let base = Instant::now();
        let mut registry = Registry::new();
        registry.set_projects(vec![(
            path("/p/beta"),
            config(vec![dev_entry("/p/beta", "next dev")]),
        )]);
        // Le serveur lui-même ne porte pas le texte du script, seul son parent le fait.
        let scene = Scene::default()
            .process(10, 1, "-zsh")
            .process(11, 10, "sh -c next dev")
            .process(12, 11, "node server.js")
            .listen(12, 3000, true);
        scan(
            &mut registry,
            base,
            0,
            &scene,
            &roots(vec![session_pane(10, "/p/beta")]),
        );

        let groups = registry.rows(None);
        let beta = group(&groups, "/p/beta");
        assert_eq!(
            row(beta, "dev").state,
            RowState::Running {
                pid: 12,
                port: 3000
            }
        );
        assert!(beta.detected.is_empty());

        // Sans cet ancêtre, le même serveur reste un repéré et la ligne `dev` arrêtée.
        let mut other = Registry::new();
        other.set_projects(vec![(
            path("/p/beta"),
            config(vec![dev_entry("/p/beta", "next dev")]),
        )]);
        let scene = Scene::default()
            .process(10, 1, "-zsh")
            .process(11, 10, "sh -c bun start")
            .process(12, 11, "node server.js")
            .listen(12, 3000, true);
        scan(
            &mut other,
            base,
            0,
            &scene,
            &roots(vec![session_pane(10, "/p/beta")]),
        );
        let groups = other.rows(None);
        let beta = group(&groups, "/p/beta");
        assert_eq!(row(beta, "dev").state, RowState::Stopped);
        assert_eq!(beta.detected.len(), 1);
    }

    #[test]
    fn the_dev_entry_ignores_a_server_of_another_project() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(700, 1, "python3 -m http.server 8813")
            .listen(700, 8813, true)
            .cwd(700, "/p/alpha");
        scan(&mut registry, base, 0, &scene, &roots(vec![]));

        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/beta"), "dev").state,
            RowState::Stopped
        );
        assert_eq!(group(&groups, "/p/alpha").detected.len(), 1);
    }

    #[test]
    fn the_dev_entry_runs_on_its_remembered_port() {
        let base = Instant::now();
        let mut registry = registry();
        registry.restore_dev_ports(HashMap::from([(path("/p/beta"), 3000)]));
        let scene = Scene::default()
            .process(10, 1, "-zsh")
            .process(11, 10, "node app.js")
            .listen(11, 3000, true);
        scan(
            &mut registry,
            base,
            0,
            &scene,
            &roots(vec![session_pane(10, "/p/beta")]),
        );

        let groups = registry.rows(None);
        let beta = group(&groups, "/p/beta");
        assert_eq!(
            row(beta, "dev").state,
            RowState::Running {
                pid: 11,
                port: 3000
            }
        );
        assert!(beta.detected.is_empty());

        // Sans serveur, la ligne garde le port retenu pour l'adresse.
        scan(&mut registry, base, 2, &Scene::default(), &roots(vec![]));
        let groups = registry.rows(None);
        let dev = row(group(&groups, "/p/beta"), "dev");
        assert_eq!(dev.state, RowState::Stopped);
        assert_eq!(dev.port, Some(3000));
    }

    #[test]
    fn starting_becomes_running_at_its_port_and_failed_after_sixty_seconds() {
        let base = Instant::now();
        let mut registry = registry();
        let web = key("/p/alpha", "web");
        let api = key("/p/alpha", "api");
        registry.mark_starting(&web, base);
        registry.mark_starting(&api, base);
        let panes = roots(vec![
            server_pane(60, "/p/alpha", "web"),
            server_pane(70, "/p/alpha", "api"),
        ]);

        let waiting = Scene::default()
            .process(60, 1, "python3 -m http.server 8811")
            .process(70, 1, "python3 -m http.server 8812");
        scan(&mut registry, base, 1, &waiting, &panes);
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").state,
            RowState::Starting
        );

        // `api` ouvre son port ; `web` n'a toujours rien ouvert.
        let opened = waiting.listen(70, 8812, true);
        scan(&mut registry, base, 59, &opened, &panes);
        let groups = registry.rows(None);
        let alpha = group(&groups, "/p/alpha");
        assert_eq!(row(alpha, "web").state, RowState::Starting);
        assert_eq!(
            row(alpha, "api").state,
            RowState::Running {
                pid: 70,
                port: 8812
            }
        );
        assert_eq!(row(alpha, "api").origin, Some(Origin::Idema));

        scan(&mut registry, base, 60, &opened, &panes);
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").state,
            RowState::Failed
        );

        // Un échec reste affiché tant que rien ne change.
        scan(&mut registry, base, 62, &opened, &panes);
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").state,
            RowState::Failed
        );
    }

    #[test]
    fn roots_without_the_session_keep_it_starting() {
        let base = Instant::now();
        let mut registry = registry();
        let web = key("/p/alpha", "web");
        registry.mark_starting(&web, base);

        // Le pane n'est pas encore listé (course avec tmux) : absent ne veut pas dire mort.
        scan(&mut registry, base, 3, &Scene::default(), &roots(vec![]));
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").state,
            RowState::Starting
        );

        // Une session fermée à la main : le délai de démarrage la rattrape.
        scan(&mut registry, base, 60, &Scene::default(), &roots(vec![]));
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").state,
            RowState::Failed
        );

        // Un nouveau démarrage repart de zéro.
        registry.mark_starting(&web, base + Duration::from_secs(70));
        assert_eq!(
            row(&registry.rows(None)[0], "web").state,
            RowState::Starting
        );
    }

    #[test]
    fn a_listed_dead_server_pane_fails_at_the_first_scan() {
        let base = Instant::now();
        let mut registry = registry();
        let web = key("/p/alpha", "web");
        registry.mark_starting(&web, base);

        // Le serveur est sorti aussitôt ; `remain-on-exit` garde le pane, tmux le liste comme mort.
        let exited = Scene::default();
        scan(
            &mut registry,
            base,
            1,
            &exited,
            &roots(vec![dead(server_pane(60, "/p/alpha", "web"))]),
        );
        let groups = registry.rows(None);
        let alpha = group(&groups, "/p/alpha");
        assert_eq!(row(alpha, "web").state, RowState::Failed);
        assert_eq!(row(alpha, "api").state, RowState::Stopped);
    }

    #[test]
    fn a_session_listening_on_another_port_than_declared_stays_visible_stoppable_and_counted() {
        let base = Instant::now();
        let mut registry = registry();
        let web = key("/p/alpha", "web");
        registry.mark_starting(&web, base);
        // `web` déclare 8811, son serveur ouvre 5173 (HTTP) et un port interne qui ne répond pas (9229 est l'inspecteur,
        // 7777 un relais sans HTTP).
        let panes = roots(vec![server_pane(60, "/p/alpha", "web")]);
        let scene = Scene::default()
            .process(60, 1, "node vite")
            .process(61, 60, "node relay")
            .listen(60, 5173, true)
            .listen(61, 7777, false)
            .listen(60, 9229, true);

        scan(&mut registry, base, 5, &scene, &panes);
        let groups = registry.rows(None);
        let web_row = row(group(&groups, "/p/alpha"), "web");
        assert_eq!(web_row.state, RowState::Starting);
        assert!(web_row.session_alive, "la session d'IdemA vit");
        assert_eq!(
            web_row.extra_ports,
            [ExtraPort {
                pid: 60,
                port: 5173
            }],
            "seul le port HTTP hors inspecteur se montre"
        );
        assert_eq!(registry.running_count(), 1, "la pastille le compte");

        // Passé les 60 s : la ligne est en échec, le processus vit toujours et reste visible.
        scan(&mut registry, base, 61, &scene, &panes);
        let groups = registry.rows(None);
        let web_row = row(group(&groups, "/p/alpha"), "web");
        assert_eq!(web_row.state, RowState::Failed);
        assert_eq!(web_row.failure.as_deref(), Some(timeout_failure().as_str()));
        assert!(web_row.session_alive);
        assert_eq!(web_row.extra_ports.len(), 1);
        assert_eq!(registry.running_count(), 1);
        assert!(
            group(&groups, "/p/alpha").detected.is_empty(),
            "pas de ligne repérée en double"
        );
    }

    #[test]
    fn the_ports_of_a_running_row_are_not_listed_as_extras() {
        let base = Instant::now();
        let mut registry = registry();
        let panes = roots(vec![server_pane(60, "/p/alpha", "web")]);
        // L'entrée tient son port 8811 ; un port HTTP interne de l'outil (un serveur de rendu) ne fait pas un serveur de plus.
        let scene = Scene::default()
            .process(60, 1, "python3 -m http.server 8811")
            .listen(60, 8811, true)
            .listen(60, 49152, true);
        scan(&mut registry, base, 1, &scene, &panes);
        let groups = registry.rows(None);
        let web_row = row(group(&groups, "/p/alpha"), "web");
        assert_eq!(
            web_row.state,
            RowState::Running {
                pid: 60,
                port: 8811
            }
        );
        assert!(web_row.extra_ports.is_empty());
        assert_eq!(registry.running_count(), 1);
    }

    #[test]
    fn the_extra_ports_of_a_session_show_when_a_foreign_process_holds_the_declared_port() {
        let base = Instant::now();
        let mut registry = registry();
        let web = key("/p/alpha", "web");
        registry.mark_starting(&web, base);
        // La session d'IdemA ouvre 5173 ; un processus sans pane, dans le projet, tient le port déclaré 8811.
        let panes = roots(vec![server_pane(60, "/p/alpha", "web")]);
        let scene = Scene::default()
            .process(60, 1, "node vite")
            .listen(60, 5173, true)
            .process(900, 1, "python3 -m http.server 8811")
            .cwd(900, "/p/alpha")
            .listen(900, 8811, true);
        scan(&mut registry, base, 1, &scene, &panes);
        let groups = registry.rows(None);
        let web_row = row(group(&groups, "/p/alpha"), "web");
        assert_eq!(
            web_row.state,
            RowState::Running {
                pid: 900,
                port: 8811
            }
        );
        assert_eq!(web_row.origin, Some(Origin::Project));
        assert_eq!(
            web_row.extra_ports,
            [ExtraPort {
                pid: 60,
                port: 5173
            }]
        );
        assert_eq!(registry.running_count(), 2, "la pastille compte les deux");
    }

    #[test]
    fn a_reattached_live_session_without_a_listener_is_starting_not_stopped() {
        let base = Instant::now();
        let mut registry = registry();
        let panes = roots(vec![server_pane(60, "/p/alpha", "web")]);
        let booting = Scene::default().process(60, 1, "python3 -m http.server 8811");

        // Aucun `mark_starting` : IdemA vient de démarrer et retrouve la session d'une instance précédente.
        scan(&mut registry, base, 1, &booting, &panes);
        let groups = registry.rows(None);
        let web_row = row(group(&groups, "/p/alpha"), "web");
        assert_eq!(web_row.state, RowState::Starting);
        assert!(web_row.session_alive);

        // Le délai de démarrage court à partir de la reprise.
        scan(&mut registry, base, 60, &booting, &panes);
        assert_eq!(
            row(&registry.rows(None)[0], "web").state,
            RowState::Starting
        );
        scan(&mut registry, base, 61, &booting, &panes);
        assert_eq!(row(&registry.rows(None)[0], "web").state, RowState::Failed);

        // Une session morte ne se reprend pas en démarrage.
        let mut other = Registry::new();
        other.set_projects(registry.projects.clone());
        scan(
            &mut other,
            base,
            1,
            &booting,
            &roots(vec![dead(server_pane(60, "/p/alpha", "web"))]),
        );
        assert_eq!(row(&other.rows(None)[0], "web").state, RowState::Stopped);
    }

    #[test]
    fn a_session_seen_before_does_not_turn_back_into_starting_while_it_is_being_stopped() {
        let base = Instant::now();
        let mut registry = registry();
        let panes = roots(vec![server_pane(60, "/p/alpha", "web")]);
        let listening = Scene::default()
            .process(60, 1, "python3 -m http.server 8811")
            .listen(60, 8811, true);
        scan(&mut registry, base, 1, &listening, &panes);
        // Le port est libéré, la session est fermée un instant plus tard : entre les deux, le pane est encore listé.
        let freed = Scene::default().process(60, 1, "python3 -m http.server 8811");
        scan(&mut registry, base, 3, &freed, &panes);
        assert_eq!(row(&registry.rows(None)[0], "web").state, RowState::Stopped);
    }

    #[test]
    fn a_failure_keeps_its_cause_until_the_next_start_and_a_stop_clears_it() {
        let base = Instant::now();
        let mut registry = registry();
        let web = key("/p/alpha", "web");
        registry.mark_failed(&web, Some("dossier introuvable : /x".to_owned()));
        let groups = registry.rows(None);
        let failed = row(group(&groups, "/p/alpha"), "web");
        assert_eq!(failed.state, RowState::Failed);
        assert_eq!(failed.failure.as_deref(), Some("dossier introuvable : /x"));
        // La cause ne se montre que sur une ligne en échec.
        registry.mark_starting(&web, base);
        let groups = registry.rows(None);
        assert_eq!(row(group(&groups, "/p/alpha"), "web").failure, None);

        registry.mark_failed(&web, Some("autre".to_owned()));
        registry.mark_stopped(&web);
        let groups = registry.rows(None);
        let stopped = row(group(&groups, "/p/alpha"), "web");
        assert_eq!(stopped.state, RowState::Stopped);
        assert_eq!(stopped.failure, None);
    }

    #[test]
    fn a_dev_entry_matches_its_script_by_whole_words() {
        let base = Instant::now();
        let mut registry = Registry::new();
        registry.set_projects(vec![(
            path("/p/beta"),
            config(vec![dev_entry("/p/beta", "vite")]),
        )]);
        // `vitest --ui` répond en HTTP dans le projet : « vite » est une sous-chaîne de son nom, pas un mot de sa commande.
        let scene = Scene::default()
            .process(700, 1, "node /p/beta/node_modules/.bin/vitest --ui")
            .listen(700, 5000, true)
            .cwd(700, "/p/beta");
        scan(&mut registry, base, 0, &scene, &roots(vec![]));
        let groups = registry.rows(None);
        let beta = group(&groups, "/p/beta");
        assert_eq!(row(beta, "dev").state, RowState::Stopped);
        assert_eq!(beta.detected.len(), 1, "vitest reste un serveur repéré");

        // `vite` lui-même, lui, est bien l'entrée.
        let real = Scene::default()
            .process(710, 1, "node /p/beta/node_modules/.bin/vite --port 3000")
            .listen(710, 3000, true)
            .cwd(710, "/p/beta");
        scan(&mut registry, base, 2, &real, &roots(vec![]));
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/beta"), "dev").state,
            RowState::Running {
                pid: 710,
                port: 3000
            }
        );
    }

    #[test]
    fn a_dead_pane_does_not_own_a_reused_pid() {
        let base = Instant::now();
        let mut registry = registry();
        // Le PID 60 de l'ancien pane mort a été repris par un processus sans lien qui écoute sur 8811.
        let scene = Scene::default()
            .process(60, 1, "python3 -m http.server 8811")
            .listen(60, 8811, true)
            .cwd(60, "/tmp");
        scan(
            &mut registry,
            base,
            1,
            &scene,
            &roots(vec![dead(server_pane(60, "/p/alpha", "web"))]),
        );
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").origin,
            Some(Origin::Outside {
                command: "python3 -m http.server 8811".to_string()
            })
        );
    }

    #[test]
    fn a_running_idema_server_whose_pane_dies_goes_back_to_stopped() {
        let base = Instant::now();
        let mut registry = registry();
        let web = key("/p/alpha", "web");
        registry.mark_starting(&web, base);
        let alive = Scene::default()
            .process(60, 1, "python3 -m http.server 8811")
            .listen(60, 8811, true);
        scan(
            &mut registry,
            base,
            1,
            &alive,
            &roots(vec![server_pane(60, "/p/alpha", "web")]),
        );
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").state,
            RowState::Running {
                pid: 60,
                port: 8811
            }
        );

        // Le serveur meurt plus tard : le pane reste, mort ; la ligne est arrêtée, pas en échec.
        scan(
            &mut registry,
            base,
            30,
            &Scene::default(),
            &roots(vec![dead(server_pane(60, "/p/alpha", "web"))]),
        );
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").state,
            RowState::Stopped
        );
    }

    #[test]
    fn mark_failed_shows_failed_until_the_next_start() {
        let base = Instant::now();
        let mut registry = registry();
        let web = key("/p/alpha", "web");
        registry.mark_starting(&web, base);
        registry.mark_failed(&web, None);
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").state,
            RowState::Failed
        );
    }

    #[test]
    fn a_dev_server_started_by_idema_remembers_its_first_port() {
        let base = Instant::now();
        let mut registry = registry();
        let dev = key("/p/beta", "dev");
        registry.mark_starting(&dev, base);
        let panes = roots(vec![server_pane(80, "/p/beta", "dev")]);

        // Le serveur ouvre l'inspecteur de Node, puis son vrai port : l'inspecteur n'est pas le premier port.
        let scene = Scene::default()
            .process(80, 1, "bun run dev")
            .process(81, 80, "node server.js")
            .listen(81, 9229, true)
            .listen(81, 3001, true);
        scan(&mut registry, base, 4, &scene, &panes);

        let groups = registry.rows(None);
        let beta = group(&groups, "/p/beta");
        let row = row(beta, "dev");
        assert_eq!(
            row.state,
            RowState::Running {
                pid: 81,
                port: 3001
            }
        );
        assert_eq!(row.origin, Some(Origin::Idema));
        assert_eq!(row.url, "http://localhost:3001");
        assert!(beta.detected.is_empty());
        assert_eq!(
            registry.remembered_dev_ports(),
            HashMap::from([(path("/p/beta"), 3001)])
        );

        // Après un redémarrage d'IdemA, la session serveur est encore là : la ligne est en marche sans état.
        let mut restarted = self::registry();
        scan(&mut restarted, base, 100, &scene, &panes);
        let groups = restarted.rows(None);
        assert_eq!(
            self::row(group(&groups, "/p/beta"), "dev").state,
            RowState::Running {
                pid: 81,
                port: 3001
            }
        );
    }

    #[test]
    fn a_detected_row_lingers_five_seconds_then_goes() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(30, 1, "python3 -m http.server 8821")
            .listen(30, 8821, true)
            .cwd(30, "/p/alpha");
        scan(&mut registry, base, 0, &scene, &roots(vec![]));
        assert_eq!(registry.running_count(), 1);

        // Le port disparaît au relevé de 2 s : la ligne passe à « arrêté » mais reste.
        let gone = Scene::default();
        scan(&mut registry, base, 2, &gone, &roots(vec![]));
        let groups = registry.rows(None);
        let alpha = group(&groups, "/p/alpha");
        assert_eq!(alpha.detected.len(), 1);
        assert_eq!(alpha.detected[0].state, RowState::Stopped);
        assert_eq!(alpha.detected[0].listener_pid, None);
        assert_eq!(registry.running_count(), 0);

        scan(&mut registry, base, 6, &gone, &roots(vec![]));
        assert_eq!(group(&registry.rows(None), "/p/alpha").detected.len(), 1);

        // 5 s après la disparition.
        scan(&mut registry, base, 7, &gone, &roots(vec![]));
        assert!(
            registry
                .rows(None)
                .iter()
                .all(|group| group.detected.is_empty())
        );
    }

    #[test]
    fn a_detected_row_that_comes_back_in_time_is_running_again() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(30, 1, "python3 -m http.server 8821")
            .listen(30, 8821, true)
            .cwd(30, "/p/alpha");
        scan(&mut registry, base, 0, &scene, &roots(vec![]));
        scan(&mut registry, base, 2, &Scene::default(), &roots(vec![]));
        scan(&mut registry, base, 4, &scene, &roots(vec![]));
        // Bien plus tard, la ligne ne s'efface pas : elle n'a jamais disparu depuis le retour.
        scan(&mut registry, base, 20, &scene, &roots(vec![]));

        let groups = registry.rows(None);
        assert_eq!(
            group(&groups, "/p/alpha").detected[0].state,
            RowState::Running {
                pid: 30,
                port: 8821
            }
        );
    }

    #[test]
    fn a_new_pid_on_the_same_port_replaces_the_origin() {
        let base = Instant::now();
        let mut registry = registry();
        let web = key("/p/alpha", "web");
        let kind = RowKind::Config(web);

        // Premier serveur : sous le pane d'une session.
        let first = Scene::default()
            .process(10, 1, "-zsh")
            .process(11, 10, "python3 -m http.server 8811")
            .listen(11, 8811, true);
        let panes = roots(vec![session_pane(10, "/p/alpha")]);
        scan(&mut registry, base, 0, &first, &panes);
        let groups = registry.rows(None);
        let web_row = row(group(&groups, "/p/alpha"), "web");
        assert_eq!(web_row.listener_pid, Some(11));
        assert_eq!(
            web_row.origin,
            Some(Origin::Session {
                session: "idema-10".to_string(),
                display_name: "Session 10".to_string()
            })
        );

        // Un arrêt est refusé pour ce PID.
        registry.mark_stop_refused(&kind);
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").state,
            RowState::StopRefused {
                pid: 11,
                port: 8811
            }
        );
        assert_eq!(registry.running_count(), 1);

        // Le port est repris entre deux relevés par un autre processus, hors d'IdemA.
        let second = Scene::default()
            .process(900, 1, "python3 -m http.server 8811 --bind 0.0.0.0")
            .listen(900, 8811, true)
            .cwd(900, "/tmp");
        scan(&mut registry, base, 2, &second, &panes);
        let groups = registry.rows(None);
        let web_row = row(group(&groups, "/p/alpha"), "web");
        assert_eq!(web_row.listener_pid, Some(900));
        assert_eq!(web_row.kind, kind);
        assert_eq!(
            web_row.state,
            RowState::Running {
                pid: 900,
                port: 8811
            },
            "le refus d'arrêt du vieux PID ne vise pas le nouveau"
        );
        assert_eq!(
            web_row.origin,
            Some(Origin::Outside {
                command: "python3 -m http.server 8811 --bind 0.0.0.0".to_string()
            })
        );
    }

    #[test]
    fn a_new_pid_on_the_same_port_replaces_a_detected_row() {
        let base = Instant::now();
        let mut registry = registry();
        let first = Scene::default()
            .process(10, 1, "-zsh")
            .process(11, 10, "vite")
            .listen(11, 5173, true);
        let panes = roots(vec![session_pane(10, "/p/alpha")]);
        scan(&mut registry, base, 0, &first, &panes);
        registry.mark_stop_refused(&RowKind::Detected {
            pid: 11,
            port: 5173,
        });
        let groups = registry.rows(None);
        assert_eq!(
            group(&groups, "/p/alpha").detected[0].state,
            RowState::StopRefused {
                pid: 11,
                port: 5173
            }
        );

        let second = Scene::default()
            .process(950, 1, "node other.js")
            .listen(950, 5173, true)
            .cwd(950, "/p/beta");
        scan(&mut registry, base, 2, &second, &panes);
        let groups = registry.rows(None);
        assert!(group(&groups, "/p/alpha").detected.is_empty());
        let beta = group(&groups, "/p/beta");
        assert_eq!(beta.detected.len(), 1, "une seule ligne, pas de fantôme");
        assert_eq!(
            beta.detected[0].kind,
            RowKind::Detected {
                pid: 950,
                port: 5173
            }
        );
        assert_eq!(beta.detected[0].origin, Some(Origin::Project));
        assert_eq!(
            beta.detected[0].state,
            RowState::Running {
                pid: 950,
                port: 5173
            }
        );
    }

    #[test]
    fn a_stop_refusal_lasts_while_the_same_process_listens() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(500, 1, "python3 -m http.server 8811")
            .listen(500, 8811, true);
        scan(&mut registry, base, 0, &scene, &roots(vec![]));
        registry.mark_stop_refused(&RowKind::Config(key("/p/alpha", "web")));
        scan(&mut registry, base, 2, &scene, &roots(vec![]));
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").state,
            RowState::StopRefused {
                pid: 500,
                port: 8811
            }
        );

        // Le port se libère enfin.
        scan(&mut registry, base, 4, &Scene::default(), &roots(vec![]));
        let groups = registry.rows(None);
        assert_eq!(
            row(group(&groups, "/p/alpha"), "web").state,
            RowState::Stopped
        );
    }

    #[test]
    fn the_context_project_comes_first_then_the_sidebar_order() {
        let mut registry = Registry::new();
        registry.set_projects(vec![
            (
                path("/p/alpha"),
                config(vec![python_entry("/p/alpha", "web", 8811)]),
            ),
            (path("/p/empty"), config(vec![])),
            (
                path("/p/beta"),
                config(vec![python_entry("/p/beta", "web", 8813)]),
            ),
            (
                path("/p/gamma"),
                ProjectConfig {
                    entries: Vec::new(),
                    errors: vec![EntryError {
                        file: path("/p/gamma/.claude/launch.json"),
                        line: Some(4),
                        message: "port absent".to_string(),
                    }],
                },
            ),
        ]);

        let order = |context: Option<&str>| -> Vec<PathBuf> {
            let context = context.map(path);
            registry
                .rows(context.as_deref())
                .into_iter()
                .map(|group| group.project)
                .collect()
        };
        // Un projet sans ligne ni erreur n'a pas de groupe ; une erreur suffit à en avoir un.
        assert_eq!(
            order(None),
            vec![path("/p/alpha"), path("/p/beta"), path("/p/gamma")]
        );
        assert_eq!(
            order(Some("/p/beta")),
            vec![path("/p/beta"), path("/p/alpha"), path("/p/gamma")]
        );
        assert_eq!(
            order(Some("/p/gamma")),
            vec![path("/p/gamma"), path("/p/alpha"), path("/p/beta")]
        );
        assert_eq!(order(Some("/p/unknown")), order(None));
        let gamma = registry.rows(None).pop().unwrap();
        assert_eq!(gamma.errors[0].line, Some(4));
        assert!(gamma.rows.is_empty());
    }

    #[test]
    fn the_tool_name_is_read_from_the_process_or_its_ancestors() {
        // (commande du processus en écoute, commande de son parent, nom attendu)
        let cases = [
            ("node /x/node_modules/.bin/next dev", "-zsh", "next dev"),
            ("next-server (v14.2.3)", "node /x/.bin/next dev", "next dev"),
            ("node /x/node_modules/.bin/vite", "bun run dev", "vite"),
            (
                "workerd serve",
                "node wrangler dev --port 8787",
                "wrangler dev",
            ),
            ("bun --hot server.ts", "-zsh", "bun --hot"),
            ("node astro dev", "-zsh", "astro dev"),
            ("node nuxt dev", "-zsh", "nuxt dev"),
            ("node remix dev", "-zsh", "remix dev"),
            (
                "/usr/bin/python3 -m http.server 8000",
                "-zsh",
                "python3 -m http.server",
            ),
            // Le `python3` de /usr/bin lance le Python des outils Xcode sous son chemin complet.
            (
                "/Library/Developer/CommandLineTools/Library/Frameworks/Python3.framework/Versions/3.9/Resources/Python.app/Contents/MacOS/Python -m http.server 8814",
                "-zsh",
                "python3 -m http.server",
            ),
            (
                "python3.12 -m http.server",
                "-zsh",
                "python3 -m http.server",
            ),
            ("php -S localhost:8000", "-zsh", "php -S"),
            ("ruby bin/rails server", "-zsh", "rails server"),
            ("hugo server -D", "-zsh", "hugo server"),
            ("jekyll serve --watch", "-zsh", "jekyll serve"),
            // Le processus lui-même passe avant ses ancêtres.
            ("node vite", "node next dev", "vite"),
            // Un outil au nom voisin n'est pas l'outil.
            ("vitest --ui", "-zsh", "vitest --ui"),
        ];
        for (index, (command, parent, expected)) in cases.into_iter().enumerate() {
            let base = Instant::now();
            let mut registry = registry();
            let port = 8900 + index as u16;
            let scene = Scene::default()
                .process(10, 1, "-zsh")
                .process(11, 10, parent)
                .process(12, 11, command)
                .listen(12, port, true);
            scan(
                &mut registry,
                base,
                0,
                &scene,
                &roots(vec![session_pane(10, "/p/alpha")]),
            );
            let groups = registry.rows(None);
            assert_eq!(
                group(&groups, "/p/alpha").detected[0].name,
                expected,
                "{command} sous {parent}"
            );
        }
    }

    #[test]
    fn a_command_is_shown_without_the_folder_of_its_program_and_any_python_is_python3() {
        for (raw, shown) in [
            (
                "/opt/bin/node app.js --port 3000",
                "node app.js --port 3000",
            ),
            (
                "node /x/node_modules/.bin/next dev",
                "node /x/node_modules/.bin/next dev",
            ),
            (
                "/usr/bin/python3 -m http.server 8000",
                "python3 -m http.server 8000",
            ),
            ("python3.12 -m http.server", "python3 -m http.server"),
            (
                "/a/b/Python.app/Contents/MacOS/Python -m http.server 8814",
                "python3 -m http.server 8814",
            ),
            (
                "python2.7 -m SimpleHTTPServer",
                "python2.7 -m SimpleHTTPServer",
            ),
            ("-zsh", "-zsh"),
            ("  bun --hot server.ts ", "bun --hot server.ts"),
            ("", ""),
        ] {
            assert_eq!(normalize_command(raw), shown, "{raw}");
        }
    }

    #[test]
    fn a_detected_row_shows_its_command_without_the_program_folder() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(10, 1, "-zsh")
            .process(
                12,
                10,
                "/Library/Developer/CommandLineTools/Library/Frameworks/Python3.framework/Versions/3.9/Resources/Python.app/Contents/MacOS/Python -m http.server 8814",
            )
            .listen(12, 8814, true);
        scan(
            &mut registry,
            base,
            0,
            &scene,
            &roots(vec![session_pane(10, "/p/alpha")]),
        );
        let groups = registry.rows(None);
        let row = &group(&groups, "/p/alpha").detected[0];
        assert_eq!(row.name, "python3 -m http.server");
        assert_eq!(row.command, "python3 -m http.server 8814");
    }

    #[test]
    fn the_pane_shell_is_not_searched_for_a_tool() {
        let base = Instant::now();
        let mut registry = registry();
        // Le shell du pane porte « vite » dans sa ligne de commande : il ne donne pas son nom au serveur.
        let scene = Scene::default()
            .process(10, 1, "zsh -c vite")
            .process(12, 10, "node server.js")
            .listen(12, 8901, true);
        scan(
            &mut registry,
            base,
            0,
            &scene,
            &roots(vec![session_pane(10, "/p/alpha")]),
        );
        let groups = registry.rows(None);
        assert_eq!(
            group(&groups, "/p/alpha").detected[0].name,
            "node server.js"
        );
    }

    #[test]
    fn a_long_command_is_abbreviated_to_forty_characters() {
        let base = Instant::now();
        let mut registry = registry();
        let long = "node /Users/someone/projects/a-very-long-path/server/index.js --flag";
        let scene = Scene::default()
            .process(12, 1, long)
            .listen(12, 8902, true)
            .cwd(12, "/p/alpha");
        scan(&mut registry, base, 0, &scene, &roots(vec![]));
        let groups = registry.rows(None);
        let server = &group(&groups, "/p/alpha").detected[0];
        assert_eq!(server.name.chars().count(), 40);
        assert!(server.name.ends_with('…'));
        assert!(long.starts_with(server.name.trim_end_matches('…')));
        assert_eq!(server.command, long, "la commande complète reste intacte");
    }

    #[test]
    fn an_empty_url_is_treated_as_absent() {
        let mut entry = python_entry("/p/alpha", "web", 8811);
        entry.url = Some(String::new());
        let mut with_url = python_entry("/p/alpha", "api", 8812);
        with_url.url = Some("https://app.test/start".to_string());
        let mut registry = Registry::new();
        registry.set_projects(vec![(path("/p/alpha"), config(vec![entry, with_url]))]);

        let groups = registry.rows(None);
        let alpha = group(&groups, "/p/alpha");
        assert_eq!(row(alpha, "web").url, "http://localhost:8811");
        assert_eq!(row(alpha, "api").url, "https://app.test/start");
    }

    #[test]
    fn a_dev_entry_without_port_or_url_has_an_empty_address() {
        let registry = registry();
        let groups = registry.rows(None);
        let dev = row(group(&groups, "/p/beta"), "dev");
        assert_eq!(dev.port, None);
        assert_eq!(dev.url, "");
        assert_eq!(dev.command, "bun run dev");
    }

    #[test]
    fn the_running_count_counts_running_and_refused_rows_only() {
        let base = Instant::now();
        let mut registry = registry();
        assert_eq!(registry.running_count(), 0);
        let scene = Scene::default()
            .process(500, 1, "python3 -m http.server 8811")
            .process(501, 1, "python3 -m http.server 8812")
            .process(502, 1, "python3 -m http.server 8821")
            .listen(500, 8811, true)
            .listen(501, 8812, true)
            .listen(502, 8821, true)
            .cwd(502, "/p/alpha");
        scan(&mut registry, base, 0, &scene, &roots(vec![]));
        assert_eq!(registry.running_count(), 3);
        registry.mark_stop_refused(&RowKind::Config(key("/p/alpha", "api")));
        assert_eq!(registry.running_count(), 3);
        registry.mark_starting(&key("/p/alpha", "web"), base);
        assert_eq!(registry.running_count(), 2);
    }

    #[test]
    fn one_process_holding_two_entries_counts_once() {
        let base = Instant::now();
        let mut registry = registry();
        registry.set_projects(vec![
            (
                path("/p/alpha"),
                config(vec![python_entry("/p/alpha", "web", 8811)]),
            ),
            (
                path("/p/gamma"),
                config(vec![python_entry("/p/gamma", "site", 8811)]),
            ),
        ]);
        let scene = Scene::default()
            .process(500, 1, "python3 -m http.server 8811")
            .listen(500, 8811, true)
            .cwd(500, "/p/gamma");
        scan(&mut registry, base, 0, &scene, &roots(vec![]));
        let rows: usize = registry
            .rows(None)
            .iter()
            .map(|group| group.rows.len())
            .sum();
        assert_eq!(rows, 2, "les deux lignes sont en marche");
        assert_eq!(registry.running_count(), 1, "pour un seul processus");
    }

    #[test]
    fn removing_a_project_forgets_its_rows_at_the_next_scan() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(500, 1, "python3 -m http.server 8811")
            .listen(500, 8811, true);
        scan(&mut registry, base, 0, &scene, &roots(vec![]));
        registry.set_projects(vec![(path("/p/beta"), ProjectConfig::default())]);
        scan(&mut registry, base, 2, &scene, &roots(vec![]));
        assert!(registry.rows(None).is_empty());
        assert_eq!(registry.running_count(), 0);
    }

    #[test]
    fn a_detected_server_is_reached_through_the_output_of_its_listener() {
        let base = Instant::now();
        let mut registry = registry();
        let scene = Scene::default()
            .process(12, 1, "node dev.js")
            .listen(12, 8903, true)
            .cwd(12, "/p/alpha")
            .stdout(12, OutputTarget::File(path("/tmp/out.log")));
        scan(&mut registry, base, 0, &scene, &roots(vec![]));
        let groups = registry.rows(None);
        assert_eq!(
            group(&groups, "/p/alpha").detected[0].output,
            Some(OutputTarget::File(path("/tmp/out.log")))
        );
    }
}
