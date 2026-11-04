//! Le clic droit d'une ligne de la liste de Changes (spec P3 §4.7) et le geste « Ajouter au .gitignore » (§4.8). Les
//! règles du menu sont sans fenêtre ; `Workspace` le dessine (`ChangesEvent::ListMenu`) et rejoue l'entrée choisie sur le
//! volet (`run_changes_gesture`).

use super::diff_view::diff_reference;
use super::*;

use gpui::{ClipboardItem, Point};

use crate::changes::ignore::{self, WRITE_FAILED};
use crate::changes::{Group, Letter};
use crate::files::document::reveal_in_finder;
use crate::files::gestures::{MenuLine, item};
use crate::files::panel::home_dir;
use crate::files::reference::display_path;

/// Ce que vise un clic droit de la liste : un fichier ou un dossier de l'arbre des changements. Le chemin est relatif à la
/// racine du dépôt, comme celui de `Change` ; le volet le complète avec la racine de sa lecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeTarget {
    pub path: PathBuf,
    pub is_dir: bool,
    /// Rien de suivi dessous : « Ajouter au .gitignore » est permis.
    pub untracked: bool,
    /// Rien n'existe plus sur le disque : ni ouvrir ni révéler.
    pub deleted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangesGesture {
    OpenInFiles,
    Reveal,
    CopyPath,
    Ignore,
}

/// Le menu du clic droit : les quatre entrées de la spec, éteintes quand elles n'ont pas de sens (jamais cachées). Une
/// ligne non suivie seule peut être ignorée ; un fichier supprimé n'a rien à ouvrir ni à révéler.
pub fn changes_menu(target: &ChangeTarget) -> Vec<MenuLine<ChangesGesture>> {
    vec![
        item(
            ChangesGesture::OpenInFiles,
            "Ouvrir dans Fichiers",
            None,
            !target.deleted,
        ),
        item(
            ChangesGesture::Reveal,
            "Révéler dans le Finder",
            None,
            !target.deleted,
        ),
        item(ChangesGesture::CopyPath, "Copier le chemin", None, true),
        MenuLine::Separator,
        item(
            ChangesGesture::Ignore,
            "Ajouter au .gitignore",
            None,
            target.untracked,
        ),
    ]
}

/// Ce que propose le menu du clic droit dans le diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffGesture {
    CopyToAgent,
}

/// Le menu du clic droit dans le diff : une seule entrée, « Copier vers l'agent ». Elle exige des lignes à référencer
/// (`reference`, celle que `diff_reference` a construite) et une session pour les recevoir.
pub fn diff_menu(reference: Option<&str>, has_session: bool) -> Vec<MenuLine<DiffGesture>> {
    vec![item(
        DiffGesture::CopyToAgent,
        "Copier vers l'agent",
        Some("⌘⌥C"),
        reference.is_some() && has_session,
    )]
}

/// La cible du clic droit sur la ligne `key`, ou `None` pour un groupe (il n'a pas de menu). Un fichier est non suivi dans
/// le groupe « Non suivis » ; un dossier l'est quand il y est, et que rien de ce qui est dessous n'a un autre statut
/// (un dossier qui porte aussi un fichier modifié est suivi) et que Git ne le dit pas suivi (`tracked_folders` : un dossier
/// suivi dont seuls les fichiers neufs sont listés). Un dossier est supprimé quand tout ce qu'il liste l'est.
pub fn target_of(
    key: &RowKey,
    changes: &[Change],
    tracked_folders: &HashSet<PathBuf>,
) -> Option<ChangeTarget> {
    match key {
        RowKey::Group(_) => None,
        RowKey::File(group, path) => {
            let change = changes
                .iter()
                .find(|change| change.group == *group && change.path == *path)?;
            Some(ChangeTarget {
                path: path.clone(),
                is_dir: false,
                untracked: change.group == Group::Untracked,
                deleted: change.letter == Letter::Deleted,
            })
        }
        RowKey::Folder(group, folder) => {
            let below: Vec<&Change> = changes
                .iter()
                .filter(|change| change.path.starts_with(folder))
                .collect();
            let own: Vec<&Change> = below
                .iter()
                .copied()
                .filter(|change| change.group == *group)
                .collect();
            if own.is_empty() {
                return None;
            }
            Some(ChangeTarget {
                path: folder.clone(),
                is_dir: true,
                untracked: *group == Group::Untracked
                    && below.iter().all(|change| change.group == Group::Untracked)
                    && !tracked_folders.contains(folder),
                deleted: own.iter().all(|change| change.letter == Letter::Deleted),
            })
        }
    }
}

