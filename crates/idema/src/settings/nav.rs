//! Les règles pures de la barre latérale de « Personnaliser » : sections, groupes, libellés cherchables et filtre.

use std::path::{Path, PathBuf};

/// Une section de la fenêtre, dans l'ordre de la barre latérale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    General,
    Agents,
    CommitMessage,
    Browser,
    McpServers,
    Plugins,
    Marketplaces,
    Skills,
}

/// Le groupe de la barre latérale : sa légende est « IdemA », « Claude Code » ou « Système ». Les deux premiers
/// portent des sections ; « Système » ne porte que des actions (`SystemItem`), qui n'affichent aucune page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavGroup {
    IdemA,
    ClaudeCode,
    System,
}

impl NavGroup {
    /// Les groupes dans l'ordre d'affichage.
    pub const ALL: [NavGroup; 3] = [NavGroup::IdemA, NavGroup::ClaudeCode, NavGroup::System];

    /// La légende du groupe.
    pub fn label(self) -> &'static str {
        match self {
            Self::IdemA => "IdemA",
            Self::ClaudeCode => "Claude Code",
            Self::System => "Système",
        }
    }
}

/// Une ligne du groupe « Système » : un fichier de la machine que la ligne ouvre dans le volet Fichiers de la
/// fenêtre principale, en onglet permanent. Une action, pas une section : aucune page ne s'affiche.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemItem {
    MacEnvironment,
    IdnmaNetwork,
}

/// L'infobulle d'une ligne « Système » éteinte : sans projet dans la fenêtre principale, il n'y a pas de volet Fichiers
/// où ouvrir le fichier.
pub const SELECT_PROJECT_HELP: &str = "Sélectionne un projet";

impl SystemItem {
    /// Les lignes dans l'ordre d'affichage.
    pub const ALL: [SystemItem; 2] = [SystemItem::MacEnvironment, SystemItem::IdnmaNetwork];

    pub fn label(self) -> &'static str {
        match self {
            Self::MacEnvironment => "Environnement Mac",
            Self::IdnmaNetwork => "Réseau idnma",
        }
    }

    /// Le fichier que la ligne ouvre, sous le dossier personnel `home`.
    pub fn path(self, home: &Path) -> PathBuf {
        match self {
            Self::MacEnvironment => home.join(".zshenv"),
            // Les agents qu'`idnma run` sait lancer.
            Self::IdnmaNetwork => home.join(".idnma.yml"),
        }
    }
}

impl Section {
    /// Toutes les sections, dans l'ordre de la barre latérale.
    pub const ALL: [Section; 8] = [
        Section::General,
        Section::Agents,
        Section::CommitMessage,
        Section::Browser,
        Section::McpServers,
        Section::Plugins,
        Section::Marketplaces,
        Section::Skills,
    ];

    /// Le libellé de la ligne de barre latérale et le titre de l'en-tête.
    pub fn label(self) -> &'static str {
        match self {
            Self::General => "Général",
            Self::Agents => "Agents",
            Self::CommitMessage => "Message de commit",
            Self::Browser => "Navigateur",
            Self::McpServers => "Serveurs MCP",
            Self::Plugins => "Plugins",
            Self::Marketplaces => "Marketplaces",
            Self::Skills => "Skills",
        }
    }

    pub fn group(self) -> NavGroup {
        match self {
            Self::General | Self::Agents | Self::CommitMessage | Self::Browser => NavGroup::IdemA,
            Self::McpServers | Self::Plugins | Self::Marketplaces | Self::Skills => {
                NavGroup::ClaudeCode
            }
        }
    }

    /// Le sous-titre de l'en-tête : la source de la section.
    pub fn source(self) -> &'static str {
        match self {
            Self::General | Self::Agents | Self::CommitMessage | Self::Browser => "config.toml",
            Self::McpServers => "~/.claude.json · portée utilisateur",
            Self::Plugins | Self::Marketplaces => "~/.claude/settings.json · portée utilisateur",
            Self::Skills => "~/.claude/skills · skillOverrides de ~/.claude/settings.json",
        }
    }
}

/// Les libellés des contrôles des sections IdemA : la recherche les trouve, les sections les affichent. Une seule source,
/// pour que l'un ne dérive pas de l'autre.
pub mod label {
    pub const UI_FONT_SIZE: &str = "Taille de l'interface";
    pub const TERMINAL_FONT_FAMILY: &str = "Police du terminal";
    pub const TERMINAL_FONT_SIZE: &str = "Taille du terminal";
    pub const TERMINAL_LINE_HEIGHT: &str = "Interligne du terminal";
    pub const SOUND_ON_WAIT: &str = "Son quand un agent attend";
    pub const TMUX_PATH: &str = "Chemin de tmux";
    pub const AGENTS: &str = "Agents";
    pub const HARNESS: &str = "Harness";
    pub const DEFAULT_AGENT: &str = "Par défaut";
    pub const ADD_AGENT: &str = "Ajouter un agent";
    pub const COMMIT_ENABLED: &str = "Activer";
    pub const API_KEY: &str = "Clé d'API";
    pub const MODEL: &str = "Modèle";
    pub const INCLUDE_PROJECT_RULES: &str = "Inclure les règles du projet";
    pub const INSTRUCTIONS: &str = "Instructions";
    pub const ENDPOINT: &str = "Point d'accès";
    pub const FAVORITES: &str = "Favoris";
    pub const ADD_FAVORITE: &str = "Ajouter un favori";
}

