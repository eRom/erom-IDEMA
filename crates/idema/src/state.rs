//! `~/.config/idema/state.json` (spec §3.4, §8) : projets, épinglages et état de la barre. tmux fait foi pour
//! les sessions ; ce fichier ne garde que ce que tmux ne sait pas.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize};

use crate::artifacts::model::KindFilter;
use crate::artifacts::view::ArtifactsLayout;
use crate::browser::tabs::SavedTabs;

/// Version écrite par cette build. Un fichier plus récent est lu sans jamais être réécrit.
pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub path: PathBuf,
    /// Horodatages RFC 3339 en UTC, à la seconde : l'ordre alphabétique est l'ordre chronologique.
    pub added_at: String,
    pub pinned_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiState {
    pub sidebar_visible: bool,
    pub expanded: Vec<PathBuf>,
    pub selected_session: Option<String>,
    /// Largeur de la barre en unités de la maquette ; `None` : `SIDEBAR_WIDTH`. Un état écrit avant ce champ
    /// se lit sans lui, d'où le `default` : sans lui, un `state.json` existant partirait en `.bak`.
    #[serde(default)]
    pub sidebar_width: Option<f32>,
    /// Les onglets du navigateur à rouvrir. Une valeur illisible se lit `None` sans rien casser d'autre.
    #[serde(default, deserialize_with = "lenient_saved_tabs")]
    pub browser_tabs: Option<SavedTabs>,
    /// Le port retenu de l'entrée `dev` de chaque projet (spec P4 §5.1). Une valeur illisible se lit sans cette entrée,
    /// sans rien casser d'autre.
    #[serde(default, deserialize_with = "lenient_dev_ports")]
    pub servers_dev_ports: HashMap<PathBuf, u16>,
    /// Largeur du volet droit, la même pour tous les volets, en unités de la maquette ; `None` :
    /// `RIGHT_PANEL_DEFAULT_WIDTH`. Les anciennes clés par volet d'un `state.json` existant sont ignorées à la lecture.
    #[serde(default)]
    pub panel_width: Option<f32>,
    /// L'arbre du volet Fichiers est replié en bande étroite.
    #[serde(default)]
    pub files_tree_collapsed: bool,
    /// Largeur de l'arbre du volet Fichiers en unités de la maquette, globale comme le repli ; `None` : `TREE_WIDTH`.
    #[serde(default)]
    pub files_tree_width: Option<f32>,
    /// Largeur de la liste du volet Changes, globale comme celle de l'arbre ; `None` : `LIST_WIDTH`.
    #[serde(default)]
    pub changes_list_width: Option<f32>,
    /// La liste du volet Changes est masquée : le diff prend toute la place.
    #[serde(default)]
    pub changes_list_hidden: bool,
    /// La section « Bouillon de neurones » est repliée dans la barre. Un état écrit avant ce champ se lit sans lui.
    #[serde(default)]
    pub ideation_collapsed: bool,
    /// Les machines repliées dans la barre, par clé (`LOCAL_MACHINE` pour Localhost, sinon la connexion). Un état écrit
    /// avant ce champ se lit sans lui : aucune machine repliée.
    #[serde(default)]
    pub collapsed_machines: Vec<String>,
    /// Grille ou liste de la galerie Artifacts. Une valeur illisible se lit « grille », sans rien casser d'autre.
    #[serde(default, deserialize_with = "lenient_or_default")]
    pub artifacts_layout: ArtifactsLayout,
    /// Les genres cochés dans le filtre de la galerie. Une valeur illisible se lit « tout coché ».
    #[serde(default, deserialize_with = "lenient_or_default")]
    pub artifacts_filter: KindFilter,
}

/// Les onglets sauvegardés se lisent sans jamais faire échouer le fichier : une valeur illisible vaut `None`
/// (un seul Accueil à l'ouverture), le reste de l'état se charge.
fn lenient_saved_tabs<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SavedTabs>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok().flatten())
}

