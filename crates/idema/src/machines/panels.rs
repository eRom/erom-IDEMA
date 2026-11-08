//! Les volets droits sur une session distante (spec ssh-tmux §3.7) : ce qui reste disponible, ce qui est grisé, quel shell
//! le volet Terminal montre et où il s'ouvre. Des fonctions pures : l'espace de travail ne fait que les appliquer.

use std::path::PathBuf;

use super::key::{LOCAL_MACHINE, machine_of};
use crate::terminal_panel::TerminalKey;
use crate::workspace::RightPanel;

/// Le texte de l'infobulle des boutons grisés et du volet resté ouvert, tel que la spec le donne.
pub const UNAVAILABLE_ON_REMOTE: &str = "Pas encore disponible sur une machine distante";

/// Le texte du volet Terminal pendant que le shell de la machine s'ouvre.
pub const OPENING_TERMINAL: &str = "Ouverture du terminal…";

/// Le volet a-t-il besoin d'un dossier de ce Mac ? Changes, Navigateur, Files et Projet : oui, donc grisés sur une
/// machine distante. Le Terminal ouvre un shell ssh et le Scratchpad est un fichier global.
pub fn unavailable_on_remote(panel: RightPanel) -> bool {
    matches!(
        panel,
        RightPanel::Changes | RightPanel::Browser | RightPanel::Files | RightPanel::Project
    )
}

/// Le volet ouvert que l'écran remplace par le texte « Pas encore disponible » : celui qui est ouvert, sur une session
/// distante, quand il est de ceux qui ne s'y ouvrent pas. Il reste ouvert et retrouve son contenu en local.
pub fn unavailable_open_panel(
    selected_is_remote: bool,
    open: Option<RightPanel>,
) -> Option<RightPanel> {
    open.filter(|panel| selected_is_remote && unavailable_on_remote(*panel))
}

/// Le shell du volet Terminal : celui de la machine de la session distante sélectionnée, sinon celui qu'« Ouvrir un
/// terminal » a demandé (`pinned`), sinon celui du projet de contexte.
pub fn terminal_target(
    remote_machine: Option<&str>,
    pinned_machine: Option<&str>,
    project: Option<PathBuf>,
) -> Option<TerminalKey> {
    remote_machine
        .or(pinned_machine)
        .map(|connection| TerminalKey::Machine(connection.to_owned()))
        .or_else(|| project.map(TerminalKey::Project))
}

/// « Ouvrir un terminal » sur l'en-tête d'une machine : ce qu'il a demandé ne vaut que pour le contexte où il a été
/// demandé (même session sélectionnée, même projet de contexte) ; le contexte change, le volet suit de nouveau le projet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineTerminalPin {
    pub connection: String,
    pub selected: Option<String>,
    pub project: Option<PathBuf>,
}

impl MachineTerminalPin {
    /// La machine demandée, si le contexte est encore celui de la demande et la machine encore déclarée.
    pub fn machine(
        &self,
        selected: Option<&str>,
        project: Option<&PathBuf>,
        declared: bool,
    ) -> Option<&str> {
        (declared && self.selected.as_deref() == selected && self.project.as_ref() == project)
            .then_some(self.connection.as_str())
    }
}

/// Le dossier où s'ouvre le shell distant : le `#{pane_current_path}` du pane actif quand tmux en a donné un chemin
/// absolu sans caractère de contrôle, sinon le `$HOME` de la machine.
pub fn remote_start_folder(pane_path: Option<&str>, home: &str) -> String {
    pane_path
        .map(str::trim)
        .filter(|path| path.starts_with('/') && !path.chars().any(char::is_control))
        .unwrap_or(home)
        .to_owned()
}

/// Où part un lien ouvert depuis un pane, la galerie Artifacts ou une page de connexion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRoute {
    /// Le volet Navigateur d'IdemA (session locale).
    Browser,
    /// Le navigateur par défaut du Mac : le Navigateur d'IdemA n'existe pas sur une session distante (spec §3.5).
    SystemBrowser,
    /// Ni l'un ni l'autre : un schéma que seul un navigateur du Mac sait ouvrir sans risque n'est pas ouvert.
    Ignored,
}

