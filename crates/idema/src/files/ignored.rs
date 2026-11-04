//! Ce que Git ignore dans un projet, pour atténuer ces lignes dans l'arbre (spec §4.3).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::git::{Gesture, run};

/// Chemins absolus ignorés par Git. Un dossier ignoré couvre tous ses descendants,
/// sans qu'on ait à les lister (un `target/` peut en compter des centaines de milliers).
#[derive(Debug, Default, Clone)]
pub struct IgnoredSet {
    entries: HashSet<PathBuf>,
}

impl IgnoredSet {
    /// Vrai quand le chemin est une entrée de l'ensemble ou se trouve sous un dossier ignoré.
    pub fn contains(&self, path: &Path) -> bool {
        path.ancestors()
            .any(|ancestor| self.entries.contains(ancestor))
    }
}

/// Interroge Git sur les fichiers ignorés sous `root`. `--directory` ramène un dossier
/// entièrement ignoré en une seule entrée (avec une barre finale) au lieu de son contenu.
/// Un échec de `git` (hors dépôt, binaire absent) rend `Err` : l'appelant garde l'ensemble vide.
pub fn git_ignored(root: &Path) -> io::Result<IgnoredSet> {
    let output = run(
        root,
        Gesture::Read,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
        None,
    )?;
    if !output.success() {
        return Err(io::Error::other("git ls-files failed"));
    }
    let entries = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let entry = entry.strip_suffix(b"/").unwrap_or(entry);
            root.join(OsStr::from_bytes(entry))
        })
        .collect();
    Ok(IgnoredSet { entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::git_init;
    use std::fs;

    #[test]
    fn git_reports_ignored_files_and_folders() {
        let dir = std::env::temp_dir().join(format!("idema-ignored-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        git_init(&dir).unwrap();
        fs::write(dir.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(dir.join("target/a.bin"), b"x").unwrap();
        fs::write(dir.join("x.log"), b"x").unwrap();
        fs::write(dir.join("src/main.rs"), b"fn main() {}").unwrap();

        let ignored = git_ignored(&dir).unwrap();

        assert!(ignored.contains(&dir.join("target/a.bin")));
        assert!(ignored.contains(&dir.join("target")));
        assert!(ignored.contains(&dir.join("x.log")));
        assert!(!ignored.contains(&dir.join("src/main.rs")));
        assert!(!ignored.contains(&dir.join("src")));
        let _ = fs::remove_dir_all(&dir);
    }
}