/// Un réglage d'interface se lit sans jamais faire échouer le fichier : une valeur illisible vaut sa valeur par défaut.
fn lenient_or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// Les ports retenus se lisent sans jamais faire échouer le fichier : une entrée qui n'est pas un port (1 à 65535) est
/// ignorée, et une valeur qui n'est pas un objet vaut « aucun port retenu ».
fn lenient_dev_ports<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<HashMap<PathBuf, u16>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let Some(map) = value.as_object() else {
        return Ok(HashMap::new());
    };
    Ok(map
        .iter()
        .filter_map(|(project, port)| {
            let port = u16::try_from(port.as_u64()?)
                .ok()
                .filter(|port| *port > 0)?;
            Some((PathBuf::from(project), port))
        })
        .collect())
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            sidebar_visible: true,
            expanded: Vec::new(),
            selected_session: None,
            sidebar_width: None,
            browser_tabs: None,
            servers_dev_ports: HashMap::new(),
            panel_width: None,
            files_tree_collapsed: false,
            files_tree_width: None,
            changes_list_width: None,
            changes_list_hidden: false,
            ideation_collapsed: false,
            collapsed_machines: Vec::new(),
            artifacts_layout: ArtifactsLayout::default(),
            artifacts_filter: KindFilter::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub projects: Vec<ProjectEntry>,
    #[serde(default)]
    pub ui: UiState,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            projects: Vec::new(),
            ui: UiState::default(),
        }
    }
}

impl State {
    /// Épinglés d'abord, par date d'épinglage, puis les autres, par date d'ajout (spec §4.1).
    pub fn ordered_projects(&self) -> Vec<&ProjectEntry> {
        let mut projects: Vec<&ProjectEntry> = self.projects.iter().collect();
        projects.sort_by(|a, b| match (&a.pinned_at, &b.pinned_at) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.added_at.cmp(&b.added_at),
        });
        projects
    }

    pub fn project(&self, path: &Path) -> Option<&ProjectEntry> {
        self.projects.iter().find(|project| project.path == path)
    }

    /// Ajoute le projet s'il manque. Vrai s'il a été ajouté. Un projet ajouté est déplié.
    pub fn add_project(&mut self, path: &Path, now: &str) -> bool {
        if self.project(path).is_some() {
            return false;
        }
        self.projects.push(ProjectEntry {
            path: path.to_path_buf(),
            added_at: now.to_string(),
            pinned_at: None,
        });
        self.set_expanded(path, true);
        true
    }

    pub fn set_pinned(&mut self, path: &Path, pinned_at: Option<&str>) {
        if let Some(project) = self.projects.iter_mut().find(|p| p.path == path) {
            project.pinned_at = pinned_at.map(str::to_string);
        }
    }

    /// Retire le projet de la liste ; le disque n'est jamais touché.
    pub fn remove_project(&mut self, path: &Path) {
        self.projects.retain(|project| project.path != path);
        self.ui.expanded.retain(|expanded| expanded != path);
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        self.ui.expanded.iter().any(|expanded| expanded == path)
    }

    pub fn set_expanded(&mut self, path: &Path, expanded: bool) {
        self.ui.expanded.retain(|p| p != path);
        if expanded {
            self.ui.expanded.push(path.to_path_buf());
        }
    }

    /// La machine `key` est-elle repliée dans la barre ?
    pub fn is_machine_collapsed(&self, key: &str) -> bool {
        self.ui.collapsed_machines.iter().any(|kept| kept == key)
    }

    pub fn set_machine_collapsed(&mut self, key: &str, collapsed: bool) {
        self.ui.collapsed_machines.retain(|kept| kept != key);
        if collapsed {
            self.ui.collapsed_machines.push(key.to_owned());
        }
    }
}

/// Ce que le chargement a dû faire, à signaler sous la liste des projets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateNotice {
    /// Fichier illisible, mis de côté en `state.json.bak` : la liste se reconstruit depuis tmux.
    Rebuilt,
    /// Fichier d'une version plus récente : lu, jamais réécrit.
    ReadOnly { version: u32 },
}

