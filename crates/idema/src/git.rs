//! Le lanceur Git unique (spec P3 §3.3, P3-8) : toute commande Git d'IdemA passe par `run`. Une liste blanche, un geste
//! par commande, `--no-optional-locks` sur les lectures pour ne jamais prendre `index.lock` devant un agent,
//! `GIT_TERMINAL_PROMPT=0` pour qu'une demande d'identifiants échoue au lieu de bloquer, et une ligne de journal par
//! commande lancée. Aucune dépendance à GPUI.

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use crate::hud;

/// Le geste de l'utilisateur (ou d'IdemA) au nom duquel une commande part. Chaque commande de la liste blanche appartient à un
/// seul geste : une lecture ne part pas sous un geste d'écriture, ni l'inverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    Read,
    Stage,
    Unstage,
    Commit,
    Push,
    /// `git init` du geste « Ouvrir un projet » de P1 (E56).
    InitProject,
}

impl Gesture {
    /// Le mot du journal.
    pub fn label(self) -> &'static str {
        match self {
            Gesture::Read => "read",
            Gesture::Stage => "stage",
            Gesture::Unstage => "unstage",
            Gesture::Commit => "commit",
            Gesture::Push => "push",
            Gesture::InitProject => "init",
        }
    }
}

/// Ce que Git a rendu. Un code non nul n'est pas une erreur du lanceur : c'est l'appelant qui juge.
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

#[derive(Debug)]
pub enum GitError {
    /// La commande n'est pas dans la liste blanche pour ce geste ; elle n'est pas lancée.
    NotAllowed(String),
    /// Le binaire `git` n'existe pas.
    Missing,
    Io(io::Error),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::NotAllowed(args) => write!(f, "git command not allowed: {args}"),
            GitError::Missing => write!(f, "git not found"),
            GitError::Io(error) => write!(f, "git failed to run: {error}"),
        }
    }
}

impl From<GitError> for io::Error {
    fn from(error: GitError) -> Self {
        match error {
            GitError::Io(error) => error,
            GitError::Missing => io::Error::new(io::ErrorKind::NotFound, "git not found"),
            GitError::NotAllowed(_) => io::Error::other(error.to_string()),
        }
    }
}

/// Lance `git -C <dir> <args>` si la liste blanche l'accepte pour ce geste. `stdin` est écrite telle quelle dans
/// l'entrée standard de Git (le message d'un commit, ou les chemins d'une case, `check_stdin`) et n'est jamais journalisée.
pub fn run(
    dir: &Path,
    gesture: Gesture,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<GitOutput, GitError> {
    run_with_env(dir, gesture, args, stdin, &[])
}

/// Comme `run`, avec des variables d'environnement en plus de celles de l'application : `LC_ALL=C` pour lire un message
/// d'erreur de Git sans dépendre de la langue de l'utilisateur. Les variables ne sont pas journalisées.
pub fn run_with_env(
    dir: &Path,
    gesture: Gesture,
    args: &[&str],
    stdin: Option<&[u8]>,
    env: &[(&str, &str)],
) -> Result<GitOutput, GitError> {
    let full_args = command_args(gesture, args)?;
    check_stdin(args, stdin)?;
    // Le journal dit les arguments réellement exécutés (`--no-optional-locks` compris), sans `-C` ni stdin : c'est ce
    // que relit AC-72.
    hud::log_debug(&format!(
        "git run gesture={} args={}",
        gesture.label(),
        full_args.join(" ")
    ));
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(&full_args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(env.iter().copied());
    let output = execute(command, stdin).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            GitError::Missing
        } else {
            GitError::Io(error)
        }
    })?;
    Ok(GitOutput {
        // Un processus tué par un signal n'a pas de code : il compte comme un échec.
        code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Lance la commande, avec `stdin` en entrée ou une entrée nulle. L'écriture se fait dans un fil à part : un message
/// plus gros que le tube ne doit pas bloquer Git qui, de son côté, remplirait sa sortie.
fn execute(mut command: Command, stdin: Option<&[u8]>) -> io::Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let Some(input) = stdin else {
        return command.stdin(Stdio::null()).output();
    };
    let mut child = command.stdin(Stdio::piped()).spawn()?;
    let mut pipe = child.stdin.take().expect("stdin was piped");
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || pipe.write_all(input));
        let output = child.wait_with_output();
        // Une commande qui ferme son entrée sans tout lire (échec précoce) casse le tube : ce n'est pas une erreur.
        let _ = writer.join();
        output
    })
}

