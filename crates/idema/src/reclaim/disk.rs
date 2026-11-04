//! Les lectures réelles de la découverte : le disque, `ps` et `git`, et le cache qui les rend peu coûteuses. Rien ici
//! ne tourne sur le fil d'interface : `scan` part sur un fil de fond, à chaque capture de l'instantané.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::git::{self, Gesture};
use crate::restore::claude::{is_uuid, sessions_dir};
use crate::servers::scan::{Process, TOOL_TIMEOUT, run_tool_with_env};

use super::discover::{IdemaPids, OutsideSession, Probe, outside_sessions};
use super::sources::{
    CaserneManifest, IdnmaAgents, LaunchEnv, LiveSession, parse_idnma_agents, parse_launch_env,
    parse_manifest, read_live_sessions, read_session_holders,
};

/// Un manifeste de la Caserne pèse quelques centaines d'octets ; au-delà, il n'est pas lu.
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;

/// Ce que la découverte retient d'un tour à l'autre. Une lecture qui échoue n'y entre jamais : elle est retentée au
/// tour suivant.
#[derive(Default)]
pub struct ReclaimCache {
    /// La racine de projet de chaque dossier de travail déjà demandé à `git` : un `cwd` coûte un seul `git` dans la vie
    /// de l'application (chaque appel de `git::run` laisse une ligne au journal).
    roots: HashMap<PathBuf, PathBuf>,
    /// L'environnement de lancement par `(pid, sessionId, procStart)` : il ne change pas pendant la vie du processus, et `ps eww`
    /// est la lecture la plus sensible. Seul le booléen est gardé, jamais la ligne.
    launch_envs: HashMap<(i32, String, String), LaunchEnv>,
    /// Les conversations dont le transcript a déjà été vu : un transcript ne disparaît pas sous une session vivante.
    transcripts: HashSet<String>,
}

impl ReclaimCache {
    /// Oublie ce qui ne concerne plus une session vivante, pour que le cache ne grossisse pas et qu'un PID réutilisé
    /// ne retrouve jamais l'environnement d'un processus mort. Les racines, elles, restent : elles ne dépendent que du
    /// dossier.
    fn retain_live(&mut self, live: &[LiveSession]) {
        self.launch_envs.retain(|(pid, session_id, proc_start), _| {
            live.iter().any(|session| {
                session.pid == *pid
                    && session.session_id == *session_id
                    && session.proc_start == *proc_start
            })
        });
        self.transcripts
            .retain(|session_id| live.iter().any(|session| session.session_id == *session_id));
    }
}

fn locked(cache: &Mutex<ReclaimCache>) -> MutexGuard<'_, ReclaimCache> {
    cache.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Où lire. Le dossier de configuration de Claude porte les sessions et les projets ; la Caserne et `idnma` vivent
/// sous `HOME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimPaths {
    pub sessions: PathBuf,
    pub projects: PathBuf,
    pub caserne: PathBuf,
    pub idnma_config: PathBuf,
}

/// Les emplacements à lire. `CLAUDE_CONFIG_DIR`, quand il est posé, remplace `~/.claude` pour `sessions` et `projects`
/// seulement. `None` sans `HOME` : sans lui la Caserne et `idnma` restent introuvables, et on ne devine pas.
pub fn paths(home: Option<&OsStr>, config_dir: Option<&OsStr>) -> Option<ReclaimPaths> {
    let home = Path::new(home?);
    let projects = match config_dir {
        Some(dir) => Path::new(dir).join("projects"),
        None => home.join(".claude").join("projects"),
    };
    Some(ReclaimPaths {
        sessions: sessions_dir(Some(home.as_os_str()), config_dir)?,
        projects,
        caserne: home.join(".config").join("caserne").join("sessions"),
        idnma_config: home.join(".idnma.yml"),
    })
}

/// Le résultat d'un tour de découverte.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimScan {
    pub sessions: Vec<OutsideSession>,
    /// La racine de projet de chaque dossier de travail rencontré (celui des projets d'IdemA et celui des sessions).
    pub roots: HashMap<PathBuf, PathBuf>,
}

