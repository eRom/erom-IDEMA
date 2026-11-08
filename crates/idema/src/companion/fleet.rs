//! L'instantané de la flotte que le téléphone affiche : les projets et les sessions dans l'ordre de la barre, avec,
//! par session, son état, l'instant où elle y est entrée, ses agents et ses panes. Les clés JSON sont les noms des
//! champs. Rien ici ne dépend de GPUI : l'espace de travail remplit les structures, le service les sert.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use idema_term::activity::ActivityState;
use serde::Serialize;

use crate::assets::logo_path;
use crate::config::AgentEntry;
use crate::machines::idnma::IdnmaAgentInfo;
use crate::sidebar_model::{SessionSummary, dock_badge_count, navigation_order};

/// La flotte entière, telle que `GET fleet` la rend et que le flux `events` la pousse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Fleet {
    /// Le nom du Mac.
    pub mac: String,
    /// L'instant de construction, en millisecondes depuis l'époque Unix.
    pub generated_at: u64,
    /// Le même nombre que le badge du Dock.
    pub badge: usize,
    /// Le libellé de la section des bouillons (`[ideation]`), absent quand la fonction est éteinte.
    pub ideation_label: Option<String>,
    pub projects: Vec<FleetProject>,
    /// Les sessions dans l'ordre de la barre : les bouillons d'abord, puis chaque projet.
    pub sessions: Vec<FleetSession>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FleetProject {
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FleetSession {
    /// Le nom tmux, `idema-<8 hex>` : l'identifiant stable.
    pub name: String,
    pub display_name: String,
    /// Le dossier du projet ; vide pour un bouillon.
    pub project: String,
    pub ideation: bool,
    pub state: &'static str,
    /// L'instant d'entrée dans l'état courant, en millisecondes depuis l'époque Unix.
    pub state_since: u64,
    pub seen: bool,
    pub agents: Vec<FleetAgent>,
    pub panes: Vec<FleetPane>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FleetAgent {
    /// Le nom de l'entrée du lanceur (ou de l'agent détecté) quand elle n'a pas de logo.
    pub label: String,
    /// Le nom du logo sans extension (`/logos/<nom>.png`), s'il y en a un.
    pub logo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FleetPane {
    pub id: u32,
    pub active: bool,
    pub state: &'static str,
}

impl Fleet {
    /// Les deux instantanés disent-ils la même chose ? `generated_at` ne compte pas : republier pour lui seul
    /// réveillerait chaque flux sans rien leur apprendre.
    pub fn differs_from(&self, other: &Self) -> bool {
        self.mac != other.mac
            || self.badge != other.badge
            || self.ideation_label != other.ideation_label
            || self.projects != other.projects
            || self.sessions != other.sessions
    }
}

/// Le nom JSON d'un état d'activité.
pub fn state_label(state: ActivityState) -> &'static str {
    match state {
        ActivityState::Idle => "idle",
        ActivityState::Running => "running",
        ActivityState::Waiting => "waiting",
        ActivityState::Error => "error",
    }
}

/// Une date en millisecondes depuis l'époque Unix ; zéro avant l'époque.
pub fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    })
}

/// Le nom du Mac, lu une fois (sans le suffixe `.local` de Bonjour).
pub fn mac_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let mut buffer = [0u8; 256];
        // SAFETY : le tampon est valide pour `buffer.len()` octets ; `gethostname` y écrit un nom terminé par un zéro.
        let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        if status != 0 {
            return "Mac".to_owned();
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        let host = String::from_utf8_lossy(&buffer[..end]);
        let host = host.strip_suffix(".local").unwrap_or(&host).trim();
        if host.is_empty() {
            "Mac".to_owned()
        } else {
            host.to_owned()
        }
    })
}

/// Le nom d'un projet dans l'instantané : celui de son dossier.
pub fn project_of(path: &Path) -> FleetProject {
    FleetProject {
        path: path.to_string_lossy().into_owned(),
        name: crate::project::display_name(path),
    }
}

/// Les agents `names` (noms d'entrées du lanceur), dans leur ordre : le logo de l'entrée, ou son nom quand elle n'en
/// a pas. Même déduplication que la barre (`agent_marks`) : Antigravity et Gemini, qui partagent un logo, n'en
/// montrent qu'un.
pub fn fleet_agents(names: &[String], launchers: &[AgentEntry]) -> Vec<FleetAgent> {
    let mut agents: Vec<FleetAgent> = Vec::new();
    for name in names {
        let logo = launchers
            .iter()
            .find(|entry| &entry.name == name)
            .map(AgentEntry::icon_name)
            .filter(|icon| logo_path(icon).is_some());
        let shown = agents.iter().any(|agent| match (&agent.logo, &logo) {
            (Some(held), Some(wanted)) => held == wanted,
            (None, None) => &agent.label == name,
            _ => false,
        });
        if !shown {
            agents.push(FleetAgent {
                label: name.clone(),
                logo,
            });
        }
    }
    agents
}

