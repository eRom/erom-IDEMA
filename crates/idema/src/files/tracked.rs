//! Ce que Git suit dans un projet, pour éteindre « Ajouter au .gitignore » sur ce qui est déjà dans le dépôt (spec P3 §4.8).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::git::{Gesture, run};

/// Chemins absolus suivis par Git : les fichiers de l'index et, pour chacun, tous les dossiers qui le contiennent (jusqu'à
/// la racine du projet comprise). Un dossier est donc « suivi » dès qu'il contient un fichier suivi.
#[derive(Debug, Default, Clone)]
pub struct TrackedSet {
    entries: HashSet<PathBuf>,
}

impl TrackedSet {
    /// Vrai pour un fichier suivi, et pour un dossier qui en contient un.
    pub fn contains(&self, path: &Path) -> bool {
        self.entries.contains(path)
    }

    fn insert_with_ancestors(&mut self, root: &Path, file: PathBuf) {
        let mut current = Some(file.as_path());
        while let Some(path) = current {
            if !path.starts_with(root) {
                break;
            }
            // Un dossier déjà là a déjà tous ses ancêtres.
            if !self.entries.insert(path.to_path_buf()) {
                break;
            }
            current = path.parent();
        }
    }
}

/// Interroge Git sur les fichiers de l'index sous `root` (`ls-files -z --cached`, en lecture : aucun verrou). Un échec de
/// `git` (hors dépôt, binaire absent) rend `Err` : l'appelant ne sait pas ce qui est suivi, et n'ose rien proposer.
pub fn git_tracked(root: &Path) -> io::Result<TrackedSet> {
    let output = run(root, Gesture::Read, &["ls-files", "-z", "--cached"], None)?;
    if !output.success() {
        return Err(io::Error::other(output.stderr.trim().to_string()));
    }
    let mut set = TrackedSet::default();
    for entry in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        set.insert_with_ancestors(root, root.join(OsStr::from_bytes(entry)));
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::test_repo::TempRepo;

    #[test]
    fn tracked_set_covers_folders() {
        let repo = TempRepo::init();
        repo.write("top.txt", b"x");
        repo.write("a/b/c.txt", b"x");
        repo.write("a/b/untracked.txt", b"x");
        repo.write("fresh/new.txt", b"x");
        repo.git(&["add", "top.txt", "a/b/c.txt"]);
        let root = repo.path();

        let tracked = git_tracked(root).unwrap();

        // Un fichier suivi, et chaque dossier qui le contient.
        assert!(tracked.contains(&root.join("top.txt")));
        assert!(tracked.contains(&root.join("a/b/c.txt")));
        assert!(tracked.contains(&root.join("a/b")));
        assert!(tracked.contains(&root.join("a")));
        // Un fichier non suivi, même dans un dossier suivi, et un dossier qui n'a que du non suivi.
        assert!(!tracked.contains(&root.join("a/b/untracked.txt")));
        assert!(!tracked.contains(&root.join("fresh")));
        assert!(!tracked.contains(&root.join("fresh/new.txt")));
    }

    #[test]
    fn a_subfolder_project_sees_only_its_own_files() {
        let repo = TempRepo::init();
        repo.write("outer.txt", b"x");
        repo.write("app/inner.txt", b"x");
        repo.git(&["add", "-A"]);
        let project = repo.path().join("app");

        let tracked = git_tracked(&project).unwrap();

        assert!(tracked.contains(&project.join("inner.txt")));
        assert!(!tracked.contains(&repo.path().join("outer.txt")));
    }

    #[test]
    fn outside_a_repository_the_read_fails() {
        let dir = std::env::temp_dir().join(format!("idema-tracked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Aucun dépôt au-dessus d'un dossier du dossier temporaire du système.
        let outcome = git_tracked(&dir);

        assert!(outcome.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
