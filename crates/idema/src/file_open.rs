//! Résoudre le chemin désigné dans un pane (⌘-clic, spec §4.7). Le geste ouvre le volet Fichiers : un fichier à sa
//! ligne et sa colonne, un dossier révélé dans l'arbre.

use std::path::{Path, PathBuf};

use idema_term::emulator::PathMatch;

/// Un fichier régulier existant, avec la position à atteindre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTarget {
    pub path: PathBuf,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// Ce qu'un chemin du pane désigne : un fichier à ouvrir, ou un dossier à révéler dans l'arbre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathTarget {
    File(FileTarget),
    Folder(PathBuf),
}

/// Ce que désigne un chemin du pane. `~/` se résout contre `home`, un chemin relatif contre le dossier du pane
/// (`cwd`) puis contre celui du projet (écart E31 : tmux ne l'a pas toujours encore donné, et un chemin imprimé
/// depuis la racine du projet ne se trouve pas depuis un sous-dossier), jamais contre le dossier d'IdemA. `None`
/// sauf pour un fichier régulier ou un dossier existant.
pub fn resolve(
    found: &PathMatch,
    cwd: Option<&Path>,
    project: Option<&Path>,
    home: &Path,
) -> Option<PathTarget> {
    let text = Path::new(&found.text);
    let candidates: Vec<PathBuf> = if let Ok(rest) = text.strip_prefix("~") {
        vec![home.join(rest)]
    } else if text.is_absolute() {
        vec![text.to_path_buf()]
    } else {
        [cwd, project]
            .into_iter()
            .flatten()
            .map(|base| base.join(text))
            .collect()
    };
    candidates.into_iter().find_map(|path| {
        let meta = std::fs::metadata(&path).ok()?;
        if meta.is_file() {
            Some(PathTarget::File(FileTarget {
                path,
                line: found.line,
                column: found.column,
            }))
        } else if meta.is_dir() {
            Some(PathTarget::Folder(path))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Un dossier neuf avec `src/theme.rs` dedans.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("idema-open-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/theme.rs"), "// thème\n").unwrap();
        dir
    }

    /// Le fichier d'une cible, ou l'échec du test si c'est un dossier ou rien.
    fn file_of(target: Option<PathTarget>) -> FileTarget {
        match target {
            Some(PathTarget::File(file)) => file,
            other => panic!("expected a file target, got {other:?}"),
        }
    }

    fn found(text: &str, line: Option<u32>, column: Option<u32>) -> PathMatch {
        PathMatch {
            text: text.to_owned(),
            line,
            column,
            start: (0, 0),
            end: (0, 0),
        }
    }

    #[test]
    fn a_relative_path_resolves_against_the_pane_folder_then_the_project() {
        let pane = folder("pane");
        let project = folder("project");
        let home = folder("home-unused");
        let relative = found("src/theme.rs", Some(42), Some(5));

        let from_pane = file_of(resolve(&relative, Some(&pane), Some(&project), &home));
        assert_eq!(from_pane.path, pane.join("src/theme.rs"));
        assert_eq!((from_pane.line, from_pane.column), (Some(42), Some(5)));

        // Le dossier du pane n'est pas encore connu (E31) : le projet sert.
        let from_project = file_of(resolve(&relative, None, Some(&project), &home));
        assert_eq!(from_project.path, project.join("src/theme.rs"));

        // Le dossier du pane ne contient pas le fichier, le projet si : un chemin imprimé depuis la racine du projet.
        let empty = std::env::temp_dir().join(format!("idema-open-{}-empty", std::process::id()));
        fs::create_dir_all(&empty).unwrap();
        let from_root = file_of(resolve(&relative, Some(&empty), Some(&project), &home));
        assert_eq!(from_root.path, project.join("src/theme.rs"));

        // Aucun des deux ne le contient, ou aucun n'est connu : jamais contre le dossier d'IdemA.
        assert_eq!(resolve(&relative, Some(&empty), Some(&empty), &home), None);
        assert_eq!(resolve(&relative, None, None, &home), None);
    }

    #[test]
    fn a_missing_path_is_not_a_target() {
        let dir = folder("kinds");
        let home = folder("home-kinds");
        let absolute = |path: &Path| found(&path.to_string_lossy(), None, None);
        assert_eq!(
            resolve(&absolute(&dir.join("src/absent.rs")), None, None, &home),
            None
        );
        let file = file_of(resolve(
            &absolute(&dir.join("src/theme.rs")),
            None,
            None,
            &home,
        ));
        assert_eq!(file.path, dir.join("src/theme.rs"));
    }

    #[test]
    fn a_folder_resolves_to_a_folder_target() {
        let pane = folder("folder-pane");
        let home = folder("folder-home");
        // Un dossier relatif se résout comme un fichier relatif, un `~/` contre `home`, un absolu tel quel.
        assert_eq!(
            resolve(&found("src", None, None), Some(&pane), None, &home),
            Some(PathTarget::Folder(pane.join("src")))
        );
        assert_eq!(
            resolve(&found("src/", None, None), Some(&pane), None, &home),
            Some(PathTarget::Folder(pane.join("src/")))
        );
        assert_eq!(
            resolve(&found("~/src", None, None), None, None, &home),
            Some(PathTarget::Folder(home.join("src")))
        );
        let absolute = pane.join("src");
        assert_eq!(
            resolve(
                &found(&absolute.to_string_lossy(), Some(7), None),
                None,
                None,
                &home
            ),
            Some(PathTarget::Folder(absolute))
        );
        // Un dossier absent n'est rien.
        assert_eq!(
            resolve(&found("nowhere/", None, None), Some(&pane), None, &home),
            None
        );
    }

    #[test]
    fn a_home_path_expands_against_home() {
        let home = folder("home");
        let target = file_of(resolve(
            &found("~/src/theme.rs", Some(3), None),
            None,
            None,
            &home,
        ));
        assert_eq!(target.path, home.join("src/theme.rs"));
        assert_eq!((target.line, target.column), (Some(3), None));
    }
}
