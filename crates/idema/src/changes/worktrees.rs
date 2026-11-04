//! Les cibles du volet Changes (spec P3 §4.1) : le dossier du projet ou l'un des worktrees de son dépôt. Ce module lit
//! la liste des worktrees et dit si une cible a disparu ; le volet et son sélecteur s'y branchent.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::files::reference::display_path;

use super::Snapshot;
use super::model::{LoadError, read, read_ok, trim_newline};

/// Un worktree tel que `git worktree list` le décrit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub root: PathBuf,
    /// Le nom court de la branche, `None` en HEAD détachée.
    pub branch: Option<String>,
    pub bare: bool,
    /// Le dossier a disparu : Git peut le purger.
    pub prunable: bool,
}

/// Une ligne du sélecteur : un worktree que l'utilisateur peut choisir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetLine {
    pub root: PathBuf,
    pub name: String,
    /// Le texte secondaire : le chemin relatif au dépôt principal, ou le chemin complet.
    pub detail: String,
}

/// Ce que le volet montre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Le dossier du projet.
    WorkingTree,
    Worktree {
        root: PathBuf,
        name: String,
    },
}

impl Target {
    /// Le dossier à lire : celui du projet, ou la racine du worktree.
    pub fn dir<'a>(&'a self, project: &'a Path) -> &'a Path {
        match self {
            Target::WorkingTree => project,
            Target::Worktree { root, .. } => root,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Target::WorkingTree => "working tree",
            Target::Worktree { name, .. } => name,
        }
    }
}

/// Lit `worktree list --porcelain -z` : un champ par NUL (`worktree <chemin>`, `HEAD <sha>`, `branch refs/heads/<b>`,
/// `detached` ou `bare`, puis `locked …` ou `prunable …`), et un champ vide clôt chaque bloc. Un bloc sans `worktree`
/// est ignoré.
pub fn parse_worktree_list(bytes: &[u8]) -> Vec<Worktree> {
    let mut list = Vec::new();
    let mut current: Option<Worktree> = None;
    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            list.extend(current.take());
            continue;
        }
        if let Some(path) = field.strip_prefix(b"worktree ") {
            list.extend(current.take());
            current = Some(Worktree {
                root: PathBuf::from(OsStr::from_bytes(path)),
                branch: None,
                bare: false,
                prunable: false,
            });
            continue;
        }
        // Les champs d'un bloc sans ligne `worktree` n'ont pas de racine à qui s'attacher : ils sont ignorés.
        let Some(block) = current.as_mut() else {
            continue;
        };
        if let Some(reference) = field.strip_prefix(b"branch ") {
            let reference = String::from_utf8_lossy(reference);
            let short = reference.strip_prefix("refs/heads/").unwrap_or(&reference);
            block.branch = Some(short.to_string());
        } else if field == b"bare" {
            block.bare = true;
        } else if field == b"prunable" || field.starts_with(b"prunable ") {
            block.prunable = true;
        }
    }
    list.extend(current);
    list
}

/// Les cibles qu'on propose : l'ordre est celui de Git, sans le worktree bare, ni celui dont le dossier a disparu, ni
/// celui que le volet montre déjà (`project_top`). Le texte secondaire est le chemin relatif au dépôt principal (le
/// premier de la liste) quand le worktree est dessous, sinon son chemin complet : le dépôt principal lui-même a donc
/// son chemin complet.
pub fn target_lines(
    worktrees: &[Worktree],
    project_top: &Path,
    home: Option<&Path>,
) -> Vec<TargetLine> {
    let main = worktrees.first().map(|worktree| worktree.root.as_path());
    worktrees
        .iter()
        .filter(|worktree| !worktree.bare && !worktree.prunable && worktree.root != project_top)
        .map(|worktree| {
            let name = worktree.branch.clone().unwrap_or_else(|| {
                worktree
                    .root
                    .file_name()
                    .map(|folder| folder.to_string_lossy().into_owned())
                    .unwrap_or_else(|| worktree.root.display().to_string())
            });
            let below_main = main
                .and_then(|main| worktree.root.strip_prefix(main).ok())
                .filter(|relative| !relative.as_os_str().is_empty());
            let detail = match below_main {
                Some(relative) => relative.display().to_string(),
                None => display_path(&worktree.root, home),
            };
            TargetLine {
                root: worktree.root.clone(),
                name,
                detail,
            }
        })
        .collect()
}

/// Les cibles du dépôt qui contient `project`. Que des lectures.
pub fn list_targets(project: &Path, home: Option<&Path>) -> Result<Vec<TargetLine>, LoadError> {
    let top = read(project, &["rev-parse", "--show-toplevel"])?;
    if !top.success() {
        return Err(LoadError::NotARepository);
    }
    let project_top = PathBuf::from(OsStr::from_bytes(trim_newline(&top.stdout)));
    let list = read_ok(&project_top, &["worktree", "list", "--porcelain", "-z"])?;
    Ok(target_lines(
        &parse_worktree_list(&list.stdout),
        &project_top,
        home,
    ))
}

