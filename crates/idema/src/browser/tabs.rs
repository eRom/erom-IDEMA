//! Les onglets du navigateur et leur sauvegarde dans `state.json`. Un modèle pur : la vue web de chaque onglet
//! vit ailleurs et n'est reliée que par l'identifiant.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::address::same_address;

/// Ce qu'un onglet montre.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabKind {
    Start,
    Web {
        url: String,
    },
    /// Le journal d'un serveur (spec P4 §4.5). Il désigne le serveur par son projet, son nom et son port : au
    /// relancement, l'onglet se rattache à la ligne qui porte ce nom, ou dit « Serveur introuvable ».
    Log {
        project: PathBuf,
        name: String,
        #[serde(default)]
        port: Option<u16>,
    },
}

/// Identifiant d'onglet : croissant depuis 1, jamais réutilisé pendant la session.
pub type TabId = u64;

#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub id: TabId,
    pub kind: TabKind,
    pub title: Option<String>,
}

/// D'où vient l'ouverture d'une adresse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenFrom {
    /// Un lien (⌘-clic) : un onglet de plus, juste après l'actif.
    Link,
    /// Une ligne de la page d'accueil, un favori, la barre d'adresse : l'onglet actif.
    Here,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opened {
    pub id: TabId,
    /// Un onglet a été créé.
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedTab {
    pub kind: TabKind,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedTabs {
    pub tabs: Vec<SavedTab>,
    pub active: usize,
}

#[derive(Debug, Clone)]
pub struct BrowserTabs {
    tabs: Vec<Tab>,
    /// Position de l'onglet actif dans `tabs` ; toujours valide, `tabs` n'est jamais vide.
    active: usize,
    next_id: TabId,
}

impl Default for BrowserTabs {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserTabs {
    /// Un seul onglet, l'Accueil, actif.
    pub fn new() -> Self {
        let mut tabs = Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
        };
        let id = tabs.allocate();
        tabs.tabs.push(Tab {
            id,
            kind: TabKind::Start,
            title: None,
        });
        tabs
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// Reste-t-il un seul onglet ? Le fermer ferme aussi le volet (`BrowserPanel::close_tab`).
    pub fn is_single(&self) -> bool {
        self.tabs.len() == 1
    }

    pub fn active(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn get(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    /// Un onglet Accueil juste après l'actif, qui devient actif.
    pub fn open_start(&mut self) -> TabId {
        self.insert_after_active(TabKind::Start)
    }

    /// Ouvre le journal d'un serveur : l'onglet qui le montre déjà devient actif, sinon un onglet naît juste après
    /// l'actif (la page d'accueil reste, on y revient).
    pub fn open_log(&mut self, project: PathBuf, name: String, port: Option<u16>) -> Opened {
        let wanted = TabKind::Log {
            project,
            name,
            port,
        };
        if let Some(index) = self.tabs.iter().position(|tab| tab.kind == wanted) {
            self.active = index;
            return Opened {
                id: self.tabs[index].id,
                created: false,
            };
        }
        Opened {
            id: self.insert_after_active(wanted),
            created: true,
        }
    }

    /// Ouvre une adresse. Un onglet qui la montre déjà (au fragment près) passe avant toute autre règle ;
    /// sinon `Here` remplace l'Accueil actif ou navigue l'onglet actif, et `Link` crée un onglet après l'actif.
    pub fn open_url(&mut self, url: &str, from: OpenFrom) -> Opened {
        let existing = self.tabs.iter().position(
            |tab| matches!(&tab.kind, TabKind::Web { url: shown } if same_address(shown, url)),
        );
        if let Some(index) = existing {
            self.active = index;
            return Opened {
                id: self.tabs[index].id,
                created: false,
            };
        }
        match from {
            // Un journal ne devient pas une page : « Ouvrir » depuis lui, ou une adresse tapée dans sa rangée, ouvre un
            // onglet de plus (le journal reste lisible).
            OpenFrom::Here if !matches!(self.tabs[self.active].kind, TabKind::Log { .. }) => {
                let tab = &mut self.tabs[self.active];
                tab.kind = TabKind::Web {
                    url: url.to_string(),
                };
                tab.title = None;
                Opened {
                    id: tab.id,
                    created: false,
                }
            }
            OpenFrom::Here | OpenFrom::Link => {
                let id = self.insert_after_active(TabKind::Web {
                    url: url.to_string(),
                });
                Opened { id, created: true }
            }
        }
    }

    /// La vue de l'onglet a navigué. Un événement d'un onglet fermé est ignoré. Un simple changement de
    /// fragment garde le titre : la page est la même.
    pub fn navigated(&mut self, id: TabId, url: &str) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        let same_page =
            matches!(&tab.kind, TabKind::Web { url: shown } if same_address(shown, url));
        tab.kind = TabKind::Web {
            url: url.to_string(),
        };
        if !same_page {
            tab.title = None;
        }
    }

    pub fn set_title(&mut self, id: TabId, title: Option<String>) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
            tab.title = title;
        }
    }

    /// Rend l'onglet actif. `false` si l'onglet n'existe pas (ou plus).
    pub fn activate(&mut self, id: TabId) -> bool {
        match self.tabs.iter().position(|tab| tab.id == id) {
            Some(index) => {
                self.active = index;
                true
            }
            None => false,
        }
    }

    /// Ferme un onglet et rend celui qui devient actif : son voisin de droite, à défaut celui de gauche.
    /// Fermer le dernier laisse un Accueil neuf (autre identifiant) : c'est ce que `state.json` garde et que le volet
    /// montre à sa réouverture, le volet lui-même se ferme (`BrowserPanel::close_tab`). `None` si l'onglet n'existe pas.
    pub fn close(&mut self, id: TabId) -> Option<TabId> {
        let index = self.tabs.iter().position(|tab| tab.id == id)?;
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            let fresh = self.allocate();
            self.tabs.push(Tab {
                id: fresh,
                kind: TabKind::Start,
                title: None,
            });
            self.active = 0;
        } else if index < self.active {
            self.active -= 1;
        } else if index == self.active {
            self.active = index.min(self.tabs.len() - 1);
        }
        Some(self.tabs[self.active].id)
    }

    pub fn next(&mut self) {
        self.active = (self.active + 1) % self.tabs.len();
    }

    pub fn previous(&mut self) {
        self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
    }

    pub fn saved(&self) -> SavedTabs {
        SavedTabs {
            tabs: self
                .tabs
                .iter()
                .map(|tab| SavedTab {
                    kind: tab.kind.clone(),
                    title: tab.title.clone(),
                })
                .collect(),
            active: self.active,
        }
    }

    /// Rouvre les onglets sauvegardés. Rien, ou une liste vide : un seul Accueil. Un actif hors bornes : le premier.
    pub fn from_saved(saved: Option<SavedTabs>) -> Self {
        let Some(saved) = saved.filter(|saved| !saved.tabs.is_empty()) else {
            return Self::new();
        };
        let active = if saved.active < saved.tabs.len() {
            saved.active
        } else {
            0
        };
        let tabs = saved
            .tabs
            .into_iter()
            .enumerate()
            .map(|(index, tab)| Tab {
                id: index as TabId + 1,
                kind: tab.kind,
                title: tab.title,
            })
            .collect::<Vec<_>>();
        let next_id = tabs.len() as TabId + 1;
        Self {
            tabs,
            active,
            next_id,
        }
    }

    fn allocate(&mut self) -> TabId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn insert_after_active(&mut self, kind: TabKind) -> TabId {
        let id = self.allocate();
        let at = self.active + 1;
        self.tabs.insert(
            at,
            Tab {
                id,
                kind,
                title: None,
            },
        );
        self.active = at;
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_switches_to_the_tab_that_shows_it_or_opens_one_after_the_active() {
        let mut tabs = BrowserTabs::new();
        let a = tabs.open_url("http://localhost:8811/", OpenFrom::Link);
        assert!(a.created);
        assert_eq!(tabs.tabs().len(), 2);
        let again = tabs.open_url("http://localhost:8811/#x", OpenFrom::Link);
        assert_eq!((again.id, again.created), (a.id, false));
        assert_eq!(tabs.active().id, a.id);
    }

    #[test]
    fn from_the_start_page_an_address_replaces_it() {
        let mut tabs = BrowserTabs::new();
        let start = tabs.active().id;
        let opened = tabs.open_url("https://vercel.com/", OpenFrom::Here);
        assert_eq!((opened.id, opened.created), (start, false));
        assert_eq!(tabs.tabs().len(), 1);
        assert_eq!(
            tabs.active().kind,
            TabKind::Web {
                url: "https://vercel.com/".into()
            }
        );
    }

    #[test]
    fn closing_the_last_tab_leaves_a_start_page() {
        let mut tabs = BrowserTabs::new();
        let only = tabs.active().id;
        let next = tabs.close(only).unwrap();
        assert_ne!(next, only);
        assert_eq!(tabs.tabs().len(), 1);
        assert_eq!(tabs.active().kind, TabKind::Start);
    }

    #[test]
    fn only_the_last_tab_is_single_and_closing_it_saves_one_fresh_start_page() {
        let mut tabs = BrowserTabs::new();
        assert!(tabs.is_single());
        let web = tabs.open_url("https://example.com/", OpenFrom::Link).id;
        assert!(!tabs.is_single());
        // Fermer l'un des deux : il reste seul.
        let first = tabs.tabs()[0].id;
        tabs.close(first);
        assert!(tabs.is_single());
        // Fermer le dernier (une page web) : ce que `state.json` garde est un seul Accueil, sans fantôme.
        tabs.close(web);
        let saved = tabs.saved();
        assert_eq!(saved.tabs.len(), 1);
        assert_eq!(saved.tabs[0].kind, TabKind::Start);
        assert_eq!(saved.active, 0);
        assert_eq!(
            BrowserTabs::from_saved(Some(saved)).active().kind,
            TabKind::Start
        );
    }

    #[test]
    fn closing_the_active_tab_activates_its_right_neighbour_then_its_left() {
        let mut tabs = BrowserTabs::new();
        let a = tabs.active().id;
        let b = tabs.open_start();
        let c = tabs.open_start();
        tabs.activate(b);
        assert_eq!(tabs.close(b), Some(c));
        assert_eq!(tabs.close(c), Some(a));
    }

    #[test]
    fn closing_a_tab_to_the_left_keeps_the_active_tab() {
        let mut tabs = BrowserTabs::new();
        let a = tabs.active().id;
        let b = tabs.open_start();
        let c = tabs.open_start();
        assert_eq!(tabs.active().id, c);
        // `a` est à gauche de l'actif : `c` reste actif, même si sa position change.
        assert_eq!(tabs.close(a), Some(c));
        assert_eq!(tabs.active().id, c);
        assert_eq!(
            tabs.tabs().iter().map(|tab| tab.id).collect::<Vec<_>>(),
            [b, c]
        );
        // Fermer un onglet à droite de l'actif ne le change pas non plus.
        tabs.activate(b);
        assert_eq!(tabs.close(c), Some(b));
        assert_eq!(tabs.active().id, b);
    }

    #[test]
    fn next_and_previous_wrap_around() {
        let mut tabs = BrowserTabs::new();
        let a = tabs.active().id;
        let b = tabs.open_start();
        tabs.next();
        assert_eq!(tabs.active().id, a);
        tabs.previous();
        assert_eq!(tabs.active().id, b);
    }

    #[test]
    fn events_of_a_closed_tab_are_dropped() {
        let mut tabs = BrowserTabs::new();
        let b = tabs.open_url("http://localhost:8811/", OpenFrom::Link).id;
        tabs.close(b);
        tabs.navigated(b, "http://localhost:8811/page2");
        tabs.set_title(b, Some("x".into()));
        assert!(tabs.get(b).is_none());
        assert!(!tabs.activate(b));
    }

    #[test]
    fn tabs_round_trip_through_saved_tabs() {
        let mut tabs = BrowserTabs::new();
        let b = tabs.open_url("http://localhost:8811/", OpenFrom::Link).id;
        tabs.set_title(b, Some("Fixture IdemA".into()));
        let restored = BrowserTabs::from_saved(Some(tabs.saved()));
        assert_eq!(restored.tabs().len(), 2);
        assert_eq!(
            restored.active().kind,
            TabKind::Web {
                url: "http://localhost:8811/".into()
            }
        );
        assert_eq!(restored.active().title.as_deref(), Some("Fixture IdemA"));
    }

    fn log(name: &str, port: Option<u16>) -> (PathBuf, String, Option<u16>) {
        (PathBuf::from("/work/alpha"), name.to_owned(), port)
    }

    #[test]
    fn a_journal_opens_after_the_active_tab_and_an_open_journal_is_reactivated() {
        let mut tabs = BrowserTabs::new();
        let start = tabs.active().id;
        let (project, name, port) = log("web", Some(8811));
        let opened = tabs.open_log(project.clone(), name.clone(), port);
        assert!(opened.created);
        assert_eq!(tabs.tabs().len(), 2, "the start page stays");
        assert_eq!(tabs.active().id, opened.id);
        tabs.activate(start);
        let again = tabs.open_log(project, name, port);
        assert_eq!((again.id, again.created), (opened.id, false));
        assert_eq!(tabs.active().id, opened.id);
        assert_eq!(tabs.tabs().len(), 2);
        // Un autre serveur, ou un autre port, est un autre journal.
        let (project, name, port) = log("web", Some(8812));
        assert!(tabs.open_log(project, name, port).created);
        assert_eq!(tabs.tabs().len(), 3);
    }

    #[test]
    fn an_address_opened_from_a_journal_gets_a_new_tab_and_the_journal_stays() {
        let mut tabs = BrowserTabs::new();
        let (project, name, port) = log("web", Some(8811));
        let journal = tabs.open_log(project.clone(), name.clone(), port).id;
        let opened = tabs.open_url("http://localhost:8811/", OpenFrom::Here);
        assert!(opened.created);
        assert_ne!(opened.id, journal);
        assert_eq!(
            tabs.get(journal).map(|tab| tab.kind.clone()),
            Some(TabKind::Log {
                project,
                name,
                port
            })
        );
    }

    #[test]
    fn a_journal_tab_is_saved_and_restored_with_the_server_it_names() {
        let mut tabs = BrowserTabs::new();
        let (project, name, port) = log("python3 -m http.server", Some(8814));
        tabs.open_log(project.clone(), name.clone(), port);
        let saved = tabs.saved();
        let json = serde_json::to_string(&saved).unwrap();
        assert!(json.contains(r#""kind":"log""#));
        let restored =
            BrowserTabs::from_saved(Some(serde_json::from_str::<SavedTabs>(&json).unwrap()));
        assert_eq!(restored.tabs().len(), 2);
        assert_eq!(
            restored.active().kind,
            TabKind::Log {
                project,
                name,
                port
            }
        );
    }

    #[test]
    fn a_saved_journal_without_a_port_is_still_read() {
        let json =
            r#"{"tabs":[{"kind":{"kind":"log","project":"/work/alpha","name":"dev"}}],"active":0}"#;
        let saved: SavedTabs = serde_json::from_str(json).unwrap();
        assert_eq!(
            BrowserTabs::from_saved(Some(saved)).active().kind,
            TabKind::Log {
                project: PathBuf::from("/work/alpha"),
                name: "dev".to_owned(),
                port: None
            }
        );
    }

    #[test]
    fn unreadable_saved_tabs_fall_back_to_one_start_tab() {
        let empty = BrowserTabs::from_saved(Some(SavedTabs {
            tabs: vec![],
            active: 3,
        }));
        assert_eq!(empty.tabs().len(), 1);
        assert_eq!(empty.active().kind, TabKind::Start);
        let out_of_range = BrowserTabs::from_saved(Some(SavedTabs {
            tabs: vec![SavedTab {
                kind: TabKind::Start,
                title: None,
            }],
            active: 9,
        }));
        assert_eq!(out_of_range.active().kind, TabKind::Start);
    }
}
