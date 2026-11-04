//! Les règles pures de la section « Bouillon de neurones » : noms Remote Control, commande, barre d'outils.

/// Le dossier fixe où tournent les bouillons.
pub const IDEATION_DIR: &str = "/Users/Shared/erom-ideation";
/// Libellé par défaut d'un bouillon, aussi la valeur de `@idema_agent`.
pub const IDEATION_LABEL: &str = "Fable";
/// Titre de la section dans la barre.
pub const SECTION_TITLE: &str = "Bouillon de neurones";

const REMOTE_PREFIX: &str = "erom-ideation-";

/// Le plus petit `erom-ideation-N` (N >= 2) absent de `taken`. Les noms qui n'ont pas cette forme sont ignorés.
pub fn next_remote_name(taken: &[&str]) -> String {
    let used: Vec<u32> = taken
        .iter()
        .filter_map(|name| name.strip_prefix(REMOTE_PREFIX)?.parse().ok())
        .collect();
    let free = (2u32..)
        .find(|n| !used.contains(n))
        .expect("an unbounded range always has a gap");
    format!("{REMOTE_PREFIX}{free}")
}

/// La commande lancée dans le pane d'un bouillon ; `remote` est à la fois le nom de session et le nom Remote Control.
pub fn ideation_command(remote: &str) -> String {
    format!(
        "\"$HOME/.local/bin/claude\" --setting-sources project,local --model fable --effort max --name {remote} --remote-control {remote}"
    )
}

/// Les boutons de la barre d'outils d'une session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarItem {
    Scratchpad,
    Terminal,
    Changes,
    Browser,
    Files,
    Project,
    Separator,
    Menu,
}

/// Les boutons à montrer, dans l'ordre : Projet est le dernier bouton de volet, juste avant le séparateur. Un bouillon
/// n'a ni projet ni dépôt : Scratchpad et Navigateur seulement.
pub fn toolbar_items(
    ideation_selected: bool,
    has_project: bool,
    has_session: bool,
) -> Vec<ToolbarItem> {
    use ToolbarItem::*;
    if ideation_selected {
        return vec![Scratchpad, Browser];
    }
    let mut items = vec![Scratchpad];
    if has_project {
        items.push(Terminal);
    }
    items.extend([Changes, Browser, Files, Project]);
    if has_session && has_project {
        items.push(Separator);
    }
    if has_session {
        items.push(Menu);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_remote_name_starts_at_two() {
        assert_eq!(next_remote_name(&[]), "erom-ideation-2");
    }

    #[test]
    fn next_remote_name_fills_the_smallest_gap() {
        assert_eq!(
            next_remote_name(&["erom-ideation-2", "erom-ideation-4"]),
            "erom-ideation-3"
        );
        assert_eq!(
            next_remote_name(&["erom-ideation", "", "foo", "erom-ideation-2"]),
            "erom-ideation-3"
        );
    }

    #[test]
    fn the_command_is_the_one_the_user_wrote() {
        assert_eq!(
            ideation_command("erom-ideation-2"),
            "\"$HOME/.local/bin/claude\" --setting-sources project,local --model fable --effort max --name erom-ideation-2 --remote-control erom-ideation-2"
        );
    }

    #[test]
    fn an_ideation_toolbar_keeps_scratchpad_and_browser() {
        let expected = vec![ToolbarItem::Scratchpad, ToolbarItem::Browser];
        assert_eq!(toolbar_items(true, true, true), expected);
        assert_eq!(toolbar_items(true, false, true), expected);
    }

    #[test]
    fn a_project_toolbar_ends_with_the_project_button() {
        use ToolbarItem::*;
        assert_eq!(
            toolbar_items(false, true, true),
            vec![
                Scratchpad, Terminal, Changes, Browser, Files, Project, Separator, Menu
            ]
        );
        assert_eq!(
            toolbar_items(false, false, false),
            vec![Scratchpad, Changes, Browser, Files, Project]
        );
    }
}
