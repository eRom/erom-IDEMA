//! Charger l'état d'un dépôt (base, fusion-base, groupes) et le patch d'un fichier. Que des lectures, par le lanceur.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::git::{self, Gesture, GitError, GitOutput};

use super::ops::LITERAL;
use super::{Change, Check, Group, Letter, Snapshot, status};

/// L'arbre vide de Git : le point de départ d'un dépôt sans commit.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Le nombre de lignes de contexte du patch : assez pour contenir le fichier entier (P3-3, §4.5).
const FULL_CONTEXT: &str = "--unified=1000000";

#[derive(Debug, PartialEq, Eq)]
pub enum LoadError {
    NotARepository,
    Missing,
    Git(String),
}

impl From<GitError> for LoadError {
    fn from(error: GitError) -> Self {
        match error {
            GitError::Missing => LoadError::Missing,
            other => LoadError::Git(other.to_string()),
        }
    }
}

/// La branche locale principale (§4.2) : celle que dit `origin/HEAD` si elle existe en local, sinon `main`, sinon
/// `master`, sinon la branche de la cible. Jamais une branche distante.
pub fn resolve_base(
    origin_head: Option<&str>,
    local_branches: &[String],
    target_branch: Option<&str>,
) -> Option<String> {
    let exists = |name: &str| local_branches.iter().any(|branch| branch == name);
    let from_origin = origin_head
        .and_then(|head| head.strip_prefix("origin/"))
        .filter(|name| exists(name));
    from_origin
        .or_else(|| ["main", "master"].into_iter().find(|name| exists(name)))
        .or(target_branch)
        .map(str::to_string)
}

/// L'attente avant de relire après une erreur de Git : un garde-fou bon marché contre un échec ponctuel de lecture.
pub(crate) const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Refait une lecture qui a échoué sur une erreur de Git, une seule fois, après `TRANSIENT_RETRY_DELAY`. Ce n'est qu'un
/// garde-fou contre un échec ponctuel : il ne couvre pas une corruption. Le « bad object HEAD » vu en test venait de la
/// maintenance automatique de Git (`git maintenance run --auto` lancé par chaque commit), qui abîme le dépôt pour de
/// bon ; ni IdemA ni un état passager de Git n'en sont la cause, et une seconde lecture échoue de la même façon.
/// `NotARepository` et `Missing` sont des réponses, pas des accidents : elles ne sont pas refaites.
pub(crate) fn retry_transient<T>(
    mut attempt: impl FnMut() -> Result<T, LoadError>,
) -> Result<T, LoadError> {
    match attempt() {
        Err(LoadError::Git(_)) => {
            std::thread::sleep(TRANSIENT_RETRY_DELAY);
            attempt()
        }
        other => other,
    }
}

/// Lit le dépôt qui contient `project` : le statut, la base, la fusion-base et les fichiers commités sur la branche.
/// Que des lectures sans verrou, pour ne jamais gêner un agent qui commite au même moment. Une erreur de Git est
/// relue une fois (`retry_transient`).
pub fn load(project: &Path) -> Result<Snapshot, LoadError> {
    retry_transient(|| load_once(project))
}