/// La `Probe` réelle. `read_env` est un paramètre pour que les tests comptent ses appels : la version réelle est
/// `ps_launch_env`.
pub struct DiskProbe<'a> {
    pub paths: &'a ReclaimPaths,
    pub cache: &'a Mutex<ReclaimCache>,
    pub read_env: fn(i32) -> Option<String>,
    /// La date de démarrage de chaque processus candidat, lue d'un seul `ps` au début du tour (`ps_starts`). Jamais
    /// gardée d'un tour à l'autre : un PID réutilisé ne peut pas rendre une date périmée.
    pub starts: HashMap<i32, String>,
}

impl Probe for DiskProbe<'_> {
    fn has_transcript(&self, session_id: &str) -> bool {
        // L'identifiant entre dans un chemin : seul un UUID passe.
        if !is_uuid(session_id) {
            return false;
        }
        if locked(self.cache).transcripts.contains(session_id) {
            return true;
        }
        let file = format!("{session_id}.jsonl");
        let Ok(folders) = std::fs::read_dir(&self.paths.projects) else {
            return false;
        };
        let found = folders
            .flatten()
            .any(|folder| folder.path().join(&file).is_file());
        if found {
            locked(self.cache)
                .transcripts
                .insert(session_id.to_string());
        }
        found
    }

    fn manifest(&self, session_id: &str) -> Option<CaserneManifest> {
        if !is_uuid(session_id) {
            return None;
        }
        let path = self.paths.caserne.join(format!("{session_id}.json"));
        if std::fs::metadata(&path).ok()?.len() > MAX_MANIFEST_BYTES {
            return None;
        }
        parse_manifest(&std::fs::read_to_string(path).ok()?)
    }

    fn launch_env(&self, session: &LiveSession) -> Option<LaunchEnv> {
        let key = (
            session.pid,
            session.session_id.clone(),
            session.proc_start.clone(),
        );
        if let Some(env) = locked(self.cache).launch_envs.get(&key) {
            return Some(*env);
        }
        // Le verrou n'est pas tenu pendant `ps` ; la ligne brute ne survit pas à cette fonction.
        let env = parse_launch_env(&(self.read_env)(session.pid)?);
        locked(self.cache).launch_envs.insert(key, env);
        Some(env)
    }

    fn project_root(&self, cwd: &Path) -> PathBuf {
        if let Some(root) = locked(self.cache).roots.get(cwd) {
            return root.clone();
        }
        // Une réponse qui n'est pas définitive (voir `git_project_root`) ne se retient pas : le dossier lui-même pour
        // ce tour, la lecture est refaite au suivant.
        let Some(root) = git_project_root(cwd) else {
            return cwd.to_path_buf();
        };
        locked(self.cache)
            .roots
            .insert(cwd.to_path_buf(), root.clone());
        root
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    /// Servie par la table du tour (un seul `ps` pour tous les candidats) : un PID absent de la table, ou un `ps` qui
    /// a échoué, n'a pas de date et la session n'est pas listée.
    fn started_at(&self, pid: i32) -> Option<String> {
        self.starts.get(&pid).cloned()
    }
}

