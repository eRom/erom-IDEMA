//! Dossiers de projet (spec §4.1) : ce qu'Idema fait d'un dossier choisi dans le Finder, et le nom affiché.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::git::{Gesture, run};

/// Ce que le dossier choisi est, pour décider entre l'ajouter, l'initialiser ou demander.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderKind {
    /// `git rev-parse --is-inside-work-tree` vaut `true` : ajouté tel quel.
    InsideRepository,
    /// Vide, `.DS_Store` ignoré : `git init`, puis ajout.
    Empty,
    /// Non vide et sans dépôt : Idema demande avant d'initialiser.
    NotEmpty,
}

/// Un saut de ligne couperait une ligne de commande du mode contrôle de tmux en deux (pièges du 14/10), et une
/// tabulation casserait `parse_sessions`, qui découpe la liste des sessions sur `\t`.
pub fn path_is_acceptable(path: &Path) -> bool {
    !path.to_string_lossy().contains(['\n', '\r', '\t'])
}

pub fn classify(dir: &Path) -> std::io::Result<FolderKind> {
    if inside_repository(dir) {
        return Ok(FolderKind::InsideRepository);
    }
    let mut entries = std::fs::read_dir(dir)?;
    let empty = entries.all(|entry| entry.is_ok_and(|entry| entry.file_name() == ".DS_Store"));
    Ok(if empty {
        FolderKind::Empty
    } else {
        FolderKind::NotEmpty
    })
}

fn inside_repository(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--is-inside-work-tree"]).is_some_and(|out| out == "true")
}

pub fn git_init(dir: &Path) -> std::io::Result<()> {
    match git(dir, &["init", "--quiet"]) {
        Some(_) => Ok(()),
        None => Err(std::io::Error::other("git init failed")),
    }
}

/// Après le `git init` d'IdemA : `.DS_Store` entre au `.gitignore` du dossier, créé s'il manque. Un `.gitignore` qui
/// l'ignore déjà n'est pas touché ; sinon la ligne s'ajoute à la fin, sans rien réécrire.
pub fn ignore_ds_store(dir: &Path) -> std::io::Result<()> {
    let path = dir.join(".gitignore");
    let current = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    if current.lines().any(|line| line.trim() == ".DS_Store") {
        return Ok(());
    }
    let separator = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    std::io::Write::write_all(&mut file, format!("{separator}.DS_Store\n").as_bytes())
}

/// Les chemins que macOS demande d'ouvrir (`open -b com.erom.idema <dossier>`, la CLI `idema`, l'action rapide du
/// Finder) : les URL `file://` décodées, sans la barre finale qu'AppKit met aux dossiers, pour retrouver le projet
/// tel que `⌘O` l'enregistre. Une autre URL est écartée.
pub fn opened_paths(urls: &[String]) -> Vec<PathBuf> {
    urls.iter()
        .filter_map(|url| url::Url::parse(url).ok()?.to_file_path().ok())
        .map(|path| path.components().collect())
        .collect()
}

/// Un dossier demandé de l'extérieur, sous la forme que le disque lui donne : macOS envoie les noms accentués
/// décomposés (NFD) quand le panneau de `⌘O` rend ceux du disque, et deux formes feraient deux projets pour un même
/// dossier. `None` pour un fichier ou un chemin disparu.
pub fn folder_on_disk(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path)
        .ok()
        .filter(|path| path.is_dir())
}

/// Les projets dont le dossier n'existe plus (spec §4.1) : recalculés à l'amorçage et à chaque activation.
pub fn missing_projects<'a>(paths: impl Iterator<Item = &'a Path>) -> HashSet<PathBuf> {
    paths
        .filter(|path| !path.is_dir())
        .map(Path::to_path_buf)
        .collect()
}

/// Branche courante, pour l'écran vide (spec §7.1). Rien hors d'un dépôt ou en tête détachée.
pub fn current_branch(dir: &Path) -> Option<String> {
    git(dir, &["branch", "--show-current"]).filter(|branch| !branch.is_empty())
}