/// Les agents `names` d'une session distante (`@idema_agent`, sinon `@idnma_agent` : la clé de l'agent dans le
/// `~/.idnma.yml` de sa machine), dans leur ordre : le `label` de l'agent dans ce fichier, sinon son nom ; le logo de son
/// `avatar`, sinon celui de son nom en minuscules (la règle de la barre, `remote_agent_marks`), sinon aucun. Dédoublonné
/// comme `fleet_agents`.
pub fn remote_fleet_agents(names: &[String], declared: &[IdnmaAgentInfo]) -> Vec<FleetAgent> {
    let mut agents: Vec<FleetAgent> = Vec::new();
    for name in names {
        let info = declared.iter().find(|agent| &agent.key == name);
        let logo = info
            .and_then(|agent| agent.avatar.as_deref())
            .map(str::to_lowercase)
            .filter(|avatar| logo_path(avatar).is_some())
            .or_else(|| Some(name.to_lowercase()).filter(|lower| logo_path(lower).is_some()));
        let label = info
            .and_then(|agent| agent.label.clone())
            .unwrap_or_else(|| name.clone());
        let shown = agents.iter().any(|agent| match (&agent.logo, &logo) {
            (Some(held), Some(wanted)) => held == wanted,
            (None, None) => agent.label == label,
            _ => false,
        });
        if !shown {
            agents.push(FleetAgent { label, logo });
        }
    }
    agents
}

/// Ce que l'espace de travail sait d'une session, avant d'en faire une ligne de l'instantané.
pub struct SessionInput {
    pub summary: SessionSummary,
    pub state_since: SystemTime,
    pub seen: bool,
    /// Les noms des agents reconnus dans ses panes (relevé des serveurs) ; ignorés pour un bouillon.
    pub agent_names: Vec<String>,
    /// Les agents déclarés par la machine d'une session distante (son `~/.idnma.yml`) ; vide pour une session locale.
    pub remote_agents: Vec<IdnmaAgentInfo>,
    pub panes: Vec<FleetPane>,
}

/// Le pseudo-projet d'une machine distante dans l'instantané : ses sessions s'y rangent comme dans un projet, sous le
/// label de la machine. La page de l'iPhone ne change pas.
fn machine_project(connection: &str) -> String {
    format!("ssh://{connection}")
}

