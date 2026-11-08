//! Les sessions des machines : qualification d'un listage par sa machine, aiguillage d'une clé, et clés d'une machine.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use idema_tmux::server::{ListedPane, ListedSession, TmuxServer};

use super::key::{LOCAL_MACHINE, SessionTarget, machine_of, remote_session_key, split_session_key};
use super::registry::{MachineStatus, RemoteMachine, adhoc_gate};

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

/// Les noms des agents qui tournent dans les panes de chaque session (clés déjà qualifiées) : `@idema_agent`, sinon
/// `@idnma_agent` du pane, sans doublon, dans l'ordre des panes. Une session sans agent n'a pas d'entrée.
pub fn pane_agent_names(panes: &[ListedPane]) -> HashMap<String, Vec<String>> {
    let mut names: HashMap<String, Vec<String>> = HashMap::new();
    for pane in panes {
        let name = if pane.agent.is_empty() {
            &pane.idnma_agent
        } else {
            &pane.agent
        };
        if name.is_empty() {
            continue;
        }
        let kept = names.entry(pane.session.clone()).or_default();
        if !kept.contains(name) {
            kept.push(name.clone());
        }
    }
    names
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

/// Le serveur de chaque machine, tenu par l'espace de travail et partagé avec le compagnon : `LOCAL_MACHINE` mène au
/// tmux du Mac, une connexion au serveur bref de sa machine (présent tant qu'elle est connectée). Le compagnon capture
/// l'écran d'un pane depuis son propre fil : il n'a pas accès à l'espace de travail, seulement à cette table.
pub type SessionServers = Arc<RwLock<HashMap<String, Arc<TmuxServer>>>>;

/// Une table vide.
pub fn no_servers() -> SessionServers {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Une table qui ne connaît que le tmux du Mac (les tests du compagnon).
#[cfg(test)]
pub fn local_servers(local: Arc<TmuxServer>) -> SessionServers {
    Arc::new(RwLock::new(HashMap::from([(
        LOCAL_MACHINE.to_owned(),
        local,
    )])))
}

/// La table des serveurs que le compagnon doit voir : le tmux du Mac s'il est prêt, et le serveur bref de chaque machine
/// connectée dont le pool existe. Une machine en connexion, en échec ou retirée n'y figure pas : le compagnon ne lit alors
/// rien de ses sessions plutôt que de tomber sur un autre serveur.
pub fn wanted_servers(
    local: Option<&Arc<TmuxServer>>,
    machines: &[RemoteMachine],
) -> HashMap<String, Arc<TmuxServer>> {
    let mut wanted = HashMap::new();
    if let Some(local) = local {
        wanted.insert(LOCAL_MACHINE.to_owned(), Arc::clone(local));
    }
    for machine in machines {
        if machine.status == MachineStatus::Connected
            && let Some(pool) = &machine.pool
        {
            wanted.insert(machine.entry.connection.clone(), pool.brief());
        }
    }
    wanted
}

/// Le serveur de la session `session_key` et son nom tmux. Un pane (`%N`) ne se cherche jamais ailleurs que sur ce
/// serveur : les numéros de pane de deux machines se recouvrent. `None` quand la machine n'a pas de serveur (déconnectée,
/// retirée) : l'appelant ne retombe jamais sur celui du Mac.
pub fn resolve(servers: &SessionServers, session_key: &str) -> Option<(Arc<TmuxServer>, String)> {
    let (machine, tmux) = match split_session_key(session_key) {
        SessionTarget::Local(name) => (LOCAL_MACHINE, name),
        SessionTarget::Remote { connection, tmux } => (connection, tmux),
    };
    // Un verrou empoisonné ne rend pas la table incohérente (chaque écriture remplace un serveur d'un bloc).
    let servers = servers
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Some((Arc::clone(servers.get(machine)?), tmux.to_owned()))
}

/// Lance `work` (bloquant) sur le serveur de la session `session_key`. Sur une machine, l'appel passe par la porte des
/// appels ponctuels (`adhoc_gate`), celle de `Workspace::spawn_for` : une machine n'a jamais plus de deux canaux brefs en
/// vol, la relecture des sessions et un seul appel ponctuel, que celui-ci vienne de l'interface ou du compagnon. `None`
/// sans serveur.
pub fn with_session_server<T>(
    servers: &SessionServers,
    session_key: &str,
    work: impl FnOnce(&TmuxServer, &str) -> T,
) -> Option<T> {
    let (server, tmux) = resolve(servers, session_key)?;
    let gate = match split_session_key(session_key) {
        SessionTarget::Local(_) => None,
        SessionTarget::Remote { connection, .. } => Some(adhoc_gate(connection)),
    };
    let _held = gate.as_ref().map(|gate| {
        gate.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    });
    Some(work(&server, &tmux))
}

/// La session à resélectionner au lancement (spec ssh-tmux §3.5) : celle que `state.json` gardait quand c'était une
/// session distante, tant que Romain n'a rien choisi d'autre depuis et que sa machine est connectée avec la session
/// encore présente (`connected_keys` : les clés des sessions des machines connectées). Une session locale n'est pas
/// concernée : le démarrage ordinaire la montre.
pub fn should_reselect(
    saved: Option<&str>,
    chosen_since_launch: bool,
    connected_keys: &[String],
) -> Option<String> {
    let saved = saved?;
    if chosen_since_launch || machine_of(saved) == LOCAL_MACHINE {
        return None;
    }
    connected_keys
        .iter()
        .any(|key| key == saved)
        .then(|| saved.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn pane_agent_names_prefer_the_idema_agent_and_dedupe_per_session() {
        let key = |name: &str| remote_session_key("dell-lan", name);
        let tagged = |session: &str, id: u32, agent: &str, idnma: &str| ListedPane {
            agent: agent.into(),
            idnma_agent: idnma.into(),
            ..pane(&key(session), id)
        };
        let panes = [
            tagged("a", 0, "Claude", "sonnet"),
            tagged("a", 1, "", "glm"),
            tagged("a", 2, "", "glm"),
            tagged("a", 3, "", ""),
            tagged("b", 4, "", ""),
        ];
        let names = pane_agent_names(&panes);
        assert_eq!(names[&key("a")], ["Claude", "glm"]);
        // Une session sans agent n'a pas d'entrée.
        assert!(!names.contains_key(&key("b")));
    }

    fn fake_server(socket: &str) -> Arc<TmuxServer> {
        Arc::new(TmuxServer::new(
            "/nonexistent/tmux".into(),
            socket,
            "/nonexistent/tmux.conf".into(),
            Vec::new(),
        ))
    }

    fn servers_of(entries: &[(&str, &Arc<TmuxServer>)]) -> SessionServers {
        Arc::new(RwLock::new(
            entries
                .iter()
                .map(|(machine, server)| ((*machine).to_owned(), Arc::clone(server)))
                .collect(),
        ))
    }

    #[test]
    fn resolve_never_crosses_machines() {
        let local = fake_server("idema-test-resolve-local");
        let remote = fake_server("idema-test-resolve-remote");
        let servers = servers_of(&[(LOCAL_MACHINE, &local), ("caserne-claude", &remote)]);
        // Le même numéro de pane `%0` existe sur les deux serveurs : chaque clé mène au sien.
        let (found, name) =
            resolve(&servers, &remote_session_key("caserne-claude", "testvpc")).unwrap();
        assert!(Arc::ptr_eq(&found, &remote));
        assert_eq!(name, "testvpc");
        let (found, name) = resolve(&servers, "idema-0a1b2c3d").unwrap();
        assert!(Arc::ptr_eq(&found, &local));
        assert_eq!(name, "idema-0a1b2c3d");
        // Une machine inconnue (déconnectée, retirée) ne retombe jamais sur le serveur local.
        assert!(resolve(&servers, &remote_session_key("dell-lan", "testvpc")).is_none());
        let only_remote = servers_of(&[("caserne-claude", &remote)]);
        assert!(resolve(&only_remote, "idema-0a1b2c3d").is_none());
    }

    #[test]
    fn a_remote_call_waits_for_the_gate_of_its_machine() {
        let local = fake_server("idema-test-gate-local");
        let remote = fake_server("idema-test-gate-remote");
        let servers = servers_of(&[(LOCAL_MACHINE, &local), ("gate-test-machine", &remote)]);
        let key = remote_session_key("gate-test-machine", "testvpc");
        // Un autre appel ponctuel tient le second canal bref de la machine.
        let gate = adhoc_gate("gate-test-machine");
        let held = gate.lock().unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let worker = {
            let (servers, key) = (Arc::clone(&servers), key.clone());
            std::thread::spawn(move || {
                with_session_server(&servers, &key, |_, tmux| {
                    done.send(tmux.to_owned()).unwrap();
                })
            })
        };
        assert!(
            finished
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "the call must wait for the gate"
        );
        drop(held);
        assert_eq!(
            finished
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            "testvpc"
        );
        assert_eq!(worker.join().unwrap(), Some(()));
        // Un appel local ne passe jamais par une porte de machine.
        let gate = adhoc_gate("gate-test-machine");
        let _held = gate.lock().unwrap();
        assert_eq!(
            with_session_server(&servers, "idema-0a1b2c3d", |_, _| 7),
            Some(7)
        );
        // Sans serveur, rien ne tourne.
        assert_eq!(
            with_session_server(&servers, &remote_session_key("absente", "x"), |_, _| 7),
            None
        );
    }

    fn machine(connection: &str, status: MachineStatus, with_pool: bool) -> RemoteMachine {
        let mut machine = RemoteMachine::new(crate::config::RemoteMachineEntry {
            label: connection.to_owned(),
            connection: connection.to_owned(),
        });
        machine.status = status;
        if with_pool {
            machine.pool = Some(crate::machines::registry::SlotPool::new(
                idema_tmux::remote::RemoteSpec {
                    destination: connection.to_owned(),
                    control_dir: std::path::PathBuf::from("/tmp/idema-501-00000000"),
                    slot: 0,
                    ssh_env: Vec::new(),
                    home: "/home/llm".into(),
                    shell: "/bin/bash".into(),
                },
            ));
        }
        machine
    }

    #[test]
    fn only_connected_machines_have_a_server_for_the_companion() {
        let local = fake_server("idema-test-wanted-local");
        let dell = machine("dell-lan", MachineStatus::Connected, true);
        let caserne = machine("caserne-claude", MachineStatus::Connecting, true);
        let failed = machine("vieille", MachineStatus::Connecting, false);
        let machines = [dell, caserne, failed];
        let wanted = wanted_servers(Some(&local), &machines);
        let mut keys: Vec<&str> = wanted.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["dell-lan", LOCAL_MACHINE]);
        assert!(Arc::ptr_eq(&wanted[LOCAL_MACHINE], &local));
        let brief = machines[0].pool.as_ref().unwrap().brief();
        assert!(Arc::ptr_eq(&wanted["dell-lan"], &brief));
        // La machine retirée de la config n'a plus de serveur ; le Mac pas prêt non plus.
        let removed = wanted_servers(Some(&local), &machines[1..]);
        assert_eq!(removed.keys().collect::<Vec<_>>(), [LOCAL_MACHINE]);
        assert!(wanted_servers(None, &[]).is_empty());
    }

    #[test]
    fn should_reselect_waits_for_the_machine_and_respects_a_new_choice() {
        let key = remote_session_key("caserne-claude", "testvpc");
        let other = remote_session_key("caserne-claude", "autre");
        // La machine n'est pas connectée (ou la session n'y existe plus) : rien.
        assert_eq!(should_reselect(Some(&key), false, &[]), None);
        assert_eq!(
            should_reselect(Some(&key), false, std::slice::from_ref(&other)),
            None
        );
        // Connectée et la session existe : on la reprend.
        assert_eq!(
            should_reselect(Some(&key), false, &[other.clone(), key.clone()]),
            Some(key.clone())
        );
        // Romain a choisi autre chose depuis le lancement : son choix gagne.
        assert_eq!(
            should_reselect(Some(&key), true, std::slice::from_ref(&key)),
            None
        );
        // Rien de sauvé, ou une session locale (le démarrage ordinaire s'en charge).
        assert_eq!(
            should_reselect(None, false, std::slice::from_ref(&key)),
            None
        );
        assert_eq!(
            should_reselect(
                Some("idema-0a1b2c3d"),
                false,
                &["idema-0a1b2c3d".to_owned()]
            ),
            None
        );
    }
}
