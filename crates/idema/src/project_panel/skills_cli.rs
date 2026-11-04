//! Le CLI `skills` (lancé par `bunx` dans le dossier du projet) et la recherche skills.sh : la seule porte du volet
//! Projet vers le monde extérieur pour l'onglet Compétences.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use regex::Regex;

use super::instructions::io_error_text;
use super::is_home;
use super::skills::{SearchHit, parse_search, search_url};
use crate::files::panel::home_dir;
use crate::hud::log_event;

/// Le shell de repli quand `$SHELL` est absent, relatif ou inutilisable.
const FALLBACK_SHELL: &str = "/bin/zsh";

/// Le seul curl que l'on lance : celui du système, jamais celui du `PATH`.
const CURL: &str = "/usr/bin/curl";

/// Les arguments de curl avant l'URL. `-q` en tout premier ignore le `~/.curlrc` de l'utilisateur ; `-w` ajoute, après
/// le corps, une ligne avec le code HTTP.
const CURL_ARGS: [&str; 8] = [
    "-q",
    "-sS",
    "--connect-timeout",
    "5",
    "--max-time",
    "10",
    "-w",
    "\n%{http_code}",
];

const DEFAULT_API_BASE: &str = "https://skills.sh";

/// Ce que `zsh -i` écrit sur stderr quand il n'a pas de terminal : du bruit, jamais une erreur du CLI.
const ZLE_NOISE: &str = "can't change option: zle";

/// Les débuts de ligne que `bunx` écrit sur stderr en installant le paquet du CLI : de la progression, jamais une
/// erreur. Relevés sur un appel réel le 2026-11-03 (`Resolving dependencies`, `Resolved, downloaded and extracted [2]`) ;
/// `Saved lockfile` est le message de bun pour un lockfile écrit.
const BUN_INSTALL_NOISE: [&str; 3] = [
    "Resolving dependencies",
    "Resolved, downloaded and extracted",
    "Saved lockfile",
];

/// La durée maximale d'un appel au CLI avant qu'on l'arrête : choix d'IdemA, un clone du CLI a ses propres délais ;
/// à ratifier.
const CLI_TIMEOUT: Duration = Duration::from_secs(180);

/// Le délai laissé au groupe de processus pour quitter sur SIGTERM avant le SIGKILL.
const TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// Un fichier ordinaire exécutable par son propriétaire, un groupe ou tout le monde.
fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Le shell de connexion : `$SHELL` s'il est absolu et exécutable, sinon `/bin/zsh`.
pub fn login_shell(env_shell: Option<&OsStr>) -> PathBuf {
    env_shell
        .map(Path::new)
        .filter(|path| path.is_absolute() && is_executable(path))
        .map_or_else(|| PathBuf::from(FALLBACK_SHELL), Path::to_path_buf)
}

/// `IDEMA_BUNX_BIN` s'il est posé, sinon `~/.bun/bin/bunx` s'il est exécutable (un `home` relatif est ignoré), sinon
/// `bunx` (cherché par le shell).
pub fn bunx_program(home: &Path, env_override: Option<&OsStr>) -> OsString {
    if let Some(custom) = env_override.filter(|value| !value.is_empty()) {
        return custom.to_owned();
    }
    let installed = home.join(".bun").join("bin").join("bunx");
    if home.is_absolute() && is_executable(&installed) {
        installed.into_os_string()
    } else {
        OsString::from("bunx")
    }
}

/// `shell -l -i -c 'exec "$0" "$@"' <bunx> <args…>` dans le dossier du projet, stdin fermé. `args` commence par
/// `skills` : c'est le premier argument de `bunx`. Le texte de `-c` est une constante : le programme et chaque
/// argument voyagent comme `$0` et `$@`, jamais réinterprétés. Comme le reste du dépôt (`idema-tmux/src/server.rs`),
/// `-l -i` donne au shell le `PATH` du terminal de l'utilisateur même quand IdemA est lancé depuis Raycast ou le
/// Finder.
pub fn skills_command(shell: &Path, bunx: &OsStr, project: &Path, args: &[String]) -> Command {
    let mut command = Command::new(shell);
    command
        .args(["-l", "-i", "-c", r#"exec "$0" "$@""#])
        .arg(bunx)
        .args(args)
        .current_dir(project)
        .stdin(Stdio::null());
    command
}

/// Un échec du CLI : le code de sortie (absent si le processus est mort sur un signal ou n'a pas pu naître) et un
/// message lisible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsError {
    pub code: Option<i32>,
    pub message: String,
}

fn ansi_codes() -> &'static Regex {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("valid ANSI pattern"))
}

fn url_userinfo() -> &'static Regex {
    static USERINFO: OnceLock<Regex> = OnceLock::new();
    USERINFO.get_or_init(|| {
        Regex::new(r"([A-Za-z][A-Za-z0-9+.-]*://)[^/@\s]+@").expect("valid userinfo pattern")
    })
}

/// Les arguments tels qu'on les journalise : `scheme://user:pass@host` devient `scheme://host`, un jeton d'accès
/// dans une URL de dépôt ne doit pas finir dans le journal.
fn loggable_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| url_userinfo().replace_all(arg, "$1"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Retire les codes de couleur et les lignes de bruit du shell, puis les blancs de bord.
fn clean_output(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let text = ansi_codes().replace_all(&text, "");
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| !line.contains(ZLE_NOISE))
        .collect();
    kept.join("\n").trim().to_string()
}