fn load_once(project: &Path) -> Result<Snapshot, LoadError> {
    let top = read(
        project,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
    )?;
    if !top.success() {
        return Err(LoadError::NotARepository);
    }
    // Trois lignes absolues : la racine, le dossier Git de la cible, le dossier Git commun.
    let paths: Vec<&[u8]> = trim_newline(&top.stdout)
        .split(|byte| *byte == b'\n')
        .collect();
    let [root, git_dir, common_dir] = paths[..] else {
        return Err(LoadError::Git(format!(
            "rev-parse returned {} lines instead of 3",
            paths.len()
        )));
    };
    let as_path = |bytes: &[u8]| PathBuf::from(OsStr::from_bytes(bytes));
    let (root, git_dir, common_dir) = (as_path(root), as_path(git_dir), as_path(common_dir));

    let status_output = read_ok(
        &root,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--branch",
        ],
    )?;
    let (branch, mut changes) = status::parse(&status_output.stdout).map_err(LoadError::Git)?;

    // Sans `origin/HEAD`, `symbolic-ref -q` sort en silence avec le code 1 : ce n'est pas une erreur.
    let origin_head = read(
        &root,
        &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
    )?;
    let origin_head = origin_head
        .success()
        .then(|| lossy_text(&origin_head.stdout));
    let local_branches = lines(
        &read_ok(
            &root,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        )?
        .stdout,
    );
    let has_origin = lines(&read_ok(&root, &["remote"])?.stdout)
        .iter()
        .any(|remote| remote == "origin");

    let base = if branch.has_commits {
        resolve_base(
            origin_head.as_deref().filter(|head| !head.is_empty()),
            &local_branches,
            branch.head.as_deref(),
        )
    } else {
        None
    };
    // Deux branches sans histoire commune n'ont pas de fusion-base : la liste se contente alors du statut.
    let merge_base = match &base {
        Some(base) => {
            let output = read(&root, &["merge-base", base, "HEAD"])?;
            output.success().then(|| lossy_text(&output.stdout))
        }
        None => None,
    };

    if let (Some(base), Some(merge_base)) = (&base, &merge_base)
        && branch.head.as_deref() != Some(base.as_str())
    {
        let committed = read_ok(&root, &["diff", "--name-status", "-z", merge_base, "HEAD"])?;
        let listed: HashSet<PathBuf> = changes.iter().map(|change| change.path.clone()).collect();
        changes.extend(
            committed_changes(&committed.stdout)?
                .into_iter()
                .filter(|change| !listed.contains(&change.path)),
        );
    }

    let tracked_folders = tracked_folders(&root, &changes)?;

    Ok(Snapshot {
        root,
        git_dir,
        common_dir,
        base,
        merge_base,
        branch,
        has_origin,
        changes,
        tracked_folders,
    })
}

/// Les dossiers qui portent un fichier suivi, parmi ceux des fichiers non suivis. Le statut ne liste que les fichiers
/// neufs d'un dossier suivi : sans cette lecture, `notes/` aurait l'air d'un dossier non suivi. Une seule lecture
/// (`ls-files --cached`, chemins littéraux) sur les dossiers de premier niveau de ces fichiers, puis chaque dossier au-dessus
/// d'un fichier suivi. Rien à lire quand aucun fichier non suivi n'est dans un dossier.
fn tracked_folders(root: &Path, changes: &[Change]) -> Result<HashSet<PathBuf>, LoadError> {
    let tops: BTreeSet<&OsStr> = changes
        .iter()
        .filter(|change| change.group == Group::Untracked)
        .filter_map(|change| {
            let mut parts = change.path.components();
            let first = parts.next()?;
            parts.next()?;
            Some(first.as_os_str())
        })
        .collect();
    let mut folders = HashSet::new();
    if tops.is_empty() {
        return Ok(folders);
    }
    let specs = tops
        .iter()
        .map(|top| Ok(format!("{LITERAL}{}", utf8_path(Path::new(top))?)))
        .collect::<Result<Vec<String>, LoadError>>()?;
    let mut args = vec!["ls-files", "-z", "--cached", "--"];
    args.extend(specs.iter().map(String::as_str));
    let output = read_ok(root, &args)?;
    for entry in output.stdout.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        for folder in Path::new(OsStr::from_bytes(entry)).ancestors().skip(1) {
            // Un dossier déjà là a déjà tous ses ancêtres ; la racine (chemin vide) n'est pas un dossier de la liste.
            if folder.as_os_str().is_empty() || !folders.insert(folder.to_path_buf()) {
                break;
            }
        }
    }
    Ok(folders)
}

/// Le patch d'un fichier, de la fusion-base aux fichiers de travail, avec tout le contexte (P3-3). Un fichier non suivi
/// n'a pas d'état dans Git : il se compare à `/dev/null`, et le code de sortie 1 de `--no-index` veut dire « il
/// diffère ». Une erreur de Git est relue une fois (`retry_transient`).
pub fn file_patch(snapshot: &Snapshot, change: &Change) -> Result<String, LoadError> {
    retry_transient(|| file_patch_once(snapshot, change))
}

