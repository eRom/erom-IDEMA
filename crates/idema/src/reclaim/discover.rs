//! Découverte des sessions Claude Code qui tournent hors d'IdemA, et de ce que IdemA sait en relancer. Du calcul pur :
//! les fichiers de session, la table des processus et une `Probe` (le disque et `ps`) entrent, une liste sort.
//!
//! Cette liste alimente un geste qui signale de vrais processus. Au moindre doute, une session n'est pas listée, ou
//! elle l'est avec `resume == None` : l'interface la montre mais ne propose pas de la ramener.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::restore::claude::{descends_from, is_claude_command, is_uuid};
use crate::servers::scan::Process;

use super::command::{ResumeSpec, flag_value, relaunch_command, remote_control_of, resume_command};
use super::sources::{CaserneManifest, IdnmaAgents, LaunchEnv, LiveSession, SessionHolder};

/// Ce que la découverte demande au monde extérieur. Une implémentation réelle lit le disque et `ps` ; les tests en
/// fournissent une en mémoire.
pub trait Probe {
    /// Vrai si le transcript de la conversation existe : sans lui, la session n'a encore rien dit et rien n'est à
    /// reprendre.
    fn has_transcript(&self, session_id: &str) -> bool;
    /// Le manifeste de la Caserne de cette conversation, s'il existe.
    fn manifest(&self, session_id: &str) -> Option<CaserneManifest>;
    /// L'environnement de lancement du processus. `None` quand il est illisible : on ne devine pas.
    fn launch_env(&self, session: &LiveSession) -> Option<LaunchEnv>;
    /// La racine du projet qui contient ce dossier de travail.
    fn project_root(&self, cwd: &Path) -> PathBuf;
    fn is_dir(&self, path: &Path) -> bool;
    /// La date de démarrage du processus, dans le même format que `procStart`. `None` si le processus est introuvable.
    fn started_at(&self, pid: i32) -> Option<String>;
}

/// Une session vivante hors d'IdemA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsideSession {
    pub pid: i32,
    pub session_id: String,
    /// Date de démarrage du processus : le geste de reprise la revérifie avant tout signal.
    pub proc_start: String,
    pub name: String,
    pub cwd: PathBuf,
    pub root: PathBuf,
    pub agent: String,
    pub model: Option<String>,
    /// La commande à taper dans un pane. `None` : IdemA ne sait pas relancer cette session.
    pub resume: Option<String>,
    /// `resume` sans le `cd` ni le `--resume` : la commande que la restauration rejoue (`@idema_resume`) pour que la
    /// session ramenée garde son modèle, son effort et son remote-control. `Some` exactement quand `resume` l'est.
    pub relaunch: Option<String>,
}

/// Les sous-commandes de `claude` qui ne sont pas une conversation.
const NON_SESSION_SUBCOMMANDS: [&str; 3] = ["remote-control", "mcp", "agents"];

/// Vrai si la ligne est une conversation Claude Code : le programme `claude` sans sous-commande qui n'en ouvre pas
/// une. Claude Code accepte une sous-commande après des options (`claude --model opus mcp`) : le premier argument ne
/// décide donc pas, un seul mot après le programme suffit à écarter la ligne. Coût accepté : une session dont un
/// argument libre vaut `mcp`, `agents` ou `remote-control` n'est pas listée (au doute, on ne liste pas).
pub fn is_session_command(command: &str) -> bool {
    if !is_claude_command(command) {
        return false;
    }
    let mut words = command.split_whitespace();
    let program_seen = words
        .by_ref()
        .any(|word| word.rsplit('/').next() == Some("claude"));
    program_seen && !words.any(|word| NON_SESSION_SUBCOMMANDS.contains(&word))
}

/// Vrai si la ligne porte `--agent-id` : un coéquipier d'une équipe d'agents, jamais une session de l'utilisateur.
fn is_teammate(command: &str) -> bool {
    command
        .split_whitespace()
        .any(|word| word == "--agent-id" || word.starts_with("--agent-id="))
}

