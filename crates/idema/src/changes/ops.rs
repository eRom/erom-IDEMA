//! Les écritures du volet Changes (spec P3 §4.4, §4.6) : cocher, décocher, commiter, pousser. Chacune passe par le
//! lanceur sous son geste, et chaque échec est classé par `errors::git_failure`. Le message d'un commit ne va qu'à
//! l'entrée standard de Git : il n'est ni journalisé, ni gardé dans l'erreur.

use std::path::{Path, PathBuf};

use crate::errors::{GitFailure, git_failure};
use crate::git::{self, Gesture};

/// Un échec classé, avec la sortie brute de Git pour la ligne de détail de la bulle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFailureRaw {
    pub failure: GitFailure,
    pub raw: String,
}

pub(crate) use crate::git::LITERAL;

/// Coche : met en index ces chemins, relatifs à la racine du dépôt, une suppression et un dossier compris. Les chemins
/// partent par l'entrée standard (E67) : en arguments, des dizaines de milliers de fichiers non suivis dépassent la limite
/// de macOS (E2BIG). Une liste vide ne lance rien : Git stagerait tout le dépôt sur une entrée vide.
pub fn stage(root: &Path, paths: &[PathBuf]) -> Result<(), GitFailureRaw> {
    if paths.is_empty() {
        return Ok(());
    }
    let input = literal_paths(paths)?;
    run_write(root, Gesture::Stage, &STAGE_ARGS, Some(&input))
}

const STAGE_ARGS: [&str; 4] = ["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"];
const RESTORE_ARGS: [&str; 4] = [
    "restore",
    "--staged",
    "--pathspec-from-file=-",
    "--pathspec-file-nul",
];
const REMOVE_CACHED_ARGS: [&str; 7] = [
    "rm",
    "--cached",
    "-r",
    "-q",
    "-f",
    "--pathspec-from-file=-",
    "--pathspec-file-nul",
];

/// Décoche : retire ces chemins de l'index sans toucher aux fichiers de travail. Sans aucun commit, `restore --staged`
/// n'a pas de HEAD où puiser : c'est `rm --cached` qui vide l'index, avec `-f` pour qu'un fichier stagé puis retouché
/// ne soit pas refusé (R5).
///
/// HEAD est relu ici, juste avant d'écrire, et non repris de la dernière lecture du dépôt : si un agent a fait le premier
/// commit entre-temps, `rm --cached` retirerait de l'index un fichier que HEAD contient, et le commit suivant le
/// supprimerait.
///
/// L'appelant passe tous les chemins du changement : pour un renommage, l'ancien (`orig_path`) et le nouveau (`path`).
/// Avec le seul nouveau chemin, l'ancien resterait supprimé dans l'index et le nouveau apparaîtrait non suivi (R6).
pub fn unstage(root: &Path, paths: &[PathBuf]) -> Result<(), GitFailureRaw> {
    if paths.is_empty() {
        return Ok(());
    }
    let input = literal_paths(paths)?;
    let args: &[&str] = if head_exists(root)? {
        &RESTORE_ARGS
    } else {
        &REMOVE_CACHED_ARGS
    };
    run_write(root, Gesture::Unstage, args, Some(&input))
}

/// HEAD désigne-t-il un commit ? Une lecture (`rev-parse --verify -q HEAD` sort en 1, sans rien dire, sur un dépôt sans
/// commit). Une autre sortie non nulle, ou un Git qui ne part pas, est un échec : mieux vaut ne rien écrire que deviner.
fn head_exists(root: &Path) -> Result<bool, GitFailureRaw> {
    let output = git::run(
        root,
        Gesture::Read,
        &["rev-parse", "--verify", "-q", "HEAD"],
        None,
    )
    .map_err(|error| GitFailureRaw {
        failure: GitFailure::Other,
        raw: error.to_string(),
    })?;
    match output.code {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(GitFailureRaw {
            failure: GitFailure::Other,
            raw: output.stderr.trim().to_string(),
        }),
    }
}

/// Commite l'index. Le message passe par l'entrée standard (`-F -`) ; les hooks et la signature de l'utilisateur s'appliquent.
pub fn commit(root: &Path, message: &str) -> Result<(), GitFailureRaw> {
    run_write(
        root,
        Gesture::Commit,
        &["commit", "-F", "-"],
        Some(message.as_bytes()),
    )
}

