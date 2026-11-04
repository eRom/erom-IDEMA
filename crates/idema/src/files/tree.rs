//! L'arbre du projet (spec §4.3), sans fenêtre : le modèle des lignes, que la vue ne fait que dessiner.
//! Seule la racine est lue à la création ; un dossier ne l'est qu'au dépliage, si bien qu'un gros
//! dossier ignoré (`target/`) n'est jamais parcouru tant qu'on ne l'ouvre pas.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use super::ignored::IgnoredSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
    pub ignored: bool,
}

/// Les entrées d'un dossier : dossiers d'abord, puis fichiers, chacun par nom sans casse.
/// `.git` et `.DS_Store` sont exclus. Un lien symbolique prend le type de sa cible (un lien vers un dossier se déplie,
/// un lien cassé est un fichier) : le dépliage reste à la demande, un lien en boucle ne lit donc jamais plus d'un niveau.
pub fn list_dir(dir: &Path) -> io::Result<Vec<Node>> {
    let mut nodes = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == ".DS_Store" {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            // Une entrée supprimée pendant la lecture n'est pas une erreur du dossier.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let path = entry.path();
        let is_dir = file_type.is_dir() || (file_type.is_symlink() && path.is_dir());
        nodes.push(Node { path, name, is_dir });
    }
    nodes.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(nodes)
}

/// `path`, sous `root`, est-il vu à travers un dossier-lien (un de ses dossiers, entre `root` et lui, est un lien) ?
/// Git ne regarde pas derrière un lien. Le lien lui-même n'est pas derrière un lien.
pub fn behind_link(root: &Path, path: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .take_while(|ancestor| *ancestor != root && ancestor.starts_with(root))
        .any(Path::is_symlink)
}

pub struct FileTree {
    root: PathBuf,
    /// Les dossiers déjà lus (la racine toujours), avec leurs entrées.
    loaded: HashMap<PathBuf, Vec<Node>>,
    expanded: HashSet<PathBuf>,
    ignored: IgnoredSet,
}

impl FileTree {
    /// Lit la racine. Une racine illisible donne un arbre vide : la vue affiche déjà
    /// « Le dossier du projet n'existe plus. » à part (spec §4.1).
    pub fn new(root: PathBuf) -> Self {
        let nodes = list_dir(&root).unwrap_or_default();
        Self {
            loaded: HashMap::from([(root.clone(), nodes)]),
            root,
            expanded: HashSet::new(),
            ignored: IgnoredSet::default(),
        }
    }

    pub fn set_ignored(&mut self, ignored: IgnoredSet) {
        self.ignored = ignored;
    }

    /// Le chemin est-il ignoré par Git, d'après le dernier calcul (un dossier ignoré couvre ce qu'il contient) ?
    pub fn is_ignored(&self, path: &Path) -> bool {
        self.ignored.contains(path)
    }