/// Ce qui, dans la table des processus, est IdemA : ses panes, les shells de son volet Terminal et lui-même.
#[derive(Debug, Clone, Copy)]
pub struct IdemaPids<'a> {
    pub pane_pids: &'a [i32],
    pub terminal_shells: &'a [i32],
    /// Le `pid` d'IdemA : une session dont il descend l'héberge (IdemA lancé depuis un `claude`), l'arrêter
    /// l'arrêterait lui.
    pub own_pid: i32,
}

/// Les sessions vivantes hors d'IdemA, triées par `pid`.
///
/// Une session est écartée quand son processus a disparu, n'est plus un `claude` de conversation, descend d'un pane
/// (`pane_pids`) ou d'un shell de terminal d'IdemA (`terminal_shells`), héberge IdemA (`own_pid` en descend), n'a pas
/// le `procStart` du fichier (PID réutilisé), n'a pas de transcript ou est un coéquipier. Les autres sont listées ;
/// `resume` n'est rempli que si tout ce qu'il faut pour la relancer est connu et sûr, dont : aucun autre processus ne
/// tient la conversation (`holders`, tous les fichiers de session, et les lignes de commande qui la nomment).
pub fn outside_sessions(
    live: &[LiveSession],
    idema: IdemaPids,
    processes: &HashMap<i32, Process>,
    holders: &[SessionHolder],
    agents: &IdnmaAgents,
    probe: &impl Probe,
) -> Vec<OutsideSession> {
    let mut found: Vec<OutsideSession> = Vec::new();
    for session in live {
        let Some(process) = processes.get(&session.pid) else {
            continue;
        };
        if !is_session_command(&process.command) || is_teammate(&process.command) {
            continue;
        }
        // Le test pur d'abord : il ne coûte rien, `has_transcript` touche le disque.
        let inside_idema = idema
            .pane_pids
            .iter()
            .chain(idema.terminal_shells)
            .any(|shell| descends_from(session.pid, *shell, processes));
        if inside_idema || descends_from(idema.own_pid, session.pid, processes) {
            continue;
        }
        if probe.started_at(session.pid).as_deref() != Some(session.proc_start.as_str()) {
            continue;
        }
        if !probe.has_transcript(&session.session_id) {
            continue;
        }
        let held_elsewhere = is_held_elsewhere(session, holders, processes, probe);
        found.push(describe(
            session,
            &process.command,
            held_elsewhere,
            agents,
            probe,
        ));
    }
    found.sort_by_key(|session| session.pid);
    found
}

/// Vrai si un autre processus écrit peut-être déjà dans la conversation : reprise, ce serait deux écrivains sur un même
/// transcript. Deux filets, sans aucun filtre sur la commande ni le genre de session (`sdk-cli`, `--print`, un
/// `claude` lancé par son chemin versionné...) :
/// - un fichier de session d'un autre PID déclare la conversation, que ce PID soit vivant (présent dans `processes`) et,
///   quand le fichier porte un `procStart`, qu'il démarre bien à cette date (sinon le PID a été réutilisé). Une date
///   illisible laisse le doute : le fichier tient ;
/// - une autre ligne de commande contient l'identifiant (`--resume <id>`, `--session-id=<id>`), même sans fichier. Un
///   ancêtre de la session (lanceur, `caffeinate`) n'écrit pas dans le transcript et ne compte pas.
fn is_held_elsewhere(
    session: &LiveSession,
    holders: &[SessionHolder],
    processes: &HashMap<i32, Process>,
    probe: &impl Probe,
) -> bool {
    let by_file = holders.iter().any(|holder| {
        holder.pid != session.pid
            && holder.session_id == session.session_id
            && processes.contains_key(&holder.pid)
            && holder.proc_start.as_deref().is_none_or(|declared| {
                probe
                    .started_at(holder.pid)
                    .is_none_or(|actual| actual == declared)
            })
    });
    by_file
        || processes.values().any(|other| {
            other.pid != session.pid
                && names_conversation(&other.command, &session.session_id)
                && !descends_from(session.pid, other.pid, processes)
        })
}

/// Vrai si un mot de la ligne est l'identifiant, seul ou derrière un `=` (`--resume=<id>`).
fn names_conversation(command: &str, session_id: &str) -> bool {
    command.split_whitespace().any(|word| {
        word == session_id
            || word
                .rsplit_once('=')
                .is_some_and(|(_, value)| value == session_id)
    })
}

