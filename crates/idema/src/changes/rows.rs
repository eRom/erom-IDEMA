//! L'arbre de la liste : les changements aplatis en lignes, groupes puis dossiers compactés puis fichiers.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{Change, Check, Group};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowKey {
    Group(Group),
    Folder(Group, PathBuf),
    File(Group, PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub key: RowKey,
    pub depth: usize,
    pub label: String,
    pub check: Check,
    /// L'index du changement, pour une ligne de fichier.
    pub change: Option<usize>,
}

/// L'état de case d'un dossier ou d'un groupe : tout coché, rien de coché, ou un mélange. Les lignes sans case ne
/// comptent pas, et un ensemble sans aucune case n'en a pas.
pub fn aggregate(checks: impl Iterator<Item = Check>) -> Check {
    let (mut checked, mut unchecked, mut mixed) = (0usize, 0usize, 0usize);
    for check in checks {
        match check {
            Check::Checked => checked += 1,
            Check::Unchecked => unchecked += 1,
            Check::Mixed => mixed += 1,
            Check::NoBox => {}
        }
    }
    if checked + unchecked + mixed == 0 {
        Check::NoBox
    } else if mixed > 0 || (checked > 0 && unchecked > 0) {
        Check::Mixed
    } else if checked > 0 {
        Check::Checked
    } else {
        Check::Unchecked
    }
}

/// Ce qu'un clic sur une case fait à l'index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckAction {
    Stage,
    Unstage,
}

/// Le geste d'un clic sur une case dans l'état `check`. Une case mixte ou vide coche tout ; une case cochée décoche ; une
/// ligne sans case ne fait rien.
pub fn next_action(check: Check) -> Option<CheckAction> {
    match check {
        Check::Checked => Some(CheckAction::Unstage),
        Check::Unchecked | Check::Mixed => Some(CheckAction::Stage),
        Check::NoBox => None,
    }
}

/// Les changements (par leur index) sur lesquels agit la case de la ligne `key` : le fichier, ou tous ceux qui sont sous
/// le dossier ou dans le groupe, de ce groupe-là seulement (un dossier peut avoir des fichiers modifiés et des fichiers
/// non suivis, chacun sous sa ligne). Les changements sans case n'en font pas partie.
pub fn row_targets(key: &RowKey, changes: &[Change]) -> Vec<usize> {
    changes
        .iter()
        .enumerate()
        .filter(|(_, change)| {
            change.check != Check::NoBox
                && match key {
                    RowKey::Group(group) => change.group == *group,
                    RowKey::Folder(group, folder) => {
                        change.group == *group && change.path.starts_with(folder)
                    }
                    RowKey::File(group, path) => change.group == *group && change.path == *path,
                }
        })
        .map(|(index, _)| index)
        .collect()
}

/// Les chemins que Git reçoit pour ces `targets`. Décocher un renommage passe l'ancien chemin comme le nouveau : avec le
/// seul nouveau, l'ancien resterait supprimé dans l'index et le nouveau apparaîtrait non suivi (R6).
pub fn write_paths(action: CheckAction, changes: &[Change], targets: &[usize]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for &index in targets {
        let change = &changes[index];
        paths.push(change.path.clone());
        if action == CheckAction::Unstage
            && let Some(original) = &change.orig_path
        {
            paths.push(original.clone());
        }
    }
    paths
}

/// Un maillon de l'arbre d'un groupe : un dossier (sans `change`) ou un fichier.
struct Node {
    name: OsString,
    children: Vec<Node>,
    change: Option<usize>,
}

impl Node {
    fn folder(name: OsString) -> Node {
        Node {
            name,
            children: Vec::new(),
            change: None,
        }
    }

    fn is_folder(&self) -> bool {
        self.change.is_none()
    }

