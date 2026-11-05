//! L'instantané de la flotte que le téléphone affiche : les projets et les sessions dans l'ordre de la barre, avec,
//! par session, son état, l'instant où elle y est entrée, ses agents et ses panes. Les clés JSON sont les noms des
//! champs. Rien ici ne dépend de GPUI : l'espace de travail remplit les structures, le service les sert.

use std::path::Path;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use idema_term::activity::ActivityState;
use serde::Serialize;

use crate::assets::logo_path;
use crate::config::AgentEntry;

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