/// Construit l'entrée d'une session retenue : son agent, son modèle et, si tout est sûr, sa commande de reprise.
fn describe(
    session: &LiveSession,
    command: &str,
    held_elsewhere: bool,
    agents: &IdnmaAgents,
    probe: &impl Probe,
) -> OutsideSession {
    // Un manifeste ne compte que s'il est ouvert et qu'il désigne ce processus : un manifeste fermé ou d'un autre PID
    // décrit une autre vie de la conversation.
    let manifest = probe
        .manifest(&session.session_id)
        .filter(|manifest| manifest.open && manifest.pid == session.pid);
    let agent = manifest
        .as_ref()
        .map_or("claude", |manifest| manifest.agent.as_str());
    let engine = manifest
        .as_ref()
        .and_then(|manifest| manifest.engine.as_deref())
        .filter(|engine| *engine != "default");
    // La ligne de commande gagne : c'est ce que le processus fait vraiment. Sans `--model`, le moteur décide.
    let model = flag_value(command, "--model")
        .map(str::to_string)
        .or_else(|| {
            manifest
                .as_ref()
                .and_then(|manifest| manifest.model.clone())
        });
    let effort = flag_value(command, "--effort");
    let remote_control = remote_control_of(command);

    // Sans manifeste, rien ne dit que `claude` parlait à Anthropic : `idnma run claude` relancerait une autre session.
    // L'environnement illisible vaut un fournisseur tiers.
    let provider_known = manifest.is_some()
        || probe
            .launch_env(session)
            .is_some_and(|env| !env.third_party);

    // Les deux commandes viennent de la même spec et des mêmes refus : `resume` et `relaunch` sont `Some` ensemble.
    let (resume, relaunch) = (!held_elsewhere
        && provider_known
        && agents.knows(agent, engine)
        && probe.is_dir(&session.cwd))
    .then(|| {
        let spec = ResumeSpec {
            cwd: &session.cwd,
            agent,
            engine,
            model: model.as_deref(),
            effort,
            remote_control,
            session_id: &session.session_id,
        };
        resume_command(&spec).zip(relaunch_command(&spec))
    })
    .flatten()
    .unzip();

    OutsideSession {
        pid: session.pid,
        session_id: session.session_id.clone(),
        proc_start: session.proc_start.clone(),
        name: session
            .name
            .clone()
            .unwrap_or_else(|| short_id(&session.session_id)),
        cwd: session.cwd.clone(),
        root: probe.project_root(&session.cwd),
        agent: agent.to_string(),
        model,
        resume,
        relaunch,
    }
}