/// La racine du projet de `cwd` : le dépôt qui le contient, un worktree rendant celui du dépôt principal. `None` quand
/// la réponse n'est pas définitive : `git` n'a pas pu tourner, ou il a échoué sans dire que le dossier est hors dépôt
/// (dossier absent, propriétaire douteux, verrou...). Rien n'est alors retenu, la lecture est refaite au tour suivant.
/// Hors dépôt (`not a git repository`, lu sous `LC_ALL=C`, pour un dossier qui existe), la racine est le dossier lui-même.
///
/// `git` n'est pas borné dans le temps : `git::run` n'a pas de délai, et le tour de découverte est déjà détaché de la
/// boucle d'instantané (`scan_is_stuck` signale un tour trop long).
fn git_project_root(cwd: &Path) -> Option<PathBuf> {
    let output = git::run_with_env(
        cwd,
        Gesture::Read,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        None,
        &[("LC_ALL", "C")],
    )
    .ok()?;
    if !output.success() {
        let outside = cwd.is_dir() && output.stderr.to_lowercase().contains(NOT_A_REPOSITORY);
        return outside.then(|| cwd.to_path_buf());
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let common = Path::new(text.trim_end_matches('\n'));
    // Le dossier commun d'un dépôt ordinaire ou d'un worktree est `<racine>/.git`. Autre chose (dépôt nu, sous-module) :
    // le dossier de travail, faute de mieux.
    let root = match common.file_name() {
        Some(name) if name == ".git" => common.parent().unwrap_or(cwd),
        _ => cwd,
    };
    Some(root.to_path_buf())
}

/// Ce que `git` écrit, en anglais, hors de tout dépôt.
const NOT_A_REPOSITORY: &str = "not a git repository";

/// La date de démarrage du processus, dans le format de `procStart` : `ps -o lstart=` en UTC et en C, soit
/// `Mon Nov  2 18:01:12 2026` (le jour sur deux colonnes). `ps` remplit la fin de la ligne d'espaces : seule la fin est
/// rognée, un début est gardé tel quel.
pub(super) fn ps_start(pid: i32) -> Option<String> {
    let output = ps_output(
        &["-o", "lstart=", "-p", &pid.to_string()],
        &[("TZ", "UTC"), ("LC_ALL", "C")],
        pid,
    )?;
    let start = output.trim_end();
    (!start.is_empty()).then(|| start.to_string())
}

/// La date de démarrage de plusieurs processus d'un seul `ps -o pid=,lstart= -p a,b,c`, dans le format de `procStart`
/// (voir `ps_start`). Un processus que `ps` ne connaît pas est absent de la table. Un `ps` qui échoue rend une table
/// vide : aucune date, donc aucune liste fausse.
pub(super) fn ps_starts(pids: &[i32]) -> HashMap<i32, String> {
    let positive: Vec<i32> = pids.iter().copied().filter(|pid| *pid > 0).collect();
    let Some(first) = positive.first() else {
        return HashMap::new();
    };
    let list = positive
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let Some(output) = ps_output(
        &["-o", "pid=,lstart=", "-p", &list],
        &[("TZ", "UTC"), ("LC_ALL", "C")],
        *first,
    ) else {
        return HashMap::new();
    };
    output.lines().filter_map(parse_start_line).collect()
}

/// Une ligne `<pid> <lstart>` de `ps -o pid=,lstart=` : le `pid` est aligné à droite, l'`lstart` commence par le nom du
/// jour et sa fin est remplie d'espaces.
fn parse_start_line(line: &str) -> Option<(i32, String)> {
    let (pid, start) = line.trim().split_once(char::is_whitespace)?;
    let start = start.trim();
    if start.is_empty() {
        return None;
    }
    Some((pid.parse().ok()?, start.to_string()))
}

/// La ligne de commande du processus suivie de son environnement (`ps eww`), pour `parse_launch_env`. Cette ligne
/// contient des secrets : elle n'est ni journalisée ni citée, et l'appelant ne garde que le booléen qu'il en tire.
/// `None` quand la ligne ne porte aucun environnement : macOS le masque pour certains processus, et une ligne sans
/// `ANTHROPIC_BASE_URL` ne dirait alors rien d'un fournisseur.
pub fn ps_launch_env(pid: i32) -> Option<String> {
    let line = ps_output(&["eww", "-o", "command=", "-p", &pid.to_string()], &[], pid)?;
    has_environment(&line).then_some(line)
}

/// Vrai si la ligne de `ps eww` porte un environnement : `PATH` ou `HOME` y figure comme mot séparé. Tout processus
/// lancé depuis un shell a l'un des deux ; une ligne qui n'a ni l'un ni l'autre est une commande seule.
fn has_environment(line: &str) -> bool {
    line.contains(" PATH=") || line.contains(" HOME=")
}

/// La sortie de `ps` en texte, bornée par `TOOL_TIMEOUT` comme les outils du relevé des serveurs. `None` si le PID n'est
/// pas strictement positif, si `ps` n'a pas pu tourner, a dépassé le délai ou a échoué avec un message. Un processus
/// introuvable rend une sortie vide. Ni la sortie ni l'erreur ne sont journalisées.
pub(super) fn ps_output(args: &[&str], env: &[(&str, &str)], pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let never_stopped = AtomicBool::new(false);
    run_tool_with_env("ps", args, env, TOOL_TIMEOUT, &never_stopped).ok()
}

/// Au-delà, un tour de découverte est tenu pour bloqué : `ps` est borné, mais `git` et le test d'un dossier (un volume
/// réseau qui ne répond plus) ne le sont pas. Le tour suivant continue d'être sauté, et la panne est écrite une fois.
pub const SCAN_STUCK_AFTER: Duration = Duration::from_secs(30);

/// Vrai si un tour commencé à `started` dure depuis plus de `SCAN_STUCK_AFTER` à l'instant `now`.
pub fn scan_is_stuck(started: Instant, now: Instant) -> bool {
    now.saturating_duration_since(started) > SCAN_STUCK_AFTER
}

/// Un tour de découverte : lit les fichiers de session, les agents d'`idnma`, puis demande à `outside_sessions` ce qui
/// tourne hors d'IdemA (`own_pid` est le `pid` d'IdemA). Bloquant (disque, `ps`, `git`) : à lancer hors du fil d'interface.
///
/// `~/.idnma.yml` est relu à chaque tour ; illisible, aucun agent n'est connu et aucune session n'est ramenable.
/// `projects` sont les dossiers des projets d'IdemA : leur racine rejoint `roots`, comparée à celle des sessions.
pub fn scan(
    paths: &ReclaimPaths,
    pane_pids: &[i32],
    terminal_shells: &[i32],
    own_pid: i32,
    processes: &HashMap<i32, Process>,
    projects: &[PathBuf],
    cache: &Mutex<ReclaimCache>,
) -> ReclaimScan {
    let live = read_live_sessions(&paths.sessions);
    // Le lecteur sans filtre : un `sdk-cli` ou un `--print` qui écrit dans une conversation la tient aussi.
    let holders = read_session_holders(&paths.sessions);
    let agents: IdnmaAgents = std::fs::read_to_string(&paths.idnma_config)
        .map(|text| parse_idnma_agents(&text))
        .unwrap_or_default();
    // Un seul `ps` pour le tour : les candidats et ceux qui tiennent peut-être leur conversation.
    let mut candidates: Vec<i32> = live
        .iter()
        .map(|session| session.pid)
        .chain(holders.iter().map(|holder| holder.pid))
        .filter(|pid| processes.contains_key(pid))
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    let probe = DiskProbe {
        paths,
        cache,
        read_env: ps_launch_env,
        starts: ps_starts(&candidates),
    };

    let sessions = outside_sessions(
        &live,
        IdemaPids {
            pane_pids,
            terminal_shells,
            own_pid,
        },
        processes,
        &holders,
        &agents,
        &probe,
    );
    let mut roots = HashMap::new();
    for cwd in projects.iter().chain(sessions.iter().map(|s| &s.cwd)) {
        roots
            .entry(cwd.clone())
            .or_insert_with(|| probe.project_root(cwd));
    }
    locked(cache).retain_live(&live);
    ReclaimScan { sessions, roots }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const ID: &str = "bbbb2222-cccc-4ddd-8eee-ffff00001111";
    /// Un `pid` qui n'est dans aucune table de test : IdemA n'y descend d'aucune session.
    const NOT_IDEMA: i32 = 999_999;

    /// Un dossier temporaire propre au test, retiré à la fin quoi qu'il arrive.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("idema-test-{}-reclaim-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("test directory");
            // Le chemin réel (`/private/var/…` sous macOS) : celui que `git` rend.
            Scratch(dir.canonicalize().expect("canonical test directory"))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn paths_in(dir: &Path) -> ReclaimPaths {
        ReclaimPaths {
            sessions: dir.join("sessions"),
            projects: dir.join("projects"),
            caserne: dir.join("caserne"),
            idnma_config: dir.join("idnma.yml"),
        }
    }

    fn live(pid: i32) -> LiveSession {
        LiveSession {
            pid,
            session_id: ID.to_string(),
            cwd: PathBuf::from("/Users/x/dev/demo"),
            name: None,
            proc_start: "Mon Nov  2 18:01:12 2026".to_string(),
        }
    }

    /// Lance `git` dans `dir` avec une identité jetable et sans la configuration de l'utilisateur.
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs")
            .status;
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn a_transcript_is_found_under_any_project_folder() {
        let scratch = Scratch::new("transcript");
        let paths = paths_in(&scratch.0);
        let folder = paths.projects.join("-x-y");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(format!("{ID}.jsonl")), "{}\n").unwrap();
        let cache = Mutex::new(ReclaimCache::default());
        let probe = DiskProbe {
            paths: &paths,
            cache: &cache,
            read_env: |_| None,
            starts: HashMap::new(),
        };

        assert!(probe.has_transcript(ID));
        assert!(!probe.has_transcript("cccc3333-dddd-4eee-8fff-000011112222"));
        // Un identifiant qui n'est pas un UUID ne sert jamais à construire un chemin.
        assert!(!probe.has_transcript("../-x-y/anything"));
    }

    #[test]
    fn a_worktree_shares_the_root_of_its_repository() {
        let scratch = Scratch::new("worktree");
        let repo = scratch.0.join("repo");
        let tree = scratch.0.join("tree");
        let plain = scratch.0.join("plain");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        git(&repo, &["init", "--quiet"]);
        git(
            &repo,
            &["commit", "--allow-empty", "--quiet", "-m", "first"],
        );
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "side",
                tree.to_str().unwrap(),
            ],
        );
        let paths = paths_in(&scratch.0);
        let cache = Mutex::new(ReclaimCache::default());
        let probe = DiskProbe {
            paths: &paths,
            cache: &cache,
            read_env: |_| None,
            starts: HashMap::new(),
        };

        assert_eq!(probe.project_root(&repo), repo);
        assert_eq!(probe.project_root(&tree), repo);
        // Hors de git : le dossier lui-même.
        assert_eq!(probe.project_root(&plain), plain);
    }

    #[test]
    fn a_root_is_asked_to_git_once_per_folder() {
        let scratch = Scratch::new("root-once");
        let repo = scratch.0.join("repo");
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        git(&repo, &["init", "--quiet"]);
        let paths = paths_in(&scratch.0);
        let cache = Mutex::new(ReclaimCache::default());
        let probe = DiskProbe {
            paths: &paths,
            cache: &cache,
            read_env: |_| None,
            starts: HashMap::new(),
        };
        // Le dossier interrogé n'est pas la racine : seul `git` sait remonter à `repo`.
        assert_eq!(probe.project_root(&sub), repo);

        // Le dépôt disparaît : un second `git` répondrait `sub` (hors dépôt), la réponse vient donc du cache.
        std::fs::remove_dir_all(repo.join(".git")).unwrap();
        assert_eq!(probe.project_root(&sub), repo);
    }

    #[test]
    fn a_folder_outside_git_is_cached_and_an_absent_folder_is_not() {
        let scratch = Scratch::new("root-failures");
        let paths = paths_in(&scratch.0);
        let cache = Mutex::new(ReclaimCache::default());
        let probe = DiskProbe {
            paths: &paths,
            cache: &cache,
            read_env: |_| None,
            starts: HashMap::new(),
        };

        // Un dossier réel hors de git : `git` répond « not a git repository », réponse définitive.
        let plain = scratch.0.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(probe.project_root(&plain), plain);
        assert!(locked(&cache).roots.contains_key(&plain));

        // Un dossier absent fait sortir `git` en 128 sans que ce soit une réponse : rien n'entre dans le cache.
        let base = scratch.0.join("later");
        let sub = base.join("sub");
        assert_eq!(probe.project_root(&sub), sub);
        assert!(!locked(&cache).roots.contains_key(&sub));

        // Le dossier existe ensuite, dans un dépôt : la lecture est refaite et rend la racine.
        std::fs::create_dir_all(&sub).unwrap();
        git(&base, &["init", "--quiet"]);
        assert_eq!(probe.project_root(&sub), base);
    }

    #[test]
    fn a_line_without_environment_is_not_an_environment() {
        // macOS masque l'environnement de certains processus : la ligne n'est alors que la commande.
        assert!(!has_environment("/usr/bin/claude --model opus"));
        assert!(!has_environment(""));
        // Une variable collée à la commande n'est pas un mot séparé.
        assert!(!has_environment("/usr/bin/claudePATH=/bin"));
        assert!(has_environment("claude --x PATH=/usr/bin TERM=xterm"));
        assert!(has_environment("claude HOME=/Users/x"));
    }

    /// Le processus fils des tests qui lisent un vrai environnement : ce binaire de test relancé sur cette seule
    /// fonction. Hors de ce rôle (variable absente), elle ne fait rien.
    #[test]
    fn sleeper_for_the_child_process_tests() {
        if std::env::var_os("IDEMA_TEST_SLEEPER").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    #[test]
    fn the_launch_env_of_a_real_child_is_read() {
        // Un binaire qui n'est pas de la plateforme : macOS montre l'environnement de ses processus, pas celui de `sleep`.
        let mut child = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "reclaim::disk::tests::sleeper_for_the_child_process_tests",
            ])
            .env("IDEMA_TEST_SLEEPER", "1")
            .env("ANTHROPIC_BASE_URL", "http://invented.example")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("child spawned");
        let pid = child.id() as i32;

        let line = ps_launch_env(pid);

        // Le fils est tué par son seul pid, puis attendu, avant toute assertion.
        child.kill().expect("child killed");
        child.wait().expect("child reaped");
        // Les assertions ne citent pas la ligne : elle porte l'environnement.
        let line = line.expect("the environment of the child is shown");
        assert!(parse_launch_env(&line).third_party);
    }

    #[test]
    fn a_scan_is_stuck_after_thirty_seconds() {
        let started = Instant::now();
        assert!(!scan_is_stuck(started, started));
        assert!(!scan_is_stuck(started, started + Duration::from_secs(30)));
        assert!(scan_is_stuck(started, started + Duration::from_secs(31)));
    }

    static ENV_READS: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn a_new_process_start_reads_the_launch_env_again() {
        let paths = paths_in(Path::new("/nonexistent"));
        let cache = Mutex::new(ReclaimCache::default());
        let probe = DiskProbe {
            paths: &paths,
            cache: &cache,
            read_env: |_| {
                ENV_READS.fetch_add(1, Ordering::SeqCst);
                Some("claude --x".to_string())
            },
            starts: HashMap::new(),
        };
        let first = live(4244);
        let mut reborn = live(4244);
        reborn.proc_start = "Tue Nov  3 09:00:00 2026".to_string();

        probe.launch_env(&first);
        probe.launch_env(&first);
        assert_eq!(ENV_READS.load(Ordering::SeqCst), 1);
        // Même pid, même conversation, autre processus : l'ancienne réponse ne vaut plus.
        probe.launch_env(&reborn);
        assert_eq!(ENV_READS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn paths_follow_the_config_dir() {
        let home = OsStr::new("/Users/x");
        let config = OsStr::new("/Users/x/.claude-alt");

        let default = paths(Some(home), None).unwrap();
        assert_eq!(default.sessions, Path::new("/Users/x/.claude/sessions"));
        assert_eq!(default.projects, Path::new("/Users/x/.claude/projects"));

        let moved = paths(Some(home), Some(config)).unwrap();
        assert_eq!(moved.sessions, Path::new("/Users/x/.claude-alt/sessions"));
        assert_eq!(moved.projects, Path::new("/Users/x/.claude-alt/projects"));
        // La Caserne et `idnma` ne suivent pas `CLAUDE_CONFIG_DIR`.
        assert_eq!(
            moved.caserne,
            Path::new("/Users/x/.config/caserne/sessions")
        );
        assert_eq!(moved.idnma_config, Path::new("/Users/x/.idnma.yml"));
        assert_eq!(default.caserne, moved.caserne);

        assert_eq!(paths(None, Some(config)), None);
        assert_eq!(paths(None, None), None);
    }

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn the_launch_env_is_read_once_per_session() {
        let paths = paths_in(Path::new("/nonexistent"));
        let cache = Mutex::new(ReclaimCache::default());
        let probe = DiskProbe {
            paths: &paths,
            cache: &cache,
            read_env: |_| {
                CALLS.fetch_add(1, Ordering::SeqCst);
                Some("claude --x ANTHROPIC_BASE_URL=http://invented.example".to_string())
            },
            starts: HashMap::new(),
        };
        let session = live(4242);

        let first = probe.launch_env(&session);
        let second = probe.launch_env(&session);

        assert_eq!(first, Some(LaunchEnv { third_party: true }));
        assert_eq!(second, first);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn a_failed_read_is_retried() {
        let paths = paths_in(Path::new("/nonexistent"));
        let cache = Mutex::new(ReclaimCache::default());
        let probe = DiskProbe {
            paths: &paths,
            cache: &cache,
            read_env: |_| {
                (ATTEMPTS.fetch_add(1, Ordering::SeqCst) > 0).then(|| "claude --x".to_string())
            },
            starts: HashMap::new(),
        };
        let session = live(4243);

        assert_eq!(probe.launch_env(&session), None);
        assert_eq!(
            probe.launch_env(&session),
            Some(LaunchEnv { third_party: false })
        );
        assert_eq!(ATTEMPTS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn one_ps_reads_the_utc_lstart_of_every_candidate() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep starts");
        let child_pid = child.id() as i32;
        let own = std::process::id() as i32;
        // Un enfant déjà récolté : son numéro est celui d'un processus que `ps` ne connaît plus.
        let mut done = Command::new("true").spawn().expect("true starts");
        let gone = done.id() as i32;
        done.wait().expect("child is reaped");

        let starts = ps_starts(&[own, child_pid, gone, -4]);
        let child_start = ps_start(child_pid);

        // Le fils est tué par son seul pid, puis attendu, avant toute assertion.
        child.kill().expect("child killed");
        child.wait().expect("child reaped");
        // Même format que la lecture d'un seul processus, celle de `procStart`.
        assert_eq!(starts.get(&own), ps_start(own).as_ref());
        assert!(child_start.is_some());
        assert_eq!(starts.get(&child_pid), child_start.as_ref());
        // Un numéro que `ps` ne connaît pas, ou qui n'est pas positif, n'a pas de date.
        assert_eq!(starts.len(), 2);

        let started = &starts[&own];
        assert_eq!(started, started.trim_end());
        assert_eq!(started, started.trim_start());
        // `Www Mmm dd hh:mm:ss yyyy` : le jour sur deux colonnes, rempli d'une espace.
        let parsed = chrono::NaiveDateTime::parse_from_str(started, "%a %b %e %H:%M:%S %Y")
            .unwrap_or_else(|error| panic!("{started:?} has the lstart shape: {error}"));
        // En UTC, le démarrage du test tombe juste avant maintenant ; en heure locale à l'est de Greenwich, il
        // tomberait dans le futur.
        let age = chrono::Utc::now().naive_utc() - parsed;
        assert!(
            (chrono::Duration::seconds(-2)..chrono::Duration::hours(1)).contains(&age),
            "the test process started {age} ago"
        );
    }

    #[test]
    fn a_probe_serves_started_at_from_its_table_only() {
        let paths = paths_in(Path::new("/nonexistent"));
        let cache = Mutex::new(ReclaimCache::default());
        let probe = DiskProbe {
            paths: &paths,
            cache: &cache,
            read_env: |_| None,
            starts: HashMap::from([(7, "Mon Nov  2 18:01:12 2026".to_string())]),
        };
        assert_eq!(
            probe.started_at(7).as_deref(),
            Some("Mon Nov  2 18:01:12 2026")
        );
        // Absent de la table (un `ps` en échec rend une table vide) : pas de date, même pour un processus bien vivant.
        assert_eq!(probe.started_at(std::process::id() as i32), None);
    }

    #[test]
    fn a_start_line_is_a_pid_then_the_padded_lstart() {
        assert_eq!(
            parse_start_line("  842 Mon Nov  2 18:01:12 2026   "),
            Some((842, "Mon Nov  2 18:01:12 2026".to_string()))
        );
        assert_eq!(parse_start_line("842"), None);
        assert_eq!(parse_start_line("x Mon Nov  2 18:01:12 2026"), None);
        assert_eq!(parse_start_line(""), None);
    }

    /// Un monde sur disque où le processus du test joue un `claude` hors d'IdemA : fichier de session, transcript,
    /// manifeste Caserne ouvert (il décide du fournisseur sans lire l'environnement) et `idnma.yml`.
    fn world(scratch: &Scratch) -> (ReclaimPaths, PathBuf, HashMap<i32, Process>) {
        let pid = std::process::id() as i32;
        let paths = paths_in(&scratch.0);
        let cwd = scratch.0.join("demo");
        for dir in [
            &paths.sessions,
            &paths.projects.join("-x-demo"),
            &paths.caserne,
            &cwd,
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let start = ps_start(pid).expect("the test process is found");
        let file = format!(
            r#"{{"pid":{pid},"sessionId":"{ID}","cwd":{cwd:?},"kind":"interactive","entrypoint":"cli","procStart":{start:?}}}"#
        );
        std::fs::write(paths.sessions.join(format!("{pid}.json")), file).unwrap();
        std::fs::write(paths.projects.join(format!("-x-demo/{ID}.jsonl")), "{}\n").unwrap();
        let manifest =
            format!(r#"{{"harness":"claude","agent":"claude","engine":null,"pid":{pid}}}"#);
        std::fs::write(paths.caserne.join(format!("{ID}.json")), manifest).unwrap();
        std::fs::write(
            &paths.idnma_config,
            "agents:\n  claude:\n    engines:\n      default:\n",
        )
        .unwrap();
        let processes = HashMap::from([(
            pid,
            Process {
                pid,
                ppid: 1,
                pgid: pid,
                command: "claude --dangerously-skip-permissions".to_string(),
            },
        )]);
        (paths, cwd, processes)
    }

    #[test]
    fn a_scan_lists_a_session_running_outside_idema() {
        let scratch = Scratch::new("scan");
        let (paths, cwd, processes) = world(&scratch);
        let cache = Mutex::new(ReclaimCache::default());
        let project = scratch.0.join("elsewhere");

        let found = scan(
            &paths,
            &[],
            &[],
            NOT_IDEMA,
            &processes,
            std::slice::from_ref(&project),
            &cache,
        );

        assert_eq!(found.sessions.len(), 1);
        let session = &found.sessions[0];
        assert_eq!(session.session_id, ID);
        assert_eq!(session.cwd, cwd);
        assert!(
            session
                .resume
                .as_deref()
                .is_some_and(|resume| resume.contains(ID))
        );
        // Le dossier de la session et celui du projet reçoivent une racine, hors de git : eux-mêmes.
        assert_eq!(found.roots.get(&cwd), Some(&cwd));
        assert_eq!(found.roots.get(&project), Some(&project));
    }

    #[test]
    fn a_scan_skips_the_session_that_hosts_idema() {
        let scratch = Scratch::new("scan-host");
        let (paths, _, mut processes) = world(&scratch);
        let host = std::process::id() as i32;
        // IdemA (processus 999 998) a été lancé depuis le `claude` du monde.
        processes.insert(
            999_998,
            Process {
                pid: 999_998,
                ppid: host,
                pgid: 999_998,
                command: "idema".to_string(),
            },
        );
        let cache = Mutex::new(ReclaimCache::default());

        let found = scan(&paths, &[], &[], 999_998, &processes, &[], &cache);

        assert!(found.sessions.is_empty());
    }

    #[test]
    fn a_scan_does_not_offer_a_conversation_another_process_holds() {
        let scratch = Scratch::new("scan-held");
        let (paths, _, mut processes) = world(&scratch);
        // Un `sdk-cli` que `read_live_sessions` écarte mais qui écrit dans la même conversation.
        let other = std::process::id() as i32 + 1;
        let held = format!(
            r#"{{"pid":{other},"sessionId":"{ID}","cwd":"/x","kind":"background","entrypoint":"sdk-cli"}}"#
        );
        std::fs::write(paths.sessions.join(format!("{other}.json")), held).unwrap();
        processes.insert(
            other,
            Process {
                pid: other,
                ppid: 1,
                pgid: other,
                command: "node worker".to_string(),
            },
        );
        let cache = Mutex::new(ReclaimCache::default());

        let found = scan(&paths, &[], &[], NOT_IDEMA, &processes, &[], &cache);

        assert_eq!(found.sessions.len(), 1);
        assert_eq!(found.sessions[0].resume, None);
    }

    #[test]
    fn a_scan_without_idnma_config_lists_but_never_offers_a_resume() {
        let scratch = Scratch::new("scan-no-agents");
        let (paths, _, processes) = world(&scratch);
        std::fs::remove_file(&paths.idnma_config).unwrap();
        let cache = Mutex::new(ReclaimCache::default());

        let found = scan(&paths, &[], &[], NOT_IDEMA, &processes, &[], &cache);

        assert_eq!(found.sessions.len(), 1);
        assert_eq!(found.sessions[0].resume, None);
    }

    #[test]
    fn a_scan_forgets_what_a_dead_session_left_in_the_cache() {
        let scratch = Scratch::new("scan-prune");
        let (paths, _, processes) = world(&scratch);
        let cache = Mutex::new(ReclaimCache::default());
        {
            let mut cache = locked(&cache);
            cache.launch_envs.insert(
                (1, ID.to_string(), String::new()),
                LaunchEnv { third_party: true },
            );
            cache
                .transcripts
                .insert("cccc3333-dddd-4eee-8fff-000011112222".to_string());
        }

        scan(&paths, &[], &[], NOT_IDEMA, &processes, &[], &cache);

        let cache = locked(&cache);
        assert!(cache.launch_envs.is_empty());
        assert_eq!(cache.transcripts, HashSet::from([ID.to_string()]));
    }
}