/// Le préfixe qui fait lire un chemin tel quel à Git (R4) : sans lui, `[id].tsx` est un motif qui prendrait aussi
/// `i.tsx` et `d.tsx`.
pub(crate) const LITERAL: &str = ":(literal)";

/// L'option qui fait lire les chemins dans l'entrée standard (écart E67).
const PATHSPEC_FROM_STDIN: &str = "--pathspec-from-file=-";

/// Contrôle l'entrée standard d'une commande avant son lancement. Quand les chemins viennent de l'entrée standard, elle ne
/// doit pas être vide (Git stagerait tout le dépôt) et chaque entrée, terminée par un NUL, doit être un chemin lu tel
/// quel : `:(literal)` puis au moins un caractère. Les autres commandes (le message d'un commit) ne sont pas contrôlées.
pub(crate) fn check_stdin(args: &[&str], stdin: Option<&[u8]>) -> Result<(), GitError> {
    if !args.contains(&PATHSPEC_FROM_STDIN) {
        return Ok(());
    }
    let refused = || GitError::NotAllowed(args.join(" "));
    let input = stdin
        .filter(|input| !input.is_empty())
        .ok_or_else(refused)?;
    let entries = input.strip_suffix(&[0]).ok_or_else(refused)?;
    let literal = LITERAL.as_bytes();
    let all_literal = entries
        .split(|byte| *byte == 0)
        .all(|entry| entry.starts_with(literal) && entry.len() > literal.len());
    if all_literal { Ok(()) } else { Err(refused()) }
}

/// Les arguments à passer à `git -C <dir>`, ou `NotAllowed`. Pure : c'est la liste blanche. Les lectures gagnent
/// `--no-optional-locks` en tête.
pub(crate) fn command_args(gesture: Gesture, args: &[&str]) -> Result<Vec<String>, GitError> {
    if classify(args) != Some(gesture) {
        return Err(GitError::NotAllowed(args.join(" ")));
    }
    let mut full: Vec<String> = Vec::with_capacity(args.len() + 1);
    if gesture == Gesture::Read {
        full.push("--no-optional-locks".to_string());
    }
    full.extend(args.iter().map(|arg| arg.to_string()));
    Ok(full)
}

/// Le seul geste sous lequel la commande est permise, `None` hors de la liste blanche. La première chaîne est toujours
/// le nom de la commande : aucune option globale (`-c`, `--exec-path`…) ne passe avant.
fn classify(args: &[&str]) -> Option<Gesture> {
    let (name, rest) = args.split_first()?;
    match *name {
        "status" | "rev-parse" | "merge-base" | "rev-list" | "for-each-ref" | "ls-files" => {
            Some(Gesture::Read)
        }
        // `--output` écrirait le patch dans un fichier.
        "diff" => (!rest.iter().any(|arg| arg.starts_with("--output"))).then_some(Gesture::Read),
        "worktree" => (rest.first() == Some(&"list")).then_some(Gesture::Read),
        "symbolic-ref" => symbolic_ref_reads(rest).then_some(Gesture::Read),
        "remote" => rest
            .iter()
            .all(|arg| matches!(*arg, "-v" | "--verbose"))
            .then_some(Gesture::Read),
        "branch" => (rest == ["--show-current"]).then_some(Gesture::Read),
        // Les chemins des écritures d'index viennent de l'entrée standard, jamais des arguments (E67).
        "add" => paths_from_stdin(rest, &["-A"]).then_some(Gesture::Stage),
        "restore" => paths_from_stdin(rest, &["--staged"]).then_some(Gesture::Unstage),
        // `-f` n'est permis qu'avec `--cached` (R5) : il force le retrait de l'index d'un fichier stagé puis retouché,
        // sans jamais toucher au fichier de travail.
        "rm" => (paths_from_stdin(rest, &["--cached", "-r", "-q"])
            || paths_from_stdin(rest, &["--cached", "-r", "-q", "-f"]))
        .then_some(Gesture::Unstage),
        "commit" => (rest == ["-F", "-"]).then_some(Gesture::Commit),
        "push" => push_is_plain(rest).then_some(Gesture::Push),
        "init" => (rest == ["--quiet"]).then_some(Gesture::InitProject),
        _ => None,
    }
}

