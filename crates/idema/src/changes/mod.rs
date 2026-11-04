//! Le modèle du volet Changes (spec P3 §4) : ce que Git dit d'un dépôt, sans fenêtre. `status` lit la sortie de
//! `git status --porcelain=v2`, `model` charge un instantané et un patch, `rows` aplatit les changements en lignes
//! d'arbre, `panel` est le volet qui les montre. Toute lecture passe par `crate::git::run`.

pub mod ignore;
pub mod model;
pub mod ops;
pub mod panel;
pub mod rows;
pub mod status;
pub mod worktrees;

use std::collections::HashSet;
use std::path::PathBuf;

/// Le groupe d'un changement, dans l'ordre d'affichage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    Conflicts,
    Modified,
    Untracked,
    Committed,
}

/// La lettre de statut d'une ligne.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Letter {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflict,
}

impl Letter {
    pub fn glyph(self) -> &'static str {
        match self {
            Letter::Modified => "M",
            Letter::Added => "A",
            Letter::Deleted => "D",
            Letter::Renamed => "R",
            Letter::Untracked => "U",
            Letter::Conflict => "!",
        }
    }
}

/// L'état de la case d'une ligne : coché veut dire stagé dans l'index (P3-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Checked,
    Unchecked,
    Mixed,
    NoBox,
}

/// Un fichier changé. Les chemins sont relatifs à la racine du dépôt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub orig_path: Option<PathBuf>,
    pub group: Group,
    pub letter: Letter,
    pub check: Check,
}

/// La branche courante. `head == None` veut dire HEAD détachée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub head: Option<String>,
    pub has_commits: bool,
    pub upstream: Option<String>,
    /// Une branche amont est notée, mais Git n'en dit pas l'avance (`branch.ab` absent) : la branche distante suivie a
    /// disparu.
    pub upstream_gone: bool,
    pub ahead: u32,
}

/// Une lecture complète du dépôt d'un projet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// La racine du dépôt (pas forcément le dossier du projet, P3-11).
    pub root: PathBuf,
    /// Le dossier Git de la cible : celui d'un worktree lié (`<dépôt>/.git/worktrees/<nom>`), le dossier commun sinon.
    /// La surveillance du volet le suit pour voir l'index d'un worktree.
    pub git_dir: PathBuf,
    /// Le dossier Git commun à tous les worktrees du dépôt (`<dépôt>/.git`), où vivent les branches.
    pub common_dir: PathBuf,
    /// La branche locale principale (§4.2), `None` sans aucun commit.
    pub base: Option<String>,
    /// Le point de départ de la branche, `None` sans commit ou sans base.
    pub merge_base: Option<String>,
    pub branch: BranchInfo,
    pub has_origin: bool,
    pub changes: Vec<Change>,
    /// Les dossiers (relatifs à la racine) qui portent un fichier suivi, parmi ceux qui ont du contenu non suivi : un
    /// dossier de cette liste n'est pas « non suivi », même si seuls ses fichiers neufs sont listés (§4.8).
    pub tracked_folders: HashSet<PathBuf>,
}

/// Les aides des tests qui ont besoin d'un vrai dépôt. Elles lancent Git directement : la règle du lanceur unique vaut
/// pour le code de production, pas pour construire une fixture.
#[cfg(test)]
pub(crate) mod test_repo {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Un dépôt jetable, supprimé à la chute.
    pub struct TempRepo {
        dir: PathBuf,
    }

    impl TempRepo {
        /// Un dépôt vide (`git init -b main`), avec une identité locale et sans signature.
        pub fn init() -> TempRepo {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("idema-changes-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            // Les dossiers temporaires de macOS passent par un lien (`/var` vers `/private/var`) : Git rend le chemin résolu.
            let dir = dir.canonicalize().unwrap();
            let repo = TempRepo { dir };
            repo.git(&["init", "--quiet", "-b", "main"]);
            repo.git(&["config", "user.name", "Test"]);
            repo.git(&["config", "user.email", "test@example.com"]);
            repo.git(&["config", "commit.gpgsign", "false"]);
            // La maintenance automatique de Git (`maintenance run --auto --detach`, repaquetages) se déclenche sous charge
            // et peut perdre des objets pendant qu'un test lit le dépôt : un aléa de Git, pas d'IdemA (ses lectures
            // n'écrivent rien sous `.git`). On la coupe dans le dépôt jetable.
            repo.git(&["config", "gc.auto", "0"]);
            repo.git(&["config", "maintenance.auto", "false"]);
            repo
        }

        pub fn path(&self) -> &Path {
            &self.dir
        }

        /// Écrit un fichier (ses dossiers sont créés).
        pub fn write(&self, relative: &str, contents: &[u8]) {
            let path = self.dir.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        pub fn remove(&self, relative: &str) {
            fs::remove_file(self.dir.join(relative)).unwrap();
        }

        /// Lance Git dans le dépôt, échoue le test sur un code non nul et rend la sortie standard.
        pub fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        pub fn commit_all(&self, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "--quiet", "-m", message]);
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn readme() -> String {
        (1..=40).map(|n| format!("ligne {n}\n")).collect()
    }

    /// Le dépôt propre de l'AC du §7 : un commit sur `main`, rien à signaler.
    pub fn committed_repo() -> TempRepo {
        let repo = TempRepo::init();
        repo.write(".gitignore", b"/.worktrees/\n");
        repo.write("README.md", readme().as_bytes());
        repo.write("old.txt", b"ancien\n");
        repo.write("logo.bin", b"\x89PNG\0\x01\x02\x03");
        repo.commit_all("Initial commit");
        repo
    }

    /// `committed_repo` puis les changements de l'AC : `README.md` modifié aux lignes 3 et 30, `old.txt` supprimé,
    /// `captures/a.png` et `notes/brouillon.md` créés, `logo.bin` modifié.
    pub fn fixture_repo() -> TempRepo {
        let repo = committed_repo();
        let edited: String = readme()
            .lines()
            .enumerate()
            .map(|(index, line)| match index + 1 {
                3 | 30 => "ligne modifiée\n".to_string(),
                _ => format!("{line}\n"),
            })
            .collect();
        repo.write("README.md", edited.as_bytes());
        repo.remove("old.txt");
        repo.write("captures/a.png", b"\x89PNG\0\x09");
        repo.write("notes/brouillon.md", b"un brouillon\n");
        repo.write("logo.bin", b"\x89PNG\0\x04\x05\x06\x07");
        repo
    }

    /// Le dépôt d'un worktree lié : un commit de `b.md` sur `main`, puis le worktree `.worktrees/demo` (branche `demo`)
    /// avec un commit qui ajoute `a.md` et `b.md` modifié sans commit. Le dépôt principal garde les changements de
    /// `fixture_repo`.
    pub fn linked_worktree_repo() -> TempRepo {
        let repo = fixture_repo();
        repo.write("b.md", b"b\n");
        repo.git(&["add", "b.md"]);
        repo.git(&["commit", "--quiet", "-m", "Add b"]);
        repo.git(&[
            "worktree",
            "add",
            "--quiet",
            ".worktrees/demo",
            "-b",
            "demo",
        ]);
        repo.write(".worktrees/demo/a.md", b"a\n");
        repo.git(&["-C", ".worktrees/demo", "add", "a.md"]);
        repo.git(&["-C", ".worktrees/demo", "commit", "--quiet", "-m", "Add a"]);
        repo.write(".worktrees/demo/b.md", b"b changed\n");
        repo
    }
}