#[derive(Debug)]
pub struct StateStore {
    path: PathBuf,
    pub state: State,
    read_only: bool,
    pub notice: Option<StateNotice>,
}

impl StateStore {
    /// Charge `path` : absent, il donne un état vide ; illisible, il part en `.bak` ; plus récent, il est lu
    /// sans être réécrit.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let mut store = Self {
            path: path.to_path_buf(),
            state: State::default(),
            read_only: false,
            notice: None,
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(store),
            Err(error) => return Err(error),
        };
        let version = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.get("version")?.as_u64())
            .and_then(|version| u32::try_from(version).ok());
        match version {
            Some(version) if version > STATE_VERSION => {
                // Ce que cette build sait lire du format plus récent ; le reste attend la build qui l'a écrit.
                store.state = serde_json::from_str(&text).unwrap_or_default();
                store.read_only = true;
                store.notice = Some(StateNotice::ReadOnly { version });
            }
            _ => match serde_json::from_str::<State>(&text) {
                Ok(state) => store.state = state,
                Err(_) => {
                    std::fs::rename(path, path.with_extension("json.bak"))?;
                    store.notice = Some(StateNotice::Rebuilt);
                }
            },
        }
        Ok(store)
    }

    /// Écrit l'état de façon atomique : fichier temporaire, puis renommage. Rien en lecture seule.
    pub fn save(&self) -> std::io::Result<()> {
        if self.read_only {
            return Ok(());
        }
        let temporary = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&self.state).map_err(std::io::Error::other)?;
        std::fs::write(&temporary, text + "\n")?;
        std::fs::rename(&temporary, &self.path)
    }
}