/// Les arguments sont exactement `options`, puis les chemins lus dans l'entrée standard, séparés par des NUL. Un
/// `--pathspec-from-file=<autre chose que ->` et des chemins en arguments n'y sont pas.
fn paths_from_stdin(rest: &[&str], options: &[&str]) -> bool {
    rest.len() == options.len() + 2
        && rest.starts_with(options)
        && rest[options.len()..] == [PATHSPEC_FROM_STDIN, "--pathspec-file-nul"]
}

/// `symbolic-ref` lit avec un seul argument qui n'est pas une option ; avec deux, il écrit `HEAD`. Les options sont
/// elles aussi bornées : `-d` supprime la référence avec un seul argument.
fn symbolic_ref_reads(rest: &[&str]) -> bool {
    let options_are_reads = rest
        .iter()
        .filter(|arg| arg.starts_with('-'))
        .all(|arg| matches!(*arg, "-q" | "--quiet" | "--short" | "--no-recurse"));
    let operands = rest.iter().filter(|arg| !arg.starts_with('-')).count();
    options_are_reads && operands == 1
}

/// `push --porcelain [-u] origin refs/heads/<b>:refs/heads/<b>` et rien d'autre (écart E64) : le remote est `origin`, la
/// référence est nommée des deux côtés avec le même nom, sans `+` ni motif. Sans refspec, la config de l'utilisateur déciderait
/// de ce qui part (`push.default=matching`, `remote.origin.push=+...`, `remote.origin.mirror`).
fn push_is_plain(rest: &[&str]) -> bool {
    let Some(rest) = rest.strip_prefix(&["--porcelain"]) else {
        return false;
    };
    let rest = rest.strip_prefix(&["-u"]).unwrap_or(rest);
    let [remote, refspec] = rest else {
        return false;
    };
    let Some((source, destination)) = refspec.split_once(':') else {
        return false;
    };
    *remote == "origin"
        && source == destination
        && source
            .strip_prefix("refs/heads/")
            .is_some_and(is_plain_branch_name)
}