/// Pousse la branche, nommée des deux côtés : `refs/heads/<branche>:refs/heads/<branche>` vers `origin`. Sans amont, `-u`
/// la publie et la suit. Aucune option ni config de l'utilisateur ne décide de ce qui part (`push.default`, `remote.origin.push`,
/// `remote.origin.mirror` ne comptent pas quand le refspec est donné), et rien n'est jamais forcé (écart E64).
pub fn push(root: &Path, branch: &str, has_upstream: bool) -> Result<(), GitFailureRaw> {
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let mut args = vec!["push", "--porcelain"];
    if !has_upstream {
        args.push("-u");
    }
    args.extend(["origin", refspec.as_str()]);
    run_write(root, Gesture::Push, &args, None)
}

/// L'entrée standard de Git pour ces chemins : chacun préfixé de `:(literal)` et terminé par un NUL. Un chemin qui n'est
/// pas de l'UTF-8 ne peut pas passer par le lanceur : il est refusé avant de lancer quoi que ce soit.
fn literal_paths(paths: &[PathBuf]) -> Result<Vec<u8>, GitFailureRaw> {
    let mut input = Vec::new();
    for path in paths {
        let text = path.to_str().ok_or_else(|| GitFailureRaw {
            failure: GitFailure::Other,
            raw: format!("path is not valid UTF-8: {}", path.display()),
        })?;
        input.extend_from_slice(LITERAL.as_bytes());
        input.extend_from_slice(text.as_bytes());
        input.push(0);
    }
    Ok(input)
}