/// La cible a disparu : son dossier n'est plus un dépôt, ou Git y lit un autre dépôt (un dossier vide recréé au même
/// endroit, dans le dépôt principal). Les deux racines passent par `canonicalize` ; un chemin qui ne se canonise plus
/// est parti. Une erreur de Git n'est pas une disparition.
pub fn target_gone(target_root: &Path, result: &Result<Snapshot, LoadError>) -> bool {
    match result {
        Err(LoadError::NotARepository) => true,
        Err(_) => false,
        Ok(snapshot) => match (
            fs::canonicalize(target_root),
            fs::canonicalize(&snapshot.root),
        ) {
            (Ok(target), Ok(read_root)) => target != read_root,
            _ => true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::model::LoadError;
    use crate::changes::test_repo::linked_worktree_repo;
    use crate::changes::{BranchInfo, Snapshot};
    use std::collections::HashSet;

    /// Les quatre blocs de la sonde Git 2.54 : un dépôt principal, un worktree lié, un en HEAD détachée et un dont le
    /// dossier a disparu (le chemin porte une espace).
    fn probe() -> Vec<u8> {
        let blocks = [
            "worktree /r\0HEAD 1111\0branch refs/heads/main\0\0",
            "worktree /r/.worktrees/demo\0HEAD 2222\0branch refs/heads/demo\0\0",
            "worktree /r/.worktrees/det\0HEAD 3333\0detached\0\0",
            "worktree /r/gone x\0HEAD 4444\0branch refs/heads/gone\0prunable gitdir file points to non-existent location\0\0",
        ];
        blocks.concat().into_bytes()
    }

    fn worktree(root: &str, branch: Option<&str>) -> Worktree {
        Worktree {
            root: PathBuf::from(root),
            branch: branch.map(str::to_string),
            bare: false,
            prunable: false,
        }
    }

    #[test]
    fn parses_the_worktree_list() {
        let list = parse_worktree_list(&probe());

        let roots: Vec<&Path> = list
            .iter()
            .map(|worktree| worktree.root.as_path())
            .collect();
        assert_eq!(
            roots,
            [
                Path::new("/r"),
                Path::new("/r/.worktrees/demo"),
                Path::new("/r/.worktrees/det"),
                Path::new("/r/gone x"),
            ]
        );
        let branches: Vec<Option<&str>> = list.iter().map(|w| w.branch.as_deref()).collect();
        assert_eq!(branches, [Some("main"), Some("demo"), None, Some("gone")]);
        let prunable: Vec<bool> = list.iter().map(|worktree| worktree.prunable).collect();
        assert_eq!(prunable, [false, false, false, true]);
        assert!(list.iter().all(|worktree| !worktree.bare));
    }

    #[test]
    fn parses_a_bare_main_worktree() {
        let list = parse_worktree_list(b"worktree /r.git\0bare\0\0");

        assert_eq!(list.len(), 1);
        assert!(list[0].bare);
        assert_eq!(list[0].branch, None);
    }

    #[test]
    fn a_block_without_a_path_is_ignored() {
        let list =
            parse_worktree_list(b"HEAD 1111\0detached\0\0worktree /r\0branch refs/heads/main\0\0");

        assert_eq!(list, [worktree("/r", Some("main"))]);
    }

    #[test]
    fn target_lines_skip_the_project_and_the_dead() {
        let mut list = parse_worktree_list(&probe());
        list.push(Worktree {
            root: PathBuf::from("/r.git"),
            branch: None,
            bare: true,
            prunable: false,
        });

        let lines = target_lines(&list, Path::new("/r"), None);

        let shown: Vec<(&str, &str)> = lines
            .iter()
            .map(|line| (line.name.as_str(), line.detail.as_str()))
            .collect();
        assert_eq!(
            shown,
            [("demo", ".worktrees/demo"), ("det", ".worktrees/det")]
        );
        assert_eq!(lines[0].root, Path::new("/r/.worktrees/demo"));
    }

    #[test]
    fn target_lines_offer_the_main_repository_from_a_linked_worktree() {
        let list = [
            worktree("/Users/x/r", Some("main")),
            worktree("/Users/x/r/.worktrees/demo", Some("demo")),
            worktree("/Users/x/elsewhere", Some("far")),
        ];

        let lines = target_lines(
            &list,
            Path::new("/Users/x/r/.worktrees/demo"),
            Some(Path::new("/Users/x")),
        );

        let shown: Vec<(&str, &str)> = lines
            .iter()
            .map(|line| (line.name.as_str(), line.detail.as_str()))
            .collect();
        assert_eq!(shown, [("main", "~/r"), ("far", "~/elsewhere")]);
    }

    fn snapshot_at(root: &Path) -> Snapshot {
        Snapshot {
            root: root.to_path_buf(),
            git_dir: root.join(".git"),
            common_dir: root.join(".git"),
            base: None,
            merge_base: None,
            branch: BranchInfo {
                head: None,
                has_commits: false,
                upstream: None,
                upstream_gone: false,
                ahead: 0,
            },
            has_origin: false,
            changes: Vec::new(),
            tracked_folders: HashSet::new(),
        }
    }

    #[test]
    fn target_gone_when_the_folder_is_not_a_repository() {
        let root = std::env::temp_dir();
        assert!(target_gone(&root, &Err(LoadError::NotARepository)));
    }

    #[test]
    fn target_is_not_gone_on_a_git_error() {
        let root = std::env::temp_dir();
        assert!(!target_gone(
            &root,
            &Err(LoadError::Git("boom".to_string()))
        ));
    }

    #[test]
    fn target_is_not_gone_when_the_snapshot_is_its_own() {
        let repo = linked_worktree_repo();
        let demo = repo.path().join(".worktrees/demo");
        let snapshot = crate::changes::model::load(&demo).unwrap();

        assert!(!target_gone(&demo, &Ok(snapshot)));
    }

    #[test]
    fn target_gone_when_the_snapshot_is_the_main_repository() {
        let repo = linked_worktree_repo();
        let demo = repo.path().join(".worktrees/demo");

        assert!(target_gone(&demo, &Ok(snapshot_at(repo.path()))));
    }

    #[test]
    fn target_gone_when_the_target_no_longer_canonicalizes() {
        let repo = linked_worktree_repo();
        let missing = repo.path().join("no-such-folder");

        assert!(target_gone(&missing, &Ok(snapshot_at(&missing))));
    }

    #[test]
    fn the_target_names_and_folders() {
        let project = Path::new("/p");
        let tree = Target::WorkingTree;
        assert_eq!(tree.name(), "working tree");
        assert_eq!(tree.dir(project), project);
        let demo = Target::Worktree {
            root: PathBuf::from("/p/.worktrees/demo"),
            name: "demo".to_string(),
        };
        assert_eq!(demo.name(), "demo");
        assert_eq!(demo.dir(project), Path::new("/p/.worktrees/demo"));
    }

    #[test]
    fn list_targets_reads_the_worktrees() {
        let repo = linked_worktree_repo();

        let lines = list_targets(repo.path(), None).unwrap();

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].name, "demo");
        assert_eq!(lines[0].detail, ".worktrees/demo");
        assert!(lines[0].root.ends_with(".worktrees/demo"));
    }

    #[test]
    fn list_targets_from_a_linked_worktree_offers_the_main_repository() {
        let repo = linked_worktree_repo();

        let lines = list_targets(&repo.path().join(".worktrees/demo"), None).unwrap();

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].name, "main");
        assert_eq!(lines[0].root, repo.path());
    }

    #[test]
    fn list_targets_outside_a_repository_is_not_a_repository() {
        let dir = std::env::temp_dir().join(format!("idema-targets-bare-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = list_targets(&dir, None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(result.unwrap_err(), LoadError::NotARepository);
    }

    #[test]
    fn a_removed_worktree_is_gone() {
        let repo = linked_worktree_repo();
        let demo = repo.path().join(".worktrees/demo");
        // Le worktree propre : le retrait n'exige pas `--force`.
        repo.git(&["-C", ".worktrees/demo", "checkout", "--quiet", "--", "b.md"]);
        repo.git(&["worktree", "remove", ".worktrees/demo"]);

        let result = crate::changes::model::load(&demo);

        assert_eq!(result.as_ref().unwrap_err(), &LoadError::NotARepository);
        assert!(target_gone(&demo, &result));
    }

    #[test]
    fn an_emptied_worktree_folder_is_gone() {
        let repo = linked_worktree_repo();
        let demo = repo.path().join(".worktrees/demo");
        repo.git(&["-C", ".worktrees/demo", "checkout", "--quiet", "--", "b.md"]);
        repo.git(&["worktree", "remove", ".worktrees/demo"]);
        std::fs::create_dir_all(&demo).unwrap();

        let result = crate::changes::model::load(&demo);

        // Le dossier recréé est vide, dans le dépôt principal : Git y lit le dépôt principal, pas le worktree.
        assert_eq!(result.as_ref().unwrap().root, repo.path());
        assert!(target_gone(&demo, &result));
    }
}