/// Où va `url` : sur une session distante, le navigateur du système pour `http` et `https` seulement, le reste est ignoré.
pub fn link_route(selected_is_remote: bool, url: &str) -> LinkRoute {
    if !selected_is_remote {
        return LinkRoute::Browser;
    }
    match url::Url::parse(url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => LinkRoute::SystemBrowser,
        _ => LinkRoute::Ignored,
    }
}

/// Les outils qui lisent ce Mac (« Ramener », la galerie Artifacts) : une session de ce Mac seulement.
pub fn offers_local_tools(session_key: &str) -> bool {
    machine_of(session_key) == LOCAL_MACHINE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machines::key::remote_session_key;

    #[test]
    fn only_the_four_folder_panels_are_unavailable_on_a_remote_machine() {
        use RightPanel::*;
        for panel in [Changes, Browser, Files, Project] {
            assert!(unavailable_on_remote(panel), "{panel:?}");
        }
        for panel in [Terminal, Scratchpad] {
            assert!(!unavailable_on_remote(panel), "{panel:?}");
        }
    }

    #[test]
    fn an_open_unavailable_panel_is_replaced_on_a_remote_session_and_comes_back_on_a_local_one() {
        use RightPanel::*;
        for panel in [Changes, Browser, Files, Project] {
            assert_eq!(unavailable_open_panel(true, Some(panel)), Some(panel));
            assert_eq!(unavailable_open_panel(false, Some(panel)), None);
        }
        for panel in [Terminal, Scratchpad] {
            assert_eq!(unavailable_open_panel(true, Some(panel)), None);
        }
        assert_eq!(unavailable_open_panel(true, None), None);
    }

    #[test]
    fn a_link_on_a_remote_session_goes_to_the_system_browser_when_it_is_http() {
        assert_eq!(link_route(false, "https://claude.ai/x"), LinkRoute::Browser);
        assert_eq!(link_route(false, "file:///etc/hosts"), LinkRoute::Browser);
        for url in [
            "https://claude.ai/artifacts?tab=mine",
            "http://localhost:3000/a",
        ] {
            assert_eq!(link_route(true, url), LinkRoute::SystemBrowser, "{url}");
        }
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "mailto:a@b.c",
            "ssh://h",
            "pas une url",
            "",
        ] {
            assert_eq!(link_route(true, url), LinkRoute::Ignored, "{url}");
        }
    }

    #[test]
    fn the_terminal_follows_the_machine_of_a_remote_session_else_the_request_else_the_project() {
        let project = Some(PathBuf::from("/p"));
        assert_eq!(
            terminal_target(Some("dell"), Some("autre"), project.clone()),
            Some(TerminalKey::Machine("dell".into()))
        );
        assert_eq!(
            terminal_target(None, Some("autre"), project.clone()),
            Some(TerminalKey::Machine("autre".into()))
        );
        assert_eq!(
            terminal_target(None, None, project),
            Some(TerminalKey::Project(PathBuf::from("/p")))
        );
        assert_eq!(terminal_target(None, None, None), None);
    }

    #[test]
    fn an_open_terminal_request_lasts_only_while_its_context_does() {
        let project = PathBuf::from("/p");
        let pin = MachineTerminalPin {
            connection: "dell".into(),
            selected: Some("idema-1".into()),
            project: Some(project.clone()),
        };
        assert_eq!(
            pin.machine(Some("idema-1"), Some(&project), true),
            Some("dell")
        );
        // Une autre session, un autre projet ou une machine retirée de la config : la demande tombe.
        assert_eq!(pin.machine(Some("idema-2"), Some(&project), true), None);
        assert_eq!(pin.machine(Some("idema-1"), None, true), None);
        assert_eq!(pin.machine(Some("idema-1"), Some(&project), false), None);
    }

    #[test]
    fn the_shell_opens_in_the_pane_folder_else_in_the_home_of_the_machine() {
        assert_eq!(
            remote_start_folder(Some("/srv/mon projet\n"), "/home/llm"),
            "/srv/mon projet"
        );
        for unusable in [None, Some(""), Some("relatif"), Some("/a\u{1b}[31m")] {
            assert_eq!(remote_start_folder(unusable, "/home/llm"), "/home/llm");
        }
    }

    #[test]
    fn the_local_tools_are_offered_on_a_local_session_only() {
        assert!(offers_local_tools("idema-0a1b2c3d"));
        assert!(!offers_local_tools(&remote_session_key(
            "dell-lan",
            "idema-0a1b2c3d"
        )));
    }
}
