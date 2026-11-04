//! L'instantané des sessions d'IdemA (`snapshot.json`, spec §3.2 et §3.3) : ce que tmux montre, ramené à ce qu'il faut
//! pour refaire les sessions après un redémarrage, et le gardien qui l'empêche d'être vidé par l'extinction.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use idema_tmux::protocol::PaneId;
use idema_tmux::server::{CapturedPane, ListedSession};
use serde::{Deserialize, Serialize};

use super::claude::descends_from;
use crate::config::{AgentEntry, IdeationConfig};
use crate::ideation::ideation_command;
use crate::servers::model::{idnma_entry, is_bare_shell};
use crate::servers::scan::Process;
use crate::servers::stop::looks_like_agent;

/// Version écrite par cette build. Un fichier d'une autre version est ignoré et laissé en place.
pub const SNAPSHOT_VERSION: u32 = 1;
/// L'âge d'une retenue avant qu'une session plus pauvre (ou absente) soit validée : l'extinction du Mac ferme les
/// panes l'un après l'autre en quelques secondes, et ce qu'elle laisse ne doit jamais remplacer l'instantané.
pub const HOLD: Duration = Duration::from_secs(10);

/// L'instantané écrit dans `snapshot.json`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub version: u32,
    pub sessions: Vec<SavedSession>,
}

impl Default for Snapshot {
    /// Vide mais à la version courante : un `version: 0` serait refusé à la relecture.
    fn default() -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            sessions: Vec::new(),
        }
    }
}

/// Une session d'IdemA telle qu'elle était.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedSession {
    /// Le nom de la session tmux.
    pub name: String,
    pub project: PathBuf,
    /// Le nom affiché dans la barre.
    pub display_name: String,
    /// `@idema_created`, en millisecondes.
    pub created: u64,
    /// `@idema_layout`.
    pub layout: String,
    /// La session est un bouillon de neurones.
    pub ideation: bool,
    /// Le nom Remote Control du bouillon.
    pub remote_name: String,
    pub panes: Vec<SavedPane>,
}

/// Un pane d'une session : de quoi le recréer et y relancer son agent.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedPane {
    pub cwd: PathBuf,
    /// `@idema_agent`, l'entrée du lanceur.
    pub agent: String,
    /// `@idema_hidden` : `Some` pour les panes de la fenêtre cachée.
    pub hidden_at: Option<u64>,
    /// Pane actif de la fenêtre courante.
    pub active: bool,
    /// L'identifiant de la conversation Claude Code qui tournait dans le pane.
    pub conversation: Option<String>,
    /// La commande à relancer dans le pane ; `None` pour un shell nu.
    pub command: Option<String>,
}

/// Les millisecondes de tmux (`u128`) ramenées à `u64`, saturées plutôt que tronquées.
fn to_millis(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// La commande de lancement de l'agent d'un pane : l'entrée du dernier `idnma run` tapé dans le pane, sinon l'entrée
/// du lanceur (`@idema_agent`) sauf shell nu. C'est la règle de `build_pane_roots`.
fn resolve_command<'a>(pane: &CapturedPane, agents: &'a [AgentEntry]) -> Option<&'a str> {
    let launched = (!is_bare_shell(&pane.agent))
        .then(|| agents.iter().find(|entry| entry.name == pane.agent))
        .flatten();
    idnma_entry(&pane.idnma_agent, launched, agents)
        .or(launched)
        .map(|entry| entry.command.as_str())
}

/// Un agent tourne au premier plan du pane : son shell ou l'un de ses descendants a l'allure de l'agent.
fn has_foreground_agent(
    pane: &CapturedPane,
    resolved: Option<&str>,
    processes: &HashMap<i32, Process>,
) -> bool {
    processes.values().any(|process| {
        looks_like_agent(&process.command, resolved)
            && descends_from(process.pid, pane.pane_pid, processes)
    })
}

/// La commande de relance d'une session ramenée dans le pane (`@idema_resume` = `<conversation> <commande>`), quand elle
/// est celle de la conversation qui tourne. Une valeur mal formée (sans espace, commande vide, caractère de contrôle)
/// ou posée pour une autre conversation est ignorée : le pane a changé de main depuis, la règle ordinaire s'applique.
fn reclaimed_command<'a>(pane: &'a CapturedPane, conversation: &str) -> Option<&'a str> {
    let (id, command) = pane.resume.split_once(' ')?;
    (id == conversation && !command.trim().is_empty() && !command.chars().any(char::is_control))
        .then_some(command)
}

/// Un pane de la capture, dans sa forme sauvegardée.
fn saved_pane(
    session: &ListedSession,
    pane: &CapturedPane,
    conversations: &HashMap<(String, PaneId), String>,
    agents: &[AgentEntry],
    processes: &HashMap<i32, Process>,
    ideation: Option<&IdeationConfig>,
) -> SavedPane {
    let conversation = conversations
        .get(&(pane.session.clone(), pane.pane))
        .cloned();
    let resolved = resolve_command(pane, agents);
    let has_agent = has_foreground_agent(pane, resolved, processes);
    let ideation_launch = ideation
        .filter(|_| session.ideation && (conversation.is_some() || has_agent))
        .map(|config| ideation_command(config, &session.remote_name));
    // Section `[ideation]` absente : le bouillon est capturé sans commande (jamais celle d'un lanceur de projet).
    let command = if session.ideation {
        ideation_launch
    } else if let Some(command) = conversation
        .as_deref()
        .and_then(|id| reclaimed_command(pane, id))
    {
        // Une session ramenée garde la commande avec laquelle elle tournait, pas l'entrée du lanceur (défaut D-1).
        Some(command.to_string())
    } else if conversation.is_some() {
        Some(resolved.unwrap_or("claude").to_string())
    } else if has_agent {
        resolved.map(str::to_string)
    } else {
        None
    };
    SavedPane {
        cwd: pane.cwd.clone(),
        agent: pane.agent.clone(),
        hidden_at: pane.stash.then(|| to_millis(pane.hidden_at)),
        active: pane.pane_active && pane.window_active && !pane.stash,
        conversation,
        command,
    }
}

