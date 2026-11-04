//! Les règles pures de la section « Bouillon de neurones » : noms Remote Control, commande, barre d'outils.

use std::path::Path;

use crate::config::IdeationConfig;

/// Titre de la section dans la barre.
pub const SECTION_TITLE: &str = "Bouillon de neurones";

/// Le préfixe des noms Remote Control, tiré du nom du dossier : minuscules, tout caractère hors `[a-z0-9]` devient
/// `-`, tirets répétés fusionnés, tirets de bord retirés, suivi d'un tiret. Un nom vide donne `ideation-`.
pub fn remote_prefix(dir: &Path) -> String {
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let slug = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "ideation-".to_owned()
    } else {
        format!("{slug}-")
    }
}

/// Le plus petit `<prefix>N` (N >= 2) absent de `taken`. Les noms qui n'ont pas cette forme sont ignorés.
pub fn next_remote_name(prefix: &str, taken: &[&str]) -> String {
    let used: Vec<u32> = taken
        .iter()
        .filter_map(|name| name.strip_prefix(prefix)?.parse().ok())
        .collect();
    let free = (2u32..)
        .find(|n| !used.contains(n))
        .expect("an unbounded range always has a gap");
    format!("{prefix}{free}")
}

/// Une valeur passée telle quelle au shell, ou entre apostrophes (chacune doublée en `'\''`) si elle contient autre
/// chose que `[A-Za-z0-9._,:/=+@%-]` ou si elle commence par `=` (zsh l'élargit en chemin de commande).
fn shell_quote(value: &str) -> String {
    let plain = !value.is_empty()
        && !value.starts_with('=')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._,:/=+@%-".contains(c));
    if plain {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// La commande lancée dans le pane d'un bouillon ; `remote` est à la fois le nom de session et le nom Remote Control.
/// Il est construit par IdemA (`[a-z0-9-]`) : jamais cité. `--model` et `--effort` n'apparaissent que s'ils sont
/// configurés.
pub fn ideation_command(config: &IdeationConfig, remote: &str) -> String {
    let mut command = String::from("\"$HOME/.local/bin/claude\" --setting-sources project,local");
    if let Some(model) = &config.model {
        command.push_str(&format!(" --model {}", shell_quote(model)));
    }
    if let Some(effort) = &config.effort {
        command.push_str(&format!(" --effort {}", shell_quote(effort)));
    }
    command.push_str(&format!(" --name {remote} --remote-control {remote}"));
    command
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
    use std::path::Path;

    fn romain() -> IdeationConfig {
        IdeationConfig {
            dir: "/Users/Shared/erom-ideation".into(),
            model: Some("fable".into()),
            effort: Some("max".into()),
            label: "Fable".into(),
        }
    }

    #[test]
    fn next_remote_name_starts_at_two() {
        assert_eq!(next_remote_name("erom-ideation-", &[]), "erom-ideation-2");
    }

    #[test]
    fn next_remote_name_fills_the_smallest_gap() {
        assert_eq!(
            next_remote_name("erom-ideation-", &["erom-ideation-2", "erom-ideation-4"]),
            "erom-ideation-3"
        );
        assert_eq!(
            next_remote_name(
                "erom-ideation-",
                &["erom-ideation", "", "foo", "erom-ideation-2"]
            ),
            "erom-ideation-3"
        );
        assert_eq!(next_remote_name("idees-", &["erom-ideation-2"]), "idees-2");
    }

    #[test]
    fn the_command_of_romain_is_unchanged() {
        assert_eq!(
            ideation_command(&romain(), "erom-ideation-2"),
            "\"$HOME/.local/bin/claude\" --setting-sources project,local --model fable --effort max --name erom-ideation-2 --remote-control erom-ideation-2"
        );
    }

    #[test]
    fn absent_model_and_effort_leave_no_flag() {
        let mut config = romain();
        config.model = None;
        config.effort = None;
        assert_eq!(
            ideation_command(&config, "idees-2"),
            "\"$HOME/.local/bin/claude\" --setting-sources project,local --name idees-2 --remote-control idees-2"
        );
        config.model = Some("opus".into());
        assert_eq!(
            ideation_command(&config, "idees-2"),
            "\"$HOME/.local/bin/claude\" --setting-sources project,local --model opus --name idees-2 --remote-control idees-2"
        );
    }

    #[test]
    fn values_that_need_it_are_quoted() {
        let command_with = |model: &str| {
            let mut config = romain();
            config.model = Some(model.into());
            config.effort = None;
            ideation_command(&config, "idees-2")
        };
        assert!(command_with("claude-opus-5-5[1m]").contains("--model 'claude-opus-5-5[1m]' "));
        assert!(command_with("a b").contains("--model 'a b' "));
        assert!(command_with("=ls").contains("--model '=ls' "));
        assert!(command_with("o'neil").contains("--model 'o'\\''neil' "));
        assert!(command_with("opus-5.5:x/y=1+2@3%4,5").contains("--model opus-5.5:x/y=1+2@3%4,5 "));
    }

    #[test]
    fn the_prefix_comes_from_the_folder_name() {
        let prefix = |dir: &str| remote_prefix(Path::new(dir));
        assert_eq!(prefix("/Users/Shared/erom-ideation"), "erom-ideation-");
        assert_eq!(prefix("/h/idees"), "idees-");
        assert_eq!(prefix("/h/Mes Idées_2"), "mes-id-es-2-");
        assert_eq!(prefix("/"), "ideation-");
        assert_eq!(prefix("/h/___"), "ideation-");
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
