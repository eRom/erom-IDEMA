//! Les sessions des machines : qualification d'un listage par sa machine, aiguillage d'une clé, et clés d'une machine.

use idema_tmux::server::{ListedPane, ListedSession};

use super::key::{SessionTarget, machine_of, remote_session_key, split_session_key};

/// Qualifie le listage d'une machine distante : les noms de ses sessions (et ceux que ses panes référencent) deviennent
/// des clés `remote_session_key`, et `project` est vidé (le dossier d'une session distante n'est pas un projet du Mac).
pub fn qualify_listing(
    connection: &str,
    sessions: Vec<ListedSession>,
    panes: Vec<ListedPane>,
) -> (Vec<ListedSession>, Vec<ListedPane>) {
    let sessions = sessions
        .into_iter()
        .map(|session| ListedSession {
            name: remote_session_key(connection, &session.name),
            project: String::new(),
            ..session
        })
        .collect();
    let panes = panes
        .into_iter()
        .map(|pane| ListedPane {
            session: remote_session_key(connection, &pane.session),
            ..pane
        })
        .collect();
    (sessions, panes)
}

/// Où part une commande sur une session : le tmux du Mac (le nom est la clé) ou celui d'une machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Local(String),
    Remote { connection: String, tmux: String },
}

/// Aiguille une clé de session.
pub fn route(key: &str) -> Route {
    match split_session_key(key) {
        SessionTarget::Local(name) => Route::Local(name.to_owned()),
        SessionTarget::Remote { connection, tmux } => Route::Remote {
            connection: connection.to_owned(),
            tmux: tmux.to_owned(),
        },
    }
}

/// Les clés de `keys` qui appartiennent à `machine` (`LOCAL_MACHINE` ou une connexion), dans leur ordre.
pub fn machine_keys<'a>(keys: impl Iterator<Item = &'a str>, machine: &str) -> Vec<String> {
    keys.filter(|key| machine_of(key) == machine)
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machines::key::LOCAL_MACHINE;
    use idema_tmux::protocol::PaneId;

    fn session(name: &str) -> ListedSession {
        ListedSession {
            name: name.into(),
            project: "/home/llm/work".into(),
            created: 7,
            layout: String::new(),
            ideation: false,
            remote_name: String::new(),
            display_name: name.into(),
        }
    }

    fn pane(session: &str, id: u32) -> ListedPane {
        ListedPane {
            session: session.into(),
            pane: PaneId(id),
            active: id == 0,
            agent: String::new(),
            idnma_agent: "glm".into(),
        }
    }

    #[test]
    fn qualify_listing_keys_sessions_and_panes_by_machine() {
        let (sessions, panes) = qualify_listing(
            "caserne-claude",
            vec![session("testvpc")],
            vec![pane("testvpc", 0), pane("testvpc", 1)],
        );
        let key = "caserne-claude\u{1f}testvpc";
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, key);
        assert!(sessions[0].project.is_empty());
        assert_eq!(sessions[0].display_name, "testvpc");
        assert_eq!(panes.len(), 2);
        assert!(panes.iter().all(|pane| pane.session == key));
        // Le reste du pane est intact.
        assert_eq!(panes[1].pane, PaneId(1));
        assert_eq!(panes[1].idnma_agent, "glm");
    }

    #[test]
    fn route_splits_keys() {
        assert_eq!(
            route("idema-0a1b2c3d"),
            Route::Local("idema-0a1b2c3d".into())
        );
        assert_eq!(
            route(&remote_session_key("dell-lan", "mon audit")),
            Route::Remote {
                connection: "dell-lan".into(),
                tmux: "mon audit".into()
            }
        );
    }

    #[test]
    fn machine_keys_keeps_only_the_keys_of_one_machine() {
        let dell = remote_session_key("dell-lan", "a");
        let caserne = remote_session_key("caserne-claude", "a");
        let keys = ["idema-1", dell.as_str(), caserne.as_str(), "idema-2"];
        assert_eq!(
            machine_keys(keys.into_iter(), LOCAL_MACHINE),
            ["idema-1", "idema-2"]
        );
        assert_eq!(
            machine_keys(keys.into_iter(), "dell-lan"),
            [remote_session_key("dell-lan", "a")]
        );
        assert!(machine_keys(keys.into_iter(), "inconnue").is_empty());
    }
}