/// Compose l'instantané : les sessions dans l'ordre de la barre avec tous les projets dépliés (les bouillons en tête
/// quand `[ideation]` est présente, donc `ideation_label` est `Some`), le badge du Dock, et pour chaque session ses
/// agents. Un bouillon porte toujours le logo de Claude sous le libellé de la section, et n'a pas de projet. Sans
/// `[ideation]`, les bouillons n'y figurent pas.
///
/// `machines` : (connexion, label) de chaque machine distante, dans l'ordre de la config. Une session distante a pour
/// projet `ssh://<connexion>`, et chaque machine qui a au moins une session ajoute `FleetProject { path:
/// "ssh://<connexion>", name: <label> }` après les projets du Mac.
pub fn compose_fleet(
    mac: &str,
    now: SystemTime,
    ideation_label: Option<&str>,
    projects: &[PathBuf],
    machines: &[(String, String)],
    launchers: &[AgentEntry],
    sessions: Vec<SessionInput>,
) -> Fleet {
    // Le dossier d'une session distante n'est pas un projet du Mac : elle prend le pseudo-projet de sa machine.
    let summaries: Vec<SessionSummary> = sessions
        .iter()
        .map(|session| {
            let mut summary = session.summary.clone();
            if let Some(connection) = &summary.machine {
                summary.project = PathBuf::from(machine_project(connection));
            }
            summary
        })
        .collect();
    let machine_projects: Vec<FleetProject> = machines
        .iter()
        .filter(|(connection, _)| {
            summaries
                .iter()
                .any(|summary| summary.machine.as_deref() == Some(connection.as_str()))
        })
        .map(|(connection, label)| FleetProject {
            path: machine_project(connection),
            name: label.clone(),
        })
        .collect();
    let ordered_projects: Vec<PathBuf> = projects
        .iter()
        .cloned()
        .chain(
            machine_projects
                .iter()
                .map(|project| PathBuf::from(&project.path)),
        )
        .collect();
    let order = navigation_order(
        &ordered_projects,
        |_| true,
        ideation_label.is_some(),
        &summaries,
    );
    let sessions = order
        .iter()
        .filter_map(|name| {
            sessions
                .iter()
                .zip(&summaries)
                .find(|(_, summary)| summary.name == *name)
        })
        .map(|(session, summary)| {
            let agents = if summary.ideation {
                vec![FleetAgent {
                    label: ideation_label.unwrap_or_default().to_owned(),
                    logo: Some("claude".into()),
                }]
            } else if summary.machine.is_some() {
                remote_fleet_agents(&session.agent_names, &session.remote_agents)
            } else {
                fleet_agents(&session.agent_names, launchers)
            };
            FleetSession {
                name: summary.name.clone(),
                display_name: summary.display_name.clone(),
                project: if summary.ideation {
                    String::new()
                } else {
                    summary.project.to_string_lossy().into_owned()
                },
                ideation: summary.ideation,
                state: state_label(summary.state),
                state_since: unix_millis(session.state_since),
                seen: session.seen,
                agents,
                panes: session.panes.clone(),
            }
        })
        .collect();
    Fleet {
        mac: mac.to_owned(),
        generated_at: unix_millis(now),
        badge: dock_badge_count(summaries.iter(), ideation_label.is_some()),
        ideation_label: ideation_label.map(str::to_owned),
        projects: projects
            .iter()
            .map(|path| project_of(path))
            .chain(machine_projects)
            .collect(),
        sessions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, icon: Option<&str>) -> AgentEntry {
        AgentEntry {
            name: name.into(),
            harness: None,
            command: format!("idnma run {}", name.to_lowercase()),
            default: false,
            icon: icon.map(str::to_string),
        }
    }

    #[test]
    fn fleet_agents_fall_back_to_the_name_and_dedupe_shared_logos() {
        let launchers = [
            entry("GLM", None),
            entry("Antigravity", Some("gemini")),
            entry("Maison", None),
            entry("Gemini", None),
        ];
        let names = ["GLM", "Antigravity", "Maison", "Gemini", "Disparu"].map(String::from);
        let agents = fleet_agents(&names, &launchers);
        assert_eq!(
            agents,
            [
                FleetAgent {
                    label: "GLM".into(),
                    logo: Some("glm".into())
                },
                FleetAgent {
                    label: "Antigravity".into(),
                    logo: Some("gemini".into())
                },
                FleetAgent {
                    label: "Maison".into(),
                    logo: None
                },
                FleetAgent {
                    label: "Disparu".into(),
                    logo: None
                },
            ],
            "Gemini shares the logo of Antigravity: one mark; unknown names keep their name"
        );
        assert!(fleet_agents(&[], &launchers).is_empty());
        // La barre montre autant de marques que le téléphone d'agents : même déduplication.
        assert_eq!(
            agents.len(),
            crate::sidebar::agent_marks(&names, &launchers).len()
        );
    }

    #[test]
    fn remote_agents_take_the_labels_and_avatars_of_their_machine() {
        let declared = |key: &str, label: Option<&str>, avatar: Option<&str>| IdnmaAgentInfo {
            key: key.into(),
            label: label.map(str::to_owned),
            avatar: avatar.map(str::to_owned),
        };
        let declared = [
            declared("sonnet", Some("Sonnet 5.5"), Some("Claude")),
            declared("glm", None, None),
            declared("maison", Some("Maison"), Some("inconnu")),
        ];
        let names = ["sonnet", "glm", "maison", "claude", "disparu"].map(String::from);
        assert_eq!(
            remote_fleet_agents(&names, &declared),
            [
                FleetAgent {
                    label: "Sonnet 5.5".into(),
                    logo: Some("claude".into())
                },
                FleetAgent {
                    label: "glm".into(),
                    logo: Some("glm".into())
                },
                FleetAgent {
                    label: "Maison".into(),
                    logo: None
                },
                FleetAgent {
                    label: "disparu".into(),
                    logo: None
                },
            ],
            "the avatar picks the logo, the label the text; `claude` shares the logo of `sonnet`: one mark"
        );
    }

    fn input(name: &str, project: &str, ideation: bool, state: ActivityState) -> SessionInput {
        SessionInput {
            summary: SessionSummary {
                name: name.into(),
                project: PathBuf::from(project),
                display_name: name.to_uppercase(),
                created: 0,
                state,
                ideation,
                machine: None,
            },
            state_since: UNIX_EPOCH + std::time::Duration::from_secs(100),
            seen: false,
            agent_names: vec!["Faux".into()],
            remote_agents: Vec::new(),
            panes: vec![FleetPane {
                id: 1,
                active: true,
                state: state_label(state),
            }],
        }
    }

    fn compose(label: Option<&str>, sessions: Vec<SessionInput>) -> Fleet {
        let projects = [PathBuf::from("/b"), PathBuf::from("/a")];
        let launchers = [entry("Faux", Some("gemini"))];
        compose_fleet(
            "Mac",
            UNIX_EPOCH + std::time::Duration::from_secs(200),
            label,
            &projects,
            &[],
            &launchers,
            sessions,
        )
    }

    #[test]
    fn the_fleet_follows_the_bar_with_ideation_first() {
        let fleet = compose(
            Some("Fable"),
            vec![
                input("in-a", "/a", false, ActivityState::Idle),
                input("in-b", "/b", false, ActivityState::Running),
                input("brew", "/ideation", true, ActivityState::Waiting),
            ],
        );
        // Les bouillons d'abord, puis les projets dans l'ordre de la barre (/b avant /a), pas l'ordre des entrées.
        let names: Vec<_> = fleet.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["brew", "in-b", "in-a"]);
        assert_eq!(fleet.ideation_label.as_deref(), Some("Fable"));
        assert_eq!(fleet.generated_at, 200_000);
        assert_eq!(fleet.mac, "Mac");
        let projects: Vec<_> = fleet.projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(projects, ["b", "a"]);

        let brew = &fleet.sessions[0];
        assert!(brew.ideation);
        assert_eq!(brew.project, "", "a brew has no project");
        assert_eq!(
            brew.agents,
            [FleetAgent {
                label: "Fable".into(),
                logo: Some("claude".into())
            }],
            "the section label and the claude logo, whatever the process scan says"
        );
        assert_eq!((brew.state, brew.state_since), ("waiting", 100_000));
        assert_eq!(fleet.badge, 1, "the waiting brew counts in the badge");

        let in_b = &fleet.sessions[1];
        assert_eq!(in_b.project, "/b");
        assert_eq!(
            in_b.agents,
            [FleetAgent {
                label: "Faux".into(),
                logo: Some("gemini".into())
            }]
        );
        assert_eq!(in_b.panes.len(), 1);
    }

    /// Une session distante telle que l'espace de travail la rend : clé qualifiée, aucun projet du Mac.
    fn remote_input(connection: &str, tmux: &str, state: ActivityState) -> SessionInput {
        SessionInput {
            summary: SessionSummary {
                name: crate::machines::key::remote_session_key(connection, tmux),
                project: PathBuf::new(),
                display_name: tmux.into(),
                created: 0,
                state,
                ideation: false,
                machine: Some(connection.into()),
            },
            state_since: UNIX_EPOCH + std::time::Duration::from_secs(100),
            seen: false,
            agent_names: vec!["sonnet".into()],
            remote_agents: vec![IdnmaAgentInfo {
                key: "sonnet".into(),
                label: Some("Sonnet 5.5".into()),
                avatar: Some("Claude".into()),
            }],
            panes: vec![FleetPane {
                id: 0,
                active: true,
                state: state_label(state),
            }],
        }
    }

    #[test]
    fn a_remote_session_groups_under_its_machine_label() {
        let machines = [
            ("caserne-claude".to_owned(), "VPS Hostinger".to_owned()),
            ("dell-lan".to_owned(), "Dell".to_owned()),
        ];
        let key = crate::machines::key::remote_session_key("caserne-claude", "testvpc");
        let fleet = compose_fleet(
            "Mac",
            UNIX_EPOCH + std::time::Duration::from_secs(200),
            None,
            &[PathBuf::from("/b")],
            &machines,
            &[entry("Faux", Some("gemini"))],
            vec![
                remote_input("caserne-claude", "testvpc", ActivityState::Waiting),
                input("in-b", "/b", false, ActivityState::Idle),
            ],
        );
        let remote = fleet.sessions.iter().find(|s| s.name == key).unwrap();
        assert_eq!(remote.project, "ssh://caserne-claude");
        assert_eq!(remote.display_name, "testvpc");
        assert!(!remote.ideation);
        // L'agent porte le label et l'avatar que la machine lui donne dans son `~/.idnma.yml`.
        assert_eq!(
            remote.agents,
            [FleetAgent {
                label: "Sonnet 5.5".into(),
                logo: Some("claude".into())
            }]
        );
        // Les projets du Mac d'abord, puis la machine qui a une session ; celle qui n'en a pas n'ajoute rien.
        assert_eq!(
            fleet.projects.last().unwrap(),
            &FleetProject {
                path: "ssh://caserne-claude".into(),
                name: "VPS Hostinger".into()
            }
        );
        assert_eq!(fleet.projects.len(), 2);
        // L'ordre de la barre : le projet local, puis la machine ; l'attente distante compte au badge.
        let names: Vec<_> = fleet.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["in-b", key.as_str()]);
        assert_eq!(fleet.badge, 1);
    }

    #[test]
    fn without_ideation_the_brews_are_not_in_the_fleet() {
        let fleet = compose(
            None,
            vec![
                input("in-a", "/a", false, ActivityState::Error),
                input("brew", "/ideation", true, ActivityState::Waiting),
            ],
        );
        let names: Vec<_> = fleet.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["in-a"]);
        assert_eq!(fleet.ideation_label, None);
        assert_eq!(fleet.badge, 1, "only the project session counts");
    }

    #[test]
    fn the_mac_has_a_name() {
        let name = mac_name();
        assert!(!name.is_empty());
        assert!(!name.ends_with(".local"));
        assert!(!name.contains('\0'));
    }

    #[test]
    fn states_have_stable_json_names() {
        assert_eq!(state_label(ActivityState::Idle), "idle");
        assert_eq!(state_label(ActivityState::Running), "running");
        assert_eq!(state_label(ActivityState::Waiting), "waiting");
        assert_eq!(state_label(ActivityState::Error), "error");
    }

    fn session(name: &str, state: ActivityState) -> FleetSession {
        FleetSession {
            name: name.into(),
            display_name: name.into(),
            project: "/p".into(),
            ideation: false,
            state: state_label(state),
            state_since: 1_000,
            seen: false,
            agents: Vec::new(),
            panes: vec![
                FleetPane {
                    id: 1,
                    active: true,
                    state: state_label(state),
                },
                FleetPane {
                    id: 2,
                    active: false,
                    state: "idle",
                },
            ],
        }
    }

    #[test]
    fn a_republication_needs_a_real_change() {
        let base = Fleet {
            mac: "Mac".into(),
            generated_at: 10,
            sessions: vec![session("a", ActivityState::Running)],
            ..Fleet::default()
        };
        let later = Fleet {
            generated_at: 99,
            ..base.clone()
        };
        assert!(
            !base.differs_from(&later),
            "the build time alone is not news"
        );

        // Un pane qui change seul d'état, la session gardant le sien : c'est une nouvelle flotte.
        let mut pane_changed = base.clone();
        pane_changed.sessions[0].panes[1].state = "waiting";
        assert!(base.differs_from(&pane_changed));
        let mut active_moved = base.clone();
        active_moved.sessions[0].panes[0].active = false;
        active_moved.sessions[0].panes[1].active = true;
        assert!(base.differs_from(&active_moved));
        let mut agents_changed = base.clone();
        agents_changed.sessions[0].agents.push(FleetAgent {
            label: "Codex".into(),
            logo: Some("codex".into()),
        });
        assert!(base.differs_from(&agents_changed));
        let mut renamed = base.clone();
        renamed.sessions[0].display_name = "b".into();
        assert!(base.differs_from(&renamed));
        let mut badge = base.clone();
        badge.badge = 1;
        assert!(base.differs_from(&badge));
    }

    #[test]
    fn the_json_keys_are_the_field_names() {
        let fleet = Fleet {
            mac: "Mac".into(),
            generated_at: 5,
            badge: 1,
            ideation_label: Some("Claude".into()),
            projects: vec![project_of(Path::new("/dev/idema"))],
            sessions: vec![session("a", ActivityState::Waiting)],
        };
        let json = serde_json::to_value(&fleet).unwrap();
        for key in [
            "mac",
            "generated_at",
            "badge",
            "ideation_label",
            "projects",
            "sessions",
        ] {
            assert!(json.get(key).is_some(), "{key}");
        }
        assert_eq!(json["projects"][0]["name"], "idema");
        let first = &json["sessions"][0];
        for key in [
            "name",
            "display_name",
            "project",
            "ideation",
            "state",
            "state_since",
            "seen",
            "agents",
            "panes",
        ] {
            assert!(first.get(key).is_some(), "{key}");
        }
        assert_eq!(first["state"], "waiting");
        assert_eq!(first["panes"][0]["id"], 1);
        assert_eq!(first["panes"][0]["active"], true);
    }
}
