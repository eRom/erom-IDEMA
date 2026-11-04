//! Volet Projet : instructions, compétences et serveurs MCP du dossier ouvert.

pub mod banner;
#[cfg(test)]
mod host_gate;
pub mod instructions;
pub mod instructions_view;
pub mod mcp_files;
pub mod mcp_model;
pub mod mcp_view;
pub mod panel;
pub mod skills;
pub mod skills_cli;
pub mod skills_view;

use std::fs;
use std::path::{Path, PathBuf};

/// Vrai si `project` est le dossier personnel (chemins canoniques, liens et `/` final résolus). Le dossier personnel
/// n'est pas un projet : sa mémoire `~/.claude/CLAUDE.md`, ses skills globales (`~/.claude/skills`, `~/.grok/skills`,
/// `~/.agents/skills`) et ses configs de harnais (`~/.codex`, `~/.grok`) sont ceux de toute la machine. Les trois
/// onglets du volet s'appuient sur ce seul prédicat. Sans dossier personnel connu, ou si un des deux chemins ne se
/// résout pas, faux.
pub fn is_home(project: &Path, home: Option<&Path>) -> bool {
    let Some(home) = home else {
        return false;
    };
    match (fs::canonicalize(project), fs::canonicalize(home)) {
        (Ok(project), Ok(home)) => project == home,
        _ => false,
    }
}

/// Les dossiers de configuration globale des harnais, sous le dossier personnel. Rien du projet n'y est écrit ni mis à
/// la Corbeille, même par un lien symbolique.
const GLOBAL_CONFIG_DIRS: [&str; 6] = [
    ".codex",
    ".grok",
    ".claude",
    ".config/opencode",
    ".gemini",
    ".agents",
];

/// `path` avec tous ses liens résolus, même s'il n'existe pas encore : le plus long préfixe qui existe est canonisé, le
/// reste suit tel quel.
fn resolve_lenient(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut head = path.to_path_buf();
    loop {
        if let Ok(real) = fs::canonicalize(&head) {
            return rest.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (head.file_name().map(ToOwned::to_owned), head.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                head = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Vrai quand `path`, ses liens résolus, tombe dans un dossier de configuration globale de `home` (`~/.codex`,
/// `~/.grok`, `~/.claude`, `~/.config/opencode`, `~/.gemini`, `~/.agents`) : un projet dont `.codex` est un lien vers
/// `~/.codex` écrirait dans la config de toute la machine. Un lien de fichier qui mène ailleurs (dotfiles) reste permis.
/// Sans dossier personnel connu, faux.
pub fn resolves_into_global_config(path: &Path, home: Option<&Path>) -> bool {
    let Some(Ok(home)) = home.map(fs::canonicalize) else {
        return false;
    };
    let resolved = resolve_lenient(path);
    GLOBAL_CONFIG_DIRS.iter().any(|dir| {
        let global = home.join(dir);
        let global = fs::canonicalize(&global).unwrap_or(global);
        resolved.starts_with(global)
    })
}

/// Le message d'un geste refusé parce que `file` (écrit relativement au projet) mène à une configuration globale.
pub fn global_config_text(file: &str) -> String {
    format!("{file} pointe vers une configuration globale : rien n'a été écrit.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("idema-is-home-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn home_is_recognised_however_it_is_written() {
        let root = scratch("forms");
        let home = root.join("home");
        fs::create_dir_all(home.join("dev/app")).unwrap();

        assert!(is_home(&home, Some(&home)));
        // Barre finale, `.` et `..` ne changent rien.
        assert!(is_home(
            Path::new(&format!("{}/", home.display())),
            Some(&home)
        ));
        assert!(is_home(&home.join("dev").join(".."), Some(&home)));
        assert!(is_home(&home.join("."), Some(&home)));
        // Un lien vers le dossier personnel en est un autre nom.
        std::os::unix::fs::symlink(&home, root.join("alias")).unwrap();
        assert!(is_home(&root.join("alias"), Some(&home)));
        assert!(is_home(&home, Some(&root.join("alias"))));

        // Un sous-dossier, le parent et un voisin de même préfixe n'en sont pas.
        assert!(!is_home(&home.join("dev/app"), Some(&home)));
        assert!(!is_home(&root, Some(&home)));
        fs::create_dir_all(root.join("home2")).unwrap();
        assert!(!is_home(&root.join("home2"), Some(&home)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_path_resolving_into_a_global_config_is_recognised_through_links() {
        let root = scratch("global");
        let home = root.join("home");
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".config/opencode")).unwrap();
        fs::create_dir_all(home.join("dotfiles")).unwrap();
        let project = home.join("work/app");
        fs::create_dir_all(&project).unwrap();

        // Un dossier de projet qui est un lien vers `~/.codex` : même un fichier qui n'existe pas encore y mène.
        std::os::unix::fs::symlink(home.join(".codex"), project.join(".codex")).unwrap();
        assert!(resolves_into_global_config(
            &project.join(".codex/config.toml"),
            Some(&home)
        ));
        // Un fichier lien vers la config globale, et la config globale elle-même.
        fs::write(home.join(".config/opencode/opencode.json"), "{}").unwrap();
        std::os::unix::fs::symlink(
            home.join(".config/opencode/opencode.json"),
            project.join("opencode.json"),
        )
        .unwrap();
        assert!(resolves_into_global_config(
            &project.join("opencode.json"),
            Some(&home)
        ));
        assert!(resolves_into_global_config(
            &home.join(".claude/CLAUDE.md"),
            Some(&home)
        ));

        // Un lien de fichier vers des dotfiles, un vrai dossier `.grok` du projet, un voisin de même préfixe : permis.
        fs::write(home.join("dotfiles/mcp.json"), "{}").unwrap();
        std::os::unix::fs::symlink(home.join("dotfiles/mcp.json"), project.join(".mcp.json"))
            .unwrap();
        assert!(!resolves_into_global_config(
            &project.join(".mcp.json"),
            Some(&home)
        ));
        fs::create_dir_all(project.join(".grok")).unwrap();
        assert!(!resolves_into_global_config(
            &project.join(".grok/config.toml"),
            Some(&home)
        ));
        assert!(!resolves_into_global_config(
            &home.join(".codex-notes/x"),
            Some(&home)
        ));
        // Sans dossier personnel, rien n'est refusé.
        assert!(!resolves_into_global_config(
            &project.join(".codex/config.toml"),
            None
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn home_is_never_guessed_when_it_cannot_be_resolved() {
        let root = scratch("unknown");
        assert!(!is_home(&root, None));
        assert!(!is_home(&root.join("missing"), Some(&root.join("missing"))));
        assert!(!is_home(&root, Some(&root.join("missing"))));
        let _ = fs::remove_dir_all(&root);
    }
}