/// Une entrée du menu des cibles : ce qu'il peint et la cible qu'un appui choisit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetItem {
    pub label: String,
    /// Le texte secondaire (le chemin du worktree) ; « working tree » n'en a pas.
    pub detail: Option<String>,
    pub checked: bool,
    pub target: Target,
}

/// Le menu des cibles : « working tree » d'abord, puis un worktree par ligne. `current` est la racine du worktree montré,
/// `None` quand le volet montre le dossier du projet.
pub fn target_menu(lines: &[TargetLine], current: Option<&Path>) -> Vec<TargetItem> {
    let working_tree = TargetItem {
        label: Target::WorkingTree.name().to_string(),
        detail: None,
        checked: current.is_none(),
        target: Target::WorkingTree,
    };
    let worktrees = lines.iter().map(|line| TargetItem {
        label: line.name.clone(),
        detail: Some(line.detail.clone()),
        checked: current == Some(line.root.as_path()),
        target: Target::Worktree {
            root: line.root.clone(),
            name: line.name.clone(),
        },
    });
    std::iter::once(working_tree).chain(worktrees).collect()
}

impl ChangesPanel {
    /// La liste est-elle peinte (donc porteuse du focus) ? Masquée ou vide, le corps du volet garde le focus.
    pub(super) fn list_painted(&self) -> bool {
        !self.list_hidden && !self.rows.is_empty()
    }

    /// Rend le clavier à la liste (à la fermeture d'un menu du volet) ; sans liste peinte, au corps du volet.
    pub fn focus_list(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.list_painted() {
            window.focus(&self.list_focus, cx);
        } else {
            window.focus(&self.focus_handle, cx);
        }
    }

    /// Rend le clavier au diff (à la fermeture de son menu : `⌘⌥C` n'y vaut que là). Un diff qui n'est plus peint (le
    /// fichier est devenu binaire ou vide, la cible a changé) n'a pas de focus à prendre : la liste le reçoit.
    pub fn focus_diff(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.diff.is_painted() {
            self.diff.focus(window, cx);
        } else {
            self.focus_list(window, cx);
        }
    }