/// Ce que tmux montre, ramené à des `SavedSession` : une par session qui a au moins un pane (une session sans pane,
/// vue entre deux lectures non atomiques, est laissée de côté), triées par création puis par nom. Les panes gardent
/// l'ordre de la capture, ceux de la fenêtre cachée viennent après, du plus récemment caché au plus ancien.
pub fn build_capture(
    sessions: &[ListedSession],
    panes: &[CapturedPane],
    conversations: &HashMap<(String, PaneId), String>,
    agents: &[AgentEntry],
    processes: &HashMap<i32, Process>,
    ideation: Option<&IdeationConfig>,
) -> Vec<SavedSession> {
    let mut saved: Vec<SavedSession> = sessions
        .iter()
        .filter_map(|session| {
            let own: Vec<&CapturedPane> = panes
                .iter()
                .filter(|pane| pane.session == session.name)
                .collect();
            if own.is_empty() {
                return None;
            }
            let (stashed, visible): (Vec<_>, Vec<_>) = own.into_iter().partition(|pane| pane.stash);
            let mut stashed = stashed;
            // L'ordre physique de la fenêtre cachée ne compte pas : IdemA ne lit que `@idema_hidden`.
            stashed.sort_by_key(|pane| std::cmp::Reverse(pane.hidden_at));
            Some(SavedSession {
                name: session.name.clone(),
                project: PathBuf::from(&session.project),
                display_name: session.display_name.clone(),
                created: to_millis(session.created),
                layout: session.layout.clone(),
                ideation: session.ideation,
                remote_name: session.remote_name.clone(),
                panes: visible
                    .into_iter()
                    .chain(stashed)
                    .map(|pane| {
                        saved_pane(session, pane, conversations, agents, processes, ideation)
                    })
                    .collect(),
            })
        })
        .collect();
    saved.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.name.cmp(&b.name)));
    saved
}

/// Ce que la lecture de `snapshot.json` a trouvé.
#[derive(Debug, PartialEq)]
pub enum SnapshotLoad {
    /// Pas de fichier (ou illisible au niveau du disque) : rien à restaurer, le premier instantané s'écrira.
    Absent,
    /// L'instantané de la version courante.
    Loaded(Snapshot),
    /// Illisible : mis de côté en `snapshot.json.bak`, comme `state.json`.
    Unreadable,
    /// D'une autre version que `SNAPSHOT_VERSION` (un IdemA plus récent l'a écrit). Le fichier reste en place et
    /// IdemA ne l'écrit jamais pendant tout ce lancement, comme `state.json` devant une version plus récente.
    UnknownVersion(u64),
}

impl SnapshotLoad {
    /// L'instantané à garder, vide sauf s'il a été lu.
    pub fn into_snapshot(self) -> Snapshot {
        match self {
            Self::Loaded(snapshot) => snapshot,
            _ => Snapshot::default(),
        }
    }
}

/// Lit l'instantané. Absent : `Absent`. D'une autre version : `UnknownVersion`, le fichier reste en place. Illisible :
/// mis de côté en `snapshot.json.bak` et `Unreadable`.
pub fn load_snapshot(path: &Path) -> SnapshotLoad {
    let Ok(text) = std::fs::read_to_string(path) else {
        return SnapshotLoad::Absent;
    };
    let version = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value.get("version")?.as_u64());
    if let Some(version) = version
        && version != u64::from(SNAPSHOT_VERSION)
    {
        return SnapshotLoad::UnknownVersion(version);
    }
    match serde_json::from_str::<Snapshot>(&text) {
        Ok(snapshot) => SnapshotLoad::Loaded(snapshot),
        Err(_) => {
            // Si le renommage échoue aussi, le fichier reste et sera écrasé par la prochaine sauvegarde : l'instantané
            // est de toute façon perdu, il n'y a rien de plus à en tirer.
            let _ = std::fs::rename(path, path.with_extension("json.bak"));
            SnapshotLoad::Unreadable
        }
    }
}

/// Écrit l'instantané de façon atomique : fichier temporaire, puis renommage.
pub fn save_snapshot(path: &Path, snapshot: &Snapshot) -> std::io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(snapshot).map_err(std::io::Error::other)?;
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all((text + "\n").as_bytes())?;
    // Les octets sont sur le disque avant le renommage : une coupure de courant juste après ne laisse pas un
    // instantané vide à la place du bon.
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
}

/// La file de l'écrivain : une seule écriture en attente, la plus récente. Pure (ni fichier ni fil), donc testable.
#[derive(Debug, Default)]
struct WriteQueue {
    pending: Option<Snapshot>,
    /// Un fil d'écriture tourne et reviendra lire `pending` avant de s'arrêter.
    running: bool,
    /// La dernière écriture a échoué.
    failed: bool,
}

impl WriteQueue {
    /// Remplace l'écriture en attente par `snapshot` ; vrai si un fil d'écriture doit être lancé.
    fn submit(&mut self, snapshot: Snapshot) -> bool {
        self.pending = Some(snapshot);
        !std::mem::replace(&mut self.running, true)
    }

    /// La prochaine écriture à faire. Sans rien en attente, le fil s'arrête (`running` retombe) dans le même geste :
    /// un `submit` qui suit relance un fil au lieu de compter sur un fil qui part.
    fn next(&mut self) -> Option<Snapshot> {
        let next = self.pending.take();
        if next.is_none() {
            self.running = false;
        }
        next
    }
}

/// L'écrivain de `snapshot.json`, hors du fil d'interface : `sync_all` coûte 5 à 15 ms sur macOS, et l'instantané
/// change à chaque changement de pane actif. Un seul fil écrit à la fois et seule la dernière version demandée part :
/// une écriture plus ancienne ne peut jamais se renommer après une plus récente (« Fermer la session », AC-4).
#[derive(Debug)]
pub struct SnapshotWriter {
    path: PathBuf,
    /// Le fichier est d'une version inconnue : rien n'est jamais écrit.
    read_only: bool,
    queue: std::sync::Arc<std::sync::Mutex<WriteQueue>>,
}