/// Nom affiché d'un projet : celui de son dossier.
pub fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let gesture = if args.first() == Some(&"init") {
        Gesture::InitProject
    } else {
        Gesture::Read
    };
    let output = run(dir, gesture, args, None).ok()?;
    output
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("idema-project-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_folder_ignores_ds_store_then_becomes_a_repository() {
        let dir = temp_dir("empty");
        std::fs::write(dir.join(".DS_Store"), b"").unwrap();
        assert_eq!(classify(&dir).unwrap(), FolderKind::Empty);
        git_init(&dir).unwrap();
        assert_eq!(classify(&dir).unwrap(), FolderKind::InsideRepository);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignoring_ds_store_creates_the_gitignore_or_completes_it_once() {
        let dir = temp_dir("gitignore");
        ignore_ds_store(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            ".DS_Store\n"
        );
        // Un `.gitignore` sans saut de ligne final garde ses lignes ; un second passage n'ajoute rien.
        std::fs::write(dir.join(".gitignore"), "target/").unwrap();
        ignore_ds_store(&dir).unwrap();
        ignore_ds_store(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "target/\n.DS_Store\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opened_urls_become_paths_without_the_trailing_slash() {
        let urls = [
            "file:///Users/me/dev/caf%C3%A9%20bar/".to_string(),
            "file:///Users/me/notes.md".to_string(),
            "https://example.com/".to_string(),
        ];
        assert_eq!(
            opened_paths(&urls),
            [
                PathBuf::from("/Users/me/dev/café bar"),
                PathBuf::from("/Users/me/notes.md"),
            ]
        );
    }

    #[test]
    fn a_decomposed_accent_finds_the_folder_as_the_disk_names_it() {
        let dir = temp_dir("nfd");
        let composed = dir.join("caf\u{e9}");
        std::fs::create_dir(&composed).unwrap();
        std::fs::write(dir.join("notes.md"), b"x").unwrap();
        let decomposed = dir.join("cafe\u{301}");
        let disk = std::fs::canonicalize(&dir).unwrap().join("caf\u{e9}");
        assert_eq!(folder_on_disk(&decomposed), Some(disk));
        assert_eq!(folder_on_disk(&dir.join("notes.md")), None);
        assert_eq!(folder_on_disk(&dir.join("gone")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_with_files_and_no_repository_asks_first() {
        let dir = temp_dir("notempty");
        std::fs::write(dir.join("notes.md"), b"x").unwrap();
        assert_eq!(classify(&dir).unwrap(), FolderKind::NotEmpty);
        assert!(!dir.join(".git").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_subfolder_of_a_repository_is_inside_it() {
        let dir = temp_dir("sub");
        git_init(&dir).unwrap();
        let sub = dir.join("crates");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(classify(&sub).unwrap(), FolderKind::InsideRepository);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_project_folder_that_vanished_is_missing() {
        let kept = temp_dir("kept");
        let gone = temp_dir("gone");
        let paths = [kept.clone(), gone.clone()];
        assert!(missing_projects(paths.iter().map(PathBuf::as_path)).is_empty());
        std::fs::remove_dir_all(&gone).unwrap();
        let missing = missing_projects(paths.iter().map(PathBuf::as_path));
        assert!(missing.contains(&gone) && !missing.contains(&kept));
        let _ = std::fs::remove_dir_all(&kept);
    }

    #[test]
    fn git_init_fails_cleanly_in_a_missing_folder() {
        let dir = temp_dir("noinit");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(git_init(&dir).is_err());
    }

    #[test]
    fn names_and_paths() {
        assert_eq!(display_name(Path::new("/Users/r/dev/lintree")), "lintree");
        assert!(path_is_acceptable(Path::new("/Users/r/dev/mon projet")));
        assert!(!path_is_acceptable(Path::new("/tmp/a\nb")));
        assert!(!path_is_acceptable(Path::new("/tmp/a\tb")));
    }
}