/// Un nom de branche sans option, sans motif ni séparateur de refspec, sans blanc.
fn is_plain_branch_name(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with(['-', '+'])
        && !branch.contains([':', '*', '?', '['])
        && !branch.chars().any(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    const WRITES: [Gesture; 5] = [
        Gesture::Stage,
        Gesture::Unstage,
        Gesture::Commit,
        Gesture::Push,
        Gesture::InitProject,
    ];

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("idema-git-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn words(line: &str) -> Vec<&str> {
        line.split(' ').collect()
    }

    fn is_allowed(gesture: Gesture, line: &str) -> bool {
        command_args(gesture, &words(line)).is_ok()
    }

    #[test]
    fn reads_get_the_no_optional_locks_flag() {
        let args = command_args(Gesture::Read, &["status", "--porcelain=v2"]).unwrap();
        assert_eq!(&args[..2], ["--no-optional-locks", "status"]);
        assert_eq!(args.len(), 3);
    }

    const STAGE_FROM_STDIN: &str = "add -A --pathspec-from-file=- --pathspec-file-nul";

    #[test]
    fn writes_never_get_the_no_optional_locks_flag() {
        let args = command_args(Gesture::Stage, &words(STAGE_FROM_STDIN)).unwrap();
        assert_eq!(args, words(STAGE_FROM_STDIN));
    }

    #[test]
    fn pathspec_stdin_must_be_literal_and_non_empty() {
        let args = words(STAGE_FROM_STDIN);
        let refused: [Option<&[u8]>; 7] = [
            None,
            Some(b""),
            Some(b"a\0"),
            Some(b":(literal)a\0b\0"),
            // Une entrée sans NUL final, ou réduite au préfixe, n'est pas une liste de chemins.
            Some(b":(literal)a"),
            Some(b":(literal)\0"),
            Some(b"\0"),
        ];
        for stdin in refused {
            assert!(
                matches!(check_stdin(&args, stdin), Err(GitError::NotAllowed(_))),
                "accepted: {stdin:?}"
            );
        }
        assert!(check_stdin(&args, Some(b":(literal)a\0:(literal)b c\0")).is_ok());
    }

    #[test]
    fn a_stdin_without_the_pathspec_flag_is_not_checked() {
        // Le message d'un commit passe lui aussi par l'entrée standard : rien à contrôler.
        assert!(check_stdin(&["commit", "-F", "-"], Some(b"message")).is_ok());
        assert!(check_stdin(&["status"], None).is_ok());
    }

    #[test]
    fn a_pathspec_write_with_a_bad_stdin_never_runs() {
        let dir = temp_dir("bad-stdin");
        run(&dir, Gesture::InitProject, &["init", "--quiet"], None).unwrap();
        fs::write(dir.join("a.txt"), "untracked\n").unwrap();

        // Sans le contrôle, Git stagerait tout le dépôt sur une entrée vide.
        let empty = run(&dir, Gesture::Stage, &words(STAGE_FROM_STDIN), Some(b""));
        let bare = run(
            &dir,
            Gesture::Stage,
            &words(STAGE_FROM_STDIN),
            Some(b"a.txt\0"),
        );

        assert!(matches!(empty, Err(GitError::NotAllowed(_))));
        assert!(matches!(bare, Err(GitError::NotAllowed(_))));
        let staged = run(
            &dir,
            Gesture::Read,
            &["diff", "--cached", "--name-only"],
            None,
        )
        .unwrap();
        assert!(staged.stdout.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn allowed_reads() {
        let reads = [
            "status --porcelain=v2 -z --untracked-files=all --branch",
            "diff",
            "diff --no-color --no-ext-diff -M --unified=1000000 abc -- a b",
            "diff --name-status -z abc HEAD",
            "diff --no-index -- /dev/null a",
            "diff --no-color --no-index --unified=1000000 -- /dev/null a",
            "worktree list --porcelain",
            "rev-parse --show-toplevel",
            "rev-parse --is-inside-work-tree",
            "merge-base main HEAD",
            "symbolic-ref -q refs/remotes/origin/HEAD",
            "symbolic-ref -q --short refs/remotes/origin/HEAD",
            "for-each-ref refs/heads",
            "for-each-ref --format=%(refname:short) refs/heads",
            "rev-list --count a..b",
            "remote",
            "branch --show-current",
            "ls-files -z --cached",
            "ls-files -z --cached --others --exclude-standard",
            "ls-files -z --others --ignored --exclude-standard --directory",
            "diff --cached --no-color --no-ext-diff --no-textconv --stat=120",
            "diff --cached --no-color --no-ext-diff --no-textconv",
            "diff --cached --no-color --no-ext-diff --no-textconv -M",
        ];
        for line in reads {
            assert!(is_allowed(Gesture::Read, line), "read refused: {line}");
            for gesture in WRITES {
                assert!(
                    !is_allowed(gesture, line),
                    "read accepted under {gesture:?}: {line}"
                );
            }
        }
    }

    #[test]
    fn writes_need_their_gesture() {
        let writes = [
            (
                Gesture::Stage,
                "add -A --pathspec-from-file=- --pathspec-file-nul",
            ),
            (
                Gesture::Unstage,
                "restore --staged --pathspec-from-file=- --pathspec-file-nul",
            ),
            (
                Gesture::Unstage,
                "rm --cached -r -q --pathspec-from-file=- --pathspec-file-nul",
            ),
            (
                Gesture::Unstage,
                "rm --cached -r -q -f --pathspec-from-file=- --pathspec-file-nul",
            ),
            (Gesture::Commit, "commit -F -"),
            (
                Gesture::Push,
                "push --porcelain origin refs/heads/main:refs/heads/main",
            ),
            (
                Gesture::Push,
                "push --porcelain -u origin refs/heads/feat/idema-p3:refs/heads/feat/idema-p3",
            ),
            (Gesture::InitProject, "init --quiet"),
        ];
        for (gesture, line) in writes {
            assert!(is_allowed(gesture, line), "write refused: {line}");
            for other in WRITES.into_iter().chain([Gesture::Read]) {
                if other != gesture {
                    assert!(
                        !is_allowed(other, line),
                        "write accepted under {other:?}: {line}"
                    );
                }
            }
        }
    }

    #[test]
    fn destructive_commands_are_refused() {
        let refused = [
            "reset --hard",
            "checkout -- a",
            "restore -- a",
            "restore --staged --worktree -- a",
            "restore --staged",
            "clean -fd",
            "stash",
            "commit --amend -F -",
            "commit -m x",
            "push --force",
            "push -f",
            "push --force-with-lease",
            "push origin +main",
            "push origin :main",
            "push origin main",
            "push --delete origin x",
            "push --mirror",
            "push -u origin +main",
            "push -u origin :main",
            "push -u origin main:other",
            "push -u origin --force",
            "branch -D x",
            "branch",
            "symbolic-ref HEAD refs/heads/x",
            "symbolic-ref -d HEAD",
            "symbolic-ref",
            "worktree remove x",
            "worktree add x",
            "remote add x y",
            "remote remove origin",
            // Les chemins en arguments sont sortis de la liste blanche (E67) ; un fichier de chemins non plus.
            "add -A -- a",
            "add -A -- a b",
            "add -A --pathspec-from-file=paths.txt",
            "add -A --pathspec-from-file=x --pathspec-file-nul",
            "add -A --pathspec-from-file=- --pathspec-file-nul a",
            "add -A --pathspec-from-file=-",
            "add --pathspec-from-file=- --pathspec-file-nul",
            "restore --staged -- a",
            "restore --staged --pathspec-from-file=paths.txt --pathspec-file-nul",
            "restore --staged --pathspec-from-file=-",
            "restore --staged --worktree --pathspec-from-file=- --pathspec-file-nul",
            "restore --pathspec-from-file=- --pathspec-file-nul",
            "rm --cached -r -q -- a",
            "rm --cached -r -q -f -- a b",
            "rm --cached -r -q -f --pathspec-from-file=paths.txt --pathspec-file-nul",
            "rm -r -q -f --pathspec-from-file=- --pathspec-file-nul",
            "rm --pathspec-from-file=- --pathspec-file-nul",
            "rm a",
            "rm -r --cached a",
            "rm -f -- a",
            "rm -f a",
            "rm -r -q -f -- a",
            "rm -f --cached -r -q -- a",
            "rm --cached -r -q -f",
            "rm --cached -r -q -f --",
            "rm --cached -r -q -f -f -- a",
            "add a",
            "add -A",
            "add -A --",
            "diff --output=x a",
            "-c core.pager=x status",
            "",
        ];
        let gestures = WRITES.into_iter().chain([Gesture::Read]);
        for gesture in gestures {
            for line in refused {
                let args: Vec<&str> = if line.is_empty() {
                    Vec::new()
                } else {
                    words(line)
                };
                assert!(
                    matches!(command_args(gesture, &args), Err(GitError::NotAllowed(_))),
                    "accepted under {gesture:?}: {line}"
                );
            }
        }
    }

    #[test]
    fn a_refused_command_never_runs() {
        let dir = temp_dir("refused");
        run(&dir, Gesture::InitProject, &["init", "--quiet"], None).unwrap();
        fs::write(dir.join("a.txt"), "staged\n").unwrap();
        let staged = run(
            &dir,
            Gesture::Stage,
            &words(STAGE_FROM_STDIN),
            Some(b":(literal)a.txt\0"),
        )
        .unwrap();
        assert!(staged.success());
        fs::write(dir.join("a.txt"), "modified after staging\n").unwrap();

        let refused = run(&dir, Gesture::Read, &["reset", "--hard"], None);

        assert!(matches!(refused, Err(GitError::NotAllowed(_))));
        assert_eq!(
            fs::read_to_string(dir.join("a.txt")).unwrap(),
            "modified after staging\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_command_is_ok_with_its_code_and_stderr() {
        let dir = temp_dir("failing");
        let output = run(&dir, Gesture::Read, &["rev-parse", "--show-toplevel"], None).unwrap();
        assert!(!output.success());
        assert!(!output.stderr.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_returns_the_raw_stdout() {
        let dir = temp_dir("stdout");
        run(&dir, Gesture::InitProject, &["init", "--quiet"], None).unwrap();
        let output = run(
            &dir,
            Gesture::Read,
            &["rev-parse", "--is-inside-work-tree"],
            None,
        )
        .unwrap();
        assert!(output.success());
        assert_eq!(output.stdout, b"true\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stdin_reaches_the_process_even_when_larger_than_a_pipe() {
        let input = vec![b'x'; 1_000_000];
        let output = execute(Command::new("cat"), Some(&input)).unwrap();
        assert_eq!(output.stdout, input);
    }

    #[test]
    fn no_stdin_means_a_null_input() {
        let output = execute(Command::new("cat"), None).unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
    }
}