    fn insert(&mut self, components: &[OsString], change: usize) {
        let Some((first, rest)) = components.split_first() else {
            return;
        };
        if rest.is_empty() {
            self.children.push(Node {
                name: first.clone(),
                children: Vec::new(),
                change: Some(change),
            });
            return;
        }
        let existing = self
            .children
            .iter()
            .position(|child| child.is_folder() && child.name == *first);
        let index = existing.unwrap_or_else(|| {
            self.children.push(Node::folder(first.clone()));
            self.children.len() - 1
        });
        self.children[index].insert(rest, change);
    }

    /// Les dossiers d'abord, puis les noms sans tenir compte de la casse, comme l'arbre de Fichiers.
    fn sort(&mut self) {
        self.children.sort_by_cached_key(|child| {
            (
                !child.is_folder(),
                child.name.to_string_lossy().to_lowercase(),
                child.name.clone(),
            )
        });
        for child in &mut self.children {
            child.sort();
        }
    }

    fn collect_checks(&self, changes: &[Change], checks: &mut Vec<Check>) {
        if let Some(index) = self.change {
            checks.push(changes[index].check);
        }
        for child in &self.children {
            child.collect_checks(changes, checks);
        }
    }
}

fn group_label(group: Group, branch: Option<&str>) -> String {
    match group {
        Group::Conflicts => "Conflits".to_string(),
        Group::Modified => "Modifiés".to_string(),
        Group::Untracked => "Non suivis".to_string(),
        Group::Committed => match branch {
            Some(name) => format!("Commités sur {name}"),
            None => "Commités sur HEAD détachée".to_string(),
        },
    }
}

/// Aplatit les changements en lignes : une ligne par groupe non vide, puis son arbre. Les dossiers à un seul enfant
/// dossier se compactent (`skills/yt`). Un groupe ou un dossier replié (`collapsed`) garde sa ligne et son agrégat, et
/// cache ses enfants.
pub fn build(changes: &[Change], collapsed: &HashSet<RowKey>, branch: Option<&str>) -> Vec<Row> {
    let mut rows = Vec::new();
    for group in [
        Group::Conflicts,
        Group::Modified,
        Group::Untracked,
        Group::Committed,
    ] {
        let mut root = Node::folder(OsString::new());
        let mut any = false;
        for (index, change) in changes.iter().enumerate().filter(|(_, c)| c.group == group) {
            let components: Vec<OsString> = change
                .path
                .components()
                .map(|component| component.as_os_str().to_os_string())
                .collect();
            root.insert(&components, index);
            any = true;
        }
        if !any {
            continue;
        }
        root.sort();
        let mut checks = Vec::new();
        root.collect_checks(changes, &mut checks);
        let key = RowKey::Group(group);
        let is_collapsed = collapsed.contains(&key);
        rows.push(Row {
            key,
            depth: 0,
            label: group_label(group, branch),
            check: aggregate(checks.into_iter()),
            change: None,
        });
        if !is_collapsed {
            emit_children(
                &root,
                Path::new(""),
                group,
                1,
                changes,
                collapsed,
                &mut rows,
            );
        }
    }
    rows
}