    /// Les lignes visibles, dans l'ordre d'affichage : un dossier replié ne montre pas ses entrées.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        self.push_rows(&self.root, 0, &mut rows);
        rows
    }

    fn push_rows(&self, dir: &Path, depth: usize, rows: &mut Vec<Row>) {
        let Some(nodes) = self.loaded.get(dir) else {
            return;
        };
        for node in nodes {
            let expanded = node.is_dir && self.expanded.contains(&node.path);
            rows.push(Row {
                path: node.path.clone(),
                name: node.name.clone(),
                depth,
                is_dir: node.is_dir,
                expanded,
                ignored: self.ignored.contains(&node.path),
            });
            if expanded {
                self.push_rows(&node.path, depth + 1, rows);
            }
        }
    }

    /// Le dossier a-t-il déjà été lu ? Un premier dépliage lit des entrées dont Git n'a pas encore dit si elles sont
    /// ignorées.
    pub fn is_loaded(&self, dir: &Path) -> bool {
        self.loaded.contains_key(dir)
    }

    /// Plie un dossier déplié, ou le lit puis le déplie. En cas d'erreur de lecture il reste replié.
    pub fn toggle(&mut self, dir: &Path) -> io::Result<()> {
        if self.expanded.remove(dir) {
            return Ok(());
        }
        self.expand(dir)
    }

    fn expand(&mut self, dir: &Path) -> io::Result<()> {
        let nodes = list_dir(dir)?;
        self.loaded.insert(dir.to_path_buf(), nodes);
        self.expanded.insert(dir.to_path_buf());
        Ok(())
    }

    /// Déplie les ancêtres de `path` sous la racine, et `path` lui-même si c'est un dossier.
    /// Un chemin hors de la racine ne change rien. En cas d'échec, l'erreur porte le dossier illisible.
    pub fn reveal(&mut self, path: &Path) -> Result<(), (PathBuf, io::Error)> {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return Ok(());
        };
        let mut chain: Vec<PathBuf> = relative
            .ancestors()
            .skip(1)
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .map(|ancestor| self.root.join(ancestor))
            .collect();
        chain.reverse();
        if path != self.root && path.is_dir() {
            chain.push(path.to_path_buf());
        }
        for dir in chain {
            // Un dossier déjà déplié garde sa lecture : on ne relit que ce qui est neuf.
            if !self.expanded.contains(&dir) {
                self.expand(&dir).map_err(|error| (dir, error))?;
            }
        }
        Ok(())
    }

    /// `from` a été renommé ou déplacé en `to` sur le disque : les dossiers lus et dépliés sous `from` (clés de
    /// `loaded`, chemins des `Node`, `expanded`) passent sous `to`, pour qu'un dossier déplié le reste. L'appelant
    /// relit ensuite les deux parents par `refresh`, qui apporte le nouveau nom.
    pub fn rename_under(&mut self, from: &Path, to: &Path) {
        let moved = |path: &Path| -> Option<PathBuf> {
            let rest = path.strip_prefix(from).ok()?;
            Some(if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            })
        };
        let loaded = std::mem::take(&mut self.loaded);
        self.loaded = loaded
            .into_iter()
            .map(|(dir, mut nodes)| {
                for node in &mut nodes {
                    if let Some(path) = moved(&node.path) {
                        node.path = path;
                    }
                }
                (moved(&dir).unwrap_or(dir), nodes)
            })
            .collect();
        let expanded = std::mem::take(&mut self.expanded);
        self.expanded = expanded
            .into_iter()
            .map(|dir| moved(&dir).unwrap_or(dir))
            .collect();
    }

    /// Relit les dossiers déjà lus qui contiennent un des chemins changés. Un dossier lu
    /// qui a disparu est oublié (donc replié). Rend ce qui a bougé et les dossiers illisibles.
    pub fn refresh(&mut self, changed: &[PathBuf]) -> RefreshOutcome {
        let dirs: HashSet<PathBuf> = changed
            .iter()
            .filter_map(|path| path.parent())
            .filter(|parent| self.loaded.contains_key(*parent))
            .map(Path::to_path_buf)
            .collect();
        self.reread(dirs)
    }

    /// Le watcher a débordé sur `dirs` (`Flag::Rescan`) : il ne dit plus ce qui a changé dessous. Relit chacun de ces
    /// dossiers et tous les dossiers déjà lus qu'il contient, plus leur parent (qui a peut-être perdu le dossier).
    pub fn rescan(&mut self, dirs: &[PathBuf]) -> RefreshOutcome {
        let targets: HashSet<PathBuf> = self
            .loaded
            .keys()
            .filter(|loaded| {
                dirs.iter().any(|dir| {
                    loaded.starts_with(dir) || dir.parent().is_some_and(|parent| *loaded == parent)
                })
            })
            .cloned()
            .collect();
        self.reread(targets)
    }

    /// Relit tous les dossiers déjà lus : un volet qui n'était plus surveillé a pu rater n'importe quel changement.
    /// Les dossiers dépliés le restent.
    pub fn reload(&mut self) -> RefreshOutcome {
        let dirs: Vec<PathBuf> = self.loaded.keys().cloned().collect();
        self.reread(dirs)
    }

    fn reread(&mut self, dirs: impl IntoIterator<Item = PathBuf>) -> RefreshOutcome {
        let mut outcome = RefreshOutcome::default();
        for dir in dirs {
            let nodes = match list_dir(&dir) {
                Ok(nodes) => nodes,
                // La racine reste dans l'arbre, vide ; tout autre dossier est élagué ci-dessous.
                // Le dossier doit avoir réellement disparu : un NotFound isolé n'efface rien.
                Err(error) if error.kind() == io::ErrorKind::NotFound && !dir.is_dir() => {
                    Vec::new()
                }
                Err(error) => {
                    outcome.errors.push((dir, error));
                    continue;
                }
            };
            let before = self.loaded.get(&dir);
            if before != Some(&nodes) {
                outcome.changed = true;
            }
            // Une entrée qui n'y était pas : de quoi demander à Git si elle est ignorée.
            let known: HashSet<&Path> = before
                .map(|before| before.iter().map(|node| node.path.as_path()).collect())
                .unwrap_or_default();
            if nodes
                .iter()
                .any(|node| !known.contains(node.path.as_path()))
            {
                outcome.gained_entries = true;
            }
            self.loaded.insert(dir, nodes);
        }
        let loaded_before = self.loaded.len();
        self.loaded
            .retain(|dir, _| *dir == self.root || dir.is_dir());
        if self.loaded.len() != loaded_before {
            outcome.changed = true;
        }
        self.expanded.retain(|dir| self.loaded.contains_key(dir));
        outcome
    }
}