/// Les libellés des contrôles d'une section, ceux que la recherche trouve. Les sections de Claude Code n'ont que leur titre.
pub fn setting_labels(section: Section) -> &'static [&'static str] {
    match section {
        Section::General => &[
            label::UI_FONT_SIZE,
            label::TERMINAL_FONT_FAMILY,
            label::TERMINAL_FONT_SIZE,
            label::TERMINAL_LINE_HEIGHT,
            label::SOUND_ON_WAIT,
            label::TMUX_PATH,
        ],
        Section::Agents => &[
            label::AGENTS,
            label::HARNESS,
            label::DEFAULT_AGENT,
            label::ADD_AGENT,
        ],
        Section::CommitMessage => &[
            label::COMMIT_ENABLED,
            label::API_KEY,
            label::MODEL,
            label::INCLUDE_PROJECT_RULES,
            label::INSTRUCTIONS,
            label::ENDPOINT,
        ],
        Section::Browser => &[label::FAVORITES, label::ADD_FAVORITE],
        Section::McpServers => &["Serveurs MCP"],
        Section::Plugins => &["Plugins"],
        Section::Marketplaces => &["Marketplaces"],
        Section::Skills => &["Skills"],
    }
}

/// Minuscules et sans accents français : la comparaison de la recherche ne voit ni casse ni diacritiques.
pub(crate) fn fold(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'â' | 'ä' => 'a',
            'ç' => 'c',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' => 'i',
            'ô' | 'ö' => 'o',
            'ù' | 'û' | 'ü' => 'u',
            'ÿ' => 'y',
            other => other,
        })
        .collect()
}

/// Les sections qui restent pour `query` : le libellé de la section ou l'un de ses réglages contient la requête. Une
/// requête vide garde tout, dans l'ordre de l'enum.
pub fn filter_nav(query: &str) -> Vec<Section> {
    let query = fold(query.trim());
    Section::ALL
        .into_iter()
        .filter(|section| {
            query.is_empty()
                || fold(section.label()).contains(&query)
                || setting_labels(*section)
                    .iter()
                    .any(|label| fold(label).contains(&query))
        })
        .collect()
}

/// Les lignes « Système » qui restent pour `query` : leur libellé contient la requête (sans casse ni accents). Une
/// requête vide garde tout, dans l'ordre.
pub fn filter_system(query: &str) -> Vec<SystemItem> {
    let query = fold(query.trim());
    SystemItem::ALL
        .into_iter()
        .filter(|item| query.is_empty() || fold(item.label()).contains(&query))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_lists_every_section_in_order() {
        assert_eq!(
            filter_nav(""),
            vec![
                Section::General,
                Section::Agents,
                Section::CommitMessage,
                Section::Browser,
                Section::McpServers,
                Section::Plugins,
                Section::Marketplaces,
                Section::Skills,
            ]
        );
    }

    #[test]
    fn query_matches_labels_without_case_or_accents() {
        assert_eq!(filter_nav("general"), vec![Section::General]);
        assert_eq!(filter_nav("MCP"), vec![Section::McpServers]);
        assert_eq!(filter_nav("Général"), vec![Section::General]);
    }

    #[test]
    fn query_matches_a_setting_label() {
        assert_eq!(filter_nav("interligne"), vec![Section::General]);
    }

    #[test]
    fn the_system_group_comes_last_and_holds_no_section() {
        assert_eq!(
            NavGroup::ALL.map(NavGroup::label),
            ["IdemA", "Claude Code", "Système"]
        );
        assert!(
            Section::ALL
                .into_iter()
                .all(|section| section.group() != NavGroup::System)
        );
    }

    #[test]
    fn the_system_rows_are_listed_in_order_with_their_files() {
        let home = Path::new("/Users/me");
        assert_eq!(
            SystemItem::ALL.map(SystemItem::label),
            ["Environnement Mac", "Réseau idnma"]
        );
        assert_eq!(
            SystemItem::MacEnvironment.path(home),
            Path::new("/Users/me/.zshenv")
        );
        assert_eq!(
            SystemItem::IdnmaNetwork.path(home),
            Path::new("/Users/me/.idnma.yml")
        );
        assert_eq!(filter_system(""), SystemItem::ALL.to_vec());
    }

    #[test]
    fn the_search_filters_system_rows_without_case_or_accents() {
        assert_eq!(
            filter_system("environnement"),
            vec![SystemItem::MacEnvironment]
        );
        assert_eq!(filter_system("IDNMA"), vec![SystemItem::IdnmaNetwork]);
        assert_eq!(filter_system("reseau"), vec![SystemItem::IdnmaNetwork]);
        assert!(filter_system("agents").is_empty());
        // Une recherche de section ne montre pas les lignes « Système » : elles ne sont pas des sections.
        assert!(filter_nav("environnement").is_empty());
    }
}