    /// Ce que `⌘⌥C` enverrait à l'agent : la référence des lignes nouvelles de la sélection du diff, ou la raison pour
    /// laquelle il n'y en a pas (`no-selection` : aucune sélection de lignes ; `no-new-side` : des lignes retirées seules).
    /// Le chemin est celui du fichier dans la cible : `snapshot.root`, la racine du worktree quand le volet en montre un.
    fn diff_selection_reference(&self, cx: &App) -> Result<String, &'static str> {
        let snapshot = self.snapshot().ok_or("no-selection")?;
        let path = self.diff.path().ok_or("no-selection")?;
        let span = self.diff.selected_span(cx).ok_or("no-selection")?;
        diff_reference(&snapshot.root, path, span, home_dir().as_deref()).ok_or("no-new-side")
    }

    /// `⌘⌥C` et l'entrée du menu du diff : `Workspace` colle la référence dans le pane de l'agent. Sans lignes à
    /// référencer, rien ne part et le journal dit pourquoi.
    pub(super) fn copy_selection_to_agent(&mut self, cx: &mut Context<Self>) {
        match self.diff_selection_reference(cx) {
            Ok(reference) => cx.emit(ChangesEvent::CopyToAgent(reference)),
            Err(reason) => hud::log_event(&format!("changes copy skipped reason={reason}")),
        }
    }

    /// Un appui droit sur le diff : `Workspace` ouvre le menu, avec la référence des lignes choisies (ou `None`). Rien ne
    /// bouge avant le choix d'une entrée ; `Workspace` marque le menu pour que le clavier revienne au diff à sa fermeture.
    pub(super) fn on_diff_menu_requested(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Le corps du volet reprendrait le focus à la remontée de l'appui : le menu l'a déjà pris.
        window.prevent_default();
        let reference = self.diff_selection_reference(cx).ok();
        cx.emit(ChangesEvent::DiffMenu {
            position,
            reference,
        });
    }

    /// Joue l'entrée choisie du menu du diff. La sélection est relue ici : elle a pu changer depuis l'ouverture du menu.
    pub fn run_diff_gesture(&mut self, gesture: DiffGesture, cx: &mut Context<Self>) {
        match gesture {
            DiffGesture::CopyToAgent => self.copy_selection_to_agent(cx),
        }
    }

    /// Un appui droit sur la ligne `key` : la liste prend le focus, un fichier devient le fichier choisi (son diff
    /// s'affiche), puis `Workspace` ouvre le menu. Un groupe n'a pas de menu.
    pub(super) fn on_row_menu_requested(
        &mut self,
        key: &RowKey,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self
            .snapshot()
            .and_then(|snapshot| target_of(key, &snapshot.changes, &snapshot.tracked_folders))
        else {
            return;
        };
        window.focus(&self.list_focus, cx);
        // Le corps du volet reprend le focus à la remontée d'un appui, sauf s'il est empêché : le menu l'a déjà pris.
        window.prevent_default();
        if !target.is_dir {
            self.select(target.path.clone(), cx);
        }
        cx.emit(ChangesEvent::ListMenu { target, position });
    }

    /// Joue l'entrée choisie du menu sur `target`.
    pub fn run_changes_gesture(
        &mut self,
        gesture: ChangesGesture,
        target: &ChangeTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = self.snapshot() else {
            return;
        };
        let root = snapshot.root.clone();
        let absolute = root.join(&target.path);
        match gesture {
            // Comme le double-clic : Fichiers prend la place de Changes. Un chemin qui n'existe plus n'a rien à ouvrir.
            ChangesGesture::OpenInFiles => {
                if absolute.exists() {
                    cx.emit(ChangesEvent::OpenInFiles(absolute));
                }
            }
            ChangesGesture::Reveal => self.reveal(&absolute, window, cx),
            ChangesGesture::CopyPath => {
                let shown = display_path(&absolute, home_dir().as_deref());
                cx.write_to_clipboard(ClipboardItem::new_string(shown));
            }
            ChangesGesture::Ignore => self.ignore(root, target, window, cx),
        }
    }

    /// « Révéler dans le Finder » : `open -R`, hors du fil d'interface ; un échec ouvre l'alerte avec le message brut.
    fn reveal(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let outcome = reveal_in_finder(path);
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        cx.spawn_in(window, async move |_, cx| {
            if let Ok(Err(raw)) = outcome.await {
                let _ = cx.update(|window, cx| {
                    alert(
                        window,
                        cx,
                        &format!("Impossible de révéler {name}."),
                        Some(&raw),
                    );
                });
            }
        })
        .detach();
    }

    /// « Ajouter au .gitignore » : la ligne s'écrit à la racine du dépôt (celle de la lecture), hors du fil d'interface.
    /// Le menu grise l'entrée d'un fichier suivi ; ceci garde le geste.
    fn ignore(
        &mut self,
        root: PathBuf,
        target: &ChangeTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !target.untracked {
            return;
        }
        let (rel, is_dir) = (target.path.clone(), target.is_dir);
        // Le geste, au journal avant l'écriture (le même mot que pour les écritures de Git).
        hud::log_event("changes gesture ignore paths=1");
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { ignore::add_unless_tracked(&root, &rel, is_dir) })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.ignore_done(result, window, cx);
            });
        })
        .detach();
    }

    /// L'écriture est revenue. Réussie, le journal dit la ligne et la liste se relit (le fichier ou le dossier la quitte).
    /// Refusée, l'alerte dit pourquoi, avec le message brut.
    fn ignore_done(
        &mut self,
        result: std::io::Result<ignore::IgnoreOutcome>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(outcome) => {
                outcome.log();
                self.refresh(cx);
            }
            Err(error) => alert(window, cx, WRITE_FAILED, Some(&error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::Check;

    fn target(untracked: bool, deleted: bool) -> ChangeTarget {
        ChangeTarget {
            path: PathBuf::from("captures"),
            is_dir: true,
            untracked,
            deleted,
        }
    }

    /// Les entrées du menu : libellé, geste et état.
    fn entries(target: &ChangeTarget) -> Vec<(&'static str, ChangesGesture, bool)> {
        changes_menu(target)
            .into_iter()
            .filter_map(|line| match line {
                MenuLine::Item(item) => Some((item.label, item.gesture, item.enabled)),
                MenuLine::Separator => None,
            })
            .collect()
    }

    fn enabled(target: &ChangeTarget, gesture: ChangesGesture) -> bool {
        entries(target)
            .into_iter()
            .find(|(_, found, _)| *found == gesture)
            .unwrap()
            .2
    }

    #[test]
    fn changes_menu_entries() {
        // Les quatre entrées, avec les libellés de la spec, dans l'ordre.
        let labels: Vec<&str> = entries(&target(true, false))
            .iter()
            .map(|(label, ..)| *label)
            .collect();
        assert_eq!(
            labels,
            [
                "Ouvrir dans Fichiers",
                "Révéler dans le Finder",
                "Copier le chemin",
                "Ajouter au .gitignore"
            ]
        );
        // « Ajouter au .gitignore » n'est allumé que pour une cible non suivie.
        assert!(enabled(&target(true, false), ChangesGesture::Ignore));
        assert!(!enabled(&target(false, false), ChangesGesture::Ignore));
        // « Révéler dans le Finder » est éteint pour une cible supprimée (comme « Ouvrir dans Fichiers »).
        assert!(enabled(&target(false, false), ChangesGesture::Reveal));
        assert!(!enabled(&target(false, true), ChangesGesture::Reveal));
        assert!(!enabled(&target(false, true), ChangesGesture::OpenInFiles));
        // Copier le chemin vaut toujours, même pour un fichier supprimé : le chemin existe encore pour Git.
        assert!(enabled(&target(false, true), ChangesGesture::CopyPath));
    }

    #[test]
    fn the_diff_menu_needs_a_reference_and_a_session() {
        let state = |reference: Option<&str>, session: bool| match diff_menu(reference, session)
            .as_slice()
        {
            [MenuLine::Item(item)] => (item.gesture, item.label, item.shortcut, item.enabled),
            other => panic!("one entry expected: {other:?}"),
        };
        let (gesture, label, shortcut, _) = state(Some("~/r/README.md:L4"), true);
        assert_eq!(gesture, DiffGesture::CopyToAgent);
        assert_eq!(label, "Copier vers l'agent");
        assert_eq!(shortcut, Some("⌘⌥C"));
        // Allumée pour (Some, true) seulement.
        for (reference, session) in [
            (Some("~/r/README.md:L4"), true),
            (Some("~/r/README.md:L4"), false),
            (None, true),
            (None, false),
        ] {
            assert_eq!(
                state(reference, session).3,
                reference.is_some() && session,
                "{reference:?} {session}"
            );
        }
    }

    fn change(path: &str, group: Group, letter: Letter) -> Change {
        Change {
            path: PathBuf::from(path),
            orig_path: None,
            group,
            letter,
            check: Check::Unchecked,
        }
    }

    fn lines() -> Vec<TargetLine> {
        let line = |name: &str| TargetLine {
            root: PathBuf::from(format!("/r/.worktrees/{name}")),
            name: name.to_string(),
            detail: format!(".worktrees/{name}"),
        };
        vec![line("demo"), line("det")]
    }

    /// Ce que le menu montre de chaque entrée : libellé, texte secondaire, coche.
    fn shown(items: &[TargetItem]) -> Vec<(&str, Option<&str>, bool)> {
        items
            .iter()
            .map(|item| (item.label.as_str(), item.detail.as_deref(), item.checked))
            .collect()
    }

    #[test]
    fn the_target_menu_starts_with_the_working_tree() {
        let items = target_menu(&lines(), None);
        assert_eq!(
            shown(&items),
            [
                ("working tree", None, true),
                ("demo", Some(".worktrees/demo"), false),
                ("det", Some(".worktrees/det"), false),
            ]
        );
        assert_eq!(items[0].target, Target::WorkingTree);
        assert_eq!(
            items[1].target,
            Target::Worktree {
                root: PathBuf::from("/r/.worktrees/demo"),
                name: "demo".to_string()
            }
        );

        // Un worktree choisi : seul lui est coché.
        let items = target_menu(&lines(), Some(Path::new("/r/.worktrees/demo")));
        assert_eq!(
            shown(&items),
            [
                ("working tree", None, false),
                ("demo", Some(".worktrees/demo"), true),
                ("det", Some(".worktrees/det"), false),
            ]
        );
    }

    #[test]
    fn a_file_target_follows_its_group_and_letter() {
        let changes = [
            change("README.md", Group::Modified, Letter::Modified),
            change("old.txt", Group::Modified, Letter::Deleted),
            change("new.md", Group::Untracked, Letter::Untracked),
        ];
        let of = |group, path: &str| {
            target_of(
                &RowKey::File(group, PathBuf::from(path)),
                &changes,
                &HashSet::new(),
            )
        };

        let readme = of(Group::Modified, "README.md").unwrap();
        assert!(!readme.untracked && !readme.deleted && !readme.is_dir);
        assert!(of(Group::Modified, "old.txt").unwrap().deleted);
        assert!(of(Group::Untracked, "new.md").unwrap().untracked);
        // Une ligne qui n'est plus dans la lecture n'a pas de cible.
        assert_eq!(of(Group::Modified, "gone.md"), None);
        // Un groupe n'a pas de menu.
        assert_eq!(
            target_of(&RowKey::Group(Group::Modified), &changes, &HashSet::new()),
            None
        );
    }

    #[test]
    fn a_folder_is_untracked_only_when_nothing_below_is_tracked() {
        let changes = [
            change("captures/a.png", Group::Untracked, Letter::Untracked),
            change("captures/b.png", Group::Untracked, Letter::Untracked),
            change("notes/new.md", Group::Untracked, Letter::Untracked),
            change("notes/old.md", Group::Modified, Letter::Modified),
            change("gone/x.md", Group::Modified, Letter::Deleted),
            change("gone/y.md", Group::Modified, Letter::Deleted),
        ];
        let folder = |group, path: &str| {
            target_of(
                &RowKey::Folder(group, PathBuf::from(path)),
                &changes,
                &HashSet::new(),
            )
            .unwrap()
        };

        let captures = folder(Group::Untracked, "captures");
        assert!(captures.is_dir && captures.untracked && !captures.deleted);
        // `notes` porte aussi un fichier modifié, donc suivi : la ligne « Non suivis » du dossier n'est pas ignorable.
        assert!(!folder(Group::Untracked, "notes").untracked);
        assert!(!folder(Group::Modified, "notes").untracked);
        // Un dossier dont tout ce qui est listé est supprimé n'a rien à révéler.
        assert!(folder(Group::Modified, "gone").deleted);
        assert!(!folder(Group::Modified, "notes").deleted);
        // Un dossier sans ligne dans ce groupe n'a pas de cible.
        assert_eq!(
            target_of(
                &RowKey::Folder(Group::Committed, PathBuf::from("captures")),
                &changes,
                &HashSet::new()
            ),
            None
        );
    }

    #[test]
    fn a_tracked_folder_gaining_a_new_file_is_not_an_untracked_folder() {
        use crate::changes::model;
        use crate::changes::test_repo::TempRepo;
        // `notes/old.md` est commité, `notes/new.md` est neuf : le statut ne liste que ce dernier, et la ligne du dossier
        // sort de la liste « Non suivis » sans rien d'autre dessous. C'est Git qui dit que `notes` est suivi.
        let repo = TempRepo::init();
        repo.write("notes/old.md", b"a");
        repo.write("captures/a.png", b"x");
        repo.write("deep/tracked/x.md", b"x");
        repo.git(&["add", "notes/old.md", "deep/tracked/x.md"]);
        repo.git(&["commit", "--quiet", "-m", "init"]);
        repo.write("notes/new.md", b"b");
        repo.write("deep/tracked/sub/y.md", b"y");
        let snapshot = model::load(repo.path()).unwrap();
        let folder = |group, path: &str| {
            target_of(
                &RowKey::Folder(group, PathBuf::from(path)),
                &snapshot.changes,
                &snapshot.tracked_folders,
            )
            .unwrap()
        };
        let file = |path: &str| {
            target_of(
                &RowKey::File(Group::Untracked, PathBuf::from(path)),
                &snapshot.changes,
                &snapshot.tracked_folders,
            )
            .unwrap()
        };

        // Le dossier suivi n'est pas ignorable, le fichier neuf qu'il porte l'est.
        assert!(!folder(Group::Untracked, "notes").untracked);
        assert!(file("notes/new.md").untracked);
        // Un dossier neuf sous un dossier suivi l'est (rien de suivi dedans), un dossier tout neuf aussi.
        assert!(folder(Group::Untracked, "deep/tracked/sub").untracked);
        assert!(folder(Group::Untracked, "captures").untracked);
    }
}