/// Lance une écriture : un code non nul devient un échec classé, avec la sortie de Git (erreur puis standard) en brut.
fn run_write(
    root: &Path,
    gesture: Gesture,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<(), GitFailureRaw> {
    let output = git::run(root, gesture, args, stdin).map_err(|error| GitFailureRaw {
        failure: GitFailure::Other,
        raw: error.to_string(),
    })?;
    if output.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let failure = git_failure(gesture, &output.stderr, &stdout);
    let raw = [output.stderr.trim(), stdout.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Err(GitFailureRaw { failure, raw })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::model::load;
    use crate::changes::test_repo::{TempRepo, committed_repo, fixture_repo};
    use crate::changes::{Change, Check, Snapshot};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Un remote nu local, supprimé à la chute : aucun push des tests ne quitte la machine.
    struct BareRemote {
        dir: PathBuf,
    }

    impl BareRemote {
        fn new() -> BareRemote {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("idema-ops-remote-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let remote = BareRemote {
                dir: dir.canonicalize().unwrap(),
            };
            remote.git(&["init", "--bare", "--quiet", "-b", "main"]);
            remote
        }

        fn git(&self, args: &[&str]) -> String {
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
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        fn main_ref(&self) -> String {
            self.git(&["rev-parse", "refs/heads/main"])
        }

        fn path_text(&self) -> &str {
            self.dir.to_str().unwrap()
        }
    }

    impl Drop for BareRemote {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    fn change<'a>(snapshot: &'a Snapshot, path: &str) -> &'a Change {
        snapshot
            .changes
            .iter()
            .find(|change| change.path == Path::new(path))
            .unwrap_or_else(|| panic!("{path} is not listed: {:?}", snapshot.changes))
    }

    fn check_of(repo: &TempRepo, path: &str) -> Check {
        change(&load(repo.path()).unwrap(), path).check
    }

    /// Un dépôt avec un `origin` nu local, `main` publiée et suivie.
    fn published_repo() -> (TempRepo, BareRemote) {
        let repo = committed_repo();
        let remote = BareRemote::new();
        repo.git(&["remote", "add", "origin", remote.path_text()]);
        repo.git(&["push", "--quiet", "-u", "origin", "main"]);
        (repo, remote)
    }

    #[test]
    fn stage_and_unstage_a_path_with_spaces() {
        let repo = fixture_repo();
        repo.write("notes/a b.md", b"espaces\n");
        assert_eq!(check_of(&repo, "notes/a b.md"), Check::Unchecked);

        stage(repo.path(), &paths(&["notes/a b.md"])).unwrap();
        assert_eq!(check_of(&repo, "notes/a b.md"), Check::Checked);

        unstage(repo.path(), &paths(&["notes/a b.md"])).unwrap();
        assert_eq!(check_of(&repo, "notes/a b.md"), Check::Unchecked);
    }

    #[test]
    fn a_glob_like_name_is_staged_literally() {
        let repo = committed_repo();
        repo.write("app/[id].tsx", b"page\n");
        repo.write("app/i.tsx", b"i\n");

        stage(repo.path(), &paths(&["app/[id].tsx"])).unwrap();

        assert_eq!(check_of(&repo, "app/[id].tsx"), Check::Checked);
        assert_eq!(check_of(&repo, "app/i.tsx"), Check::Unchecked);

        unstage(repo.path(), &paths(&["app/[id].tsx"])).unwrap();
        assert_eq!(check_of(&repo, "app/[id].tsx"), Check::Unchecked);
    }

    #[test]
    fn staging_a_folder_stages_its_files() {
        let repo = fixture_repo();

        stage(repo.path(), &paths(&["notes"])).unwrap();

        assert_eq!(check_of(&repo, "notes/brouillon.md"), Check::Checked);
        assert_eq!(check_of(&repo, "captures/a.png"), Check::Unchecked);
    }

    #[test]
    fn unstaging_a_deletion_keeps_the_file_deleted() {
        let repo = fixture_repo();
        stage(repo.path(), &paths(&["old.txt"])).unwrap();
        assert_eq!(check_of(&repo, "old.txt"), Check::Checked);

        unstage(repo.path(), &paths(&["old.txt"])).unwrap();

        assert_eq!(check_of(&repo, "old.txt"), Check::Unchecked);
        assert!(!repo.path().join("old.txt").exists());
        assert!(repo.git(&["status", "--porcelain"]).contains(" D old.txt"));
    }

    #[test]
    fn commit_takes_only_the_index() {
        let repo = fixture_repo();
        stage(repo.path(), &paths(&["README.md", "notes"])).unwrap();
        let message = "Ajoute les notes\n\nDeuxième ligne : é à ü, 日本";

        commit(repo.path(), message).unwrap();

        let files = repo.git(&["log", "-1", "--name-only", "--format="]);
        let mut listed: Vec<&str> = files.lines().filter(|line| !line.is_empty()).collect();
        listed.sort();
        assert_eq!(listed, ["README.md", "notes/brouillon.md"]);
        assert_eq!(repo.git(&["log", "-1", "--format=%B"]).trim_end(), message);
        // Le reste du travail n'est pas commité.
        assert_eq!(check_of(&repo, "captures/a.png"), Check::Unchecked);
    }

    #[test]
    fn a_failing_hook_keeps_head() {
        let repo = fixture_repo();
        let hook = repo.path().join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\necho 'hook says no' >&2\nexit 1\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        stage(repo.path(), &paths(&["README.md"])).unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]);

        let error = commit(repo.path(), "Refusé").unwrap_err();

        assert_eq!(error.failure, GitFailure::CommitFailed);
        assert!(error.raw.contains("hook says no"), "{}", error.raw);
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);
        assert_eq!(check_of(&repo, "README.md"), Check::Checked);
    }

    #[test]
    fn the_first_commit_of_an_unborn_repository() {
        let repo = TempRepo::init();
        repo.write("one.md", b"1\n");

        stage(repo.path(), &paths(&["one.md"])).unwrap();
        assert_eq!(check_of(&repo, "one.md"), Check::Checked);
        unstage(repo.path(), &paths(&["one.md"])).unwrap();
        assert_eq!(check_of(&repo, "one.md"), Check::Unchecked);
        stage(repo.path(), &paths(&["one.md"])).unwrap();
        commit(repo.path(), "First commit").unwrap();

        assert!(!repo.git(&["rev-parse", "HEAD"]).is_empty());
        assert_eq!(
            repo.git(&["log", "-1", "--format=%s"]).trim(),
            "First commit"
        );
    }

    #[test]
    fn unstaging_an_edited_file_in_an_unborn_repository() {
        let repo = TempRepo::init();
        repo.write("f.md", b"staged\n");
        stage(repo.path(), &paths(&["f.md"])).unwrap();
        repo.write("f.md", b"edited after staging\n");

        unstage(repo.path(), &paths(&["f.md"])).unwrap();

        assert_eq!(check_of(&repo, "f.md"), Check::Unchecked);
        assert_eq!(repo.git(&["status", "--porcelain"]).trim(), "?? f.md");
        assert_eq!(
            fs::read_to_string(repo.path().join("f.md")).unwrap(),
            "edited after staging\n"
        );
    }

    #[test]
    fn unstaging_after_an_agent_made_the_first_commit_never_stages_a_deletion() {
        let repo = TempRepo::init();
        repo.write("f.md", b"staged\n");
        stage(repo.path(), &paths(&["f.md"])).unwrap();
        // La lecture du volet dit « aucun commit » ...
        assert!(!load(repo.path()).unwrap().branch.has_commits);
        // ... puis un agent fait le premier commit, avant que le décochage parte.
        repo.git(&["commit", "--quiet", "-m", "First commit by an agent"]);

        unstage(repo.path(), &paths(&["f.md"])).unwrap();

        // `rm --cached` aurait mis `D f.md` dans l'index ; `restore --staged` ne change rien, f.md est dans HEAD.
        assert_eq!(repo.git(&["status", "--porcelain"]).trim(), "");
        assert!(repo.path().join("f.md").exists());
    }

    #[test]
    fn unstaging_a_rename_restores_both_paths() {
        let repo = committed_repo();
        repo.git(&["mv", "old.txt", "renamed.txt"]);
        let renamed = change(&load(repo.path()).unwrap(), "renamed.txt").clone();
        assert_eq!(renamed.check, Check::Checked);
        let orig = renamed
            .orig_path
            .clone()
            .expect("a rename keeps its old path");

        unstage(repo.path(), &[orig, renamed.path]).unwrap();

        let status = repo.git(&["status", "--porcelain"]);
        let lines: Vec<&str> = status.lines().collect();
        assert!(lines.contains(&" D old.txt"), "{status}");
        assert!(lines.contains(&"?? renamed.txt"), "{status}");
        assert_eq!(lines.len(), 2, "nothing may stay staged: {status}");
    }

    #[test]
    fn push_with_upstream_publishes_and_clears_ahead() {
        let (repo, remote) = published_repo();
        repo.write("a.md", b"a\n");
        repo.commit_all("Add a");
        assert_eq!(load(repo.path()).unwrap().branch.ahead, 1);

        push(repo.path(), "main", true).unwrap();

        assert_eq!(remote.main_ref(), repo.git(&["rev-parse", "HEAD"]).trim());
        assert_eq!(load(repo.path()).unwrap().branch.ahead, 0);
    }

    #[test]
    fn push_without_upstream_sets_it() {
        let repo = committed_repo();
        let remote = BareRemote::new();
        repo.git(&["remote", "add", "origin", remote.path_text()]);
        assert_eq!(load(repo.path()).unwrap().branch.upstream, None);

        push(repo.path(), "main", false).unwrap();

        assert_eq!(remote.main_ref(), repo.git(&["rev-parse", "HEAD"]).trim());
        let snapshot = load(repo.path()).unwrap();
        assert_eq!(snapshot.branch.upstream.as_deref(), Some("origin/main"));
        assert_eq!(snapshot.branch.ahead, 0);
    }

    /// Le remote avance de son côté (un commit publié), puis le local s'écarte de lui. Rend la tête du remote.
    fn diverge_from_remote(repo: &TempRepo, remote: &BareRemote) -> String {
        repo.write("remote.md", b"remote\n");
        repo.commit_all("Remote side");
        repo.git(&["push", "--quiet"]);
        let moved = remote.main_ref();
        repo.git(&["reset", "--quiet", "--hard", "HEAD~1"]);
        repo.write("local.md", b"local\n");
        repo.commit_all("Local side");
        moved
    }

    #[test]
    fn push_refused_when_remote_moved() {
        let (repo, remote) = published_repo();
        let moved = diverge_from_remote(&repo, &remote);

        let error = push(repo.path(), "main", true).unwrap_err();

        assert_eq!(error.failure, GitFailure::PushRejected);
        assert!(error.raw.contains("rejected"), "{}", error.raw);
        assert_eq!(remote.main_ref(), moved);
    }

    #[test]
    fn a_forcing_push_config_cannot_force_the_push() {
        let (repo, remote) = published_repo();
        let moved = diverge_from_remote(&repo, &remote);
        // Avec cette config, un `git push` sans refspec écraserait le commit du remote.
        repo.git(&["config", "remote.origin.push", "+refs/heads/*:refs/heads/*"]);

        let error = push(repo.path(), "main", true).unwrap_err();

        assert_eq!(error.failure, GitFailure::PushRejected);
        assert_eq!(remote.main_ref(), moved, "the remote head must not move");
    }

    #[test]
    fn push_default_matching_pushes_only_the_branch() {
        let (repo, remote) = published_repo();
        repo.git(&["config", "push.default", "matching"]);
        repo.git(&["switch", "--quiet", "-c", "other"]);
        repo.write("other.md", b"1\n");
        repo.commit_all("Other 1");
        repo.git(&["push", "--quiet", "-u", "origin", "other"]);
        let other_on_remote = remote.git(&["rev-parse", "refs/heads/other"]);
        repo.write("other.md", b"2\n");
        repo.commit_all("Other 2");
        repo.git(&["switch", "--quiet", "main"]);
        repo.write("main.md", b"m\n");
        repo.commit_all("Main 2");

        push(repo.path(), "main", true).unwrap();

        assert_eq!(remote.main_ref(), repo.git(&["rev-parse", "HEAD"]).trim());
        assert_eq!(
            remote.git(&["rev-parse", "refs/heads/other"]),
            other_on_remote,
            "another branch must stay where it was"
        );
    }

    #[test]
    fn a_mirror_config_sends_nothing() {
        let (repo, remote) = published_repo();
        repo.git(&["branch", "extra"]);
        repo.write("a.md", b"a\n");
        repo.commit_all("Add a");
        repo.git(&["config", "remote.origin.mirror", "true"]);
        let before = remote.main_ref();

        // Git refuse de mêler le miroir et un refspec : l'échec est le bon résultat, rien ne part.
        assert!(push(repo.path(), "main", true).is_err());

        assert_eq!(remote.main_ref(), before);
        assert!(remote.git(&["branch", "--list", "extra"]).is_empty());
    }

    #[test]
    fn publishing_a_branch_tracks_the_same_name_on_origin() {
        let (repo, remote) = published_repo();
        repo.git(&["switch", "--quiet", "-c", "feat/x"]);
        repo.write("x.md", b"x\n");
        repo.commit_all("Add x");

        push(repo.path(), "feat/x", false).unwrap();

        assert_eq!(
            remote.git(&["rev-parse", "refs/heads/feat/x"]),
            repo.git(&["rev-parse", "HEAD"]).trim()
        );
        let snapshot = load(repo.path()).unwrap();
        assert_eq!(snapshot.branch.upstream.as_deref(), Some("origin/feat/x"));
        assert_eq!(snapshot.branch.ahead, 0);
    }

    #[test]
    fn a_push_to_a_missing_remote_is_a_plain_failure() {
        let repo = committed_repo();
        repo.git(&["remote", "add", "origin", "/nonexistent/idema-remote"]);

        let error = push(repo.path(), "main", false).unwrap_err();

        assert_eq!(error.failure, GitFailure::PushFailed);
        assert!(!error.raw.is_empty());
    }

    #[test]
    fn a_locked_index_is_busy() {
        let repo = fixture_repo();
        let lock = repo.path().join(".git/index.lock");
        fs::write(&lock, "").unwrap();

        let result = stage(repo.path(), &paths(&["README.md"]));
        let still_there = lock.exists();
        // Le verrou est celui du test : on le retire pour ne pas laisser le dépôt jetable bloqué.
        fs::remove_file(&lock).unwrap();

        assert_eq!(result.unwrap_err().failure, GitFailure::Busy);
        assert!(still_there, "the lock must never be removed by the writer");
    }

    #[test]
    fn staging_no_path_runs_nothing() {
        let repo = fixture_repo();

        // Git stagerait tout le dépôt sur une entrée vide : une liste vide ne doit rien lancer.
        stage(repo.path(), &[]).unwrap();
        unstage(repo.path(), &[]).unwrap();

        assert!(repo.git(&["diff", "--cached", "--name-only"]).is_empty());
    }

    #[test]
    fn staging_more_paths_than_the_argument_limit() {
        let repo = committed_repo();
        // 20 000 noms de 60 octets : plus de 1,2 Mo de chemins, au-delà de la limite d'arguments de macOS (E2BIG).
        let names: Vec<String> = (0..20_000)
            .map(|n| format!("many/{n:05}-{}.txt", "x".repeat(46)))
            .collect();
        for name in &names {
            repo.write(name, b"x\n");
        }
        assert!(names.iter().all(|name| name.len() >= 60));
        let all: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();

        stage(repo.path(), &all).unwrap();

        let staged = repo.git(&["diff", "--cached", "--name-only"]);
        let mut staged: Vec<&str> = staged.lines().collect();
        let mut wanted: Vec<&str> = names.iter().map(String::as_str).collect();
        staged.sort_unstable();
        wanted.sort_unstable();
        assert_eq!(staged, wanted);

        unstage(repo.path(), &all).unwrap();

        assert!(repo.git(&["diff", "--cached", "--name-only"]).is_empty());
    }

    #[test]
    fn a_path_that_is_not_utf8_is_refused_without_running_git() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let repo = fixture_repo();
        let bad = PathBuf::from(OsStr::from_bytes(b"caf\xe9.txt"));

        let error = stage(repo.path(), &[bad]).unwrap_err();

        assert_eq!(error.failure, GitFailure::Other);
        assert!(error.raw.contains("UTF-8"), "{}", error.raw);
    }
}