fn emit_children(
    parent: &Node,
    parent_path: &Path,
    group: Group,
    depth: usize,
    changes: &[Change],
    collapsed: &HashSet<RowKey>,
    rows: &mut Vec<Row>,
) {
    for child in &parent.children {
        if let Some(index) = child.change {
            let change = &changes[index];
            rows.push(Row {
                key: RowKey::File(group, change.path.clone()),
                depth,
                label: child.name.to_string_lossy().into_owned(),
                check: change.check,
                change: Some(index),
            });
            continue;
        }
        // Compacte la chaîne de dossiers qui n'ont qu'un dossier pour enfant.
        let mut node = child;
        let mut relative = PathBuf::from(&child.name);
        while let [only] = node.children.as_slice() {
            if !only.is_folder() {
                break;
            }
            relative.push(&only.name);
            node = only;
        }
        let path = parent_path.join(&relative);
        let mut checks = Vec::new();
        node.collect_checks(changes, &mut checks);
        let key = RowKey::Folder(group, path.clone());
        let is_collapsed = collapsed.contains(&key);
        rows.push(Row {
            key,
            depth,
            label: relative.to_string_lossy().into_owned(),
            check: aggregate(checks.into_iter()),
            change: None,
        });
        if !is_collapsed {
            emit_children(node, &path, group, depth + 1, changes, collapsed, rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::Letter;

    fn change(path: &str, group: Group, check: Check) -> Change {
        Change {
            path: PathBuf::from(path),
            orig_path: None,
            group,
            letter: Letter::Untracked,
            check,
        }
    }

    fn shape(rows: &[Row]) -> Vec<(usize, &str)> {
        rows.iter()
            .map(|row| (row.depth, row.label.as_str()))
            .collect()
    }

    #[test]
    fn rows_compact_single_child_folders() {
        let changes = [
            change(
                "skills/yt/references/AI.md",
                Group::Untracked,
                Check::Unchecked,
            ),
            change("skills/yt/SKILL.md", Group::Untracked, Check::Unchecked),
        ];
        let rows = build(&changes, &HashSet::new(), Some("main"));
        assert_eq!(
            shape(&rows),
            [
                (0, "Non suivis"),
                (1, "skills/yt"),
                (2, "references"),
                (3, "AI.md"),
                (2, "SKILL.md"),
            ]
        );
        assert_eq!(
            rows[1].key,
            RowKey::Folder(Group::Untracked, PathBuf::from("skills/yt"))
        );
        assert_eq!(
            rows[3].key,
            RowKey::File(
                Group::Untracked,
                PathBuf::from("skills/yt/references/AI.md")
            )
        );
        assert_eq!(rows[3].change, Some(0));
        assert_eq!(rows[4].change, Some(1));
        assert_eq!(rows[0].change, None);
    }

    #[test]
    fn rows_sort_folders_first_then_names_without_case() {
        let changes = [
            change("b.md", Group::Modified, Check::Unchecked),
            change("Zed.md", Group::Modified, Check::Unchecked),
            change("a.md", Group::Modified, Check::Unchecked),
            change("dir/x.md", Group::Modified, Check::Unchecked),
            change("Dir2/y.md", Group::Modified, Check::Unchecked),
        ];
        let rows = build(&changes, &HashSet::new(), None);
        let labels: Vec<&str> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Modifiés",
                "dir",
                "x.md",
                "Dir2",
                "y.md",
                "a.md",
                "b.md",
                "Zed.md"
            ]
        );
    }

    #[test]
    fn groups_come_in_display_order_and_empty_ones_are_absent() {
        let changes = [
            change("t.md", Group::Untracked, Check::Unchecked),
            change("c.md", Group::Conflicts, Check::NoBox),
            change("m.md", Group::Modified, Check::Checked),
        ];
        let rows = build(&changes, &HashSet::new(), None);
        let labels: Vec<&str> = rows
            .iter()
            .filter(|row| row.depth == 0)
            .map(|row| row.label.as_str())
            .collect();
        assert_eq!(labels, ["Conflits", "Modifiés", "Non suivis"]);
    }

    #[test]
    fn a_folder_mixes_its_children() {
        let changes = [
            change("d/a.md", Group::Modified, Check::Checked),
            change("d/b.md", Group::Modified, Check::Unchecked),
        ];
        let rows = build(&changes, &HashSet::new(), None);
        assert_eq!(rows[0].check, Check::Mixed);
        assert_eq!(rows[1].label, "d");
        assert_eq!(rows[1].check, Check::Mixed);

        let folder = RowKey::Folder(Group::Modified, PathBuf::from("d"));
        let collapsed = HashSet::from([folder]);
        let rows = build(&changes, &collapsed, None);
        assert_eq!(shape(&rows), [(0, "Modifiés"), (1, "d")]);
        assert_eq!(rows[1].check, Check::Mixed);

        let group_collapsed = HashSet::from([RowKey::Group(Group::Modified)]);
        let rows = build(&changes, &group_collapsed, None);
        assert_eq!(shape(&rows), [(0, "Modifiés")]);
        assert_eq!(rows[0].check, Check::Mixed);
    }

    #[test]
    fn aggregate_follows_the_rules() {
        use Check::*;
        assert_eq!(aggregate([Checked, Checked].into_iter()), Checked);
        assert_eq!(aggregate([Unchecked, Unchecked].into_iter()), Unchecked);
        assert_eq!(aggregate([Checked, Unchecked].into_iter()), Mixed);
        assert_eq!(aggregate([Mixed, Unchecked].into_iter()), Mixed);
        assert_eq!(aggregate([Mixed].into_iter()), Mixed);
        assert_eq!(aggregate([NoBox, Checked].into_iter()), Checked);
        assert_eq!(aggregate([NoBox, NoBox].into_iter()), NoBox);
        assert_eq!(aggregate(std::iter::empty()), NoBox);
    }

    #[test]
    fn clicking_a_mixed_box_stages() {
        assert_eq!(next_action(Check::Mixed), Some(CheckAction::Stage));
        assert_eq!(next_action(Check::Unchecked), Some(CheckAction::Stage));
        assert_eq!(next_action(Check::Checked), Some(CheckAction::Unstage));
        assert_eq!(next_action(Check::NoBox), None);
    }

    #[test]
    fn a_box_acts_on_its_own_group_and_folder_only() {
        let changes = [
            change("notes/a.md", Group::Modified, Check::Unchecked),
            change("notes/b.md", Group::Untracked, Check::Unchecked),
            change("notes2/c.md", Group::Untracked, Check::Unchecked),
            change("notes/sub/d.md", Group::Untracked, Check::Checked),
            change("conflict.md", Group::Conflicts, Check::NoBox),
            change("old.md", Group::Committed, Check::NoBox),
        ];
        let folder = |group, path: &str| row_targets(&RowKey::Folder(group, path.into()), &changes);
        // Le dossier du groupe « Non suivis » ne touche ni le fichier modifié de `notes`, ni `notes2`.
        assert_eq!(folder(Group::Untracked, "notes"), [1, 3]);
        assert_eq!(folder(Group::Modified, "notes"), [0]);
        assert_eq!(
            row_targets(&RowKey::Group(Group::Untracked), &changes),
            [1, 2, 3]
        );
        assert_eq!(
            row_targets(
                &RowKey::File(Group::Modified, "notes/a.md".into()),
                &changes
            ),
            [0]
        );
        // Un groupe sans case ne cible rien.
        assert!(row_targets(&RowKey::Group(Group::Conflicts), &changes).is_empty());
        assert!(row_targets(&RowKey::Group(Group::Committed), &changes).is_empty());
    }

    #[test]
    fn unstaging_a_rename_passes_both_paths() {
        let mut renamed = change("new name.md", Group::Modified, Check::Checked);
        renamed.orig_path = Some(PathBuf::from("old.md"));
        let changes = [renamed, change("b.md", Group::Modified, Check::Checked)];
        let targets = row_targets(&RowKey::Group(Group::Modified), &changes);
        assert_eq!(
            write_paths(CheckAction::Unstage, &changes, &targets),
            ["new name.md", "old.md", "b.md"].map(PathBuf::from)
        );
        // Stager un renommage ne demande que le nouveau chemin.
        assert_eq!(
            write_paths(CheckAction::Stage, &changes, &targets),
            ["new name.md", "b.md"].map(PathBuf::from)
        );
    }

    #[test]
    fn the_committed_group_is_named_after_the_branch() {
        let changes = [change("a.md", Group::Committed, Check::NoBox)];
        let rows = build(&changes, &HashSet::new(), Some("demo"));
        assert_eq!(rows[0].label, "Commités sur demo");
        assert_eq!(rows[0].check, Check::NoBox);
        assert_eq!(rows[1].check, Check::NoBox);
    }
}