fn file_patch_once(snapshot: &Snapshot, change: &Change) -> Result<String, LoadError> {
    let path = utf8_path(&change.path)?;
    let output = if change.group == Group::Untracked {
        let output = read(
            &snapshot.root,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-index",
                FULL_CONTEXT,
                "--",
                "/dev/null",
                path,
            ],
        )?;
        // Seuls 0 et 1 sont des sorties de `--no-index` : un git tué par un signal (code -1) n'est pas un patch vide. Le
        // code 1 sans patch mais avec un message est une erreur que `--no-index` sort aussi par 1 (un dépôt imbriqué :
        // « Could not access 'nested/null' », un fichier disparu entre le statut et le diff) : ce n'est pas « Fichier vide ».
        let failed = !matches!(output.code, 0 | 1)
            || (output.code == 1 && output.stdout.is_empty() && !output.stderr.trim().is_empty());
        if failed {
            return Err(LoadError::Git(output.stderr));
        }
        output
    } else {
        // Sans fusion-base : HEAD s'il y a des commits, l'arbre vide sinon.
        let from = match (&snapshot.merge_base, snapshot.branch.has_commits) {
            (Some(merge_base), _) => merge_base.as_str(),
            (None, true) => "HEAD",
            (None, false) => EMPTY_TREE,
        };
        // Les chemins sont des motifs pour Git : `:(literal)` empêche `[id].tsx` de prendre aussi `i.tsx` (R4).
        // L'ancien chemin d'un renommage vient d'abord, pour que Git retrouve le rapprochement.
        let mut pathspecs = Vec::new();
        if let Some(orig_path) = &change.orig_path {
            pathspecs.push(format!("{LITERAL}{}", utf8_path(orig_path)?));
        }
        pathspecs.push(format!("{LITERAL}{path}"));
        let mut args = vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-M",
            FULL_CONTEXT,
            from,
            "--",
        ];
        args.extend(pathspecs.iter().map(String::as_str));
        read_ok(&snapshot.root, &args)?
    };
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn utf8_path(path: &Path) -> Result<&str, LoadError> {
    path.to_str()
        .ok_or_else(|| LoadError::Git(format!("path is not valid UTF-8: {}", path.display())))
}

pub(super) fn read(dir: &Path, args: &[&str]) -> Result<GitOutput, LoadError> {
    Ok(git::run(dir, Gesture::Read, args, None)?)
}

/// Une lecture qui doit réussir : un code non nul porte le message de Git.
pub(super) fn read_ok(dir: &Path, args: &[&str]) -> Result<GitOutput, LoadError> {
    let output = read(dir, args)?;
    if output.success() {
        Ok(output)
    } else {
        Err(LoadError::Git(output.stderr))
    }
}

pub(super) fn trim_newline(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

fn lossy_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(trim_newline(bytes)).into_owned()
}

fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect()
}