impl SnapshotWriter {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            read_only: false,
            queue: Default::default(),
        }
    }

    /// Un écrivain qui n'écrit jamais : le `snapshot.json` en place est d'une version que cette build ne connaît pas.
    pub fn read_only(path: PathBuf) -> Self {
        Self {
            read_only: true,
            ..Self::new(path)
        }
    }

    /// Demande l'écriture de `snapshot` ; rend la main tout de suite.
    pub fn submit(&self, snapshot: Snapshot) {
        if self.read_only {
            return;
        }
        let spawn = lock(&self.queue).submit(snapshot);
        if !spawn {
            return;
        }
        let path = self.path.clone();
        let queue = std::sync::Arc::clone(&self.queue);
        let spawned = std::thread::Builder::new()
            .name("idema-snapshot".into())
            .spawn(move || Self::drain(&path, &queue));
        if let Err(error) = spawned {
            let mut state = lock(&self.queue);
            state.running = false;
            state.failed = true;
            crate::hud::log_event(&format!("snapshot save failed: {error}"));
        }
    }

    /// La dernière écriture a échoué : l'appelant redemande à la capture suivante.
    pub fn failed(&self) -> bool {
        lock(&self.queue).failed
    }

    fn drain(path: &Path, queue: &std::sync::Mutex<WriteQueue>) {
        loop {
            // Le verrou est lâché avant l'écriture disque : `submit` ne doit jamais attendre un `sync_all`.
            let next = lock(queue).next();
            let Some(snapshot) = next else {
                break;
            };
            let result = save_snapshot(path, &snapshot);
            let mut state = lock(queue);
            if let Err(error) = &result {
                // Une ligne par panne, pas une par tentative.
                if !state.failed {
                    crate::hud::log_event(&format!("snapshot save failed: {error}"));
                }
            }
            state.failed = result.is_err();
        }
    }
}

fn lock(queue: &std::sync::Mutex<WriteQueue>) -> std::sync::MutexGuard<'_, WriteQueue> {
    queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Pourquoi une session est gardée telle quelle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldReason {
    /// La capture a moins de panes, de conversations ou de commandes que l'instantané.
    Poorer,
    /// La capture ne la contient plus.
    Missing,
}

impl HoldReason {
    /// Le mot écrit dans le journal.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Poorer => "poorer",
            Self::Missing => "missing",
        }
    }
}

/// Ce qu'une capture a changé.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// L'instantané diffère d'avant l'appel.
    pub changed: bool,
    /// Les retenues qui commencent à cette capture.
    pub held: Vec<(String, HoldReason)>,
}

/// Le nombre de panes, de conversations et de commandes : ce que la restauration perdrait.
fn richness(session: &SavedSession) -> (usize, usize, usize) {
    let count = |has: fn(&SavedPane) -> bool| session.panes.iter().filter(|pane| has(pane)).count();
    (
        session.panes.len(),
        count(|pane| pane.conversation.is_some()),
        count(|pane| pane.command.is_some()),
    )
}

/// `capture` perd quelque chose de ce que `committed` garde.
fn is_poorer(capture: &SavedSession, committed: &SavedSession) -> bool {
    let (panes, conversations, commands) = richness(capture);
    let (kept_panes, kept_conversations, kept_commands) = richness(committed);
    panes < kept_panes || conversations < kept_conversations || commands < kept_commands
}

/// Le gardien de l'instantané : il applique la règle de l'extinction (spec §3.3). Une session plus pauvre ou absente
/// n'est validée qu'une fois la retenue âgée de `HOLD` ; une session fermée dans IdemA (`forget`) part tout de suite.
#[derive(Debug)]
pub struct SnapshotKeeper {
    committed: Snapshot,
    /// Début de la retenue de chaque session gardée telle quelle.
    holds: HashMap<String, Instant>,
    /// Les sessions fermées dans IdemA : une capture partie avant la fermeture ne les fait pas revenir.
    tombstones: HashSet<String>,
    suspended: bool,
}

impl SnapshotKeeper {
    pub fn new(committed: Snapshot) -> Self {
        Self {
            committed,
            holds: HashMap::new(),
            tombstones: HashSet::new(),
            suspended: false,
        }
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.committed
    }

