//! La restauration des sessions et des agents après un redémarrage du Mac : lecture des fichiers de session de
//! Claude Code (`claude`), instantané et son gardien (`snapshot`), capture de tmux (`capture_now`), puis plan de
//! restauration et notice (`plan`). La relance elle-même (recréation des sessions, commandes de lancement) est
//! pilotée par le `Workspace`.

use std::collections::HashMap;
use std::path::Path;

use idema_tmux::server::TmuxServer;

use crate::config::AgentEntry;
use crate::servers::scan::Process;

pub mod claude;
pub mod plan;
pub mod snapshot;

use snapshot::{SavedSession, build_capture};

/// Ce qu'une lecture de tmux rapporte : les sessions dans la forme de l'instantané, et le PID du processus de chaque pane
/// (la découverte des sessions hors d'IdemA écarte tout ce qui en descend).
#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    pub sessions: Vec<SavedSession>,
    pub pane_pids: Vec<i32>,
}

/// Ce que tmux montre à l'instant, dans la forme de l'instantané. Une lecture qui échoue est une erreur, jamais une
/// capture vide : l'appelant ne doit rien changer à l'instantané (spec §3.3, « un serveur injoignable ou une lecture
/// en erreur ne change rien »). Un dossier de sessions Claude absent ne donne simplement aucune conversation.
///
/// `processes` est la table de processus du dernier relevé des serveurs (un seul `ps` toutes les 2 s pour les deux).
pub fn capture_now(
    tmux: &TmuxServer,
    agents: &[AgentEntry],
    sessions_dir: Option<&Path>,
    processes: &HashMap<i32, Process>,
) -> Result<Capture, String> {
    let (sessions, panes) = tmux.capture().map_err(|error| error.to_string())?;
    let claude_sessions = sessions_dir
        .map(claude::read_claude_sessions)
        .unwrap_or_default();
    let conversations = claude::conversations(&claude_sessions, &panes, processes);
    Ok(Capture {
        sessions: build_capture(&sessions, &panes, &conversations, agents, processes),
        pane_pids: panes.iter().map(|pane| pane.pane_pid).collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use idema_tmux::env::server_env;
    use idema_tmux::server::{NewSession, locate_tmux};

    use super::*;

    /// Serveur jetable : socket et dossier propres au test, `kill-server` à la fin quoi qu'il arrive.
    struct Disposable {
        server: TmuxServer,
        dir: PathBuf,
    }

    impl Drop for Disposable {
        fn drop(&mut self) {
            let _ = self.server.kill();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn disposable(name: &str) -> Disposable {
        let path = std::env::var("PATH").ok();
        let binary = locate_tmux(None, path.as_deref()).expect("tmux installed");
        let dir = std::env::temp_dir().join(format!("idema-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test directory");
        // tmux rend le chemin réel (`/private/var/…`) : le test compare avec ce chemin-là.
        let dir = dir.canonicalize().expect("canonical test directory");
        // Le socket vit dans le dossier du test (`-S`) : le nettoyage du dossier le retire.
        let socket = dir.join("sock").to_string_lossy().into_owned();
        let mut env = server_env(std::env::vars(), None);
        env.retain(|(key, _)| key != "SHELL");
        env.push(("SHELL".into(), "/bin/sh".into()));
        let server = TmuxServer::new(binary, socket, dir.join("tmux.conf"), env);
        server.write_config().expect("test config");
        Disposable { server, dir }
    }

    #[test]
    fn capture_now_reads_a_real_server() {
        let test = disposable("capture");
        let created = test
            .server
            .create_session(&NewSession {
                project: &test.dir,
                display_name: "Shell",
                agent: "Shell",
                columns: 80,
                lines: 24,
                remote_name: None,
            })
            .expect("session created");

        let captured =
            capture_now(&test.server, &[], None, &HashMap::new()).expect("the server answers");

        // Le `pane_pid` du pane créé, tel que tmux le dit.
        let (_, panes) = test.server.capture().expect("the server answers");
        let pane = panes
            .iter()
            .find(|pane| pane.pane == created.pane)
            .expect("the created pane is listed");
        assert!(pane.pane_pid > 0);
        assert!(captured.pane_pids.contains(&pane.pane_pid));

        let session = captured
            .sessions
            .iter()
            .find(|session| session.name == created.name)
            .expect("the created session is captured");
        assert_eq!(session.project, test.dir);
        assert_eq!(session.panes.len(), 1);
        assert_eq!(session.panes[0].cwd, test.dir);
        assert_eq!(session.panes[0].command, None);
        assert_eq!(session.panes[0].conversation, None);
    }

    #[test]
    fn capture_now_is_an_error_when_no_server_runs() {
        let test = disposable("capture-down");
        // Aucune session créée : le serveur n'a jamais démarré, ce n'est pas « zéro session ».
        assert!(capture_now(&test.server, &[], None, &HashMap::new()).is_err());
    }
}
