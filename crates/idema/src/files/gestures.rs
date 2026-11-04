//! Les gestes de l'arbre et de l'éditeur, sans fenêtre : ce que chaque menu contextuel propose, ce qui est grisé, et
//! où tombe un dépôt (spec P2 §4.3). La vue traduit ces lignes en menu ; les règles se testent ici.

use std::path::{Path, PathBuf};

use super::tree::Row;

/// Ce qu'un clic droit ou un geste vise dans l'arbre : la racine du projet (zone vide, en-tête) ou une entrée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeTarget {
    Root,
    Entry { path: PathBuf, is_dir: bool },
}

impl TreeTarget {
    /// Le dossier qui reçoit une création : la racine, le dossier visé, ou le parent d'un fichier.
    pub fn dir(&self, root: &Path) -> PathBuf {
        match self {
            TreeTarget::Root => root.to_path_buf(),
            TreeTarget::Entry { path, is_dir: true } => path.clone(),
            TreeTarget::Entry {
                path,
                is_dir: false,
            } => path
                .parent()
                .map_or_else(|| root.to_path_buf(), Path::to_path_buf),
        }
    }

    /// Le chemin visé : la racine, ou l'entrée.
    pub fn path<'a>(&'a self, root: &'a Path) -> &'a Path {
        match self {
            TreeTarget::Root => root,
            TreeTarget::Entry { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeGesture {
    NewFile,
    NewFolder,
    Rename,
    Trash,
    OpenDefault,
    Reveal,
    /// « Ajouter au .gitignore » (spec P3 §4.8).
    Ignore,
    CopyPath,
    CopyToAgent,
}

/// Ce que Git dit d'une entrée de l'arbre : suivie (un fichier de l'index, ou un dossier qui en contient un), ou déjà
/// ignorée (elle, ou un dossier au-dessus).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryGit {
    pub tracked: bool,
    pub ignored: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorGesture {
    CopyToAgent,
    Cut,
    Copy,
    Paste,
    SelectAll,
}

/// Une entrée de menu : le geste, son libellé exact, son raccourci affiché et s'il est permis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GestureItem<G> {
    pub gesture: G,
    pub label: &'static str,
    pub shortcut: Option<&'static str>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuLine<G> {
    Item(GestureItem<G>),
    Separator,
}

pub(crate) fn item<G>(
    gesture: G,
    label: &'static str,
    shortcut: Option<&'static str>,
    enabled: bool,
) -> MenuLine<G> {
    MenuLine::Item(GestureItem {
        gesture,
        label,
        shortcut,
        enabled,
    })
}

/// Le menu contextuel de l'arbre. La racine ne se renomme ni ne se supprime ; un dossier ne s'ouvre pas dans l'app par
/// défaut (E48) ; copier vers l'agent demande une session ; « Ajouter au .gitignore » ne vaut que pour une entrée que Git
/// ne suit pas et n'ignore pas déjà (jamais la racine).
pub fn tree_menu(
    target: &TreeTarget,
    agent_available: bool,
    git: EntryGit,
) -> Vec<MenuLine<TreeGesture>> {
    let is_root = matches!(target, TreeTarget::Root);
    let is_file = matches!(target, TreeTarget::Entry { is_dir: false, .. });
    let can_ignore = !is_root && !git.tracked && !git.ignored;
    vec![
        item(TreeGesture::NewFile, "Nouveau fichier", None, true),
        item(TreeGesture::NewFolder, "Nouveau dossier", None, true),
        MenuLine::Separator,
        item(TreeGesture::Rename, "Renommer", Some("↩"), !is_root),
        item(TreeGesture::Trash, "Supprimer", Some("⌘⌫"), !is_root),
        MenuLine::Separator,
        item(
            TreeGesture::OpenDefault,
            "Ouvrir dans l'app par défaut",
            None,
            is_file,
        ),
        item(TreeGesture::Reveal, "Révéler dans le Finder", None, true),
        item(
            TreeGesture::Ignore,
            "Ajouter au .gitignore",
            None,
            can_ignore,
        ),
        MenuLine::Separator,
        item(TreeGesture::CopyPath, "Copier le chemin", None, true),
        item(
            TreeGesture::CopyToAgent,
            "Copier vers l'agent",
            None,
            agent_available,
        ),
    ]
}

/// Le menu contextuel de l'éditeur. Couper et Copier exigent une sélection ; copier vers l'agent, une session.
pub fn editor_menu(has_selection: bool, agent_available: bool) -> Vec<MenuLine<EditorGesture>> {
    vec![
        item(
            EditorGesture::CopyToAgent,
            "Copier vers l'agent",
            Some("⌘⌥C"),
            agent_available,
        ),
        MenuLine::Separator,
        item(EditorGesture::Cut, "Couper", Some("⌘X"), has_selection),
        item(EditorGesture::Copy, "Copier", Some("⌘C"), has_selection),
        item(EditorGesture::Paste, "Coller", Some("⌘V"), true),
        MenuLine::Separator,
        item(
            EditorGesture::SelectAll,
            "Tout sélectionner",
            Some("⌘A"),
            true,
        ),
    ]
}

/// Le dossier qui reçoit un dépôt (E47) : le dossier survolé, le dossier d'un fichier survolé, ou la racine.
pub fn drop_dir(hovered: Option<&Row>, root: &Path) -> PathBuf {
    match hovered {
        Some(row) => TreeTarget::Entry {
            path: row.path.clone(),
            is_dir: row.is_dir,
        }
        .dir(root),
        None => root.to_path_buf(),
    }
}

/// `row` est-elle dans la zone de dépôt de `target` : le dossier lui-même ou ce qui est dessous ?
pub fn in_drop_region(row: &Path, target: &Path) -> bool {
    row.starts_with(target)
}

/// Une ligne de la zone de dépôt est-elle au bord haut, au bord bas de la zone ? Les lignes voisines (`prev`, `next`)
/// disent si la zone continue : seul un bord arrondit ses coins, le reste est un rectangle d'un seul tenant.
pub fn region_edges(prev: Option<&Path>, next: Option<&Path>, target: &Path) -> (bool, bool) {
    let continues = |row: Option<&Path>| row.is_some_and(|row| in_drop_region(row, target));
    (!continues(prev), !continues(next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_outer_rows_of_a_drop_region_are_edges() {
        let target = Path::new("/p/notes");
        let rows = [
            Path::new("/p/a.md"),
            Path::new("/p/notes"),
            Path::new("/p/notes/sub"),
            Path::new("/p/notes/x.md"),
            Path::new("/p/z.md"),
        ];
        let edges = |index: usize| {
            let prev = index.checked_sub(1).map(|i| rows[i]);
            region_edges(prev, rows.get(index + 1).copied(), target)
        };
        assert_eq!(edges(1), (true, false));
        assert_eq!(edges(2), (false, false));
        assert_eq!(edges(3), (false, true));
        // Une zone d'une seule ligne est son propre haut et son propre bas.
        assert_eq!(region_edges(None, None, Path::new("/p")), (true, true));
    }

    fn items<G: Copy>(menu: &[MenuLine<G>]) -> Vec<&GestureItem<G>> {
        menu.iter()
            .filter_map(|line| match line {
                MenuLine::Item(item) => Some(item),
                MenuLine::Separator => None,
            })
            .collect()
    }

    fn enabled(menu: &[MenuLine<TreeGesture>], gesture: TreeGesture) -> bool {
        items(menu)
            .into_iter()
            .find(|item| item.gesture == gesture)
            .unwrap()
            .enabled
    }

    /// Une entrée que Git ne suit pas et n'ignore pas : tout lui est permis.
    const UNTRACKED: EntryGit = EntryGit {
        tracked: false,
        ignored: false,
    };

    fn file(path: &str) -> TreeTarget {
        TreeTarget::Entry {
            path: PathBuf::from(path),
            is_dir: false,
        }
    }

    fn folder(path: &str) -> TreeTarget {
        TreeTarget::Entry {
            path: PathBuf::from(path),
            is_dir: true,
        }
    }

    fn row(path: &str, is_dir: bool) -> Row {
        Row {
            path: PathBuf::from(path),
            name: path.rsplit('/').next().unwrap().to_owned(),
            depth: 0,
            is_dir,
            expanded: false,
            ignored: false,
        }
    }

    #[test]
    fn the_root_menu_disables_rename_trash_and_open() {
        let menu = tree_menu(&TreeTarget::Root, true, UNTRACKED);
        assert!(!enabled(&menu, TreeGesture::Rename));
        assert!(!enabled(&menu, TreeGesture::Trash));
        assert!(!enabled(&menu, TreeGesture::OpenDefault));
        // Ignorer la racine n'a pas de sens, même quand Git ne la suit ni ne l'ignore.
        assert!(!enabled(&menu, TreeGesture::Ignore));
        // Créer, révéler, copier le chemin et copier vers l'agent restent permis sur la racine.
        for gesture in [
            TreeGesture::NewFile,
            TreeGesture::NewFolder,
            TreeGesture::Reveal,
            TreeGesture::CopyPath,
            TreeGesture::CopyToAgent,
        ] {
            assert!(enabled(&menu, gesture), "{gesture:?}");
        }
        // Une entrée ordinaire a tout permis.
        let menu = tree_menu(&file("/p/a.md"), true, UNTRACKED);
        assert!(items(&menu).iter().all(|item| item.enabled));
    }

    #[test]
    fn the_menu_shows_the_exact_labels_groups_and_shortcuts() {
        let menu = tree_menu(&file("/p/a.md"), true, UNTRACKED);
        let shown: Vec<Option<(&str, Option<&str>)>> = menu
            .iter()
            .map(|line| match line {
                MenuLine::Item(item) => Some((item.label, item.shortcut)),
                MenuLine::Separator => None,
            })
            .collect();
        assert_eq!(
            shown,
            [
                Some(("Nouveau fichier", None)),
                Some(("Nouveau dossier", None)),
                None,
                Some(("Renommer", Some("↩"))),
                Some(("Supprimer", Some("⌘⌫"))),
                None,
                Some(("Ouvrir dans l'app par défaut", None)),
                Some(("Révéler dans le Finder", None)),
                Some(("Ajouter au .gitignore", None)),
                None,
                Some(("Copier le chemin", None)),
                Some(("Copier vers l'agent", None)),
            ]
        );
    }

    #[test]
    fn tree_menu_offers_ignore_only_for_untracked_entries() {
        let ignore = |target: &TreeTarget, git: EntryGit| {
            enabled(&tree_menu(target, true, git), TreeGesture::Ignore)
        };
        let tracked = EntryGit {
            tracked: true,
            ignored: false,
        };
        let ignored = EntryGit {
            tracked: false,
            ignored: true,
        };
        // Ni suivie ni ignorée : allumée, pour un fichier comme pour un dossier.
        assert!(ignore(&file("/p/notes/a.md"), UNTRACKED));
        assert!(ignore(&folder("/p/captures"), UNTRACKED));
        // Suivie (README.md), ou déjà ignorée : éteinte.
        assert!(!ignore(&file("/p/README.md"), tracked));
        assert!(!ignore(&folder("/p/src"), tracked));
        assert!(!ignore(&file("/p/target/x"), ignored));
        // La racine : éteinte.
        assert!(!ignore(&TreeTarget::Root, UNTRACKED));
        // L'entrée existe toujours : éteinte, jamais cachée.
        assert!(tree_menu(&file("/p/README.md"), true, tracked).iter().any(
            |line| matches!(line, MenuLine::Item(item) if item.gesture == TreeGesture::Ignore)
        ));
    }

    #[test]
    fn a_folder_cannot_be_opened_in_the_default_app() {
        let menu = tree_menu(&folder("/p/notes"), true, UNTRACKED);
        assert!(!enabled(&menu, TreeGesture::OpenDefault));
        // Renommer et supprimer un dossier restent permis.
        assert!(enabled(&menu, TreeGesture::Rename));
        assert!(enabled(&menu, TreeGesture::Trash));
        assert!(enabled(
            &tree_menu(&file("/p/a.md"), true, UNTRACKED),
            TreeGesture::OpenDefault
        ));
    }

    #[test]
    fn copy_to_agent_needs_a_session() {
        for target in [TreeTarget::Root, file("/p/a.md"), folder("/p/notes")] {
            assert!(!enabled(
                &tree_menu(&target, false, UNTRACKED),
                TreeGesture::CopyToAgent
            ));
            assert!(enabled(
                &tree_menu(&target, true, UNTRACKED),
                TreeGesture::CopyToAgent
            ));
        }
        let menu = editor_menu(true, false);
        let item = items(&menu)[0];
        assert_eq!(item.gesture, EditorGesture::CopyToAgent);
        assert!(!item.enabled);
        assert!(items(&editor_menu(true, true))[0].enabled);
    }

    #[test]
    fn cut_and_copy_need_a_selection() {
        let state = |menu: &[MenuLine<EditorGesture>], gesture| {
            items(menu)
                .into_iter()
                .find(|item| item.gesture == gesture)
                .unwrap()
                .enabled
        };
        let without = editor_menu(false, true);
        assert!(!state(&without, EditorGesture::Cut));
        assert!(!state(&without, EditorGesture::Copy));
        assert!(state(&without, EditorGesture::Paste));
        assert!(state(&without, EditorGesture::SelectAll));
        let with = editor_menu(true, true);
        assert!(state(&with, EditorGesture::Cut));
        assert!(state(&with, EditorGesture::Copy));
        // Libellés, groupes et raccourcis.
        let shown: Vec<Option<(&str, Option<&str>)>> = with
            .iter()
            .map(|line| match line {
                MenuLine::Item(item) => Some((item.label, item.shortcut)),
                MenuLine::Separator => None,
            })
            .collect();
        assert_eq!(
            shown,
            [
                Some(("Copier vers l'agent", Some("⌘⌥C"))),
                None,
                Some(("Couper", Some("⌘X"))),
                Some(("Copier", Some("⌘C"))),
                Some(("Coller", Some("⌘V"))),
                None,
                Some(("Tout sélectionner", Some("⌘A"))),
            ]
        );
    }

    #[test]
    fn a_creation_lands_in_the_folder_the_target_designates() {
        let root = Path::new("/p");
        assert_eq!(TreeTarget::Root.dir(root), PathBuf::from("/p"));
        assert_eq!(folder("/p/notes").dir(root), PathBuf::from("/p/notes"));
        assert_eq!(file("/p/notes/a.md").dir(root), PathBuf::from("/p/notes"));
        assert_eq!(TreeTarget::Root.path(root), root);
        assert_eq!(file("/p/a.md").path(root), Path::new("/p/a.md"));
    }

    #[test]
    fn a_file_row_drops_into_its_folder() {
        let root = Path::new("/p");
        assert_eq!(
            drop_dir(Some(&row("/p/a.md", false)), root),
            PathBuf::from("/p")
        );
        assert_eq!(
            drop_dir(Some(&row("/p/notes/b.md", false)), root),
            PathBuf::from("/p/notes")
        );
        assert_eq!(
            drop_dir(Some(&row("/p/notes", true)), root),
            PathBuf::from("/p/notes")
        );
        assert_eq!(drop_dir(None, root), PathBuf::from("/p"));
    }

    #[test]
    fn the_drop_region_is_the_folder_and_what_is_below() {
        let target = Path::new("/p/notes");
        assert!(in_drop_region(Path::new("/p/notes"), target));
        assert!(in_drop_region(Path::new("/p/notes/a.md"), target));
        assert!(in_drop_region(Path::new("/p/notes/deep/b.md"), target));
        // Un dossier voisin au nom qui commence pareil n'est pas dans la zone.
        assert!(!in_drop_region(Path::new("/p/notes2"), target));
        assert!(!in_drop_region(Path::new("/p/other.md"), target));
    }
}