/// Ce qu'une relecture a trouvé : de quoi décider s'il faut redessiner, et s'il faut redemander à Git ce qui est
/// ignoré. Un lot sous un dossier replié (jamais lu) ne donne rien de tout cela.
#[derive(Debug, Default)]
pub struct RefreshOutcome {
    /// Les lignes d'au moins un dossier lu ont changé (ou un dossier lu a disparu).
    pub changed: bool,
    /// Un dossier déjà lu a gagné au moins une entrée.
    pub gained_entries: bool,
    /// Les dossiers qu'on n'a pas pu relire, avec la cause : l'appelant les journalise.
    pub errors: Vec<(PathBuf, io::Error)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::ignored::git_ignored;
    use crate::project::git_init;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("idema-tree-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|row| row.name.as_str()).collect()
    }

    #[test]
    fn rows_put_folders_first_then_files_case_insensitively() {
        let dir = temp_dir("order");
        fs::write(dir.join("B.md"), b"").unwrap();
        fs::write(dir.join("a.md"), b"").unwrap();
        fs::write(dir.join(".env"), b"").unwrap();
        fs::create_dir(dir.join("Zdir")).unwrap();
        fs::create_dir(dir.join("adir")).unwrap();
        fs::create_dir(dir.join(".git")).unwrap();
        fs::write(dir.join(".DS_Store"), b"").unwrap();

        let tree = FileTree::new(dir.clone());
        let rows = tree.rows();

        assert_eq!(names(&rows), ["adir", "Zdir", ".env", "a.md", "B.md"]);
        assert!(rows[0].is_dir && rows[1].is_dir && !rows[2].is_dir);
        assert!(rows.iter().all(|row| row.depth == 0 && !row.expanded));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Remet les droits puis retire le dossier, même quand une assertion échoue en route.
    struct Cleanup {
        dir: PathBuf,
        locked: PathBuf,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.locked, fs::Permissions::from_mode(0o755));
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_folder_counts_as_loaded_only_once_it_was_unfolded() {
        let dir = temp_dir("is-loaded");
        fs::create_dir(dir.join("sub")).unwrap();
        let sub = dir.join("sub");
        let mut tree = FileTree::new(dir.clone());
        assert!(tree.is_loaded(&dir));
        assert!(!tree.is_loaded(&sub));
        tree.toggle(&sub).unwrap();
        assert!(tree.is_loaded(&sub));
        // Replié, il garde sa lecture : le dépliage suivant n'est plus une première lecture.
        tree.toggle(&sub).unwrap();
        assert!(tree.is_loaded(&sub));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folded_folder_is_never_listed() {
        let dir = temp_dir("folded");
        fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        let target_path = dir.join("target");
        let locked = target_path.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(target_path.join("a.bin"), b"x").unwrap();
        let _cleanup = Cleanup {
            dir: dir.clone(),
            locked: locked.clone(),
        };
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        git_init(&dir).unwrap();

        let mut tree = FileTree::new(dir.clone());
        tree.set_ignored(git_ignored(&dir).unwrap());
        // Le cache du modèle n'a jamais vu `target` : la racine seule a été lue.
        assert!(!tree.loaded.contains_key(&target_path));
        let rows = tree.rows();
        let target = rows.iter().find(|row| row.name == "target").unwrap();
        assert!(target.is_dir && !target.expanded && target.ignored);
        assert!(!rows.iter().any(|row| row.name == "a.bin"));
        // `is_ignored` dit la même chose que les lignes : le dossier et ce qu'il contient, pas ses voisins.
        assert!(tree.is_ignored(&target_path));
        assert!(tree.is_ignored(&target_path.join("deep/x.bin")));
        assert!(!tree.is_ignored(&dir.join("src/main.rs")));

        // Déplier `target` ne lit que lui ; le sous-dossier verrouillé n'est touché qu'à son propre dépliage.
        tree.toggle(&target_path).unwrap();
        assert!(tree.loaded.contains_key(&target_path));
        assert!(!tree.loaded.contains_key(&locked));
        assert!(names(&tree.rows()).contains(&"locked"));
        let error = tree.toggle(&locked).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!tree.loaded.contains_key(&locked));
        assert!(
            tree.rows()
                .iter()
                .all(|row| row.path != locked || !row.expanded)
        );
    }

    #[test]
    fn reveal_unfolds_the_ancestors() {
        let dir = temp_dir("reveal");
        fs::create_dir_all(dir.join("docs/specs")).unwrap();
        fs::write(dir.join("docs/specs/a.md"), b"").unwrap();
        fs::write(dir.join("docs/other.md"), b"").unwrap();

        let mut tree = FileTree::new(dir.clone());
        tree.reveal(&dir.join("docs/specs/a.md")).unwrap();
        let rows = tree.rows();

        let shown: Vec<(&str, usize, bool)> = rows
            .iter()
            .map(|row| (row.name.as_str(), row.depth, row.expanded))
            .collect();
        assert_eq!(
            shown,
            [
                ("docs", 0, true),
                ("specs", 1, true),
                ("a.md", 2, false),
                ("other.md", 1, false),
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reveal_reports_the_folder_it_could_not_read() {
        let dir = temp_dir("reveal-locked");
        let locked = dir.join("docs/locked");
        fs::create_dir_all(locked.join("inner")).unwrap();
        let _cleanup = Cleanup {
            dir: dir.clone(),
            locked: locked.clone(),
        };
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let mut tree = FileTree::new(dir.clone());
        let (failed, error) = tree.reveal(&locked.join("inner/a.md")).unwrap_err();
        // C'est le dossier illisible qui est nommé, pas la cible demandée.
        assert_eq!(failed, locked);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn reveal_unfolds_a_folder_itself_and_ignores_paths_outside_the_root() {
        let dir = temp_dir("reveal-dir");
        fs::create_dir_all(dir.join("docs/specs")).unwrap();
        fs::write(dir.join("docs/specs/a.md"), b"").unwrap();

        let mut tree = FileTree::new(dir.clone());
        tree.reveal(&dir.join("docs/specs")).unwrap();
        assert_eq!(names(&tree.rows()), ["docs", "specs", "a.md"]);

        let before = tree.rows();
        tree.reveal(Path::new("/elsewhere/file.md")).unwrap();
        assert_eq!(tree.rows(), before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_to_a_folder_is_a_folder_that_unfolds() {
        // Une racine canonique, comme celle du volet : `TMPDIR` passe par le lien `/var`.
        let dir = fs::canonicalize(temp_dir("symlink-dir")).unwrap();
        let store = fs::canonicalize(temp_dir("symlink-store")).unwrap();
        fs::create_dir(store.join("books")).unwrap();
        fs::write(store.join("INDEX.md"), b"").unwrap();
        std::os::unix::fs::symlink(&store, dir.join("erom-store")).unwrap();
        std::os::unix::fs::symlink(store.join("INDEX.md"), dir.join("index-link.md")).unwrap();
        std::os::unix::fs::symlink(dir.join("missing"), dir.join("broken")).unwrap();
        // Un lien vers son propre dossier : le dépliage reste à la demande, donc fini.
        std::os::unix::fs::symlink(&dir, dir.join("loop")).unwrap();

        let mut tree = FileTree::new(dir.clone());
        let rows = tree.rows();
        assert_eq!(
            names(&rows),
            ["erom-store", "loop", "broken", "index-link.md"]
        );
        assert!(rows[0].is_dir && rows[1].is_dir && !rows[2].is_dir && !rows[3].is_dir);

        tree.toggle(&dir.join("erom-store")).unwrap();
        assert_eq!(
            names(&tree.rows()),
            [
                "erom-store",
                "books",
                "INDEX.md",
                "loop",
                "broken",
                "index-link.md"
            ]
        );

        // Les lignes sous le lien gardent le chemin du lien : une seule forme de chemin dans l'arbre.
        assert_eq!(tree.rows()[2].path, dir.join("erom-store/INDEX.md"));
        assert!(behind_link(&dir, &dir.join("erom-store/INDEX.md")));
        assert!(behind_link(&dir, &dir.join("erom-store/books/a.md")));
        assert!(!behind_link(&dir, &dir.join("erom-store")));
        assert!(!behind_link(&dir, &dir.join("index-link.md")));

        // Le lien vers la racine se déplie d'un niveau par geste, deux fois de suite, sans fin de pile.
        tree.toggle(&dir.join("loop")).unwrap();
        tree.toggle(&dir.join("loop/loop")).unwrap();
        let depths: Vec<usize> = tree.rows().iter().map(|row| row.depth).collect();
        assert_eq!(depths.iter().max(), Some(&2));
        assert_eq!(tree.rows().len(), 6 + 4 + 4);

        let mut tree = FileTree::new(dir.clone());
        tree.reveal(&dir.join("erom-store")).unwrap();
        assert!(tree.rows()[0].expanded);

        // Un fichier retiré sous le lien quitte l'arbre à la relecture de son dossier (celle des gestes).
        fs::remove_file(store.join("INDEX.md")).unwrap();
        let outcome = tree.refresh(&[dir.join("erom-store/INDEX.md")]);
        assert!(outcome.changed);
        assert_eq!(names(&tree.rows())[..2], ["erom-store", "books"]);
        assert!(!names(&tree.rows()).contains(&"INDEX.md"));

        // La cible disparaît : à la relecture, le lien cassé redevient un fichier et son contenu part.
        fs::remove_dir_all(&store).unwrap();
        tree.reload();
        let rows = tree.rows();
        assert_eq!(
            names(&rows),
            ["loop", "broken", "erom-store", "index-link.md"]
        );
        assert!(!rows[2].is_dir && !rows[2].expanded);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_renamed_folder_stays_unfolded() {
        let dir = temp_dir("renamed");
        fs::create_dir_all(dir.join("docs/sub")).unwrap();
        fs::create_dir_all(dir.join("docs2")).unwrap();
        fs::write(dir.join("docs/a.md"), b"").unwrap();
        fs::write(dir.join("docs/sub/x.md"), b"").unwrap();
        fs::write(dir.join("docs2/y.md"), b"").unwrap();
        let mut tree = FileTree::new(dir.clone());
        tree.toggle(&dir.join("docs")).unwrap();
        tree.toggle(&dir.join("docs/sub")).unwrap();
        // Un voisin au nom qui commence pareil, lu et déplié : le préfixe se compare par composants.
        tree.toggle(&dir.join("docs2")).unwrap();

        fs::rename(dir.join("docs"), dir.join("notes")).unwrap();
        tree.rename_under(&dir.join("docs"), &dir.join("notes"));
        assert!(
            tree.refresh(&[dir.join("docs"), dir.join("notes")])
                .errors
                .is_empty()
        );

        let rows = tree.rows();
        let shown: Vec<(&Path, usize, bool)> = rows
            .iter()
            .map(|row| (row.path.as_path(), row.depth, row.expanded))
            .collect();
        let expected = [
            (dir.join("docs2"), 0, true),
            (dir.join("docs2/y.md"), 1, false),
            (dir.join("notes"), 0, true),
            (dir.join("notes/sub"), 1, true),
            (dir.join("notes/sub/x.md"), 2, false),
            (dir.join("notes/a.md"), 1, false),
        ];
        let expected: Vec<(&Path, usize, bool)> = expected
            .iter()
            .map(|(path, depth, expanded)| (path.as_path(), *depth, *expanded))
            .collect();
        assert_eq!(shown, expected);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_moved_into_a_folded_parent_is_unfolded_when_that_parent_opens() {
        let dir = temp_dir("moved-into-folded");
        fs::create_dir_all(dir.join("docs/sub")).unwrap();
        fs::create_dir_all(dir.join("seen")).unwrap();
        fs::create_dir_all(dir.join("unseen")).unwrap();
        fs::write(dir.join("docs/sub/x.md"), b"").unwrap();
        fs::write(dir.join("seen/z.md"), b"").unwrap();
        let mut tree = FileTree::new(dir.clone());
        tree.toggle(&dir.join("docs")).unwrap();
        tree.toggle(&dir.join("docs/sub")).unwrap();
        // `seen` a été lu puis replié ; `unseen` n'a jamais été lu.
        tree.toggle(&dir.join("seen")).unwrap();
        tree.toggle(&dir.join("seen")).unwrap();
        assert!(tree.is_loaded(&dir.join("seen")));
        assert!(!tree.is_loaded(&dir.join("unseen")));

        for parent in ["seen", "unseen"] {
            let from = dir.join("docs");
            let to = dir.join(parent).join("docs");
            fs::rename(&from, &to).unwrap();
            tree.rename_under(&from, &to);
            assert!(tree.refresh(&[from.clone(), to.clone()]).errors.is_empty());
            // Replié : le dossier déplacé n'est pas encore à l'écran.
            assert!(!names(&tree.rows()).contains(&"x.md"));
            tree.toggle(&dir.join(parent)).unwrap();
            let rows = tree.rows();
            let moved: Vec<(&Path, usize, bool)> = rows
                .iter()
                .filter(|row| row.path.starts_with(&to))
                .map(|row| (row.path.as_path(), row.depth, row.expanded))
                .collect();
            let sub = to.join("sub");
            let leaf = sub.join("x.md");
            assert_eq!(
                moved,
                [
                    (to.as_path(), 1, true),
                    (sub.as_path(), 2, true),
                    (leaf.as_path(), 3, false)
                ],
                "{parent}"
            );
            // On remet le dossier à sa place pour la passe suivante.
            tree.toggle(&dir.join(parent)).unwrap();
            fs::rename(&to, &from).unwrap();
            tree.rename_under(&to, &from);
            assert!(tree.refresh(&[to, from]).errors.is_empty());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_picks_up_created_and_removed_entries() {
        let dir = temp_dir("refresh");
        fs::create_dir_all(dir.join("docs")).unwrap();
        fs::create_dir_all(dir.join("gone/inner")).unwrap();
        fs::write(dir.join("docs/a.md"), b"").unwrap();

        let mut tree = FileTree::new(dir.clone());
        tree.toggle(&dir.join("docs")).unwrap();
        tree.reveal(&dir.join("gone/inner")).unwrap();
        assert!(names(&tree.rows()).contains(&"inner"));

        let created = dir.join("docs/b.md");
        fs::write(&created, b"").unwrap();
        assert!(tree.refresh(&[created]).errors.is_empty());
        assert!(names(&tree.rows()).contains(&"b.md"));

        fs::remove_dir_all(dir.join("gone")).unwrap();
        assert!(tree.refresh(&[dir.join("gone")]).errors.is_empty());
        let rows = tree.rows();
        assert!(!names(&rows).contains(&"gone") && !names(&rows).contains(&"inner"));
        // Les dossiers dépliés voisins ne sont pas touchés.
        assert!(rows.iter().any(|row| row.name == "docs" && row.expanded));

        // Recréé, le dossier reparaît replié : l'ancien dépliage a été oublié.
        fs::create_dir_all(dir.join("gone")).unwrap();
        assert!(tree.refresh(&[dir.join("gone")]).errors.is_empty());
        let gone = tree
            .rows()
            .into_iter()
            .find(|row| row.name == "gone")
            .unwrap();
        assert!(!gone.expanded);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_rereads_every_loaded_folder_and_keeps_them_unfolded() {
        let dir = temp_dir("reload");
        fs::create_dir_all(dir.join("docs/specs")).unwrap();
        fs::write(dir.join("docs/specs/a.md"), b"").unwrap();

        let mut tree = FileTree::new(dir.clone());
        tree.reveal(&dir.join("docs/specs")).unwrap();
        // Changements que personne n'a signalés : un fichier à la racine, un dans le dossier profond, un dossier perdu.
        fs::write(dir.join("root.md"), b"").unwrap();
        fs::write(dir.join("docs/specs/b.md"), b"").unwrap();
        fs::remove_file(dir.join("docs/specs/a.md")).unwrap();
        assert!(tree.reload().errors.is_empty());

        assert_eq!(
            names(&tree.rows()),
            ["docs", "specs", "b.md", "root.md"],
            "the unfolded folders stay unfolded and show the new content"
        );
        fs::remove_dir_all(dir.join("docs/specs")).unwrap();
        assert!(tree.reload().errors.is_empty());
        assert_eq!(names(&tree.rows()), ["docs", "root.md"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rescan_rereads_the_folder_itself_and_every_loaded_folder_below_it() {
        let dir = temp_dir("rescan");
        fs::create_dir_all(dir.join("docs/specs")).unwrap();
        fs::create_dir_all(dir.join("other")).unwrap();
        let mut tree = FileTree::new(dir.clone());
        tree.reveal(&dir.join("docs/specs")).unwrap();
        tree.toggle(&dir.join("other")).unwrap();
        // Des changements que le watcher n'a pas décrits, à trois profondeurs.
        fs::write(dir.join("root.md"), b"").unwrap();
        fs::write(dir.join("docs/mid.md"), b"").unwrap();
        fs::write(dir.join("docs/specs/deep.md"), b"").unwrap();
        fs::write(dir.join("other/untouched.md"), b"").unwrap();

        // Le débordement porte sur `docs` : lui, son sous-dossier lu et son parent se relisent ; `other` non.
        let outcome = tree.rescan(&[dir.join("docs")]);
        assert!(outcome.changed && outcome.gained_entries && outcome.errors.is_empty());
        assert_eq!(
            names(&tree.rows()),
            ["docs", "specs", "deep.md", "mid.md", "other", "root.md"]
        );
        // `other` est hors du débordement : sa lecture, périmée, n'a pas été touchée.
        assert!(!names(&tree.rows()).contains(&"untouched.md"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_reports_gained_entries_apart_from_other_changes() {
        let dir = temp_dir("signal");
        fs::create_dir_all(dir.join("docs")).unwrap();
        fs::create_dir_all(dir.join("folded")).unwrap();
        fs::write(dir.join("a.md"), b"").unwrap();
        fs::write(dir.join("b.md"), b"").unwrap();
        let mut tree = FileTree::new(dir.clone());
        tree.toggle(&dir.join("docs")).unwrap();

        // Rien n'a changé : ni redessin, ni question à Git.
        let outcome = tree.refresh(&[dir.join("a.md")]);
        assert!(!outcome.changed && !outcome.gained_entries && outcome.errors.is_empty());

        // Un lot sous un dossier replié (jamais lu) ne déclenche rien, même s'il y a du neuf dedans.
        fs::write(dir.join("folded/new.bin"), b"").unwrap();
        let outcome = tree.refresh(&[dir.join("folded/new.bin")]);
        assert!(!outcome.changed && !outcome.gained_entries);

        // Une entrée neuve dans un dossier lu : la racine, puis un dossier déplié.
        fs::write(dir.join("fresh.log"), b"").unwrap();
        let outcome = tree.refresh(&[dir.join("fresh.log")]);
        assert!(outcome.changed && outcome.gained_entries);
        fs::write(dir.join("docs/c.md"), b"").unwrap();
        let outcome = tree.refresh(&[dir.join("docs/c.md")]);
        assert!(outcome.changed && outcome.gained_entries);

        // Seulement des retraits : les lignes changent, Git n'a rien de neuf à dire.
        fs::remove_file(dir.join("b.md")).unwrap();
        let outcome = tree.refresh(&[dir.join("b.md")]);
        assert!(outcome.changed && !outcome.gained_entries);
        let _ = fs::remove_dir_all(&dir);
    }
}