/// Les 8 premiers caractères d'un UUID : de quoi reconnaître une session sans nom.
fn short_id(session_id: &str) -> String {
    debug_assert!(is_uuid(session_id));
    session_id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "bbbb2222-cccc-4ddd-8eee-ffff00001111";
    const ID2: &str = "cccc3333-dddd-4eee-8fff-000011112222";
    const START: &str = "Mon Nov  2 18:01:12 2026";
    const PS_DEMO: &str = "/Users/me/.local/bin/claude --dangerously-skip-permissions --remote-control demo-poc-ceres -n demo-poc-ceres";

    /// Un monde en mémoire : chaque champ répond à la méthode de même nom.
    struct FakeProbe {
        transcripts: Vec<String>,
        manifests: HashMap<String, CaserneManifest>,
        launch_env: Option<LaunchEnv>,
        missing_dirs: bool,
        started_at: HashMap<i32, String>,
    }

    impl FakeProbe {
        fn new() -> Self {
            FakeProbe {
                transcripts: vec![ID.to_string(), ID2.to_string()],
                manifests: HashMap::new(),
                launch_env: Some(LaunchEnv { third_party: false }),
                missing_dirs: false,
                started_at: HashMap::new(),
            }
        }
    }

    impl Probe for FakeProbe {
        fn has_transcript(&self, session_id: &str) -> bool {
            self.transcripts.iter().any(|id| id == session_id)
        }
        fn manifest(&self, session_id: &str) -> Option<CaserneManifest> {
            self.manifests.get(session_id).cloned()
        }
        fn launch_env(&self, _session: &LiveSession) -> Option<LaunchEnv> {
            self.launch_env
        }
        fn project_root(&self, cwd: &Path) -> PathBuf {
            cwd.parent().unwrap_or(cwd).to_path_buf()
        }
        fn is_dir(&self, _path: &Path) -> bool {
            !self.missing_dirs
        }
        fn started_at(&self, pid: i32) -> Option<String> {
            self.started_at.get(&pid).cloned()
        }
    }

    fn live(pid: i32, session_id: &str) -> LiveSession {
        LiveSession {
            pid,
            session_id: session_id.to_string(),
            cwd: PathBuf::from("/Users/x/dev/demo"),
            name: Some("demo".to_string()),
            proc_start: START.to_string(),
        }
    }

    fn process(pid: i32, ppid: i32, command: &str) -> (i32, Process) {
        (
            pid,
            Process {
                pid,
                ppid,
                pgid: pid,
                command: command.to_string(),
            },
        )
    }

    /// Un `claude` (pid 200, fils du shell étranger 100) que la sonde reconnaît, avec l'agent `claude` et `glm` connus.
    fn world(command: &str) -> (Vec<LiveSession>, HashMap<i32, Process>, FakeProbe) {
        let processes = HashMap::from([process(100, 1, "-zsh"), process(200, 100, command)]);
        let mut probe = FakeProbe::new();
        probe.started_at.insert(200, START.to_string());
        (vec![live(200, ID)], processes, probe)
    }

    fn agents() -> IdnmaAgents {
        crate::reclaim::sources::parse_idnma_agents(
            "agents:\n  claude:\n    engines:\n      default:\n  glm:\n    engines:\n      default:\n      gflash:\n",
        )
    }

    /// Les pids d'IdemA des tests : `own_pid` est un numéro absent de la table, donc ancêtre de personne.
    fn outside_idema<'a>(pane_pids: &'a [i32], terminal_shells: &'a [i32]) -> IdemaPids<'a> {
        IdemaPids {
            pane_pids,
            terminal_shells,
            own_pid: 999_999,
        }
    }

    fn run(
        live: &[LiveSession],
        processes: &HashMap<i32, Process>,
        probe: &FakeProbe,
    ) -> Vec<OutsideSession> {
        outside_sessions(
            live,
            outside_idema(&[], &[]),
            processes,
            &[],
            &agents(),
            probe,
        )
    }

    fn holder(pid: i32, session_id: &str, proc_start: Option<&str>) -> SessionHolder {
        SessionHolder {
            pid,
            session_id: session_id.to_string(),
            proc_start: proc_start.map(str::to_string),
        }
    }

    /// Comme `run`, avec les fichiers de session qui disent qui tient quelle conversation.
    fn held(
        live: &[LiveSession],
        holders: &[SessionHolder],
        processes: &HashMap<i32, Process>,
        probe: &FakeProbe,
    ) -> Vec<OutsideSession> {
        outside_sessions(
            live,
            outside_idema(&[11], &[]),
            processes,
            holders,
            &agents(),
            probe,
        )
    }

    fn manifest(agent: &str, engine: Option<&str>, open: bool, pid: i32) -> CaserneManifest {
        CaserneManifest {
            agent: agent.to_string(),
            engine: engine.map(str::to_string),
            model: None,
            pid,
            open,
        }
    }

    #[test]
    fn only_sessions_outside_idema_are_listed() {
        let processes = HashMap::from([
            process(10, 1, "tmux"),
            process(11, 10, "-zsh"),
            process(12, 11, "claude --model opus"),
            process(20, 1, "Terminal"),
            process(21, 20, "-zsh"),
            process(22, 21, "claude --model opus"),
            process(30, 1, "iTerm2"),
            process(31, 30, "-zsh"),
            process(32, 31, "claude --model opus"),
        ]);
        let mut probe = FakeProbe::new();
        probe
            .transcripts
            .push("11111111-1111-4111-8111-111111111111".to_string());
        probe
            .transcripts
            .push("22222222-2222-4222-8222-222222222222".to_string());
        for pid in [12, 22, 32] {
            probe.started_at.insert(pid, START.to_string());
        }
        let sessions = [
            live(12, ID),
            live(22, "11111111-1111-4111-8111-111111111111"),
            live(32, "22222222-2222-4222-8222-222222222222"),
        ];
        let found = outside_sessions(
            &sessions,
            outside_idema(&[11], &[21]),
            &processes,
            &[],
            &agents(),
            &probe,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pid, 32);
    }

    #[test]
    fn the_session_that_hosts_idema_is_not_listed_but_its_neighbour_is() {
        // IdemA (300) a été lancé depuis le shell 110 d'un `claude` (210) : ce `claude` l'héberge. Son voisin (220),
        // lancé depuis un autre shell, reste listé.
        let processes = HashMap::from([
            process(1, 0, "launchd"),
            process(100, 1, "-zsh"),
            process(210, 100, "claude --model opus"),
            process(110, 210, "-zsh"),
            process(300, 110, "idema"),
            process(220, 100, "claude --model opus"),
        ]);
        let mut probe = FakeProbe::new();
        for pid in [210, 220] {
            probe.started_at.insert(pid, START.to_string());
        }
        let sessions = [live(210, ID), live(220, ID2)];
        let idema = IdemaPids {
            pane_pids: &[],
            terminal_shells: &[],
            own_pid: 300,
        };
        let found = outside_sessions(&sessions, idema, &processes, &[], &agents(), &probe);
        assert_eq!(found.iter().map(|s| s.pid).collect::<Vec<_>>(), [220]);
    }

    #[test]
    fn a_dead_pid_or_a_foreign_command_is_not_listed() {
        let (sessions, mut processes, probe) = world("claude --model opus");
        assert_eq!(run(&sessions, &processes, &probe).len(), 1);
        processes.remove(&200);
        assert!(run(&sessions, &processes, &probe).is_empty());
        let (sessions, processes, probe) = world("sleep 100");
        assert!(run(&sessions, &processes, &probe).is_empty());
    }

    #[test]
    fn a_reused_pid_is_not_listed() {
        let (sessions, processes, mut probe) = world("claude --model opus");
        probe
            .started_at
            .insert(200, "Tue Nov  3 09:00:00 2026".to_string());
        assert!(run(&sessions, &processes, &probe).is_empty());
        probe.started_at.clear();
        assert!(run(&sessions, &processes, &probe).is_empty());
    }

    #[test]
    fn claude_subcommands_are_not_sessions() {
        for command in [
            "/x/claude remote-control --name HQ",
            "claude mcp serve",
            "claude agents",
        ] {
            assert!(!is_session_command(command), "{command}");
            let (sessions, processes, probe) = world(command);
            assert!(run(&sessions, &processes, &probe).is_empty(), "{command}");
        }
        // Claude Code accepte une sous-commande après des options : le premier argument ne décide pas.
        for command in [
            "claude --model opus mcp",
            "claude --debug agents list",
            "/x/claude --verbose remote-control",
        ] {
            assert!(!is_session_command(command), "{command}");
        }
        assert!(is_session_command(PS_DEMO));
        assert!(is_session_command("node /usr/local/bin/claude --resume x"));
        assert!(!is_session_command("sleep 100"));
    }

    #[test]
    fn a_silent_session_is_not_listed() {
        let (sessions, processes, mut probe) = world("claude --model opus");
        probe.transcripts.clear();
        assert!(run(&sessions, &processes, &probe).is_empty());
    }

    #[test]
    fn a_teammate_is_not_listed() {
        let (sessions, processes, probe) = world("claude --agent-id researcher@team --model opus");
        assert!(run(&sessions, &processes, &probe).is_empty());
    }

    #[test]
    fn the_manifest_gives_agent_and_engine() {
        for engine in [Some("default"), None] {
            let (sessions, processes, mut probe) = world("claude --dangerously-skip-permissions");
            probe
                .manifests
                .insert(ID.to_string(), manifest("glm", engine, true, 200));
            let found = run(&sessions, &processes, &probe);
            let resume = found[0].resume.as_deref().unwrap();
            assert!(resume.contains("idnma run 'glm'"), "{resume}");
            assert!(!resume.contains("--engine"), "{resume}");
            assert_eq!(found[0].agent, "glm");
        }
        let (sessions, processes, mut probe) = world("claude");
        probe
            .manifests
            .insert(ID.to_string(), manifest("glm", Some("gflash"), true, 200));
        let found = run(&sessions, &processes, &probe);
        assert!(
            found[0]
                .resume
                .as_deref()
                .unwrap()
                .contains("idnma run 'glm' --engine 'gflash'")
        );
    }

    #[test]
    fn the_relaunch_command_keeps_model_effort_and_remote_control() {
        let (sessions, processes, mut probe) =
            world("claude --model haiku --effort medium --remote-control");
        probe
            .manifests
            .insert(ID.to_string(), manifest("claude", None, true, 200));
        let found = run(&sessions, &processes, &probe);
        let relaunch = found[0].relaunch.as_deref().expect("a relaunch command");
        assert_eq!(
            relaunch,
            "idnma run 'claude' --model 'haiku' --effort 'medium' --remote-control"
        );
        // Le clic tape `cd .. && <relaunch> --resume <id>` : les deux commandes ne se contredisent pas.
        let resume = found[0].resume.as_deref().expect("a resume command");
        assert!(
            resume.ends_with(&format!(" && {relaunch} --resume {ID}")),
            "{resume}"
        );
    }

    #[test]
    fn a_session_that_cannot_be_resumed_has_no_relaunch_command() {
        let (sessions, processes, probe) = world("claude");
        let found = run(&sessions, &processes, &probe);
        assert_eq!(found[0].resume.is_some(), found[0].relaunch.is_some());
        let (sessions, processes, mut probe) = world("claude");
        probe
            .manifests
            .insert(ID.to_string(), manifest("unknown-agent", None, true, 200));
        let found = run(&sessions, &processes, &probe);
        assert_eq!(found[0].resume, None);
        assert_eq!(found[0].relaunch, None);
    }

    #[test]
    fn the_command_line_wins_over_the_manifest_model() {
        let (sessions, processes, mut probe) = world("claude --model sonnet");
        let mut with_model = manifest("glm", None, true, 200);
        with_model.model = Some("opus".to_string());
        probe.manifests.insert(ID.to_string(), with_model.clone());
        let found = run(&sessions, &processes, &probe);
        assert_eq!(found[0].model.as_deref(), Some("sonnet"));
        assert!(
            found[0]
                .resume
                .as_deref()
                .unwrap()
                .contains("--model 'sonnet'")
        );

        // Sans `--model` sur la ligne, un manifeste qui compte donne le sien.
        let (sessions, processes, mut probe) = world("claude");
        probe.manifests.insert(ID.to_string(), with_model);
        assert_eq!(
            run(&sessions, &processes, &probe)[0].model.as_deref(),
            Some("opus")
        );
    }

    #[test]
    fn a_session_opened_from_the_phone_keeps_no_model() {
        let (sessions, processes, probe) = world(PS_DEMO);
        let found = run(&sessions, &processes, &probe);
        assert_eq!(found[0].model, None);
        let resume = found[0].resume.as_deref().unwrap();
        assert!(!resume.contains("--model"), "{resume}");
        assert!(
            resume.contains("--remote-control 'demo-poc-ceres' --resume"),
            "{resume}"
        );
    }

    #[test]
    fn a_closed_or_foreign_manifest_does_not_count() {
        for (open, pid) in [(false, 200), (true, 999)] {
            let (sessions, processes, mut probe) = world("claude --model opus");
            let mut stale = manifest("glm", Some("gflash"), open, pid);
            stale.model = Some("glm-x".to_string());
            probe.manifests.insert(ID.to_string(), stale);
            let found = run(&sessions, &processes, &probe);
            let resume = found[0].resume.as_deref().unwrap();
            assert!(resume.contains("idnma run 'claude'"), "{resume}");
            assert!(!resume.contains("--engine"), "{resume}");
            assert_eq!(found[0].agent, "claude");
        }
        // Un manifeste écarté ne prête pas non plus son modèle.
        let (sessions, processes, mut probe) = world("claude");
        let mut stale = manifest("glm", None, false, 200);
        stale.model = Some("glm-x".to_string());
        probe.manifests.insert(ID.to_string(), stale);
        assert_eq!(run(&sessions, &processes, &probe)[0].model, None);
    }

    #[test]
    fn no_manifest_means_claude() {
        let (sessions, processes, probe) = world("claude --model opus --effort high");
        let found = run(&sessions, &processes, &probe);
        assert_eq!(found[0].agent, "claude");
        assert_eq!(found[0].root, PathBuf::from("/Users/x/dev"));
        assert_eq!(
            found[0].resume.as_deref(),
            Some(format!("cd '/Users/x/dev/demo' && idnma run 'claude' --model 'opus' --effort 'high' --resume {ID}").as_str())
        );
    }

    #[test]
    fn an_agent_or_engine_unknown_to_idnma_cannot_be_resumed() {
        for (agent, engine) in [("free", None), ("glm", Some("nope"))] {
            let (sessions, processes, mut probe) = world("claude");
            probe
                .manifests
                .insert(ID.to_string(), manifest(agent, engine, true, 200));
            let found = run(&sessions, &processes, &probe);
            assert_eq!(found.len(), 1, "{agent}");
            assert_eq!(found[0].resume, None, "{agent}");
        }
    }

    #[test]
    fn a_third_party_without_manifest_cannot_be_resumed() {
        let (sessions, processes, mut probe) = world("claude");
        probe.launch_env = Some(LaunchEnv { third_party: true });
        let found = run(&sessions, &processes, &probe);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].resume, None);
        probe.launch_env = None;
        let found = run(&sessions, &processes, &probe);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].resume, None);
    }

    #[test]
    fn a_manifest_spares_the_launch_env() {
        // Le manifeste dit déjà le fournisseur : un environnement illisible ne bloque pas la reprise.
        let (sessions, processes, mut probe) = world("claude");
        probe.launch_env = None;
        probe
            .manifests
            .insert(ID.to_string(), manifest("glm", None, true, 200));
        assert!(run(&sessions, &processes, &probe)[0].resume.is_some());
    }

    #[test]
    fn a_missing_cwd_cannot_be_resumed() {
        let (sessions, processes, mut probe) = world("claude");
        probe.missing_dirs = true;
        let found = run(&sessions, &processes, &probe);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].resume, None);
    }

    #[test]
    fn a_command_the_builder_refuses_gives_no_resume() {
        // Un caractère de contrôle dans le modèle : `resume_command` rend `None`, la session reste listée.
        let (sessions, processes, probe) = world("claude --model opus\u{1b}[2J");
        let found = run(&sessions, &processes, &probe);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].resume, None);

        // Un modèle qui commence par `-` n'est pas une valeur : `flag_value` l'écarte, la reprise reste sûre.
        let (sessions, processes, probe) = world("claude --model --effort high");
        let found = run(&sessions, &processes, &probe);
        assert_eq!(found[0].model, None);

        // Un `cwd` relatif est refusé par le constructeur.
        let (mut sessions, processes, probe) = world("claude");
        sessions[0].cwd = PathBuf::from("relative/dir");
        let found = run(&sessions, &processes, &probe);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].resume, None);
    }

    #[test]
    fn a_conversation_held_twice_cannot_be_resumed() {
        let processes = HashMap::from([
            process(10, 1, "tmux"),
            process(11, 10, "-zsh"),
            process(12, 11, "claude"),
            process(100, 1, "-zsh"),
            process(200, 100, "claude"),
        ]);
        let mut probe = FakeProbe::new();
        probe.started_at.insert(12, START.to_string());
        probe.started_at.insert(200, START.to_string());
        let sessions = [live(12, ID), live(200, ID)];
        let holders = [holder(12, ID, Some(START)), holder(200, ID, Some(START))];
        let found = held(&sessions, &holders, &processes, &probe);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pid, 200);
        assert_eq!(found[0].resume, None);

        // Le seul fichier restant est le sien : rien d'autre ne tient la conversation.
        let alone = [live(200, ID)];
        let found = held(&alone, &[holder(200, ID, Some(START))], &processes, &probe);
        assert!(found[0].resume.is_some());

        // Le fichier de l'autre exemplaire n'a plus de processus : il ne tient plus rien.
        let found = held(&alone, &[holder(999, ID, Some(START))], &processes, &probe);
        assert!(found[0].resume.is_some());
    }

    #[test]
    fn a_second_writer_of_any_kind_holds_the_conversation() {
        let (sessions, mut processes, mut probe) = world("claude");
        // Un `claude` lancé par chemin versionné, que `is_claude_command` ne reconnaît pas : seul son fichier le trahit
        // (la ligne ne nomme pas la conversation).
        processes.extend([process(
            300,
            1,
            "/Users/x/.local/share/claude/versions/2.1.316 --print",
        )]);
        probe.started_at.insert(300, START.to_string());
        let found = held(
            &sessions,
            &[holder(300, ID, Some(START))],
            &processes,
            &probe,
        );
        assert_eq!(found[0].resume, None);

        // Un fichier sans `procStart` sur un processus vivant tient aussi, quelle que soit sa commande.
        let (sessions, mut processes, probe) = world("claude");
        processes.extend([process(400, 1, "node worker.js")]);
        let found = held(&sessions, &[holder(400, ID, None)], &processes, &probe);
        assert_eq!(found[0].resume, None);
    }

    #[test]
    fn a_process_naming_the_conversation_holds_it_without_any_file() {
        for argument in [format!("--resume {ID}"), format!("--resume={ID}")] {
            let (sessions, mut processes, probe) = world("claude");
            processes.extend([process(300, 1, &format!("claude --print {argument}"))]);
            let found = held(&sessions, &[], &processes, &probe);
            assert_eq!(found[0].resume, None, "{argument}");
        }
        // Une autre conversation ne gêne pas.
        let (sessions, mut processes, probe) = world("claude");
        processes.extend([process(300, 1, &format!("claude --print --resume {ID2}"))]);
        assert!(held(&sessions, &[], &processes, &probe)[0].resume.is_some());
    }

    #[test]
    fn a_wrapper_above_the_session_does_not_hold_it() {
        // Un lanceur qui a la conversation dans ses arguments et la session pour enfant n'écrit pas dedans.
        let processes = HashMap::from([
            process(100, 1, "-zsh"),
            process(150, 100, &format!("caffeinate -i claude --resume {ID}")),
            process(200, 150, &format!("claude --resume {ID}")),
        ]);
        let mut probe = FakeProbe::new();
        probe.started_at.insert(200, START.to_string());
        let found = held(&[live(200, ID)], &[], &processes, &probe);
        assert!(found[0].resume.is_some());
    }

    #[test]
    fn a_stale_holder_does_not_hold_the_conversation() {
        let (sessions, mut processes, mut probe) = world("claude");
        processes.extend([process(300, 1, "-zsh")]);
        // Le PID 300 a été réutilisé : il ne démarre plus à l'heure que le fichier annonce.
        probe
            .started_at
            .insert(300, "Tue Nov  3 09:00:00 2026".to_string());
        let stale = [holder(300, ID, Some(START))];
        assert!(
            held(&sessions, &stale, &processes, &probe)[0]
                .resume
                .is_some()
        );
        // Même PID, même démarrage : le fichier dit vrai.
        probe.started_at.insert(300, START.to_string());
        assert_eq!(held(&sessions, &stale, &processes, &probe)[0].resume, None);
        // Démarrage illisible : dans le doute, il tient.
        probe.started_at.clear();
        probe.started_at.insert(200, START.to_string());
        assert_eq!(held(&sessions, &stale, &processes, &probe)[0].resume, None);
    }

    #[test]
    fn two_holders_outside_idema_are_both_listed_without_resume() {
        let processes = HashMap::from([
            process(100, 1, "-zsh"),
            process(200, 100, "claude"),
            process(210, 100, "claude"),
        ]);
        let mut probe = FakeProbe::new();
        probe.started_at.insert(200, START.to_string());
        probe.started_at.insert(210, START.to_string());
        let sessions = [live(200, ID), live(210, ID)];
        let holders = [holder(200, ID, Some(START)), holder(210, ID, Some(START))];
        let found = held(&sessions, &holders, &processes, &probe);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|session| session.resume.is_none()));
    }

    #[test]
    fn a_session_without_a_name_shows_its_short_id() {
        let (mut sessions, processes, probe) = world("claude");
        sessions[0].name = None;
        assert_eq!(run(&sessions, &processes, &probe)[0].name, "bbbb2222");
        sessions[0].name = Some("demo".to_string());
        assert_eq!(run(&sessions, &processes, &probe)[0].name, "demo");
    }
}