    /// Applique une capture à l'instant `now`. Suspendu, rien ne change.
    pub fn apply(&mut self, capture: Vec<SavedSession>, now: Instant) -> Applied {
        if self.suspended {
            return Applied {
                changed: false,
                held: Vec::new(),
            };
        }
        // Une pierre tombale tombe dès qu'une capture ne contient plus le nom ; tant qu'il y est, c'est une capture
        // périmée qui est ignorée.
        self.tombstones
            .retain(|name| capture.iter().any(|session| &session.name == name));
        let capture: Vec<SavedSession> = capture
            .into_iter()
            .filter(|session| !self.tombstones.contains(&session.name))
            .collect();

        let before = self.committed.sessions.clone();
        let mut held = Vec::new();
        let mut next = Vec::with_capacity(capture.len());
        for old in before.iter() {
            let current = capture.iter().find(|session| session.name == old.name);
            let reason = match current {
                Some(current) if is_poorer(current, old) => Some(HoldReason::Poorer),
                Some(_) => None,
                None => Some(HoldReason::Missing),
            };
            let Some(reason) = reason else {
                self.holds.remove(&old.name);
                next.extend(current.cloned());
                continue;
            };
            let started = *self.holds.entry(old.name.clone()).or_insert_with(|| {
                held.push((old.name.clone(), reason));
                now
            });
            if now.saturating_duration_since(started) < HOLD {
                next.push(old.clone());
            } else {
                self.holds.remove(&old.name);
                next.extend(current.cloned());
            }
        }
        for session in capture {
            if !before.iter().any(|old| old.name == session.name) {
                next.push(session);
            }
        }
        self.holds
            .retain(|name, _| next.iter().any(|session| &session.name == name));
        // Comme `build_capture` : l'ordre du fichier ne dépend pas de l'ordre d'arrivée des sessions.
        next.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.name.cmp(&b.name)));
        self.committed.sessions = next;
        Applied {
            changed: self.committed.sessions != before,
            held,
        }
    }

    /// Retire la session tout de suite, même suspendu, et pose sa pierre tombale. `true` si elle était dans
    /// l'instantané.
    pub fn forget(&mut self, name: &str) -> bool {
        self.holds.remove(name);
        self.tombstones.insert(name.to_string());
        let before = self.committed.sessions.len();
        self.committed
            .sessions
            .retain(|session| session.name != name);
        self.committed.sessions.len() != before
    }

    /// Gèle l'instantané (la restauration reconstruit les sessions : ce que tmux montre alors n'est pas l'état voulu).
    pub fn suspend(&mut self) {
        self.suspended = true;
    }

    /// Reprend les captures. Les retenues repartent de zéro : le temps passé suspendu ne compte pas.
    pub fn resume(&mut self) {
        self.suspended = false;
        self.holds.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idema_tmux::protocol::WindowId;

    const CONVERSATION_U: &str = "aaaa1111-bbbb-4ccc-8ddd-eeeeffff0000";

    fn listed(name: &str, created: u128) -> ListedSession {
        ListedSession {
            name: name.into(),
            project: "/p/alpha".into(),
            created,
            layout: "default".into(),
            ideation: false,
            remote_name: String::new(),
            display_name: format!("shown {name}"),
        }
    }

    fn captured(session: &str, pane: u32, pane_pid: i32) -> CapturedPane {
        CapturedPane {
            session: session.into(),
            window: WindowId(1),
            stash: false,
            pane: PaneId(pane),
            pane_active: true,
            window_active: true,
            pane_pid,
            hidden_at: 0,
            agent: String::new(),
            idnma_agent: String::new(),
            resume: String::new(),
            cwd: PathBuf::from("/p/alpha"),
        }
    }

    fn process(pid: i32, ppid: i32, command: &str) -> (i32, Process) {
        let process = Process {
            pid,
            ppid,
            pgid: pid,
            command: command.into(),
        };
        (pid, process)
    }

    fn entry(name: &str, command: &str) -> AgentEntry {
        AgentEntry {
            name: name.into(),
            harness: None,
            command: command.into(),
            default: false,
            icon: None,
        }
    }

    fn romain() -> IdeationConfig {
        IdeationConfig {
            dir: "/Users/Shared/erom-ideation".into(),
            model: Some("fable".into()),
            effort: Some("max".into()),
            label: "Fable".into(),
        }
    }

    fn conversation_map(session: &str, pane: u32, id: &str) -> HashMap<(String, PaneId), String> {
        HashMap::from([((session.to_string(), PaneId(pane)), id.to_string())])
    }

    fn first_pane(sessions: &[SavedSession]) -> &SavedPane {
        &sessions[0].panes[0]
    }

    // build_capture

    #[test]
    fn a_claude_pane_gets_its_conversation_and_its_launcher_command() {
        let mut pane = captured("s", 1, 10);
        pane.agent = "Claude".into();
        pane.idnma_agent = "claude".into();
        let saved = build_capture(
            &[listed("s", 1)],
            &[pane],
            &conversation_map("s", 1, CONVERSATION_U),
            &[entry("Claude", "idnma run claude")],
            &HashMap::new(),
            None,
        );
        let pane = first_pane(&saved);
        assert_eq!(pane.command.as_deref(), Some("idnma run claude"));
        assert_eq!(pane.conversation.as_deref(), Some(CONVERSATION_U));
        assert_eq!(pane.agent, "Claude");
    }

    /// Un pane d'agent « Opus 5.5 » (entrée `--model opus`) où une session a été ramenée avec la commande `reclaimed`.
    fn reclaimed_pane(resume: &str) -> CapturedPane {
        let mut pane = captured("s", 1, 10);
        pane.agent = "Opus 5.5".into();
        pane.idnma_agent = "claude".into();
        pane.resume = resume.into();
        pane
    }

    fn capture_command(pane: CapturedPane, conversation: Option<&str>) -> Option<String> {
        let conversations =
            conversation.map_or_else(HashMap::new, |id| conversation_map("s", 1, id));
        let saved = build_capture(
            &[listed("s", 1)],
            &[pane],
            &conversations,
            &[entry("Opus 5.5", "idnma run claude --model opus")],
            &HashMap::new(),
            None,
        );
        first_pane(&saved).command.clone()
    }

    #[test]
    fn a_reclaimed_session_keeps_the_command_it_was_brought_back_with() {
        let resume = format!("{CONVERSATION_U} idnma run 'claude' --model 'haiku'");
        assert_eq!(
            capture_command(reclaimed_pane(&resume), Some(CONVERSATION_U)).as_deref(),
            Some("idnma run 'claude' --model 'haiku'")
        );
    }

    #[test]
    fn a_reclaim_note_for_another_conversation_is_ignored() {
        let resume = format!("{CONVERSATION_U} idnma run 'claude' --model 'haiku'");
        assert_eq!(
            capture_command(reclaimed_pane(&resume), Some("V")).as_deref(),
            Some("idnma run claude --model opus")
        );
    }

    #[test]
    fn a_reclaim_note_without_a_conversation_changes_nothing() {
        let resume = format!("{CONVERSATION_U} idnma run 'claude' --model 'haiku'");
        // Sans agent au premier plan ni conversation, aucune commande : la note seule ne relance rien.
        assert_eq!(capture_command(reclaimed_pane(&resume), None), None);
    }

    #[test]
    fn a_malformed_reclaim_note_is_ignored() {
        for resume in [
            CONVERSATION_U.to_string(),
            format!("{CONVERSATION_U} "),
            format!("{CONVERSATION_U}   "),
            format!("{CONVERSATION_U} idnma run 'a\u{15}b'"),
            String::new(),
        ] {
            assert_eq!(
                capture_command(reclaimed_pane(&resume), Some(CONVERSATION_U)).as_deref(),
                Some("idnma run claude --model opus"),
                "{resume:?}"
            );
        }
    }

    #[test]
    fn an_ideation_pane_keeps_the_ideation_command_over_a_reclaim_note() {
        let mut session = listed("s", 1);
        session.ideation = true;
        session.remote_name = "erom-ideation-3".into();
        let resume = format!("{CONVERSATION_U} idnma run 'claude' --model 'haiku'");
        let saved = build_capture(
            &[session],
            &[reclaimed_pane(&resume)],
            &conversation_map("s", 1, CONVERSATION_U),
            &[entry("Opus 5.5", "idnma run claude --model opus")],
            &HashMap::new(),
            Some(&romain()),
        );
        assert_eq!(
            first_pane(&saved).command.as_deref(),
            Some(ideation_command(&romain(), "erom-ideation-3").as_str())
        );
    }

    #[test]
    fn a_pane_with_a_conversation_but_no_launcher_relaunches_claude() {
        let saved = build_capture(
            &[listed("s", 1)],
            &[captured("s", 1, 10)],
            &conversation_map("s", 1, CONVERSATION_U),
            &[entry("Claude", "idnma run claude")],
            &HashMap::new(),
            None,
        );
        assert_eq!(first_pane(&saved).command.as_deref(), Some("claude"));
        assert_eq!(
            first_pane(&saved).conversation.as_deref(),
            Some(CONVERSATION_U)
        );
    }

    #[test]
    fn a_non_claude_agent_running_keeps_its_command_without_conversation() {
        let mut pane = captured("s", 1, 10);
        pane.idnma_agent = "codex".into();
        let processes = HashMap::from([
            process(10, 1, "zsh"),
            process(11, 10, "bun /x/idnma run codex"),
        ]);
        let saved = build_capture(
            &[listed("s", 1)],
            &[pane],
            &HashMap::new(),
            &[entry("Codex", "idnma run codex")],
            &processes,
            None,
        );
        assert_eq!(
            first_pane(&saved).command.as_deref(),
            Some("idnma run codex")
        );
        assert_eq!(first_pane(&saved).conversation, None);
    }

    #[test]
    fn an_idnma_run_typed_by_hand_wins_over_the_launcher_entry() {
        let mut pane = captured("s", 1, 10);
        pane.agent = "Claude".into();
        pane.idnma_agent = "codex".into();
        let processes = HashMap::from([
            process(10, 1, "zsh"),
            process(11, 10, "bun /x/idnma run codex"),
        ]);
        let saved = build_capture(
            &[listed("s", 1)],
            &[pane],
            &HashMap::new(),
            &[
                entry("Claude", "idnma run claude"),
                entry("Codex", "idnma run codex"),
            ],
            &processes,
            None,
        );
        assert_eq!(
            first_pane(&saved).command.as_deref(),
            Some("idnma run codex")
        );
    }

    #[test]
    fn a_bare_shell_has_no_command() {
        let mut pane = captured("s", 1, 10);
        pane.agent = "Claude".into();
        let processes = HashMap::from([process(10, 1, "zsh"), process(11, 10, "vim notes.md")]);
        let saved = build_capture(
            &[listed("s", 1)],
            &[pane],
            &HashMap::new(),
            &[entry("Claude", "idnma run claude")],
            &processes,
            None,
        );
        assert_eq!(first_pane(&saved).command, None);
        assert_eq!(first_pane(&saved).conversation, None);
    }

    #[test]
    fn an_agent_running_without_a_launcher_or_a_conversation_has_no_command() {
        // Aucune entrée du lanceur ne le décrit et aucun fichier de session ne le lie : rien de sûr à relancer.
        let processes = HashMap::from([process(10, 1, "zsh"), process(11, 10, "codex")]);
        let saved = build_capture(
            &[listed("s", 1)],
            &[captured("s", 1, 10)],
            &HashMap::new(),
            &[],
            &processes,
            None,
        );
        assert_eq!(first_pane(&saved).command, None);
    }

    #[test]
    fn an_agent_that_is_the_pane_process_itself_counts() {
        // Le shell de connexion a fait `exec` : le `pane_pid` est l'agent lui-même.
        let mut pane = captured("s", 1, 10);
        pane.agent = "Codex".into();
        let processes = HashMap::from([process(10, 1, "codex --yolo")]);
        let saved = build_capture(
            &[listed("s", 1)],
            &[pane],
            &HashMap::new(),
            &[entry("Codex", "codex --yolo")],
            &processes,
            None,
        );
        assert_eq!(first_pane(&saved).command.as_deref(), Some("codex --yolo"));
    }

    #[test]
    fn an_agent_of_another_pane_does_not_count() {
        let mut pane = captured("s", 1, 10);
        pane.agent = "Codex".into();
        let processes = HashMap::from([
            process(10, 1, "zsh"),
            process(20, 1, "zsh"),
            process(21, 20, "codex --yolo"),
        ]);
        let saved = build_capture(
            &[listed("s", 1)],
            &[pane],
            &HashMap::new(),
            &[entry("Codex", "codex --yolo")],
            &processes,
            None,
        );
        assert_eq!(first_pane(&saved).command, None);
    }

    #[test]
    fn an_ideation_pane_relaunches_the_ideation_command() {
        let mut session = listed("s", 1);
        session.ideation = true;
        session.remote_name = "erom-ideation-3".into();
        let saved = build_capture(
            &[session],
            &[captured("s", 1, 10)],
            &conversation_map("s", 1, "Z"),
            &[],
            &HashMap::new(),
            Some(&romain()),
        );
        let pane = first_pane(&saved);
        assert_eq!(
            pane.command.as_deref(),
            Some(ideation_command(&romain(), "erom-ideation-3").as_str())
        );
        assert_eq!(pane.conversation.as_deref(), Some("Z"));
        assert!(saved[0].ideation);
        assert_eq!(saved[0].remote_name, "erom-ideation-3");
    }

    #[test]
    fn an_ideation_pane_with_no_agent_and_no_conversation_has_no_command() {
        let mut session = listed("s", 1);
        session.ideation = true;
        session.remote_name = "erom-ideation-3".into();
        let saved = build_capture(
            &[session],
            &[captured("s", 1, 10)],
            &HashMap::new(),
            &[],
            &HashMap::new(),
            Some(&romain()),
        );
        assert_eq!(first_pane(&saved).command, None);
    }

    #[test]
    fn an_ideation_pane_captured_while_disabled_has_no_command() {
        let mut session = listed("s", 1);
        session.ideation = true;
        session.remote_name = "erom-ideation-3".into();
        let saved = build_capture(
            &[session],
            &[captured("s", 1, 10)],
            &conversation_map("s", 1, "Z"),
            &[],
            &HashMap::new(),
            None,
        );
        let pane = first_pane(&saved);
        assert_eq!(pane.command, None);
        assert_eq!(pane.conversation.as_deref(), Some("Z"));
        assert!(saved[0].ideation);
    }

    #[test]
    fn stashed_panes_come_last_with_their_hidden_at() {
        let mut hidden = captured("s", 1, 10);
        hidden.stash = true;
        hidden.hidden_at = 777;
        let saved = build_capture(
            &[listed("s", 1)],
            &[hidden, captured("s", 2, 20), captured("s", 3, 30)],
            &HashMap::new(),
            &[],
            &HashMap::new(),
            None,
        );
        let panes = &saved[0].panes;
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].hidden_at, None);
        assert_eq!(panes[1].hidden_at, None);
        assert_eq!(panes[2].hidden_at, Some(777));
    }

    #[test]
    fn stashed_panes_are_ordered_by_hidden_at() {
        let hidden = |pane: u32, at: u128| {
            let mut pane = captured("s", pane, 10 * pane as i32);
            pane.stash = true;
            pane.hidden_at = at;
            pane
        };
        let saved = build_capture(
            &[listed("s", 1)],
            &[hidden(1, 100), hidden(2, 300), hidden(3, 200)],
            &HashMap::new(),
            &[],
            &HashMap::new(),
            None,
        );
        let order: Vec<_> = saved[0].panes.iter().map(|pane| pane.hidden_at).collect();
        assert_eq!(order, vec![Some(300), Some(200), Some(100)]);
    }

    #[test]
    fn only_the_visible_active_pane_of_the_current_window_is_active() {
        let mut inactive_pane = captured("s", 1, 10);
        inactive_pane.pane_active = false;
        let mut other_window = captured("s", 2, 20);
        other_window.window_active = false;
        let mut hidden = captured("s", 3, 30);
        hidden.stash = true;
        hidden.hidden_at = 5;
        let saved = build_capture(
            &[listed("s", 1)],
            &[inactive_pane, other_window, captured("s", 4, 40), hidden],
            &HashMap::new(),
            &[],
            &HashMap::new(),
            None,
        );
        let active: Vec<_> = saved[0].panes.iter().map(|pane| pane.active).collect();
        assert_eq!(active, vec![false, false, true, false]);
    }

    #[test]
    fn sessions_are_sorted_by_creation_then_name_and_empty_ones_are_left_out() {
        let saved = build_capture(
            &[
                listed("c", 5),
                listed("b", 5),
                listed("a", 9),
                listed("empty", 1),
            ],
            &[
                captured("a", 1, 1),
                captured("b", 1, 2),
                captured("c", 1, 3),
            ],
            &HashMap::new(),
            &[],
            &HashMap::new(),
            None,
        );
        let names: Vec<_> = saved.iter().map(|session| session.name.as_str()).collect();
        assert_eq!(names, vec!["b", "c", "a"]);
    }

    #[test]
    fn a_session_carries_its_listed_fields_and_panes_in_capture_order() {
        let mut session = listed("s", 1_793_184_343_878);
        session.layout = "split".into();
        let mut second = captured("s", 2, 20);
        second.cwd = PathBuf::from("/p/alpha/sub");
        second.agent = "Codex".into();
        let saved = build_capture(
            &[session],
            &[captured("s", 1, 10), second, captured("other", 1, 99)],
            &HashMap::new(),
            &[],
            &HashMap::new(),
            None,
        );
        let saved = &saved[0];
        assert_eq!(saved.project, PathBuf::from("/p/alpha"));
        assert_eq!(saved.display_name, "shown s");
        assert_eq!(saved.created, 1_793_184_343_878);
        assert_eq!(saved.layout, "split");
        assert_eq!(saved.panes.len(), 2);
        assert_eq!(saved.panes[1].cwd, PathBuf::from("/p/alpha/sub"));
        assert_eq!(saved.panes[1].agent, "Codex");
    }

    #[test]
    fn oversized_timestamps_saturate_instead_of_wrapping() {
        let mut hidden = captured("s", 1, 10);
        hidden.stash = true;
        hidden.hidden_at = u128::MAX;
        let saved = build_capture(
            &[listed("s", u128::MAX)],
            &[hidden],
            &HashMap::new(),
            &[],
            &HashMap::new(),
            None,
        );
        assert_eq!(saved[0].created, u64::MAX);
        assert_eq!(saved[0].panes[0].hidden_at, Some(u64::MAX));
    }

    // SnapshotKeeper

    fn pane(conversation: Option<&str>, command: Option<&str>) -> SavedPane {
        SavedPane {
            cwd: PathBuf::from("/p/alpha"),
            agent: String::new(),
            hidden_at: None,
            active: true,
            conversation: conversation.map(String::from),
            command: command.map(String::from),
        }
    }

    fn session(name: &str, panes: Vec<SavedPane>) -> SavedSession {
        SavedSession {
            name: name.into(),
            project: PathBuf::from("/p/alpha"),
            display_name: name.into(),
            created: 1,
            layout: String::new(),
            ideation: false,
            remote_name: String::new(),
            panes,
        }
    }

    fn keeper_with(sessions: Vec<SavedSession>) -> SnapshotKeeper {
        SnapshotKeeper::new(Snapshot {
            version: SNAPSHOT_VERSION,
            sessions,
        })
    }

    fn names(keeper: &SnapshotKeeper) -> Vec<&str> {
        keeper
            .snapshot()
            .sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect()
    }

    #[test]
    fn a_richer_or_equal_capture_is_committed_at_once() {
        let t0 = Instant::now();
        let mut keeper = keeper_with(vec![session("a", vec![pane(Some("A"), Some("claude"))])]);
        let applied = keeper.apply(
            vec![session("a", vec![pane(Some("B"), Some("claude"))])],
            t0,
        );
        assert!(applied.changed);
        assert!(applied.held.is_empty());
        assert_eq!(
            keeper.snapshot().sessions[0].panes[0]
                .conversation
                .as_deref(),
            Some("B")
        );
        // Une capture identique ne change rien.
        let applied = keeper.apply(
            vec![session("a", vec![pane(Some("B"), Some("claude"))])],
            t0,
        );
        assert!(!applied.changed);
    }

    #[test]
    fn the_committed_order_follows_creation_not_arrival() {
        let t0 = Instant::now();
        let mut newer = session("b", vec![pane(None, None)]);
        newer.created = 20;
        let mut older = session("a", vec![pane(None, None)]);
        older.created = 10;
        let mut keeper = keeper_with(vec![newer.clone()]);
        // Une session plus ancienne arrive après : elle se range devant, pas derrière.
        let applied = keeper.apply(vec![newer, older], t0);
        assert!(applied.changed);
        assert_eq!(names(&keeper), vec!["a", "b"]);
    }

    #[test]
    fn a_poorer_session_is_held_ten_seconds_then_committed() {
        let t0 = Instant::now();
        let two = session("a", vec![pane(None, None), pane(None, None)]);
        let one = session("a", vec![pane(None, None)]);
        let mut keeper = keeper_with(vec![two.clone()]);

        let applied = keeper.apply(vec![one.clone()], t0);
        assert!(!applied.changed);
        assert_eq!(applied.held, vec![("a".to_string(), HoldReason::Poorer)]);
        assert_eq!(keeper.snapshot().sessions, vec![two.clone()]);

        let applied = keeper.apply(vec![one.clone()], t0 + Duration::from_secs(9));
        assert!(!applied.changed);
        assert!(applied.held.is_empty());
        assert_eq!(keeper.snapshot().sessions, vec![two]);

        let applied = keeper.apply(vec![one.clone()], t0 + Duration::from_secs(10));
        assert!(applied.changed);
        assert_eq!(keeper.snapshot().sessions, vec![one]);
    }

    #[test]
    fn a_session_losing_a_conversation_or_a_command_is_held() {
        let t0 = Instant::now();
        let full = session("a", vec![pane(Some("A"), Some("claude"))]);

        let mut keeper = keeper_with(vec![full.clone()]);
        let applied = keeper.apply(vec![session("a", vec![pane(None, Some("claude"))])], t0);
        assert_eq!(applied.held, vec![("a".to_string(), HoldReason::Poorer)]);
        assert_eq!(keeper.snapshot().sessions, vec![full.clone()]);

        let mut keeper = keeper_with(vec![full.clone()]);
        let applied = keeper.apply(vec![session("a", vec![pane(Some("A"), None)])], t0);
        assert_eq!(applied.held, vec![("a".to_string(), HoldReason::Poorer)]);
        assert_eq!(keeper.snapshot().sessions, vec![full]);
    }

    #[test]
    fn a_poorer_session_back_to_equal_cancels_its_hold() {
        let t0 = Instant::now();
        let two = session("a", vec![pane(None, None), pane(None, None)]);
        let one = session("a", vec![pane(None, None)]);
        let mut keeper = keeper_with(vec![two.clone()]);
        keeper.apply(vec![one.clone()], t0);
        keeper.apply(vec![two.clone()], t0 + Duration::from_secs(5));
        // Une nouvelle retenue repart de zéro : à 12 s l'ancienne aurait expiré.
        let applied = keeper.apply(vec![one.clone()], t0 + Duration::from_secs(6));
        assert_eq!(applied.held, vec![("a".to_string(), HoldReason::Poorer)]);
        keeper.apply(vec![one], t0 + Duration::from_secs(12));
        assert_eq!(keeper.snapshot().sessions, vec![two]);
    }

    #[test]
    fn a_missing_session_is_removed_only_after_ten_seconds() {
        let t0 = Instant::now();
        let mut keeper = keeper_with(vec![session("a", vec![pane(None, None)])]);

        let applied = keeper.apply(Vec::new(), t0);
        assert!(!applied.changed);
        assert_eq!(applied.held, vec![("a".to_string(), HoldReason::Missing)]);
        assert_eq!(names(&keeper), vec!["a"]);

        let applied = keeper.apply(Vec::new(), t0 + Duration::from_secs(10));
        assert!(applied.changed);
        assert!(applied.held.is_empty());
        assert!(names(&keeper).is_empty());
    }

    #[test]
    fn a_session_back_within_ten_seconds_cancels_the_hold() {
        let t0 = Instant::now();
        let a = session("a", vec![pane(None, None)]);
        let mut keeper = keeper_with(vec![a.clone()]);
        keeper.apply(Vec::new(), t0);
        keeper.apply(vec![a], t0 + Duration::from_secs(5));
        keeper.apply(Vec::new(), t0 + Duration::from_secs(6));
        keeper.apply(Vec::new(), t0 + Duration::from_secs(12));
        assert_eq!(names(&keeper), vec!["a"]);
        keeper.apply(Vec::new(), t0 + Duration::from_secs(16));
        assert!(names(&keeper).is_empty());
    }

    #[test]
    fn a_new_session_is_added_at_once() {
        let t0 = Instant::now();
        let mut keeper = keeper_with(vec![session("a", vec![pane(None, None)])]);
        let applied = keeper.apply(
            vec![
                session("a", vec![pane(None, None)]),
                session("b", vec![pane(None, None)]),
            ],
            t0,
        );
        assert!(applied.changed);
        assert!(applied.held.is_empty());
        assert_eq!(names(&keeper), vec!["a", "b"]);
    }

    #[test]
    fn forget_removes_a_session_at_once() {
        let mut keeper = keeper_with(vec![session("a", vec![pane(None, None)])]);
        assert!(keeper.forget("a"));
        assert!(names(&keeper).is_empty());
        assert!(!keeper.forget("unknown"));
    }

    #[test]
    fn nothing_changes_while_suspended() {
        let t0 = Instant::now();
        let a = session("a", vec![pane(None, None)]);
        let mut keeper = keeper_with(vec![a.clone()]);
        keeper.suspend();
        let applied = keeper.apply(Vec::new(), t0 + Duration::from_secs(60));
        assert!(!applied.changed);
        assert!(applied.held.is_empty());
        assert_eq!(keeper.snapshot().sessions, vec![a]);

        keeper.resume();
        let applied = keeper.apply(Vec::new(), t0 + Duration::from_secs(61));
        assert_eq!(applied.held, vec![("a".to_string(), HoldReason::Missing)]);
        assert_eq!(names(&keeper), vec!["a"]);
    }

    #[test]
    fn suspension_does_not_count_toward_a_hold() {
        // Une retenue commencée avant la suspension repart de zéro à la reprise.
        let t0 = Instant::now();
        let mut keeper = keeper_with(vec![session("a", vec![pane(None, None)])]);
        keeper.apply(Vec::new(), t0);
        keeper.suspend();
        keeper.resume();
        keeper.apply(Vec::new(), t0 + Duration::from_secs(30));
        assert_eq!(names(&keeper), vec!["a"]);
    }

    #[test]
    fn a_forgotten_session_is_not_brought_back_by_a_stale_capture() {
        let t0 = Instant::now();
        let a = session("a", vec![pane(None, None)]);
        let mut keeper = keeper_with(vec![a.clone()]);
        keeper.forget("a");

        let applied = keeper.apply(vec![a.clone()], t0);
        assert!(!applied.changed);
        assert!(names(&keeper).is_empty());

        keeper.apply(Vec::new(), t0 + Duration::from_secs(2));
        assert!(names(&keeper).is_empty());

        let applied = keeper.apply(vec![a], t0 + Duration::from_secs(4));
        assert!(applied.changed);
        assert_eq!(names(&keeper), vec!["a"]);
    }

    #[test]
    fn a_session_forgotten_before_it_was_ever_committed_stays_gone() {
        let t0 = Instant::now();
        let mut keeper = keeper_with(Vec::new());
        assert!(!keeper.forget("a"));
        keeper.apply(vec![session("a", vec![pane(None, None)])], t0);
        assert!(names(&keeper).is_empty());
    }

    #[test]
    fn forget_works_while_suspended() {
        let mut keeper = keeper_with(vec![session("a", vec![pane(None, None)])]);
        keeper.suspend();
        assert!(keeper.forget("a"));
        assert!(names(&keeper).is_empty());
        // La pierre tombale tient aussi : à la reprise, une capture périmée ne la fait pas revenir.
        keeper.resume();
        keeper.apply(vec![session("a", vec![pane(None, None)])], Instant::now());
        assert!(names(&keeper).is_empty());
    }

    #[test]
    fn hold_reasons_have_stable_names() {
        assert_eq!(HoldReason::Poorer.as_str(), "poorer");
        assert_eq!(HoldReason::Missing.as_str(), "missing");
    }

    // Le fichier

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("idema-snapshot-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn snapshot_round_trips() {
        let dir = temp_dir("round-trip");
        let path = dir.join("snapshot.json");
        let snapshot = Snapshot {
            version: SNAPSHOT_VERSION,
            sessions: vec![session("a", vec![pane(Some("A"), Some("claude"))])],
        };
        save_snapshot(&path, &snapshot).unwrap();
        assert!(!dir.join("snapshot.json.tmp").exists());
        assert_eq!(load_snapshot(&path), SnapshotLoad::Loaded(snapshot));
    }

    #[test]
    fn an_unreadable_snapshot_goes_to_bak() {
        let dir = temp_dir("broken");
        let path = dir.join("snapshot.json");
        std::fs::write(&path, "{ pas du json").unwrap();
        assert_eq!(load_snapshot(&path), SnapshotLoad::Unreadable);
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("snapshot.json.bak")).unwrap(),
            "{ pas du json"
        );
    }

    #[test]
    fn a_snapshot_of_the_right_version_but_the_wrong_shape_goes_to_bak() {
        let dir = temp_dir("wrong-shape");
        let path = dir.join("snapshot.json");
        std::fs::write(&path, r#"{"version":1,"sessions":"nope"}"#).unwrap();
        assert_eq!(load_snapshot(&path), SnapshotLoad::Unreadable);
        assert!(!path.exists());
        assert!(dir.join("snapshot.json.bak").exists());
    }

    #[test]
    fn an_unknown_version_is_reported_and_left_in_place() {
        let dir = temp_dir("version");
        let path = dir.join("snapshot.json");
        let text = r#"{"version":2,"sessions":[]}"#;
        std::fs::write(&path, text).unwrap();
        assert_eq!(load_snapshot(&path), SnapshotLoad::UnknownVersion(2));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(!dir.join("snapshot.json.bak").exists());
    }

    #[test]
    fn a_missing_snapshot_is_none() {
        let dir = temp_dir("missing");
        assert_eq!(
            load_snapshot(&dir.join("snapshot.json")),
            SnapshotLoad::Absent
        );
        assert!(!dir.join("snapshot.json.bak").exists());
    }

    // L'écrivain de fond

    fn named(name: &str) -> Snapshot {
        Snapshot {
            version: SNAPSHOT_VERSION,
            sessions: vec![session(name, vec![pane(Some("A"), Some("claude"))])],
        }
    }

    #[test]
    fn the_queue_keeps_only_the_latest_request_and_one_writer() {
        let mut queue = WriteQueue::default();
        assert!(
            queue.submit(named("a")),
            "the first request starts a writer"
        );
        assert_eq!(queue.next(), Some(named("a")));
        // Pendant que le fil écrit « a », deux demandes arrivent : un seul fil, et seule la dernière reste.
        assert!(!queue.submit(named("b")));
        assert!(!queue.submit(named("c")));
        assert_eq!(queue.next(), Some(named("c")));
        assert_eq!(queue.next(), None, "an older request never comes back");
        assert!(
            queue.submit(named("d")),
            "an idle queue starts a new writer"
        );
    }

    #[test]
    fn a_request_made_after_the_writer_saw_an_empty_queue_starts_a_new_writer() {
        let mut queue = WriteQueue::default();
        assert!(queue.submit(named("a")));
        assert_eq!(queue.next(), Some(named("a")));
        assert_eq!(queue.next(), None);
        assert!(queue.submit(named("b")));
    }

    #[test]
    fn the_writer_leaves_the_latest_request_on_disk() {
        let dir = temp_dir("writer-latest");
        let path = dir.join("snapshot.json");
        let writer = SnapshotWriter::new(path.clone());
        // Une salve : l'état final doit être la dernière demande, jamais une plus ancienne.
        for index in 0..50 {
            writer.submit(named(&format!("s{index}")));
        }
        writer.submit(named("last"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while load_snapshot(&path) != SnapshotLoad::Loaded(named("last")) {
            assert!(Instant::now() < deadline, "the last request never landed");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Le temps de laisser un éventuel fil en retard se manifester : le fichier ne change plus.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(load_snapshot(&path), SnapshotLoad::Loaded(named("last")));
        assert!(!writer.failed());
    }

    #[test]
    fn a_failed_write_is_reported_and_cleared_by_the_next_one() {
        let dir = temp_dir("writer-failure");
        let path = dir.join("missing-dir").join("snapshot.json");
        let writer = SnapshotWriter::new(path.clone());
        writer.submit(named("a"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !writer.failed() {
            assert!(Instant::now() < deadline, "the failure was never reported");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Le fil d'échec peut ne pas être encore revenu à l'état « arrêté » : on redemande jusqu'à l'écriture.
        while writer.failed() {
            assert!(Instant::now() < deadline, "the retry never succeeded");
            writer.submit(named("b"));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(load_snapshot(&path), SnapshotLoad::Loaded(named("b")));
    }

    #[test]
    fn a_snapshot_of_an_older_or_newer_version_is_unknown_not_unreadable() {
        let dir = temp_dir("versions");
        let path = dir.join("snapshot.json");
        for version in [0, 2, 99] {
            std::fs::write(&path, format!(r#"{{"version":{version},"sessions":[]}}"#)).unwrap();
            assert_eq!(load_snapshot(&path), SnapshotLoad::UnknownVersion(version));
        }
        assert!(path.exists());
        assert!(!dir.join("snapshot.json.bak").exists());
    }

    #[test]
    fn a_read_only_writer_never_touches_the_file() {
        let dir = temp_dir("read-only-writer");
        let path = dir.join("snapshot.json");
        let text = r#"{"version":2,"sessions":[]}"#;
        std::fs::write(&path, text).unwrap();
        let writer = SnapshotWriter::read_only(path.clone());
        writer.submit(named("a"));
        writer.submit(named("b"));
        // Un écrivain qui écrirait le ferait en quelques millisecondes ; on lui laisse largement le temps.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(!dir.join("snapshot.json.tmp").exists());
        assert!(!writer.failed());
    }

    #[test]
    fn only_a_loaded_snapshot_is_kept() {
        assert_eq!(SnapshotLoad::Loaded(named("a")).into_snapshot(), named("a"));
        for other in [
            SnapshotLoad::Absent,
            SnapshotLoad::Unreadable,
            SnapshotLoad::UnknownVersion(2),
        ] {
            assert_eq!(other.into_snapshot(), Snapshot::default());
        }
    }
}