/// Horodatage RFC 3339 en UTC, à la seconde (« 2026-10-14T12:00:00Z »).
pub fn rfc3339(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Jours depuis 1970 vers une date civile (algorithme de Howard Hinnant, calendrier grégorien).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::tabs::{SavedTab, TabKind};
    use std::time::Duration;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("idema-state-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rfc3339_formats_utc_dates() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_791_979_200)),
            "2026-10-14T12:00:00Z"
        );
        // 29 février d'une année bissextile.
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_709_208_000)),
            "2024-02-29T12:00:00Z"
        );
    }

    #[test]
    fn the_spec_example_round_trips() {
        let text = r#"{
  "version": 1,
  "projects": [
    { "path": "/Users/me/dev/erom-tool-idema", "added_at": "2026-10-14T12:00:00Z", "pinned_at": null }
  ],
  "ui": {
    "sidebar_visible": true,
    "expanded": ["/Users/me/dev/erom-tool-idema"],
    "selected_session": "idema-3f9a1c2e"
  }
}"#;
        let state: State = serde_json::from_str(text).unwrap();
        assert_eq!(state.ui.selected_session.as_deref(), Some("idema-3f9a1c2e"));
        let again: State = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(again, state);
    }

    #[test]
    fn pinned_projects_lead_in_pin_order_then_the_rest_in_add_order() {
        let mut state = State::default();
        for (path, added) in [
            ("/a", "2026-10-01T00:00:00Z"),
            ("/b", "2026-10-02T00:00:00Z"),
            ("/c", "2026-10-03T00:00:00Z"),
        ] {
            state.add_project(Path::new(path), added);
        }
        state.set_pinned(Path::new("/c"), Some("2026-10-10T00:00:00Z"));
        state.set_pinned(Path::new("/b"), Some("2026-10-11T00:00:00Z"));
        let order: Vec<&Path> = state
            .ordered_projects()
            .iter()
            .map(|p| p.path.as_path())
            .collect();
        assert_eq!(order, [Path::new("/c"), Path::new("/b"), Path::new("/a")]);
        state.set_pinned(Path::new("/c"), None);
        let order: Vec<&Path> = state
            .ordered_projects()
            .iter()
            .map(|p| p.path.as_path())
            .collect();
        assert_eq!(order, [Path::new("/b"), Path::new("/a"), Path::new("/c")]);
    }

    #[test]
    fn adding_twice_keeps_one_entry_and_removing_forgets_it() {
        let mut state = State::default();
        assert!(state.add_project(Path::new("/a"), "2026-10-01T00:00:00Z"));
        assert!(!state.add_project(Path::new("/a"), "2026-10-02T00:00:00Z"));
        assert!(state.is_expanded(Path::new("/a")));
        state.remove_project(Path::new("/a"));
        assert!(state.projects.is_empty() && !state.is_expanded(Path::new("/a")));
    }

    #[test]
    fn a_missing_file_gives_an_empty_state_and_save_round_trips() {
        let dir = temp_dir("missing");
        let path = dir.join("state.json");
        let mut store = StateStore::load(&path).unwrap();
        assert_eq!(store.state, State::default());
        store
            .state
            .add_project(Path::new("/p"), "2026-10-01T00:00:00Z");
        store.save().unwrap();
        assert!(!dir.join("state.json.tmp").exists());
        assert_eq!(StateStore::load(&path).unwrap().state, store.state);
    }

    #[test]
    fn an_unreadable_file_is_set_aside_and_rebuilt() {
        let dir = temp_dir("broken");
        let path = dir.join("state.json");
        std::fs::write(&path, "{ pas du json").unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, Some(StateNotice::Rebuilt));
        assert_eq!(store.state, State::default());
        assert_eq!(
            std::fs::read_to_string(dir.join("state.json.bak")).unwrap(),
            "{ pas du json"
        );
    }

    #[test]
    fn a_newer_file_is_read_but_never_written() {
        let dir = temp_dir("newer");
        let path = dir.join("state.json");
        let text = r#"{"version": 2, "projects": [{"path": "/p", "added_at": "2026-10-01T00:00:00Z", "pinned_at": null, "color": "blue"}], "ui": {"sidebar_visible": false, "expanded": [], "selected_session": null}, "extra": 1}"#;
        std::fs::write(&path, text).unwrap();
        let mut store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, Some(StateNotice::ReadOnly { version: 2 }));
        assert_eq!(store.state.projects.len(), 1);
        store
            .state
            .add_project(Path::new("/q"), "2026-10-02T00:00:00Z");
        store.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn a_jalon3_state_without_widths_loads_unchanged() {
        let dir = temp_dir("jalon3");
        let path = dir.join("state.json");
        // Le JSON exact que `tests/acceptance/launch.sh` écrit : aucune des deux clés de largeur.
        let text = r#"{"version":1,"projects":[{"path":"/p","added_at":"2026-10-15T10:00:00Z","pinned_at":"2026-10-16T08:00:00Z"},{"path":"/q","added_at":"2026-10-15T11:00:00Z","pinned_at":null}],"ui":{"sidebar_visible":true,"expanded":["/p"],"selected_session":null}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(!dir.join("state.json.bak").exists());
        assert_eq!(store.state.projects.len(), 2);
        assert_eq!(
            store
                .state
                .project(Path::new("/p"))
                .unwrap()
                .pinned_at
                .as_deref(),
            Some("2026-10-16T08:00:00Z")
        );
        assert_eq!(store.state.ui.sidebar_width, None);
        assert_eq!(store.state.ui.panel_width, None);
    }

    #[test]
    fn a_state_without_files_fields_loads_unchanged() {
        let dir = temp_dir("jalon4b");
        let path = dir.join("state.json");
        // Le `state.json` du jalon 4b : les largeurs de la barre et du volet Terminal, rien du volet Fichiers.
        let text = r#"{"version":1,"projects":[{"path":"/p","added_at":"2026-10-15T10:00:00Z","pinned_at":null}],"ui":{"sidebar_visible":true,"expanded":["/p"],"selected_session":null,"sidebar_width":300.0,"panel_width":500.0}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(!dir.join("state.json.bak").exists());
        assert_eq!(store.state.ui.panel_width, Some(500.));
        assert!(!store.state.ui.files_tree_collapsed);
    }

    #[test]
    fn a_state_without_the_tree_width_reads_as_default() {
        let dir = temp_dir("tree-width-absent");
        let path = dir.join("state.json");
        // Le `state.json` d'avant la largeur de l'arbre : le repli y est, la largeur non.
        let text = r#"{"version":1,"projects":[{"path":"/p","added_at":"2026-10-15T10:00:00Z","pinned_at":null}],"ui":{"sidebar_visible":true,"expanded":["/p"],"selected_session":null,"sidebar_width":300.0,"panel_width":500.0,"files_panel_width":420.0,"files_tree_collapsed":true}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(!dir.join("state.json.bak").exists());
        assert_eq!(store.state.ui.files_tree_width, None);
        assert!(store.state.ui.files_tree_collapsed);
    }

    #[test]
    fn a_state_without_the_changes_fields_reads_as_default() {
        let dir = temp_dir("changes-absent");
        let path = dir.join("state.json");
        // Le `state.json` d'avant le volet Changes : les largeurs de l'arbre y sont, rien de Changes.
        let text = r#"{"version":1,"projects":[{"path":"/p","added_at":"2026-10-15T10:00:00Z","pinned_at":null}],"ui":{"sidebar_visible":true,"expanded":["/p"],"selected_session":null,"sidebar_width":300.0,"panel_width":500.0,"files_panel_width":420.0,"files_tree_collapsed":true,"files_tree_width":250.0}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(!dir.join("state.json.bak").exists());
        assert_eq!(store.state.ui.changes_list_width, None);
        assert!(!store.state.ui.changes_list_hidden);
        assert_eq!(store.state.ui.files_tree_width, Some(250.));
    }

    #[test]
    fn an_old_state_reads_without_ideation_collapsed() {
        let dir = temp_dir("ideation-absent");
        let path = dir.join("state.json");
        // Le `state.json` d'avant la section Bouillon de neurones : aucun champ de repli.
        let text = r#"{"version":1,"projects":[{"path":"/p","added_at":"2026-10-15T10:00:00Z","pinned_at":null}],"ui":{"sidebar_visible":true,"expanded":["/p"],"selected_session":null,"sidebar_width":300.0}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(!dir.join("state.json.bak").exists());
        assert!(!store.state.ui.ideation_collapsed);
    }

    #[test]
    fn a_state_without_the_browser_fields_reads_as_default() {
        let dir = temp_dir("browser-absent");
        let path = dir.join("state.json");
        // Le `state.json` d'avant le navigateur : les champs de Changes y sont, rien du navigateur.
        let text = r#"{"version":1,"projects":[{"path":"/p","added_at":"2026-10-15T10:00:00Z","pinned_at":null}],"ui":{"sidebar_visible":true,"expanded":["/p"],"selected_session":null,"sidebar_width":300.0,"panel_width":500.0,"changes_panel_width":410.0,"changes_list_hidden":true}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(!dir.join("state.json.bak").exists());
        assert_eq!(store.state.ui.browser_tabs, None);
    }

    #[test]
    fn the_former_per_panel_widths_are_ignored_and_the_terminal_width_becomes_the_shared_one() {
        let dir = temp_dir("per-panel-widths");
        let path = dir.join("state.json");
        // Le `state.json` d'avant la largeur commune : une largeur par volet.
        let text = r#"{"version":1,"projects":[],"ui":{"sidebar_visible":true,"expanded":[],"selected_session":null,"panel_width":500.0,"files_panel_width":420.0,"changes_panel_width":410.0,"browser_panel_width":600.0,"scratchpad_panel_width":610.0}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(!dir.join("state.json.bak").exists());
        assert_eq!(store.state.ui.panel_width, Some(500.));
    }

    #[test]
    fn browser_tabs_round_trip() {
        let dir = temp_dir("browser-tabs");
        let path = dir.join("state.json");
        let mut store = StateStore::load(&path).unwrap();
        store.state.ui.browser_tabs = Some(SavedTabs {
            tabs: vec![
                SavedTab {
                    kind: TabKind::Start,
                    title: None,
                },
                SavedTab {
                    kind: TabKind::Web {
                        url: "http://localhost:8811/".into(),
                    },
                    title: Some("Fixture IdemA".into()),
                },
            ],
            active: 1,
        });
        store.save().unwrap();
        let again = StateStore::load(&path).unwrap();
        assert_eq!(again.state.ui.browser_tabs, store.state.ui.browser_tabs);
        assert_eq!(again.state, store.state);
    }

    #[test]
    fn unreadable_browser_tabs_do_not_break_the_rest_of_the_state() {
        let dir = temp_dir("browser-tabs-bad");
        let path = dir.join("state.json");
        let text = r#"{"version":1,"projects":[{"path":"/p","added_at":"2026-10-15T10:00:00Z","pinned_at":null}],"ui":{"sidebar_visible":true,"expanded":["/p"],"selected_session":null,"sidebar_width":300.0,"panel_width":500.0,"browser_tabs":{"tabs":[{"kind":"nope"}],"active":0}}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(!dir.join("state.json.bak").exists());
        assert_eq!(store.state.ui.browser_tabs, None);
        assert_eq!(store.state.ui.sidebar_width, Some(300.));
        assert_eq!(store.state.projects.len(), 1);
    }

    #[test]
    fn dev_ports_round_trip_and_unreadable_ones_are_dropped_alone() {
        let dir = temp_dir("dev-ports");
        let path = dir.join("state.json");
        let mut store = StateStore::load(&path).unwrap();
        store
            .state
            .ui
            .servers_dev_ports
            .insert(PathBuf::from("/p/beta"), 3000);
        store.save().unwrap();
        let again = StateStore::load(&path).unwrap();
        assert_eq!(
            again.state.ui.servers_dev_ports,
            store.state.ui.servers_dev_ports
        );

        let text = r#"{"version":1,"projects":[{"path":"/p","added_at":"2026-10-15T10:00:00Z","pinned_at":null}],"ui":{"sidebar_visible":true,"expanded":[],"selected_session":null,"panel_width":500.0,"servers_dev_ports":{"/p/ok":3000,"/p/big":70000,"/p/zero":0,"/p/text":"x"}}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(!dir.join("state.json.bak").exists());
        assert_eq!(
            store.state.ui.servers_dev_ports,
            HashMap::from([(PathBuf::from("/p/ok"), 3000)])
        );
        assert_eq!(store.state.ui.panel_width, Some(500.));

        let text = r#"{"version":1,"projects":[],"ui":{"sidebar_visible":true,"expanded":[],"selected_session":null,"servers_dev_ports":[1,2]}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(store.state.ui.servers_dev_ports.is_empty());
    }

    #[test]
    fn ui_state_without_artifacts_fields_reads_defaults() {
        let dir = temp_dir("artifacts-absent");
        let path = dir.join("state.json");
        let text = r#"{"version":1,"projects":[],"ui":{"sidebar_visible":true,"expanded":[],"selected_session":null,"panel_width":500.0}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(!dir.join("state.json.bak").exists());
        assert_eq!(store.state.ui.artifacts_layout, ArtifactsLayout::Grid);
        assert_eq!(store.state.ui.artifacts_filter, KindFilter::default());
        assert_eq!(store.state.ui.panel_width, Some(500.));
    }

    #[test]
    fn unreadable_artifacts_fields_do_not_break_the_rest_of_the_state() {
        let dir = temp_dir("artifacts-bad");
        let path = dir.join("state.json");
        let text = r#"{"version":1,"projects":[],"ui":{"sidebar_visible":true,"expanded":[],"selected_session":null,"panel_width":500.0,"artifacts_layout":"mosaic","artifacts_filter":{"docs":"oui"}}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(!dir.join("state.json.bak").exists());
        assert_eq!(store.state.ui.artifacts_layout, ArtifactsLayout::Grid);
        assert_eq!(store.state.ui.artifacts_filter, KindFilter::default());
        assert_eq!(store.state.ui.panel_width, Some(500.));
    }

    #[test]
    fn a_v1_filter_reads_with_new_kinds_shown() {
        let dir = temp_dir("artifacts-filter-v1");
        let path = dir.join("state.json");
        let text = r#"{"version":1,"projects":[],"ui":{"sidebar_visible":true,"expanded":[],"selected_session":null,"artifacts_filter":{"docs":true,"slides":false,"pages":false}}}"#;
        std::fs::write(&path, text).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(!dir.join("state.json.bak").exists());
        assert_eq!(
            store.state.ui.artifacts_filter,
            KindFilter {
                docs: true,
                slides: false,
                pages: false,
                sheets: true,
                design: true,
                data: true,
                whiteboard: true,
            }
        );
    }

    #[test]
    fn artifacts_ui_round_trip() {
        let dir = temp_dir("artifacts-round-trip");
        let path = dir.join("state.json");
        let mut store = StateStore::load(&path).unwrap();
        store.state.ui.artifacts_layout = ArtifactsLayout::List;
        store.state.ui.artifacts_filter = KindFilter {
            pages: false,
            ..KindFilter::default()
        };
        store.save().unwrap();
        let again = StateStore::load(&path).unwrap();
        assert_eq!(again.state.ui.artifacts_layout, ArtifactsLayout::List);
        assert!(!again.state.ui.artifacts_filter.pages);
        assert!(again.state.ui.artifacts_filter.docs);
        assert_eq!(again.state, store.state);
    }

    #[test]
    fn widths_round_trip() {
        let dir = temp_dir("widths");
        let path = dir.join("state.json");
        let mut store = StateStore::load(&path).unwrap();
        store.state.ui.sidebar_width = Some(301.5);
        store.state.ui.panel_width = Some(512.);
        store.state.ui.files_tree_width = Some(262.5);
        store.save().unwrap();
        let again = StateStore::load(&path).unwrap();
        assert_eq!(again.state.ui.sidebar_width, Some(301.5));
        assert_eq!(again.state.ui.panel_width, Some(512.));
        assert_eq!(again.state.ui.files_tree_width, Some(262.5));
        assert_eq!(again.state, store.state);
    }

    #[test]
    fn collapsed_machines_round_trip() {
        let dir = temp_dir("collapsed-machines");
        let path = dir.join("state.json");
        let mut store = StateStore::load(&path).unwrap();
        assert!(store.state.ui.collapsed_machines.is_empty());
        store.state.ui.collapsed_machines = vec!["local".into(), "dell-lan".into()];
        store.save().unwrap();
        let again = StateStore::load(&path).unwrap();
        assert_eq!(again.state.ui.collapsed_machines, ["local", "dell-lan"]);
        assert_eq!(again.state, store.state);
        // Un `state.json` d'avant les machines se lit sans le champ, sans partir en `.bak`.
        let old = r#"{"version":1,"projects":[],"ui":{"sidebar_visible":true,"expanded":[],"selected_session":null}}"#;
        std::fs::write(&path, old).unwrap();
        let store = StateStore::load(&path).unwrap();
        assert_eq!(store.notice, None);
        assert!(store.state.ui.collapsed_machines.is_empty());
        assert!(!dir.join("state.json.bak").exists());
    }

    #[test]
    fn a_machine_collapses_and_expands_by_key() {
        let mut state = State::default();
        assert!(!state.is_machine_collapsed("dell-lan"));
        state.set_machine_collapsed("dell-lan", true);
        state.set_machine_collapsed("dell-lan", true);
        assert!(state.is_machine_collapsed("dell-lan"));
        assert_eq!(state.ui.collapsed_machines, ["dell-lan"]);
        state.set_machine_collapsed("dell-lan", false);
        assert!(!state.is_machine_collapsed("dell-lan"));
    }
}