/// `diff --name-status -z` : un statut (`M`, `A`, `D`, `T`, `R100`, `C75`), puis un chemin, ou deux pour un
/// renommage et une copie (l'ancien d'abord).
fn committed_changes(bytes: &[u8]) -> Result<Vec<Change>, LoadError> {
    let malformed = || LoadError::Git("malformed diff --name-status output".to_string());
    let mut records = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    let mut changes = Vec::new();
    while let Some(status) = records.next() {
        let letter = match status[0] {
            b'A' => Letter::Added,
            b'D' => Letter::Deleted,
            b'R' | b'C' => Letter::Renamed,
            _ => Letter::Modified,
        };
        let (path, orig_path) = if letter == Letter::Renamed {
            let orig = records.next().ok_or_else(malformed)?;
            let path = records.next().ok_or_else(malformed)?;
            (path, Some(PathBuf::from(OsStr::from_bytes(orig))))
        } else {
            (records.next().ok_or_else(malformed)?, None)
        };
        changes.push(Change {
            path: PathBuf::from(OsStr::from_bytes(path)),
            orig_path,
            group: Group::Committed,
            letter,
            check: Check::NoBox,
        });
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::test_repo::{TempRepo, committed_repo, fixture_repo, linked_worktree_repo};
    use crate::changes::{Check, Group, Letter};
    use gpui_component::diff::DiffFile;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    fn find<'a>(snapshot: &'a Snapshot, path: &str) -> &'a Change {
        snapshot
            .changes
            .iter()
            .find(|change| change.path == Path::new(path))
            .unwrap_or_else(|| panic!("{path} is not listed: {:?}", snapshot.changes))
    }

    #[test]
    fn resolve_base_reads_origin_head_when_the_local_branch_exists() {
        let base = resolve_base(Some("origin/trunk"), &names(&["trunk", "x"]), Some("x"));
        assert_eq!(base.as_deref(), Some("trunk"));
    }

    #[test]
    fn resolve_base_never_returns_a_missing_local_branch() {
        let base = resolve_base(Some("origin/main"), &names(&["x"]), Some("x"));
        assert_eq!(base.as_deref(), Some("x"));
    }

    #[test]
    fn resolve_base_falls_back_on_master_then_main_first() {
        let base = resolve_base(None, &names(&["master", "x"]), Some("x"));
        assert_eq!(base.as_deref(), Some("master"));
        let base = resolve_base(None, &names(&["main", "master"]), Some("x"));
        assert_eq!(base.as_deref(), Some("main"));
        let base = resolve_base(None, &names(&["main", "master"]), None);
        assert_eq!(base.as_deref(), Some("main"));
    }

    #[test]
    fn resolve_base_is_none_without_any_branch() {
        assert_eq!(resolve_base(None, &[], None), None);
    }

    #[test]
    fn load_names_the_tracked_folders_above_untracked_content() {
        let repo = TempRepo::init();
        repo.write("notes/old.md", b"a");
        repo.write("deep/tracked/x.md", b"x");
        repo.write("lone/tracked.md", b"x");
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "--quiet", "-m", "init"]);
        repo.write("notes/new.md", b"b");
        repo.write("captures/a.png", b"x");
        repo.write("deep/tracked/sub/y.md", b"y");
        repo.write("top.md", b"t");

        let snapshot = load(repo.path()).unwrap();

        // Les dossiers d'un fichier suivi, au-dessus de contenu non suivi : `lone` n'a rien de neuf, il n'est pas lu.
        let folders: HashSet<PathBuf> = ["notes", "deep", "deep/tracked"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(snapshot.tracked_folders, folders);
    }

    #[test]
    fn load_reads_no_tracked_folder_when_nothing_untracked_is_in_a_folder() {
        let repo = committed_repo();
        repo.write("top.md", b"t");

        let snapshot = load(repo.path()).unwrap();

        assert!(snapshot.tracked_folders.is_empty());
    }

    #[test]
    fn load_lists_the_fixture_groups() {
        let repo = fixture_repo();
        let snapshot = load(repo.path()).unwrap();

        assert_eq!(snapshot.base.as_deref(), Some("main"));
        assert_eq!(snapshot.root, repo.path());
        assert!(snapshot.branch.has_commits);
        assert!(!snapshot.has_origin);

        let readme = find(&snapshot, "README.md");
        assert_eq!(
            (readme.group, readme.letter),
            (Group::Modified, Letter::Modified)
        );
        assert_eq!(readme.check, Check::Unchecked);
        let logo = find(&snapshot, "logo.bin");
        assert_eq!(
            (logo.group, logo.letter),
            (Group::Modified, Letter::Modified)
        );
        let old = find(&snapshot, "old.txt");
        assert_eq!((old.group, old.letter), (Group::Modified, Letter::Deleted));
        for path in ["captures/a.png", "notes/brouillon.md"] {
            let untracked = find(&snapshot, path);
            assert_eq!(untracked.group, Group::Untracked);
            assert_eq!(untracked.check, Check::Unchecked);
        }
        assert_eq!(snapshot.changes.len(), 5);
    }

    #[test]
    fn a_branch_shows_its_committed_files() {
        let repo = committed_repo();
        repo.write("b.md", b"b\n");
        repo.commit_all("Add b");
        repo.git(&["switch", "--quiet", "-c", "demo"]);
        repo.write("a.md", b"a\n");
        repo.commit_all("Add a");
        repo.write("b.md", b"b changed\n");

        let snapshot = load(repo.path()).unwrap();

        assert_eq!(snapshot.base.as_deref(), Some("main"));
        assert_eq!(snapshot.branch.head.as_deref(), Some("demo"));
        assert!(snapshot.merge_base.is_some());
        let a = find(&snapshot, "a.md");
        assert_eq!(
            (a.group, a.letter, a.check),
            (Group::Committed, Letter::Added, Check::NoBox)
        );
        let b = find(&snapshot, "b.md");
        assert_eq!(b.group, Group::Modified);
        assert_eq!(snapshot.changes.len(), 2);
    }

    #[test]
    fn the_base_branch_has_no_committed_group() {
        let repo = committed_repo();
        repo.write("a.md", b"a\n");
        repo.commit_all("Add a on main");
        let snapshot = load(repo.path()).unwrap();
        assert!(snapshot.changes.is_empty(), "{:?}", snapshot.changes);
    }

    #[test]
    fn a_committed_rename_keeps_its_old_path() {
        let repo = committed_repo();
        repo.git(&["switch", "--quiet", "-c", "demo"]);
        repo.git(&["mv", "old.txt", "renamed.txt"]);
        repo.commit_all("Rename old");

        let snapshot = load(repo.path()).unwrap();

        let renamed = find(&snapshot, "renamed.txt");
        assert_eq!(renamed.group, Group::Committed);
        assert_eq!(renamed.letter, Letter::Renamed);
        assert_eq!(renamed.orig_path, Some(PathBuf::from("old.txt")));
        assert_eq!(snapshot.changes.len(), 1);
    }

    #[test]
    fn a_subfolder_project_lists_the_whole_repository() {
        let repo = fixture_repo();
        let snapshot = load(&repo.path().join("notes")).unwrap();
        assert_eq!(snapshot.root, repo.path());
        find(&snapshot, "README.md");
        find(&snapshot, "notes/brouillon.md");
    }

    #[test]
    fn a_folder_outside_any_repository_is_not_a_repository() {
        let dir = std::env::temp_dir().join(format!("idema-changes-bare-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = load(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(result.unwrap_err(), LoadError::NotARepository);
    }

    #[test]
    fn an_unborn_repository_has_no_base_and_lists_everything() {
        let repo = TempRepo::init();
        repo.write("one.md", b"1\n");
        repo.write("dir/two.md", b"2\n");

        let snapshot = load(repo.path()).unwrap();

        assert_eq!(snapshot.changes.len(), 2);
        assert!(
            snapshot
                .changes
                .iter()
                .all(|change| change.group == Group::Untracked)
        );
        assert!(!snapshot.branch.has_commits);
        assert_eq!(snapshot.branch.head.as_deref(), Some("main"));
        assert_eq!(snapshot.base, None);
        assert_eq!(snapshot.merge_base, None);
    }

    #[test]
    fn an_unborn_repository_patches_staged_files_against_the_empty_tree() {
        let repo = TempRepo::init();
        repo.write("one.md", b"1\n2\n");
        repo.git(&["add", "one.md"]);
        let snapshot = load(repo.path()).unwrap();
        let patch = file_patch(&snapshot, find(&snapshot, "one.md")).unwrap();
        let files = DiffFile::parse(&patch).unwrap();
        assert_eq!((files[0].additions(), files[0].deletions()), (2, 0));
    }

    #[test]
    fn file_patch_carries_the_whole_file() {
        let repo = fixture_repo();
        let snapshot = load(repo.path()).unwrap();

        let patch = file_patch(&snapshot, find(&snapshot, "README.md")).unwrap();
        let files = DiffFile::parse(&patch).unwrap();
        assert_eq!(files.len(), 1);
        let numstat = repo.git(&["diff", "--numstat", "--", "README.md"]);
        let mut columns = numstat.split_whitespace();
        let added: usize = columns.next().unwrap().parse().unwrap();
        let removed: usize = columns.next().unwrap().parse().unwrap();
        assert_eq!(
            (files[0].additions(), files[0].deletions()),
            (added, removed)
        );
        assert!(added > 0 && removed > 0);
        // Le contexte complet : le patch contient aussi les lignes que rien n'a touchées.
        assert!(patch.contains("ligne 20"), "no far context in the patch");

        let patch = file_patch(&snapshot, find(&snapshot, "notes/brouillon.md")).unwrap();
        let files = DiffFile::parse(&patch).unwrap();
        assert_eq!((files[0].additions(), files[0].deletions()), (1, 0));

        let patch = file_patch(&snapshot, find(&snapshot, "logo.bin")).unwrap();
        let files = DiffFile::parse(&patch).unwrap();
        assert!(files[0].is_binary());

        let patch = file_patch(&snapshot, find(&snapshot, "old.txt")).unwrap();
        let files = DiffFile::parse(&patch).unwrap();
        assert_eq!((files[0].additions(), files[0].deletions()), (0, 1));
    }

    #[test]
    fn file_patch_of_a_branch_starts_at_the_merge_base() {
        let repo = committed_repo();
        repo.git(&["switch", "--quiet", "-c", "demo"]);
        repo.write("a.md", b"a\n");
        repo.commit_all("Add a");
        let snapshot = load(repo.path()).unwrap();

        let patch = file_patch(&snapshot, find(&snapshot, "a.md")).unwrap();

        let files = DiffFile::parse(&patch).unwrap();
        assert_eq!((files[0].additions(), files[0].deletions()), (1, 0));
    }

    #[test]
    fn file_patch_of_a_rename_follows_the_old_path() {
        let repo = committed_repo();
        repo.git(&["mv", "old.txt", "renamed.txt"]);
        let snapshot = load(repo.path()).unwrap();
        let renamed = find(&snapshot, "renamed.txt");
        assert_eq!(renamed.letter, Letter::Renamed);

        let patch = file_patch(&snapshot, renamed).unwrap();

        let files = DiffFile::parse(&patch).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].additions(), files[0].deletions()), (0, 0));
    }

    #[test]
    fn file_patch_of_a_nested_repository_is_an_error_not_an_empty_file() {
        let repo = committed_repo();
        repo.git(&["init", "--quiet", "nested"]);
        repo.git(&[
            "-C",
            "nested",
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Nested",
        ]);
        let snapshot = load(repo.path()).unwrap();
        let nested = find(&snapshot, "nested");
        assert_eq!(nested.group, Group::Untracked);

        let error = file_patch(&snapshot, nested).unwrap_err();

        assert!(
            matches!(error, LoadError::Git(ref message) if !message.is_empty()),
            "{error:?}"
        );
    }

    #[test]
    fn file_patch_of_a_file_deleted_after_the_status_is_an_error() {
        let repo = committed_repo();
        repo.write("gone.md", b"bientot disparu\n");
        let snapshot = load(repo.path()).unwrap();
        repo.remove("gone.md");

        let error = file_patch(&snapshot, find(&snapshot, "gone.md")).unwrap_err();

        assert!(
            matches!(error, LoadError::Git(ref message) if !message.is_empty()),
            "{error:?}"
        );
    }

    #[test]
    fn file_patch_of_a_glob_like_name_is_that_file() {
        let repo = committed_repo();
        repo.write("app/[id].tsx", b"page\n");
        repo.write("app/i.tsx", b"i\n");
        repo.commit_all("Add app");
        repo.write("app/[id].tsx", b"page changed\n");
        repo.write("app/i.tsx", b"i changed\n");
        repo.write("app/[new].tsx", b"new\n");
        repo.write("app/n.tsx", b"n\n");
        let snapshot = load(repo.path()).unwrap();

        for (name, other) in [
            ("app/[id].tsx", "app/i.tsx"),
            ("app/[new].tsx", "app/n.tsx"),
        ] {
            let patch = file_patch(&snapshot, find(&snapshot, name)).unwrap();

            let files = DiffFile::parse(&patch).unwrap();
            assert_eq!(files.len(), 1, "{name}: {patch}");
            assert!(patch.contains(name), "{name}: {patch}");
            assert!(!patch.contains(other), "{name}: {patch}");
        }
    }

    #[test]
    fn load_takes_no_lock() {
        let repo = fixture_repo();
        let dir = repo.path().to_path_buf();
        let reading = Arc::new(AtomicBool::new(true));
        let still_reading = Arc::clone(&reading);
        // L'agent commite au moins 20 fois, et continue tant qu'IdemA lit : les deux se chevauchent sur toute la durée.
        let committer = std::thread::spawn(move || {
            let repo = TempRepoRef(dir);
            let mut n = 0;
            while n < 20 || still_reading.load(Ordering::Relaxed) {
                repo.run(&["add", "-A"]);
                repo.run(&[
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    &format!("commit {n}"),
                ]);
                n += 1;
            }
        });
        for _ in 0..200 {
            load(repo.path()).unwrap();
        }
        reading.store(false, Ordering::Relaxed);
        committer
            .join()
            .expect("an agent's git command failed while IdemA was reading");
    }

    #[test]
    fn load_reads_a_linked_worktree() {
        let repo = linked_worktree_repo();
        let demo = repo.path().join(".worktrees/demo");

        let snapshot = load(&demo).unwrap();

        assert!(
            snapshot.root.ends_with(".worktrees/demo"),
            "{:?}",
            snapshot.root
        );
        assert_eq!(snapshot.git_dir, repo.path().join(".git/worktrees/demo"));
        assert_eq!(snapshot.common_dir, repo.path().join(".git"));
        assert_eq!(snapshot.base.as_deref(), Some("main"));
        assert_eq!(snapshot.branch.head.as_deref(), Some("demo"));
        let a = find(&snapshot, "a.md");
        assert_eq!(a.group, Group::Committed);
        let b = find(&snapshot, "b.md");
        assert_eq!((b.group, b.letter), (Group::Modified, Letter::Modified));
        assert_eq!(b.check, Check::Unchecked);
    }

    #[test]
    fn load_names_the_git_dirs_of_a_plain_repository() {
        let repo = committed_repo();

        let snapshot = load(repo.path()).unwrap();

        assert_eq!(snapshot.git_dir, repo.path().join(".git"));
        assert_eq!(snapshot.common_dir, repo.path().join(".git"));
    }

    fn git_error() -> LoadError {
        LoadError::Git("bad object HEAD".to_string())
    }

    #[test]
    fn a_transient_git_error_is_read_again_once() {
        let mut attempts = 0;
        let result = retry_transient(|| {
            attempts += 1;
            if attempts == 1 {
                Err(git_error())
            } else {
                Ok(attempts)
            }
        });
        assert_eq!(result, Ok(2));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn a_lasting_git_error_is_returned() {
        let mut attempts = 0;
        let result: Result<(), LoadError> = retry_transient(|| {
            attempts += 1;
            Err(git_error())
        });
        assert_eq!(result, Err(git_error()));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn not_a_repository_is_not_read_again() {
        let mut attempts = 0;
        let result: Result<(), LoadError> = retry_transient(|| {
            attempts += 1;
            Err(LoadError::NotARepository)
        });
        assert_eq!(result, Err(LoadError::NotARepository));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn a_missing_git_is_not_read_again() {
        let mut attempts = 0;
        let result: Result<(), LoadError> = retry_transient(|| {
            attempts += 1;
            Err(LoadError::Missing)
        });
        assert_eq!(result, Err(LoadError::Missing));
        assert_eq!(attempts, 1);
    }

    /// Le fil du commiteur n'a pas le `TempRepo` (qui supprime le dépôt à la chute) : seulement son chemin.
    struct TempRepoRef(PathBuf);

    impl TempRepoRef {
        fn run(&self, args: &[&str]) {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.0)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