/// Retire de `text` les lignes de progression d'installation de bun, puis les blancs de bord.
fn without_bun_noise(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let line = line.trim_start();
            !BUN_INSTALL_NOISE
                .iter()
                .any(|noise| line.starts_with(noise))
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Le plus de lignes qu'un message d'échec montre : le volet n'a pas la place d'un journal.
const MAX_FAILURE_LINES: usize = 3;

/// La raison que `skills add --json` donne d'une skill absente de sa source (`cli.mjs`, `runAdd`).
const NO_MATCH_REASON: &str = "No matching skill found in source";

/// Les glyphes de `@clack/prompts` : le rail, les puces et les images du spinner. Une ligne qui n'en porte que est vide.
const CLACK_GLYPHS: [char; 19] = [
    '│', '┌', '└', '├', '◇', '◆', '■', '●', '○', '▲', '─', '╮', '╯', '╭', '╰', '◒', '◐', '◓', '◑',
];

/// Les caractères de la bannière « SKILLS » en lettres pleines.
const BANNER_GLYPHS: [char; 7] = ['█', '╗', '╔', '╝', '╚', '║', '═'];

/// Les quatre images du spinner de clack : une ligne qui en porte une est de la progression.
const SPINNER_FRAMES: [char; 4] = ['◒', '◐', '◓', '◑'];

/// Une ligne qui dit quelque chose : ni blanche, ni un simple rail de clack, ni la bannière, ni une image du spinner.
fn is_meaningful(line: &str) -> bool {
    let line = line.trim();
    let bare = line.trim_matches(|c: char| c.is_whitespace() || CLACK_GLYPHS.contains(&c));
    !bare.is_empty() && !line.contains(BANNER_GLYPHS) && !line.contains(SPINNER_FRAMES)
}

/// Les `max` dernières lignes utiles de `text` : l'erreur est à la fin du journal, jamais dans sa tête.
fn last_meaningful_lines(text: &str, max: usize) -> String {
    let kept: Vec<&str> = text.lines().filter(|line| is_meaningful(line)).collect();
    kept[kept.len().saturating_sub(max)..].join("\n")
}

/// Le message d'un `skills add --json` raté, lu sur le JSON de stdout (un tableau de résultats) : une phrase par skill
/// absente de `source` (l'index de skills.sh garde des skills que le dépôt n'a plus), puis l'erreur courte de chaque
/// échec. `None` quand stdout n'est pas ce JSON ou ne nomme aucun échec : le message se lit alors dans le journal.
fn add_json_failure(stdout: &str, source: &str) -> Option<String> {
    // Le `.zshrc` peut écrire sur stdout avant le JSON : le tableau se cherche comme celui de `skills list`.
    let value: serde_json::Value =
        serde_json::from_str(super::skills::json_array_in(stdout)?).ok()?;
    let mut lines = Vec::new();
    for entry in value.as_array()? {
        let text = |key: &str| entry.get(key).and_then(serde_json::Value::as_str);
        match text("status") {
            Some("skipped") if text("reason") == Some(NO_MATCH_REASON) => {
                if let Some(name) = text("name") {
                    lines.push(format!(
                        "« {name} » n'est plus dans {source} : skills.sh n'est pas à jour."
                    ));
                }
            }
            Some("failed") => {
                if let Some(error) = text("error") {
                    let error = ansi_codes().replace_all(error, "");
                    lines.push(format!("L'ajout a échoué : {}", error.trim()));
                }
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        return None;
    }
    let message = lines.join("\n");
    Some(
        message
            .lines()
            .take(MAX_FAILURE_LINES)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// La source d'un appel `skills add <source> ...`.
fn add_source(args: &[String]) -> Option<&str> {
    match args {
        [skills, add, source, ..] if skills == "skills" && add == "add" => Some(source),
        _ => None,
    }
}

/// Le message d'un échec, de trois lignes au plus. Un `skills add --json` dit son échec en JSON sur stdout (voir
/// `add_json_failure`). Sinon, le CLI écrit ses erreurs dans le journal de `@clack/prompts` : stderr d'abord, stdout
/// s'il ne reste rien, une fois retirées les lignes d'installation de bun (le vrai `bunx` en écrit à chaque appel, et
/// elles cachaient l'erreur du CLI), puis les dernières lignes utiles seulement (l'erreur est à la fin, la bannière et
/// les images du spinner ne sont pas un message). Un code 127 dont stderr dit que la commande est absente devient
/// « bunx introuvable » avec ce qui a été essayé.
fn failure_message(
    code: Option<i32>,
    bunx: &OsStr,
    shell: &Path,
    args: &[String],
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    if let Some(source) = add_source(args)
        && let Some(message) = add_json_failure(&String::from_utf8_lossy(stdout), source)
    {
        return message;
    }
    let from_stderr = without_bun_noise(&clean_output(stderr));
    if code == Some(127) {
        let lowered = from_stderr.to_lowercase();
        if lowered.contains("not found") || lowered.contains("no such file") {
            let tried = format!(
                "bunx introuvable : essayé « {} » via {} -l -i",
                bunx.to_string_lossy(),
                shell.display()
            );
            let detail = last_meaningful_lines(&from_stderr, MAX_FAILURE_LINES - 1);
            return format!("{tried}\n{detail}");
        }
    }
    for text in [from_stderr, clean_output(stdout)] {
        let detail = last_meaningful_lines(&text, MAX_FAILURE_LINES);
        if !detail.is_empty() {
            return detail;
        }
    }
    match code {
        Some(code) => format!("skills a échoué (code {code})"),
        None => "skills a été interrompu".to_string(),
    }
}

/// Le message d'un appel refusé parce que le projet est le dossier personnel.
pub const HOME_REFUSED: &str = "Le dossier personnel n'est pas un projet.";

/// Lance `bunx <args>` (`args` commence par `skills`) dans `project` par le shell de connexion de l'utilisateur. Tout
/// appel du CLI passe ici : c'est la dernière ligne de défense, le dossier personnel n'est jamais un projet (`skills
/// add -y` y écrirait les dossiers de skills globaux de toute la machine).
pub fn run_skills(project: &Path, args: &[String]) -> Result<String, SkillsError> {
    let shell = login_shell(std::env::var_os("SHELL").as_deref());
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .unwrap_or_default();
    let bunx = bunx_program(&home, std::env::var_os("IDEMA_BUNX_BIN").as_deref());
    run_guarded(
        home_dir().as_deref(),
        &shell,
        &bunx,
        project,
        args,
        CLI_TIMEOUT,
    )
}

/// `run_skills_with`, sauf pour le dossier personnel `home` : rien n'est lancé.
fn run_guarded(
    home: Option<&Path>,
    shell: &Path,
    bunx: &OsStr,
    project: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<String, SkillsError> {
    if is_home(project, home) {
        log_event(&format!(
            "skills call refused reason=home args={}",
            loggable_args(args)
        ));
        return Err(SkillsError {
            code: None,
            message: HOME_REFUSED.to_string(),
        });
    }
    run_skills_with(shell, bunx, project, args, timeout)
}

/// Comme `run_skills`, avec le shell, le programme `bunx` et la durée maximale donnés (les tests n'écrivent jamais
/// l'environnement).
pub fn run_skills_with(
    shell: &Path,
    bunx: &OsStr,
    project: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<String, SkillsError> {
    let joined = loggable_args(args);
    let captured = match capture(skills_command(shell, bunx, project, args), timeout) {
        Ok(Some(captured)) => captured,
        Ok(None) => {
            log_event(&format!("skills call args={joined} exit=timeout"));
            return Err(SkillsError {
                code: None,
                message: format!(
                    "skills ne répond plus ({} s), appel arrêté",
                    timeout.as_secs()
                ),
            });
        }
        Err(error) => {
            log_event(&format!("skills call args={joined} exit=spawn-failed"));
            log_event(&format!("skills spawn failed error={error}"));
            // Le volet montre la raison en français ; le texte du système (anglais) reste dans le journal.
            return Err(SkillsError {
                code: None,
                message: format!(
                    "Impossible de lancer le shell {} : {}",
                    shell.display(),
                    io_error_text(&error)
                ),
            });
        }
    };
    let code = captured.status.code();
    log_event(&format!(
        "skills call args={joined} exit={}",
        code.map_or_else(|| "signal".to_string(), |code| code.to_string())
    ));
    if captured.status.success() {
        return Ok(String::from_utf8_lossy(&captured.stdout).into_owned());
    }
    Err(SkillsError {
        code,
        message: failure_message(code, bunx, shell, args, &captured.stdout, &captured.stderr),
    })
}

struct Captured {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// Lance la commande dans son propre groupe de processus et attend au plus `timeout`. `Ok(None)` : le délai est
/// écoulé et le groupe a été arrêté (SIGTERM, puis SIGKILL après `TERMINATE_GRACE`). On ne signale que le pid lancé
/// ici, jamais un nom ou un motif.
fn capture(mut command: Command, timeout: Duration) -> io::Result<Option<Captured>> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn()?;
    let group = child.id() as libc::pid_t;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                terminate(&mut child, group);
                return Err(error);
            }
        }
    };
    let Some(status) = status else {
        // Les threads de lecture sont laissés à eux-mêmes : ils finissent quand le dernier tube se ferme.
        terminate(&mut child, group);
        return Ok(None);
    };
    Ok(Some(Captured {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    }))
}

/// SIGTERM au groupe, `TERMINATE_GRACE` pour quitter, puis SIGKILL ; le processus lancé est toujours récolté.
fn terminate(child: &mut Child, group: libc::pid_t) {
    // SAFETY: `kill` n'a pas d'effet mémoire ; `-group` désigne le groupe créé pour ce seul enfant.
    unsafe { libc::kill(-group, libc::SIGTERM) };
    let deadline = Instant::now() + TERMINATE_GRACE;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: voir plus haut.
    unsafe { libc::kill(-group, libc::SIGKILL) };
    let _ = child.wait();
}

/// `SKILLS_API_URL` ou `https://skills.sh`.
pub fn api_base() -> String {
    std::env::var("SKILLS_API_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

/// Interroge skills.sh avec le curl du système. Le texte cherché n'est jamais qu'un morceau d'URL encodée : aucun
/// shell n'est impliqué.
pub fn search(base: &str, query: &str) -> Result<Vec<SearchHit>, String> {
    let result = fetch(&search_url(base, query)).and_then(|body| parse_search(&body));
    match &result {
        Ok(hits) => log_event(&format!(
            "skills search query_len={} hits={}",
            query.chars().count(),
            hits.len()
        )),
        Err(reason) => log_event(&format!("skills search failed reason={reason}")),
    }
    result
}

/// Le corps de la réponse, quand le code HTTP (dernière ligne, ajoutée par `-w`) est un 2xx.
fn fetch(url: &str) -> Result<String, String> {
    let output = Command::new(CURL)
        .args(CURL_ARGS)
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Impossible de lancer curl : {error}"))?;
    if !output.status.success() {
        let detail = clean_output(&output.stderr);
        return Err(if detail.is_empty() {
            "La recherche sur skills.sh a échoué.".to_string()
        } else {
            format!("La recherche sur skills.sh a échoué : {detail}")
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (body, status) = text
        .rsplit_once('\n')
        .ok_or_else(|| "La réponse de skills.sh est illisible.".to_string())?;
    if !status.trim().starts_with('2') {
        return Err(format!("skills.sh a répondu {}", status.trim()));
    }
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Dossier jetable sous `temp_dir`, supprimé à la fin du test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "idema-skills-cli-{name}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const ZLE: &str = "(eval):1: can't change option: zle\n";

    /// Un faux `bunx` : écrit son `pwd -P` puis chaque argument sur sa ligne dans `out.txt`, puis rend le contenu de
    /// `stdout.txt` / `stderr.txt` (s'ils existent) et sort du code de `exit.txt` (0 par défaut).
    fn fake_bunx(dir: &Path, stdout: &str, stderr: &str, exit: i32) -> PathBuf {
        let script = dir.join("bunx");
        fs::write(
            &script,
            r#"#!/bin/sh
dir="$(dirname "$0")"
pwd -P > "$dir/out.txt"
for arg in "$@"; do printf '%s\n' "$arg" >> "$dir/out.txt"; done
[ -f "$dir/stdout.txt" ] && cat "$dir/stdout.txt"
[ -f "$dir/stderr.txt" ] && cat "$dir/stderr.txt" >&2
exit "$(cat "$dir/exit.txt" 2>/dev/null || echo 0)"
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("stdout.txt"), stdout).unwrap();
        fs::write(dir.join("stderr.txt"), stderr).unwrap();
        fs::write(dir.join("exit.txt"), exit.to_string()).unwrap();
        script
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    fn run(bunx: &Path, project: &Path, args: &[&str]) -> Result<String, SkillsError> {
        run_skills_with(
            Path::new("/bin/zsh"),
            bunx.as_os_str(),
            project,
            &strings(args),
            CLI_TIMEOUT,
        )
    }

    #[test]
    fn arguments_arrive_verbatim() {
        let scratch = Scratch::new("verbatim");
        let bunx = fake_bunx(scratch.path(), "", "", 0);
        let args = ["skills", "add", "it's $(touch x) `y`", "-y"];
        run(&bunx, scratch.path(), &args).unwrap();
        let out = fs::read_to_string(scratch.path().join("out.txt")).unwrap();
        let lines: Vec<&str> = out.lines().skip(1).collect();
        assert_eq!(lines, args);
        assert!(!scratch.path().join("x").exists());
    }

    fn guarded(home: Option<&Path>, bunx: &Path, project: &Path) -> Result<String, SkillsError> {
        run_guarded(
            home,
            Path::new("/bin/zsh"),
            bunx.as_os_str(),
            project,
            &strings(&["skills", "add", "x", "-y"]),
            CLI_TIMEOUT,
        )
    }

    #[test]
    fn no_cli_call_is_ever_run_in_the_home_folder() {
        let scratch = Scratch::new("guard-home");
        let bunx = fake_bunx(scratch.path(), "", "", 0);
        let home = scratch.path().join("home");
        fs::create_dir_all(home.join("dev/app")).unwrap();
        let marker = scratch.path().join("out.txt");

        // Le dossier personnel, écrit de trois façons, et un lien qui y mène : rien ne se lance.
        let link = scratch.path().join("link-to-home");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        for project in [
            home.clone(),
            PathBuf::from(format!("{}/", home.display())),
            home.join("dev").join(".."),
            link,
        ] {
            let error = guarded(Some(&home), &bunx, &project).unwrap_err();
            assert_eq!(error.message, HOME_REFUSED, "{project:?}");
            assert!(!marker.exists(), "{project:?} ran the CLI");
        }

        // Un sous-dossier du dossier personnel est un projet : l'appel part.
        guarded(Some(&home), &bunx, &home.join("dev/app")).unwrap();
        assert!(marker.exists());
    }

    #[test]
    fn a_failure_to_spawn_the_shell_reads_in_french() {
        let scratch = Scratch::new("spawn-failed");
        let error = run_skills_with(
            &scratch.path().join("no-such-shell"),
            OsStr::new("bunx"),
            scratch.path(),
            &strings(&["skills", "list"]),
            CLI_TIMEOUT,
        )
        .unwrap_err();
        assert!(
            error.message.ends_with(" : fichier introuvable"),
            "{}",
            error.message
        );
        assert!(!error.message.contains("os error"), "{}", error.message);
    }

    #[test]
    fn the_cli_runs_in_the_project() {
        let scratch = Scratch::new("cwd");
        let bunx = fake_bunx(scratch.path(), "", "", 0);
        let project = scratch.path().join("l'été x");
        fs::create_dir_all(&project).unwrap();
        run(&bunx, &project, &["skills", "list"]).unwrap();
        let out = fs::read_to_string(scratch.path().join("out.txt")).unwrap();
        assert_eq!(out.lines().next().unwrap(), project.to_str().unwrap());
    }

    #[test]
    fn a_success_returns_stdout() {
        let scratch = Scratch::new("success");
        let bunx = fake_bunx(scratch.path(), "[]\n", ZLE, 0);
        assert_eq!(run(&bunx, scratch.path(), &["skills"]).unwrap(), "[]\n");
    }

    #[test]
    fn a_failure_on_stdout_is_shown() {
        let scratch = Scratch::new("stdout-failure");
        let bunx = fake_bunx(
            scratch.path(),
            "\x1b[31mNo matching skills found for: x\x1b[0m\n",
            "",
            1,
        );
        let error = run(&bunx, scratch.path(), &["skills", "add", "x"]).unwrap_err();
        assert_eq!(error.code, Some(1));
        assert_eq!(error.message, "No matching skills found for: x");
    }

    #[test]
    fn a_failure_on_stderr_wins() {
        let scratch = Scratch::new("stderr-failure");
        let stderr = format!("{ZLE}boom\n{ZLE}");
        let bunx = fake_bunx(scratch.path(), "ignored on stdout\n", &stderr, 2);
        let error = run(&bunx, scratch.path(), &["skills"]).unwrap_err();
        assert_eq!(error.code, Some(2));
        assert_eq!(error.message, "boom");
    }

    #[test]
    fn bun_install_noise_does_not_hide_the_cli_error() {
        // Les lignes relevées sur un vrai `bunx skills add ... --skill does-not-exist` : stderr porte la progression de
        // bun, stdout l'erreur du CLI.
        let scratch = Scratch::new("bun-noise");
        let stderr =
            format!("{ZLE}{ZLE}Resolving dependencies\nResolved, downloaded and extracted [2]\n");
        let stdout = "■  No matching skills found for: does-not-exist\n";
        let bunx = fake_bunx(scratch.path(), stdout, &stderr, 1);
        let error = run(&bunx, scratch.path(), &["skills", "add", "x"]).unwrap_err();
        assert_eq!(error.code, Some(1));
        assert!(
            error
                .message
                .contains("No matching skills found for: does-not-exist"),
            "{}",
            error.message
        );
        assert!(!error.message.contains("Resolving"), "{}", error.message);
        assert!(!error.message.contains("Resolved"), "{}", error.message);
    }

    #[test]
    fn colour_codes_around_a_bun_noise_line_do_not_hide_the_cli_error() {
        // Un terminal qui colore la progression de bun : le code précède le texte, la ligne reste du bruit.
        let scratch = Scratch::new("bun-noise-ansi");
        let stderr = format!(
            "{ZLE}\x1b[2mResolving dependencies\x1b[0m\n\x1b[32mResolved, downloaded and extracted [2]\x1b[0m\n"
        );
        let stdout = "\x1b[31m■\x1b[0m  No matching skills found for: nope\n";
        let bunx = fake_bunx(scratch.path(), stdout, &stderr, 1);
        let error = run(&bunx, scratch.path(), &["skills", "add", "x"]).unwrap_err();
        assert_eq!(error.message, "■  No matching skills found for: nope");
    }

    #[test]
    fn a_real_bun_error_on_stderr_is_still_shown() {
        let scratch = Scratch::new("bun-error");
        let stderr = "Resolving dependencies\nerror: failed to resolve \"skills\"\n";
        let bunx = fake_bunx(scratch.path(), "", stderr, 1);
        let error = run(&bunx, scratch.path(), &["skills"]).unwrap_err();
        assert_eq!(error.message, "error: failed to resolve \"skills\"");
    }

    #[test]
    fn a_failure_with_only_zle_noise_falls_back_to_stdout() {
        let scratch = Scratch::new("zle-only");
        let bunx = fake_bunx(scratch.path(), "from stdout\n", ZLE, 1);
        let error = run(&bunx, scratch.path(), &["skills"]).unwrap_err();
        assert_eq!(error.message, "from stdout");
    }

    #[test]
    fn a_silent_failure_names_skills() {
        let scratch = Scratch::new("silent");
        let bunx = fake_bunx(scratch.path(), "", ZLE, 3);
        let error = run(&bunx, scratch.path(), &["skills"]).unwrap_err();
        assert_eq!(error.code, Some(3));
        assert_eq!(error.message, "skills a échoué (code 3)");
    }

    /// Le JSON que le vrai CLI 1.7.4 rend sur stdout quand `skills add --json` ne trouve pas la skill (capture du
    /// 2026-11-03) et celui d'un ajout réussi.
    const ADD_NO_MATCH: &str = include_str!("testdata/add-json-nomatch.sample.json");
    const ADD_OK: &str = include_str!("testdata/add-json-ok.sample.json");

    /// Ce que le CLI écrit sur stderr en mode `--json` : bruit de bun, puis tout le journal de clack.
    const ADD_JSON_STDERR: &str = "Resolving dependencies\nResolved, downloaded and extracted [2]\n\n│\n◇   claude-code agent detected\n│\n■  No matching skills found for: hyperframes-animation\n│\n◇  Available skills:\n│\n│    - figma\n│\n│    - hyperframes\n│\n│    - slideshow\n";

    fn run_args(bunx: &Path, project: &Path, args: &[String]) -> Result<String, SkillsError> {
        run_skills_with(
            Path::new("/bin/zsh"),
            bunx.as_os_str(),
            project,
            args,
            CLI_TIMEOUT,
        )
    }

    fn line_count(message: &str) -> usize {
        message.lines().count()
    }

    #[test]
    fn a_stale_index_hit_gives_one_french_sentence_and_nothing_of_the_transcript() {
        let scratch = Scratch::new("add-no-match");
        let bunx = fake_bunx(scratch.path(), ADD_NO_MATCH, ADD_JSON_STDERR, 1);
        let args = add_args("heygen-com/hyperframes", Some("hyperframes-animation")).unwrap();
        let error = run_args(&bunx, scratch.path(), &args).unwrap_err();
        assert_eq!(error.code, Some(1));
        assert_eq!(
            error.message,
            "« hyperframes-animation » n'est plus dans heygen-com/hyperframes : skills.sh n'est pas à jour."
        );
    }

    #[test]
    fn shell_noise_on_stdout_before_the_json_keeps_the_french_sentence() {
        let scratch = Scratch::new("add-no-match-noise");
        // Un `.zshrc` qui écrit sur stdout, `[` compris, avant le JSON du CLI.
        let stdout = format!("Welcome back [zsh]\n{ADD_NO_MATCH}");
        let bunx = fake_bunx(scratch.path(), &stdout, ADD_JSON_STDERR, 1);
        let args = add_args("heygen-com/hyperframes", Some("hyperframes-animation")).unwrap();
        let error = run_args(&bunx, scratch.path(), &args).unwrap_err();
        assert_eq!(
            error.message,
            "« hyperframes-animation » n'est plus dans heygen-com/hyperframes : skills.sh n'est pas à jour."
        );
    }

    #[test]
    fn a_successful_json_add_is_not_an_error() {
        let scratch = Scratch::new("add-ok");
        let bunx = fake_bunx(scratch.path(), ADD_OK, "", 0);
        let args = add_args("emilkowalski/skills", Some("review-animations")).unwrap();
        assert_eq!(
            run_args(&bunx, scratch.path(), &args).unwrap(),
            ADD_OK.to_string()
        );
        assert_eq!(add_json_failure(ADD_OK, "emilkowalski/skills"), None);
    }

    #[test]
    fn a_failed_json_entry_gives_the_clis_own_short_error() {
        let stdout = "[{\"name\":\"x\",\"status\":\"failed\",\"error\":\"\\u001b[31mClone failed: timeout\\u001b[0m\"}]";
        assert_eq!(
            add_json_failure(stdout, "o/r").as_deref(),
            Some("L'ajout a échoué : Clone failed: timeout")
        );
    }

    #[test]
    fn each_skipped_name_gets_its_sentence_and_the_message_stays_within_three_lines() {
        let entry = |name: &str| {
            format!(
                "{{\"name\":\"{name}\",\"status\":\"skipped\",\"reason\":\"No matching skill found in source\"}}"
            )
        };
        let two = format!("[{},{}]", entry("a"), entry("b"));
        assert_eq!(
            add_json_failure(&two, "o/r").as_deref(),
            Some(
                "« a » n'est plus dans o/r : skills.sh n'est pas à jour.\n« b » n'est plus dans o/r : skills.sh n'est pas à jour."
            )
        );
        let five = format!("[{}]", ["a", "b", "c", "d", "e"].map(entry).join(","));
        assert_eq!(line_count(&add_json_failure(&five, "o/r").unwrap()), 3);
    }

    #[test]
    fn json_that_names_no_failure_falls_back() {
        assert_eq!(add_json_failure("[]", "o/r"), None);
        assert_eq!(add_json_failure("not json at all", "o/r"), None);
        assert_eq!(add_json_failure("{\"status\":\"failed\"}", "o/r"), None);
    }

    #[test]
    fn garbage_stdout_shows_at_most_three_meaningful_lines() {
        // Le journal de clack d'avant `--json` : la bannière, les images du spinner, puis le vrai message.
        let mut transcript = String::from(
            "███████╗██╗  ██╗██╗██╗     ██╗     ███████╗\n╚════██║██╔═██╗ ██║██║     ██║     ╚════██║\n\nTip: use the --yes (-y) and --global (-g) flags to install without prompts.\n",
        );
        for _ in 0..20 {
            transcript.push_str("◒  Fetching skills…◐  Fetching skills…◓  Fetching skills…\n");
        }
        transcript.push_str("◇  Cloning repository…\n│\n■  No matching skills found for: x\n│\n◇  Available skills:\n│\n│    - figma\n");
        let scratch = Scratch::new("garbage");
        let bunx = fake_bunx(scratch.path(), &transcript, "", 1);
        let args = add_args("o/r", Some("x")).unwrap();
        let error = run_args(&bunx, scratch.path(), &args).unwrap_err();
        assert!(line_count(&error.message) <= 3, "{}", error.message);
        assert!(!error.message.contains('█'), "{}", error.message);
        assert!(!error.message.contains("Fetching"), "{}", error.message);
        assert!(error.message.contains("- figma"), "{}", error.message);
    }

    #[test]
    fn a_removal_failure_is_capped_to_three_lines_too() {
        let transcript: String = (0..40).map(|n| format!("line {n}\n")).collect();
        let scratch = Scratch::new("remove-cap");
        let bunx = fake_bunx(scratch.path(), "", &transcript, 1);
        let error = run(&bunx, scratch.path(), &["skills", "remove", "x", "-y"]).unwrap_err();
        assert_eq!(error.message, "line 37\nline 38\nline 39");
    }

    #[test]
    fn a_missing_bunx_message_stays_within_three_lines() {
        let scratch = Scratch::new("missing-cap");
        let stderr: String = (0..20).map(|n| format!("env: not found {n}\n")).collect();
        let bunx = fake_bunx(scratch.path(), "", &stderr, 127);
        let error = run(&bunx, scratch.path(), &["skills"]).unwrap_err();
        assert!(error.message.starts_with("bunx introuvable"));
        assert_eq!(line_count(&error.message), 3, "{}", error.message);
    }

    #[test]
    fn a_missing_bunx_says_so() {
        let scratch = Scratch::new("missing");
        let bunx = scratch.path().join("no-such-bunx");
        let error = run(&bunx, scratch.path(), &["skills"]).unwrap_err();
        assert_eq!(error.code, Some(127));
        assert!(
            error.message.starts_with("bunx introuvable"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains(bunx.to_str().unwrap()),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("/bin/zsh -l -i"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_127_that_is_not_a_missing_command_keeps_its_own_message() {
        let scratch = Scratch::new("other-127");
        let bunx = fake_bunx(scratch.path(), "", "boom", 127);
        let error = run(&bunx, scratch.path(), &["skills"]).unwrap_err();
        assert_eq!(error.message, "boom");

        let bunx = fake_bunx(scratch.path(), "", "", 127);
        let error = run(&bunx, scratch.path(), &["skills"]).unwrap_err();
        assert_eq!(error.message, "skills a échoué (code 127)");

        let bunx = fake_bunx(scratch.path(), "", "env: COMMAND NOT FOUND: node", 127);
        let error = run(&bunx, scratch.path(), &["skills"]).unwrap_err();
        assert!(
            error.message.starts_with("bunx introuvable"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("COMMAND NOT FOUND: node"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_missing_shell_is_reported_not_panicked() {
        let scratch = Scratch::new("no-shell");
        let bunx = fake_bunx(scratch.path(), "", "", 0);
        let error = run_skills_with(
            Path::new("/no/such/shell"),
            bunx.as_os_str(),
            scratch.path(),
            &strings(&["skills"]),
            CLI_TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(error.code, None);
        assert!(!error.message.is_empty());
    }

    #[test]
    fn bunx_prefers_the_override_then_the_bun_home() {
        let home = Scratch::new("home");
        assert_eq!(bunx_program(home.path(), None), OsString::from("bunx"));

        let bin = home.path().join(".bun/bin");
        fs::create_dir_all(&bin).unwrap();
        let installed = bin.join("bunx");
        fs::write(&installed, "#!/bin/sh\n").unwrap();
        // Présent mais pas exécutable : le shell cherchera `bunx` lui-même.
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(bunx_program(home.path(), None), OsString::from("bunx"));

        fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(bunx_program(home.path(), None), installed.as_os_str());

        // Un HOME relatif (ou vide) n'est jamais cherché depuis le dossier courant.
        assert_eq!(
            bunx_program(Path::new("./relative-home"), None),
            OsString::from("bunx")
        );
        assert_eq!(bunx_program(Path::new(""), None), OsString::from("bunx"));

        let custom = OsString::from("/opt/custom/bunx");
        assert_eq!(bunx_program(home.path(), Some(&custom)), custom);
        assert_eq!(
            bunx_program(home.path(), Some(OsStr::new(""))),
            installed.as_os_str()
        );
    }

    #[test]
    fn login_shell_takes_an_executable_absolute_shell() {
        assert_eq!(
            login_shell(Some(OsStr::new("/bin/bash"))),
            PathBuf::from("/bin/bash")
        );
        assert_eq!(login_shell(None), PathBuf::from("/bin/zsh"));
        assert_eq!(
            login_shell(Some(OsStr::new("zsh"))),
            PathBuf::from("/bin/zsh")
        );
        assert_eq!(
            login_shell(Some(OsStr::new("/no/such/shell"))),
            PathBuf::from("/bin/zsh")
        );
        assert_eq!(login_shell(Some(OsStr::new(""))), PathBuf::from("/bin/zsh"));
    }

    /// Un serveur HTTP local à usage unique : rend `body` puis renvoie la première ligne de la requête.
    fn serve_once(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        serve_status(200, body)
    }

    fn serve_status(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            request.lines().next().unwrap_or_default().to_string()
        });
        (base, handle)
    }

    #[test]
    fn search_asks_skills_sh_and_sorts_by_installs() {
        let body = r#"{"skills":[{"id":"a/b/low","source":"a/b","skillId":"low","name":"low","installs":14},{"id":"a/b/high","source":"a/b","skillId":"high","name":"high","installs":900}]}"#;
        let (base, request) = serve_once(body);
        let hits = search(&base, "mo tion").unwrap();
        assert_eq!(
            hits.iter().map(|hit| hit.name.as_str()).collect::<Vec<_>>(),
            ["high", "low"]
        );
        assert_eq!(
            request.join().unwrap(),
            "GET /api/search?q=mo+tion&limit=20 HTTP/1.1"
        );
    }

    #[test]
    fn search_reports_an_unreachable_server() {
        // Un port qui vient d'être libéré : la connexion est refusée tout de suite.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let message = search(&format!("http://127.0.0.1:{port}"), "motion").unwrap_err();
        assert!(!message.is_empty());
    }

    #[test]
    fn search_reports_an_unreadable_answer() {
        let (base, request) = serve_once("<html>nope</html>");
        let message = search(&base, "motion").unwrap_err();
        request.join().unwrap();
        assert_eq!(message, "La réponse de skills.sh est illisible.");
    }

    #[test]
    fn search_names_the_http_status_of_a_rejected_request() {
        let (base, request) = serve_status(500, r#"{"error":"boom"}"#);
        let message = search(&base, "motion").unwrap_err();
        request.join().unwrap();
        assert_eq!(message, "skills.sh a répondu 500");

        let (base, request) = serve_status(429, "slow down");
        let message = search(&base, "motion").unwrap_err();
        request.join().unwrap();
        assert_eq!(message, "skills.sh a répondu 429");
    }

    #[test]
    fn logged_args_hide_url_credentials() {
        let args = strings(&[
            "skills",
            "add",
            "https://user:s3cret@github.com/org/repo.git",
            "--from=ssh://token@host/x",
            "org/repo",
            "git@github.com:org/repo",
        ]);
        assert_eq!(
            loggable_args(&args),
            "skills add https://github.com/org/repo.git --from=ssh://host/x org/repo git@github.com:org/repo"
        );
    }

    /// Vrai tant qu'un processus du groupe `group` existe encore.
    fn group_exists(group: libc::pid_t) -> bool {
        // SAFETY: signal 0, simple test d'existence.
        let result = unsafe { libc::kill(-group, 0) };
        !(result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
    }

    #[test]
    fn a_hung_cli_is_stopped_with_its_whole_group() {
        let scratch = Scratch::new("hang");
        let script = scratch.path().join("bunx");
        let pid_file = scratch.path().join("pid.txt");
        // Le faux écrit son pid avant toute autre chose (pas même un sous-processus), puis bloque avec un enfant.
        fs::write(
            &script,
            "#!/bin/sh\necho $$ > \"${0%/*}/pid.txt\"\nsleep 60 &\nsleep 60\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        // Le délai court à partir du lancement du shell : sur une machine chargée, `sh -l -i` peut ne pas avoir
        // atteint le script à temps. Le groupe n'est vérifié que lorsque le faux a prouvé qu'il a démarré (son
        // fichier de pid existe) ; sinon on recommence avec une limite doublée.
        let mut limit = Duration::from_secs(2);
        let mut group = None;
        for _ in 0..5 {
            let _ = fs::remove_file(&pid_file);
            let error = run_skills_with(
                Path::new("/bin/sh"),
                script.as_os_str(),
                scratch.path(),
                &strings(&["skills", "find"]),
                limit,
            )
            .unwrap_err();
            assert_eq!(error.code, None);
            assert_eq!(
                error.message,
                format!(
                    "skills ne répond plus ({} s), appel arrêté",
                    limit.as_secs()
                )
            );
            let started = fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<libc::pid_t>().ok());
            if started.is_some() {
                group = started;
                break;
            }
            limit *= 2;
        }
        let group =
            group.expect("le faux bunx n'a jamais démarré, même avec la plus longue limite");
        // Les `sleep` orphelins sont récoltés par launchd : on attend leur disparition, avec une borne large.
        let deadline = Instant::now() + Duration::from_secs(10);
        while group_exists(group) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!group_exists(group), "le groupe {group} survit");
    }

    // Porte de la Tâche 11 : le vrai CLI `skills` et le vrai skills.sh, sur le Mac, dans le projet jetable de
    // `IDEMA_HOST_PROJECT` (voir `host_gate`). Tous `#[ignore]` ; la porte les lance un à la fois.

    use crate::project_panel::host_gate::host_project;
    use crate::project_panel::skills::{SKILL_DIRS, add_args, list_args, parse_list, remove_args};

    const REAL_SOURCE: &str = "emilkowalski/skills";
    const REAL_SKILL: &str = "review-animations";
    const LOCAL_SKILL: &str = "host-local-demo";

    /// Vrai si le chemin existe, lien cassé compris.
    fn present(path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    fn lock_names(project: &Path) -> Vec<String> {
        let Ok(text) = fs::read_to_string(project.join("skills-lock.json")) else {
            return Vec::new();
        };
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["skills"]
            .as_object()
            .map(|skills| skills.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn listed_dirs(project: &Path) -> Vec<String> {
        let json = run_skills(project, &list_args()).unwrap();
        parse_list(&json)
            .unwrap()
            .into_iter()
            .map(|skill| skill.dir)
            .collect()
    }

    #[test]
    #[ignore = "host gate: real skills CLI, needs IDEMA_HOST_PROJECT"]
    fn host_real_cli_adds_lists_and_removes_a_skill() {
        let project = host_project();
        run_skills(&project, &add_args(REAL_SOURCE, Some(REAL_SKILL)).unwrap()).unwrap();
        for root in SKILL_DIRS {
            let manifest = project.join(root).join(REAL_SKILL).join("SKILL.md");
            assert!(manifest.is_file(), "missing {}", manifest.display());
        }
        assert!(lock_names(&project).contains(&REAL_SKILL.to_string()));

        let listed = parse_list(&run_skills(&project, &list_args()).unwrap()).unwrap();
        let entry = listed
            .iter()
            .find(|skill| skill.dir == REAL_SKILL)
            .expect("the CLI lists the installed skill");
        assert_eq!(entry.source.as_deref(), Some(REAL_SOURCE));
        assert_eq!(entry.kind.as_deref(), Some("github"));

        run_skills(&project, &remove_args(REAL_SKILL).unwrap()).unwrap();
        for root in SKILL_DIRS {
            let dir = project.join(root).join(REAL_SKILL);
            assert!(!present(&dir), "still there: {}", dir.display());
        }
        assert!(!lock_names(&project).contains(&REAL_SKILL.to_string()));
        assert!(!listed_dirs(&project).contains(&REAL_SKILL.to_string()));
    }

    #[test]
    #[ignore = "host gate: real skills CLI, needs IDEMA_HOST_PROJECT"]
    fn host_real_cli_adds_a_local_folder() {
        let project = host_project();
        let source = project.join(".host-local-skills");
        let skill_dir = source.join(LOCAL_SKILL);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {LOCAL_SKILL}\ndescription: Local skill of the host gate\n---\n\nSay hello.\n"),
        )
        .unwrap();

        run_skills(
            &project,
            &add_args(source.to_str().unwrap(), Some(LOCAL_SKILL)).unwrap(),
        )
        .unwrap();
        for root in SKILL_DIRS {
            let manifest = project.join(root).join(LOCAL_SKILL).join("SKILL.md");
            assert!(manifest.is_file(), "missing {}", manifest.display());
        }
        let listed = parse_list(&run_skills(&project, &list_args()).unwrap()).unwrap();
        let entry = listed
            .iter()
            .find(|skill| skill.dir == LOCAL_SKILL)
            .expect("the CLI lists the local skill");
        assert_eq!(entry.kind.as_deref(), Some("local"));
        let lock: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(project.join("skills-lock.json")).unwrap())
                .unwrap();
        assert_eq!(lock["skills"][LOCAL_SKILL]["sourceType"], "local");

        run_skills(&project, &remove_args(LOCAL_SKILL).unwrap()).unwrap();
        for root in SKILL_DIRS {
            assert!(!present(&project.join(root).join(LOCAL_SKILL)));
        }
        assert!(!lock_names(&project).contains(&LOCAL_SKILL.to_string()));
        fs::remove_dir_all(&source).unwrap();
    }

    #[test]
    #[ignore = "host gate: real skills CLI, needs IDEMA_HOST_PROJECT"]
    fn host_real_cli_error_is_readable() {
        let project = host_project();
        let error = run_skills(
            &project,
            &add_args(REAL_SOURCE, Some("does-not-exist")).unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            format!(
                "« does-not-exist » n'est plus dans {REAL_SOURCE} : skills.sh n'est pas à jour."
            )
        );
        assert!(error.code.is_some_and(|code| code != 0), "{:?}", error.code);
    }

    #[test]
    #[ignore = "host gate: real skills CLI, needs IDEMA_HOST_PROJECT"]
    fn host_bunx_is_found_from_a_launchd_env() {
        let project = host_project();
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        let user = std::env::var_os("USER").unwrap_or_default();
        // Le programme que choisit le produit (bunx de `~/.bun/bin` s'il existe), puis `bunx` nu, que seul le `PATH`
        // du shell de connexion peut trouver.
        for bunx in [bunx_program(&home, None), OsString::from("bunx")] {
            let mut command = skills_command(Path::new("/bin/zsh"), &bunx, &project, &list_args());
            // L'environnement d'une app ouverte par Raycast ou le Finder : rien d'autre que ces quatre variables.
            command
                .env_clear()
                .env("HOME", &home)
                .env("USER", &user)
                .env("SHELL", "/bin/zsh")
                .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            let captured = capture(command, CLI_TIMEOUT)
                .unwrap()
                .expect("the CLI answers before the timeout");
            assert!(
                captured.status.success(),
                "bunx {:?} failed: {}",
                bunx,
                clean_output(&captured.stderr)
            );
            parse_list(&String::from_utf8_lossy(&captured.stdout)).unwrap();
        }
    }

    #[test]
    #[ignore = "host gate: real skills.sh, needs network and jq"]
    fn host_real_search_matches_curl() {
        let _project = host_project();
        let hits = search(&api_base(), "motion").unwrap();
        let ours: Vec<&str> = hits.iter().take(3).map(|hit| hit.name.as_str()).collect();

        let url = search_url(&api_base(), "motion");
        let output = Command::new("/bin/sh")
            .args([
                "-c",
                r#"/usr/bin/curl -s "$1" | jq -r '.skills | sort_by(-.installs) | .[0:3] | .[].name'"#,
                "sh",
                &url,
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let expected = String::from_utf8(output.stdout).unwrap();
        let expected: Vec<&str> = expected.lines().collect();
        assert_eq!(expected.len(), 3, "the oracle returned {expected:?}");
        assert_eq!(ours, expected);
    }
}
