//! Relevé des ports TCP en écoute et de leurs processus (spec P4 §5.2), hors du fil principal.
//! Les analyseurs (`parse_*`) sont purs ; `Scanner` possède son fil, qui lance `lsof` et `ps` et sonde le HTTP,
//! et envoie chaque relevé par un canal que le fil principal vide.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::hud;

/// Période du relevé (spec §5.2).
pub const SCAN_PERIOD: Duration = Duration::from_secs(2);
/// Délai de la sonde HTTP (spec §5.2).
pub const HTTP_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// Le port de l'inspecteur de Node : écarté du classement des serveurs (spec §5.2).
pub const NODE_INSPECTOR_PORT: u16 = 9229;

/// Au-delà, un `lsof` ou un `ps` est tenu pour bloqué et abandonné.
pub(crate) const TOOL_TIMEOUT: Duration = Duration::from_secs(5);
/// Le fil dort par tranches de cette durée au plus, pour que `Drop` n'attende pas une période entière.
const SLEEP_SLICE: Duration = Duration::from_millis(100);

/// Un processus de `ps`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub command: String,
}

/// Un processus qui écoute sur un port (IPv4 et IPv6 confondus).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listener {
    pub pid: i32,
    pub port: u16,
}

/// Où va la sortie standard d'un processus, d'après `lsof`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    /// Un fichier ordinaire : on peut en suivre la fin.
    File(PathBuf),
    /// Autre chose, tel que `lsof` le dit : `/dev/null`, `pipe`, `/dev/ttys004`.
    Other(String),
}

/// Un relevé complet.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub processes: HashMap<i32, Process>,
    pub listeners: Vec<Listener>,
    /// Dossier courant, pour les seuls PID qui écoutent.
    pub cwd: HashMap<i32, PathBuf>,
    /// Sortie standard, pour les seuls PID qui écoutent.
    pub stdout: HashMap<i32, OutputTarget>,
    /// Les couples (pid, port) qui répondent à une requête HTTP sur `127.0.0.1`, parmi ceux que le `ProbeScope` admet :
    /// un couple absent n'a pas répondu, ou n'a pas été sondé.
    pub http: HashSet<(i32, u16)>,
    pub taken_at: Instant,
}

/// Un relevé réussi, ou la raison de l'échec (jamais un relevé vide pris pour un succès).
pub type ScanResult = Result<Snapshot, String>;

/// Ce que la sonde HTTP vise (spec §5.2) : les processus qui descendent d'un pane d'IdemA, ceux dont le dossier courant
/// est dans un projet connu, et les ports déclarés par une entrée de configuration. Seuls ceux-là peuvent devenir une
/// ligne ; les ports des autres services du Mac (une base de données, Docker, un modèle local) ne sont pas sondés.
#[derive(Debug, Clone, Default)]
pub struct ProbeScope {
    /// Les PID des racines de pane vivantes.
    pub pane_pids: HashSet<i32>,
    pub projects: Vec<PathBuf>,
    pub declared_ports: HashSet<u16>,
    /// Les PID des services de launchd (`launchctl list`, sans les apps), lus à chaque relevé : un service installé
    /// (LaunchAgent) dont le dossier est dans un projet n'est pas un serveur de ce projet. L'orphelin d'un `cmd &`
    /// d'agent, rattaché à launchd, n'est pas un job et reste repéré (P4-9).
    pub launchd_jobs: HashSet<i32>,
}

impl ProbeScope {
    /// Vrai si le couple peut devenir une ligne, donc mérite la sonde.
    pub fn admits(
        &self,
        listener: Listener,
        processes: &HashMap<i32, Process>,
        cwd: &HashMap<i32, PathBuf>,
    ) -> bool {
        self.declared_ports.contains(&listener.port)
            || (cwd
                .get(&listener.pid)
                .is_some_and(|dir| self.projects.iter().any(|project| dir.starts_with(project)))
                && !self.runs_under_a_launchd_job(listener.pid, processes))
            || self.descends_from_a_pane(listener.pid, processes)
    }

    /// Le processus, ou l'un de ses ancêtres avant launchd, est un service de launchd.
    fn runs_under_a_launchd_job(&self, pid: i32, processes: &HashMap<i32, Process>) -> bool {
        let mut visited = HashSet::new();
        let mut current = pid;
        while visited.insert(current) {
            if self.launchd_jobs.contains(&current) {
                return true;
            }
            match processes.get(&current) {
                Some(process) if process.ppid > 1 => current = process.ppid,
                _ => return false,
            }
        }
        false
    }

    /// Le processus, ou l'un de ses ancêtres avant launchd, est une racine de pane (la remontée du registre).
    fn descends_from_a_pane(&self, pid: i32, processes: &HashMap<i32, Process>) -> bool {
        let mut visited = HashSet::new();
        let mut current = pid;
        while visited.insert(current) {
            if self.pane_pids.contains(&current) {
                return true;
            }
            match processes.get(&current) {
                Some(process) if process.ppid > 1 => current = process.ppid,
                _ => return false,
            }
        }
        false
    }
}

/// Sortie de `launchctl list` (« PID\tStatus\tLabel ») : les PID des services en marche ; `-` pour un job arrêté.
/// Les apps lancées par LaunchServices y sont aussi, sous un label `application.*` (Zed, Ghostty, IdemA) : elles sont
/// écartées, sans quoi tout serveur lancé depuis le terminal d'une app remonterait jusqu'à elle et perdrait son projet.
pub fn parse_launchctl_list(output: &str) -> HashSet<i32> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let pid = fields.next()?.trim().parse().ok()?;
            let label = fields.nth(1)?.trim();
            (!label.starts_with("application.")).then_some(pid)
        })
        .collect()
}

/// Sortie de `lsof -nP -iTCP -sTCP:LISTEN -F pn` : un `Listener` par couple (pid, port).
pub fn parse_lsof_listen(output: &str) -> Vec<Listener> {
    let mut listeners: Vec<Listener> = Vec::new();
    let mut pid: Option<i32> = None;
    for line in output.lines() {
        let Some((field, value)) = split_field(line) else {
            continue;
        };
        match field {
            'p' => pid = value.parse().ok(),
            'n' => {
                let (Some(pid), Some(port)) = (pid, port_of(value)) else {
                    continue;
                };
                let listener = Listener { pid, port };
                if !listeners.contains(&listener) {
                    listeners.push(listener);
                }
            }
            _ => {}
        }
    }
    listeners
}

/// Le port d'une adresse `*:8811`, `127.0.0.1:8811` ou `[::1]:8811`.
fn port_of(name: &str) -> Option<u16> {
    let (_, port) = name.rsplit_once(':')?;
    port.parse().ok()
}

/// Sépare une ligne `-F` en son identifiant d'un caractère et sa valeur.
fn split_field(line: &str) -> Option<(char, &str)> {
    let mut chars = line.chars();
    let field = chars.next()?;
    Some((field, chars.as_str()))
}

/// Sortie de `ps -axo pid=,ppid=,pgid=,command=` ; la commande garde ses espaces.
pub fn parse_ps(output: &str) -> HashMap<i32, Process> {
    let mut processes = HashMap::new();
    for line in output.lines() {
        let Some((pid, rest)) = next_number(line) else {
            continue;
        };
        let Some((ppid, rest)) = next_number(rest) else {
            continue;
        };
        let Some((pgid, rest)) = next_number(rest) else {
            continue;
        };
        processes.insert(
            pid,
            Process {
                pid,
                ppid,
                pgid,
                command: rest.trim().to_string(),
            },
        );
    }
    processes
}

/// Lit un entier en tête de ligne et rend le reste.
fn next_number(text: &str) -> Option<(i32, &str)> {
    let text = text.trim_start();
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    let number = text[..end].parse().ok()?;
    Some((number, &text[end..]))
}

/// Sortie de `lsof -a -p <pids> -d cwd,1 -F pftn` : le dossier courant et la sortie standard de chaque PID.
pub fn parse_lsof_fd(output: &str) -> (HashMap<i32, PathBuf>, HashMap<i32, OutputTarget>) {
    let mut cwd = HashMap::new();
    let mut stdout = HashMap::new();
    let mut pid: Option<i32> = None;
    let mut fd: Option<String> = None;
    let mut kind = String::new();
    let mut name = String::new();

    // Ferme l'enregistrement du descripteur courant.
    let mut flush =
        |pid: Option<i32>, fd: &mut Option<String>, kind: &mut String, name: &mut String| {
            if let (Some(pid), Some(fd)) = (pid, fd.take()) {
                match fd.as_str() {
                    "cwd" => {
                        cwd.insert(pid, PathBuf::from(&*name));
                    }
                    "1" => {
                        let target = match kind.as_str() {
                            "REG" => OutputTarget::File(PathBuf::from(&*name)),
                            "PIPE" => OutputTarget::Other("pipe".to_string()),
                            _ => OutputTarget::Other(name.clone()),
                        };
                        stdout.insert(pid, target);
                    }
                    _ => {}
                }
            }
            *fd = None;
            kind.clear();
            name.clear();
        };

    for line in output.lines() {
        let Some((field, value)) = split_field(line) else {
            continue;
        };
        match field {
            'p' => {
                flush(pid, &mut fd, &mut kind, &mut name);
                pid = value.parse().ok();
            }
            'f' => {
                flush(pid, &mut fd, &mut kind, &mut name);
                fd = Some(value.to_string());
            }
            't' => kind = value.to_string(),
            'n' => name = value.to_string(),
            _ => {}
        }
    }
    flush(pid, &mut fd, &mut kind, &mut name);
    (cwd, stdout)
}

/// Vrai si le port répond à une requête HTTP (n'importe quel statut) avant `timeout`, sur `127.0.0.1` d'abord,
/// puis sur `[::1]` si la première connexion échoue (Vite sur Node 17+ n'écoute parfois qu'en IPv6).
pub fn probe_http(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
    .into_iter()
    .any(|address| match connect_before(&address, deadline) {
        Some(stream) => answers_http(stream, &address, deadline),
        None => false,
    })
}

fn time_left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// La connexion, si elle aboutit avant l'échéance.
fn connect_before(address: &SocketAddr, deadline: Instant) -> Option<TcpStream> {
    let left = time_left(deadline);
    if left.is_zero() {
        return None;
    }
    TcpStream::connect_timeout(address, left).ok()
}

/// Envoie `HEAD /` et lit le début de la réponse avant l'échéance : vrai si elle commence par `HTTP/`.
/// L'en-tête `Host` est obligatoire en pratique : sans lui, Bun.serve passe l'URL relative `"/"` au gestionnaire,
/// et le serveur MCP local de Claude Code imprime une `TypeError: Invalid URL` par-dessus son interface.
fn answers_http(mut stream: TcpStream, address: &SocketAddr, deadline: Instant) -> bool {
    // `SocketAddr` s'affiche `127.0.0.1:port` ou `[::1]:port`, la forme attendue par `Host`.
    let request = format!("HEAD / HTTP/1.0\r\nHost: {address}\r\n\r\n");
    if stream
        .set_write_timeout(Some(time_left(deadline).max(Duration::from_millis(1))))
        .is_err()
        || stream.write_all(request.as_bytes()).is_err()
    {
        return false;
    }
    let mut head = [0u8; 5];
    let mut filled = 0;
    while filled < head.len() {
        let left = time_left(deadline);
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return false;
        }
        match stream.read(&mut head[filled..]) {
            Ok(0) | Err(_) => return false,
            Ok(count) => filled += count,
        }
    }
    &head == b"HTTP/"
}

/// Lance un outil et rend sa sortie. Un code de sortie non nul avec une sortie vide **et** un message sur stderr
/// est une erreur ; sans message (`lsof` rend 1 quand rien n'écoute), c'est un relevé vide. Un outil absent,
/// plus long que `timeout` ou interrompu par `stop` est aussi une erreur.
pub(super) fn run_tool(
    program: &str,
    args: &[&str],
    timeout: Duration,
    stop: &AtomicBool,
) -> Result<String, String> {
    run_tool_with_env(program, args, &[], timeout, stop)
}

/// Comme `run_tool`, avec des variables d'environnement en plus de celles de l'application (`TZ=UTC LC_ALL=C ps`).
pub(crate) fn run_tool_with_env(
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
    timeout: Duration,
    stop: &AtomicBool,
) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{program} cannot start: {error}"))?;
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout_reader = reader(
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
    let stderr_reader = reader(
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Err(format!("{program} wait failed: {error}")),
        }
        if stop.load(Ordering::Relaxed) || started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(if stop.load(Ordering::Relaxed) {
                format!("{program} interrupted")
            } else {
                format!("{program} timed out after {} ms", timeout.as_millis())
            });
        }
        thread::sleep(Duration::from_millis(5));
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() && stdout.is_empty() {
        let message = String::from_utf8_lossy(&stderr);
        let message = message.trim();
        if !message.is_empty() {
            let first_line = message.lines().next().unwrap_or(message);
            return Err(format!("{program} failed ({status}): {first_line}"));
        }
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Au plus ce nombre de sondes à la fois.
const PROBE_BATCH: usize = 32;
/// Un couple muet est resondé jusqu'à ce nombre de fois au total, puis abandonné tant qu'il existe.
const PROBE_MAX_ATTEMPTS: u32 = 5;
/// Délai minimal entre deux sondes d'un même couple muet.
const PROBE_RETRY_AFTER: Duration = Duration::from_secs(5);

/// Ce que la sonde a appris d'un couple (pid, port).
struct ProbeAnswer {
    http: bool,
    attempts: u32,
    last_probe: Instant,
}

/// Ce que le fil se rappelle d'un relevé à l'autre : la sonde HTTP vise les couples nouveaux, et resonde
/// quelques fois ceux qui n'ont pas répondu (un serveur qui écoute avant de savoir répondre, comme Next qui compile).
#[derive(Default)]
struct ProbeMemory {
    answers: HashMap<(i32, u16), ProbeAnswer>,
}

impl ProbeMemory {
    /// Sonde ce qu'il faut sonder à l'instant `now` (par lots, en parallèle : un port muet coûte 500 ms),
    /// oublie les couples disparus. Un `stop` levé abandonne les sondes restantes.
    fn update(
        &mut self,
        listeners: &[Listener],
        now: Instant,
        stop: &AtomicBool,
    ) -> HashSet<(i32, u16)> {
        let current: HashSet<(i32, u16)> = listeners
            .iter()
            .map(|listener| (listener.pid, listener.port))
            .collect();
        self.answers.retain(|key, _| current.contains(key));
        let due: Vec<(i32, u16)> = current
            .iter()
            .copied()
            .filter(|key| match self.answers.get(key) {
                None => true,
                Some(answer) => {
                    !answer.http
                        && answer.attempts < PROBE_MAX_ATTEMPTS
                        && now.saturating_duration_since(answer.last_probe) >= PROBE_RETRY_AFTER
                }
            })
            .collect();
        for batch in due.chunks(PROBE_BATCH) {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let results: Vec<((i32, u16), bool)> = thread::scope(|scope| {
                let handles: Vec<_> = batch
                    .iter()
                    .map(|&key| {
                        scope.spawn(move || {
                            let answered = !stop.load(Ordering::Relaxed)
                                && probe_http(key.1, HTTP_PROBE_TIMEOUT);
                            (key, answered)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .filter_map(|handle| handle.join().ok())
                    .collect()
            });
            for (key, http) in results {
                let attempts = self.answers.get(&key).map_or(0, |answer| answer.attempts) + 1;
                self.answers.insert(
                    key,
                    ProbeAnswer {
                        http,
                        attempts,
                        last_probe: now,
                    },
                );
            }
        }
        self.answers
            .iter()
            .filter(|(_, answer)| answer.http)
            .map(|(key, _)| *key)
            .collect()
    }
}

/// Un relevé complet : `lsof` des ports, `ps`, puis `lsof` des descripteurs des seuls PID qui écoutent.
fn scan_once(memory: &mut ProbeMemory, scope: &Mutex<ProbeScope>, stop: &AtomicBool) -> ScanResult {
    // Pris avant le premier outil : un relevé ne décrit rien de ce qui s'est passé après cet instant. La garde contre
    // les relevés d'avant un démarrage s'appuie là-dessus.
    let taken_at = Instant::now();
    let listen = run_tool(
        "lsof",
        &["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pn"],
        TOOL_TIMEOUT,
        stop,
    )?;
    let listeners = parse_lsof_listen(&listen);
    let ps = run_tool(
        "ps",
        &["-axo", "pid=,ppid=,pgid=,command="],
        TOOL_TIMEOUT,
        stop,
    )?;
    let processes = parse_ps(&ps);

    let mut pids: Vec<i32> = listeners.iter().map(|listener| listener.pid).collect();
    pids.sort_unstable();
    pids.dedup();
    let (cwd, stdout) = if pids.is_empty() {
        (HashMap::new(), HashMap::new())
    } else {
        let list = pids
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let fd = run_tool(
            "lsof",
            &["-a", "-p", &list, "-d", "cwd,1", "-F", "pftn"],
            TOOL_TIMEOUT,
            stop,
        )?;
        parse_lsof_fd(&fd)
    };
    // La sonde ne vise que ce qui peut devenir une ligne : le périmètre est lu au dernier moment, pour suivre le pane
    // ou le projet ouvert pendant le relevé.
    let mut scope = scope.lock().unwrap_or_else(PoisonError::into_inner).clone();
    // Sans `launchctl`, le relevé continue comme avant : un service installé se montre, rien ne disparaît.
    scope.launchd_jobs = match run_tool("launchctl", &["list"], TOOL_TIMEOUT, stop) {
        Ok(output) => parse_launchctl_list(&output),
        Err(error) => {
            hud::log_debug(&format!("launchctl list failed: {error}"));
            HashSet::new()
        }
    };
    let probed: Vec<Listener> = listeners
        .iter()
        .copied()
        .filter(|&listener| scope.admits(listener, &processes, &cwd))
        .collect();
    let http = memory.update(&probed, Instant::now(), stop);
    Ok(Snapshot {
        processes,
        listeners,
        cwd,
        stdout,
        http,
        taken_at,
    })
}

/// Le fil de relevé : il envoie un `ScanResult` toutes les `period`, et s'arrête (et est attendu) au `Drop`.
pub struct Scanner {
    stop: Arc<AtomicBool>,
    /// Levé par `scan_now` : le fil écourte son attente et relève aussitôt.
    wake: Arc<AtomicBool>,
    /// Ce que la sonde HTTP vise, posé par le fil principal ; vide au départ, donc rien n'est sondé avant lui.
    scope: Arc<Mutex<ProbeScope>>,
    handle: Option<JoinHandle<()>>,
}

impl Scanner {
    pub fn spawn(period: Duration, sender: Sender<ScanResult>) -> Scanner {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let wake = Arc::new(AtomicBool::new(false));
        let thread_wake = Arc::clone(&wake);
        let scope = Arc::new(Mutex::new(ProbeScope::default()));
        let thread_scope = Arc::clone(&scope);
        let handle = thread::Builder::new()
            .name("idema-servers-scan".to_string())
            .spawn(move || scan_loop(period, &sender, &thread_scope, &thread_stop, &thread_wake))
            .expect("spawn idema-servers-scan");
        Scanner {
            stop,
            wake,
            scope,
            handle: Some(handle),
        }
    }

    /// Demande un relevé sans attendre la fin de la période (après un arrêt ou un démarrage). Un relevé déjà en cours
    /// finit d'abord : le suivant part ensuite aussitôt.
    pub fn scan_now(&self) {
        self.wake.store(true, Ordering::Relaxed);
    }

    /// Remplace ce que la sonde HTTP vise ; le prochain relevé s'en sert.
    pub fn set_probe_scope(&self, scope: ProbeScope) {
        *self.scope.lock().unwrap_or_else(PoisonError::into_inner) = scope;
    }
}

impl Drop for Scanner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn scan_loop(
    period: Duration,
    sender: &Sender<ScanResult>,
    scope: &Mutex<ProbeScope>,
    stop: &AtomicBool,
    wake: &AtomicBool,
) {
    let mut memory = ProbeMemory::default();
    let mut last_failure: Option<String> = None;
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        // Une demande faite pendant le relevé précédent est servie par celui-ci seulement si elle l'a précédé.
        wake.store(false, Ordering::Relaxed);
        let result = scan_once(&mut memory, scope, stop);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Une ligne au journal par panne, pas une toutes les 2 s tant qu'elle dure.
        match &result {
            Ok(_) => last_failure = None,
            Err(message) => {
                if last_failure.as_ref() != Some(message) {
                    hud::log_line(&format!("servers scan failed reason={message}"));
                }
                last_failure = Some(message.clone());
            }
        }
        if sender.send(result).is_err() {
            break;
        }
        while !stop.load(Ordering::Relaxed)
            && !wake.load(Ordering::Relaxed)
            && started.elapsed() < period
        {
            thread::sleep(SLEEP_SLICE.min(period.saturating_sub(started.elapsed())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    // Sorties réelles de `lsof`, PID et ports anonymisés.
    const LISTEN_OUTPUT: &str = "p100\nf9\nn*:7000\nf10\nn*:7000\nf11\nn*:5000\nf12\nn*:5000\np200\nf4\nn127.0.0.1:8811\nf5\nn[::1]:8811\np300\nf7\nn[::1]:7679\n";

    #[test]
    fn listen_output_gives_one_listener_per_pid_and_port() {
        let listeners = parse_lsof_listen(LISTEN_OUTPUT);
        assert_eq!(
            listeners,
            vec![
                Listener {
                    pid: 100,
                    port: 7000
                },
                Listener {
                    pid: 100,
                    port: 5000
                },
                Listener {
                    pid: 200,
                    port: 8811
                },
                Listener {
                    pid: 300,
                    port: 7679
                },
            ]
        );
    }

    #[test]
    fn listen_output_ignores_lines_it_does_not_understand() {
        let listeners = parse_lsof_listen("p10\nnnot-an-address\nf3\nn*:99999\nn*:80\n\nx\n");
        assert_eq!(listeners, vec![Listener { pid: 10, port: 80 }]);
    }

    #[test]
    fn ps_output_keeps_commands_with_spaces() {
        let output = "    1     0     1 /sbin/launchd\n  412   100   412 /Applications/Some App.app/Contents/MacOS/Some App --flag value\n 9001   412   412 node /work/app/node_modules/.bin/next dev -p 3000\n";
        let processes = parse_ps(output);
        assert_eq!(processes.len(), 3);
        assert_eq!(
            processes[&412],
            Process {
                pid: 412,
                ppid: 100,
                pgid: 412,
                command: "/Applications/Some App.app/Contents/MacOS/Some App --flag value"
                    .to_string(),
            }
        );
        assert_eq!(
            processes[&9001].command,
            "node /work/app/node_modules/.bin/next dev -p 3000"
        );
        assert_eq!(processes[&1].ppid, 0);
    }

    #[test]
    fn fd_output_gives_cwd_and_a_regular_stdout_file() {
        let output = "p100\nfcwd\ntDIR\nn/work/app\nf1\ntREG\nn/work/logs/app.log\np200\nfcwd\ntDIR\nn/work/other\nf1\ntCHR\nn/dev/null\np300\nfcwd\ntDIR\nn/work/third\nf1\ntPIPE\nn->0x45a41a3a2804e98f\np400\nfcwd\ntDIR\nn/work/tty\nf1\ntCHR\nn/dev/ttys004\n";
        let (cwd, stdout) = parse_lsof_fd(output);
        assert_eq!(cwd[&100], PathBuf::from("/work/app"));
        assert_eq!(cwd[&300], PathBuf::from("/work/third"));
        assert_eq!(
            stdout[&100],
            OutputTarget::File(PathBuf::from("/work/logs/app.log"))
        );
        assert_eq!(stdout[&200], OutputTarget::Other("/dev/null".to_string()));
        assert_eq!(stdout[&300], OutputTarget::Other("pipe".to_string()));
        assert_eq!(
            stdout[&400],
            OutputTarget::Other("/dev/ttys004".to_string())
        );
    }

    #[test]
    fn fd_output_without_stdout_still_gives_cwd() {
        let (cwd, stdout) = parse_lsof_fd("p100\nfcwd\ntDIR\nn/work/app\n");
        assert_eq!(cwd[&100], PathBuf::from("/work/app"));
        assert!(stdout.is_empty());
    }

    #[test]
    fn a_failed_lsof_is_a_scan_error_not_an_empty_scan() {
        let stop = AtomicBool::new(false);
        let timeout = Duration::from_secs(5);
        // Code non nul, rien sur stdout, un message sur stderr : une erreur.
        let failed = run_tool(
            "sh",
            &["-c", "echo 'lsof: WARNING: boom' >&2; exit 1"],
            timeout,
            &stop,
        );
        let message = failed.expect_err("a failing tool must be an error");
        assert!(message.contains("boom"), "{message}");
        // Code 1 sans message (rien n'écoute) : un relevé vide, pas une erreur.
        assert_eq!(
            run_tool("sh", &["-c", "exit 1"], timeout, &stop),
            Ok(String::new())
        );
        // Code 1 avec une sortie partielle (un PID disparu) : la sortie est gardée.
        assert_eq!(
            run_tool(
                "sh",
                &["-c", "echo p1; echo gone >&2; exit 1"],
                timeout,
                &stop
            ),
            Ok("p1\n".to_string())
        );
        // Outil absent : une erreur.
        assert!(run_tool("idema-no-such-tool", &[], timeout, &stop).is_err());
    }

    #[test]
    fn a_tool_runs_with_the_extra_environment() {
        let stop = AtomicBool::new(false);
        let output = run_tool_with_env(
            "sh",
            &["-c", "printf %s \"$IDEMA_TEST_EXTRA\""],
            &[("IDEMA_TEST_EXTRA", "invented")],
            Duration::from_secs(5),
            &stop,
        );
        assert_eq!(output, Ok("invented".to_string()));
    }

    #[test]
    fn a_slow_tool_is_abandoned_with_an_error() {
        let stop = AtomicBool::new(false);
        let started = Instant::now();
        let result = run_tool("sleep", &["10"], Duration::from_millis(200), &stop);
        assert!(result.expect_err("must time out").contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn the_http_probe_answers_for_an_http_server_and_not_for_a_raw_socket() {
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let http_port = http.local_addr().unwrap().port();
        thread::spawn(move || {
            while let Ok((mut stream, _)) = http.accept() {
                let mut request = [0u8; 64];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(b"HTTP/1.0 200 OK\r\n\r\n");
            }
        });
        assert!(probe_http(http_port, HTTP_PROBE_TIMEOUT));

        let raw = TcpListener::bind("127.0.0.1:0").unwrap();
        let raw_port = raw.local_addr().unwrap().port();
        let (release, held) = mpsc::channel::<()>();
        let holder = thread::spawn(move || {
            // Accepte, garde la connexion ouverte et se tait.
            let connection = raw.accept();
            let _ = held.recv();
            drop(connection);
        });
        let started = Instant::now();
        assert!(!probe_http(raw_port, HTTP_PROBE_TIMEOUT));
        assert!(
            started.elapsed() <= Duration::from_millis(600),
            "{:?}",
            started.elapsed()
        );
        drop(release);
        holder.join().unwrap();
    }

    /// Un serveur qui exige l'en-tête `Host` (comme Bun.serve, dont l'URL devient `"/"` sans lui et fait planter
    /// le serveur MCP local de Claude Code) : il raccroche sans répondre à une requête qui n'en porte pas.
    #[test]
    fn the_http_probe_sends_a_host_header() {
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = http.local_addr().unwrap().port();
        thread::spawn(move || {
            while let Ok((mut stream, _)) = http.accept() {
                let mut request = [0u8; 256];
                let read = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]).to_string();
                let expected = format!("\r\nHost: 127.0.0.1:{port}\r\n");
                if request.contains(&expected) {
                    let _ = stream.write_all(b"HTTP/1.0 200 OK\r\n\r\n");
                }
            }
        });
        assert!(probe_http(port, HTTP_PROBE_TIMEOUT));
    }

    #[test]
    fn the_http_probe_is_false_for_a_closed_port() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(!probe_http(port, HTTP_PROBE_TIMEOUT));
    }

    #[test]
    fn the_http_probe_answers_for_an_ipv6_only_server() {
        let Ok(http) = TcpListener::bind("[::1]:0") else {
            return; // pas d'IPv6 sur cet hôte
        };
        let port = http.local_addr().unwrap().port();
        thread::spawn(move || {
            while let Ok((mut stream, _)) = http.accept() {
                let mut request = [0u8; 64];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(b"HTTP/1.0 200 OK\r\n\r\n");
            }
        });
        assert!(probe_http(port, HTTP_PROBE_TIMEOUT));
    }

    /// Un serveur muet tant que le test n'a pas levé le drapeau, puis qui répond.
    fn late_server() -> (u16, Arc<AtomicBool>) {
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = http.local_addr().unwrap().port();
        let ready = Arc::new(AtomicBool::new(false));
        let server_ready = Arc::clone(&ready);
        thread::spawn(move || {
            let mut silent = Vec::new();
            while let Ok((mut stream, _)) = http.accept() {
                let mut request = [0u8; 64];
                let _ = stream.read(&mut request);
                if server_ready.load(Ordering::Relaxed) {
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
                } else {
                    silent.push(stream); // garde la connexion ouverte et se tait
                }
            }
        });
        (port, ready)
    }

    #[test]
    fn the_probe_memory_probes_only_new_pairs_and_keeps_positive_answers() {
        let (port, ready) = late_server();
        let mut memory = ProbeMemory::default();
        let (now, stop) = (Instant::now(), AtomicBool::new(false));
        let here = [Listener { pid: 4242, port }];
        ready.store(true, Ordering::Relaxed);
        assert!(memory.update(&here, now, &stop).contains(&(4242, port)));
        // Le serveur se tait : si le couple était resondé, la réponse positive serait perdue.
        ready.store(false, Ordering::Relaxed);
        let later = now + PROBE_RETRY_AFTER * 2;
        assert!(memory.update(&here, later, &stop).contains(&(4242, port)));
        // Un autre PID sur le même port est un nouveau couple : il est sondé (et muet), l'ancien est oublié.
        let replaced = [Listener { pid: 4343, port }];
        assert!(memory.update(&replaced, later, &stop).is_empty());
        assert_eq!(memory.answers.len(), 1);
        assert!(memory.answers.contains_key(&(4343, port)));
        // Le couple disparu est oublié.
        assert!(memory.update(&[], later, &stop).is_empty());
        assert!(memory.answers.is_empty());
    }

    #[test]
    fn a_silent_pair_is_probed_again_later_and_becomes_http() {
        let (port, ready) = late_server();
        let mut memory = ProbeMemory::default();
        let stop = AtomicBool::new(false);
        let start = Instant::now();
        let listeners = [Listener { pid: 77, port }];
        assert!(memory.update(&listeners, start, &stop).is_empty());
        ready.store(true, Ordering::Relaxed);
        // Trop tôt : pas de nouvelle sonde, donc le serveur prêt n'est pas encore vu.
        let soon = start + Duration::from_secs(2);
        assert!(memory.update(&listeners, soon, &stop).is_empty());
        // Assez tard : le couple est resondé et répond.
        let later = start + PROBE_RETRY_AFTER;
        assert!(
            memory
                .update(&listeners, later, &stop)
                .contains(&(77, port))
        );
    }

    #[test]
    fn a_silent_pair_is_given_up_after_the_attempt_limit() {
        // Une socket qui n'accepte jamais : toujours muette, donc resondée jusqu'à la limite seulement.
        // Elle reste liée pour que le port ne soit pas repris par un autre test.
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = silent.local_addr().unwrap().port();
        let mut memory = ProbeMemory::default();
        let stop = AtomicBool::new(false);
        let start = Instant::now();
        let listeners = [Listener { pid: 78, port }];
        for round in 0..(PROBE_MAX_ATTEMPTS + 3) {
            memory.update(&listeners, start + PROBE_RETRY_AFTER * round, &stop);
        }
        assert_eq!(memory.answers[&(78, port)].attempts, PROBE_MAX_ATTEMPTS);
    }

    #[test]
    fn a_raised_stop_skips_the_remaining_probes() {
        let mut memory = ProbeMemory::default();
        let stop = AtomicBool::new(true);
        let listeners: Vec<Listener> = (0..40)
            .map(|index| Listener {
                pid: 100 + index,
                port: 1,
            })
            .collect();
        let started = Instant::now();
        assert!(memory.update(&listeners, started, &stop).is_empty());
        assert!(memory.answers.is_empty());
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn a_snapshot_is_dated_before_its_tools_run() {
        let before = Instant::now();
        let mut memory = ProbeMemory::default();
        let scope = Mutex::new(ProbeScope::default());
        let Ok(snapshot) = scan_once(&mut memory, &scope, &AtomicBool::new(false)) else {
            return;
        };
        // `lsof` et `ps` prennent des centaines de millisecondes : la date est celle du départ, pas de la fin.
        assert!(snapshot.taken_at >= before);
        assert!(snapshot.taken_at.duration_since(before) < Duration::from_millis(50));
    }

    /// Un serveur HTTP qui compte les connexions reçues.
    fn counting_server() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = http.local_addr().unwrap().port();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&hits);
        thread::spawn(move || {
            while let Ok((mut stream, _)) = http.accept() {
                counted.fetch_add(1, Ordering::Relaxed);
                let mut request = [0u8; 64];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(b"HTTP/1.0 200 OK\r\n\r\n");
            }
        });
        (port, hits)
    }

    #[test]
    fn the_probe_reaches_only_the_listeners_in_scope() {
        let (port, hits) = counting_server();
        let me = Listener {
            pid: std::process::id() as i32,
            port,
        };
        let mut memory = ProbeMemory::default();
        let stop = AtomicBool::new(false);
        // Hors du périmètre (périmètre vide) : `lsof` le voit, mais ce relevé ne le sonde pas. Le constat se fait côté
        // sondeur : un IdemA qui tourne sur le Mac peut, lui, sonder le serveur du test (il descend de son pane).
        let outside = Mutex::new(ProbeScope::default());
        let Ok(snapshot) = scan_once(&mut memory, &outside, &stop) else {
            return;
        };
        assert!(
            snapshot.listeners.contains(&me),
            "lsof sees the test listener"
        );
        assert!(!snapshot.http.contains(&(me.pid, port)));
        assert!(
            !memory.answers.contains_key(&(me.pid, port)),
            "a listener out of scope was probed"
        );
        // Son port déclaré par une entrée : il est sondé, et répond.
        let declared = Mutex::new(ProbeScope {
            declared_ports: HashSet::from([port]),
            ..ProbeScope::default()
        });
        let Ok(snapshot) = scan_once(&mut memory, &declared, &stop) else {
            return;
        };
        assert!(snapshot.http.contains(&(me.pid, port)));
        assert!(hits.load(Ordering::Relaxed) >= 1, "the server was reached");
    }

    fn processes(chain: &[(i32, i32)]) -> HashMap<i32, Process> {
        chain
            .iter()
            .map(|&(pid, ppid)| {
                (
                    pid,
                    Process {
                        pid,
                        ppid,
                        pgid: pid,
                        command: format!("p{pid}"),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn the_scope_admits_a_descendant_of_a_live_pane() {
        // 30 écoute, fils de 20, fils du pane 10, lui-même fils de launchd.
        let tree = processes(&[(30, 20), (20, 10), (10, 1)]);
        let scope = ProbeScope {
            pane_pids: HashSet::from([10]),
            ..ProbeScope::default()
        };
        let listener = |pid| Listener { pid, port: 5173 };
        assert!(scope.admits(listener(30), &tree, &HashMap::new()));
        // La racine du pane elle-même (une commande lancée par `exec`).
        assert!(scope.admits(listener(10), &tree, &HashMap::new()));
        // Un autre arbre, rattaché à launchd : rien.
        let other = processes(&[(40, 1)]);
        assert!(!scope.admits(listener(40), &other, &HashMap::new()));
        // Un processus que `ps` n'a pas vu : rien.
        assert!(!scope.admits(listener(50), &tree, &HashMap::new()));
    }

    #[test]
    fn the_scope_admits_a_process_running_in_a_known_project_only() {
        let scope = ProbeScope {
            projects: vec![PathBuf::from("/work/alpha")],
            ..ProbeScope::default()
        };
        let tree = processes(&[(60, 1)]);
        let listener = Listener {
            pid: 60,
            port: 3000,
        };
        let at = |dir: &str| HashMap::from([(60, PathBuf::from(dir))]);
        assert!(scope.admits(listener, &tree, &at("/work/alpha")));
        assert!(scope.admits(listener, &tree, &at("/work/alpha/web")));
        // Un préfixe de texte n'est pas un dossier parent.
        assert!(!scope.admits(listener, &tree, &at("/work/alpha-2")));
        assert!(!scope.admits(listener, &tree, &at("/Users/someone")));
        assert!(!scope.admits(listener, &tree, &HashMap::new()));
    }

    #[test]
    fn a_launchd_service_in_a_project_is_not_one_of_its_servers() {
        // 80 est un job de launchd (LaunchAgent), 81 son fils ; 90 l'orphelin d'un `cmd &`, rattaché à launchd sans en
        // être un job.
        let scope = ProbeScope {
            projects: vec![PathBuf::from("/work/hq")],
            launchd_jobs: HashSet::from([80]),
            ..ProbeScope::default()
        };
        let tree = processes(&[(80, 1), (81, 80), (90, 1)]);
        let cwd = HashMap::from([
            (80, PathBuf::from("/work/hq/dashboard")),
            (81, PathBuf::from("/work/hq/dashboard")),
            (90, PathBuf::from("/work/hq/web")),
        ]);
        let listener = |pid| Listener { pid, port: 7900 };
        assert!(!scope.admits(listener(80), &tree, &cwd));
        assert!(!scope.admits(listener(81), &tree, &cwd));
        assert!(scope.admits(listener(90), &tree, &cwd));
        // Le port déclaré par une entrée du projet reste admis, qui que ce soit qui l'écoute.
        let declared = ProbeScope {
            declared_ports: HashSet::from([7900]),
            ..scope
        };
        assert!(declared.admits(listener(80), &tree, &cwd));
    }

    #[test]
    fn a_server_started_from_an_app_terminal_keeps_its_project() {
        // Zed (62912, une app de `launchctl list`) → login → zsh → node : la lecture de `launchctl` n'en fait pas un
        // service, le serveur reste celui du projet de son dossier.
        let output = "PID\tStatus\tLabel\n62912\t0\tapplication.dev.zed.Zed.341877034.453183453\n";
        let scope = ProbeScope {
            projects: vec![PathBuf::from("/work/hq")],
            launchd_jobs: parse_launchctl_list(output),
            ..ProbeScope::default()
        };
        let tree = processes(&[(62912, 1), (100, 62912), (101, 100), (102, 101)]);
        let cwd = HashMap::from([(102, PathBuf::from("/work/hq/web"))]);
        assert!(scope.admits(
            Listener {
                pid: 102,
                port: 5173
            },
            &tree,
            &cwd
        ));
    }

    #[test]
    fn launchctl_list_gives_the_pids_of_running_jobs() {
        let output = "PID\tStatus\tLabel\n949\t0\tcom.example.server\n-\t0\tcom.apple.stopped\n14516\t-15\tcom.example.worker\n62912\t0\tapplication.dev.zed.Zed.341877034.453183453\n";
        assert_eq!(parse_launchctl_list(output), HashSet::from([949, 14516]));
    }

    #[test]
    fn the_scope_admits_a_declared_port_whoever_listens() {
        let scope = ProbeScope {
            declared_ports: HashSet::from([8811]),
            ..ProbeScope::default()
        };
        let tree = processes(&[(70, 1)]);
        assert!(scope.admits(
            Listener {
                pid: 70,
                port: 8811
            },
            &tree,
            &HashMap::new()
        ));
        assert!(!scope.admits(
            Listener {
                pid: 70,
                port: 8812
            },
            &tree,
            &HashMap::new()
        ));
    }

    #[test]
    fn scan_now_does_not_wait_for_the_period() {
        let (sender, receiver) = mpsc::channel();
        let scanner = Scanner::spawn(Duration::from_secs(120), sender);
        let first = receiver.recv_timeout(Duration::from_secs(10));
        assert!(first.is_ok(), "a first scan result");
        scanner.scan_now();
        let second = receiver.recv_timeout(Duration::from_secs(20));
        assert!(
            second.is_ok(),
            "a second scan result long before the period ends"
        );
    }

    #[test]
    fn the_scanner_sends_results_and_stops_quickly_on_drop() {
        let (sender, receiver) = mpsc::channel();
        let scanner = Scanner::spawn(Duration::from_millis(50), sender);
        let first = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("a first scan result");
        if let Ok(snapshot) = &first {
            assert!(
                snapshot
                    .processes
                    .values()
                    .any(|process| process.pid == std::process::id() as i32)
            );
        }
        let started = Instant::now();
        drop(scanner);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        while receiver.try_recv().is_ok() {}
        assert!(
            receiver.recv_timeout(Duration::from_millis(300)).is_err(),
            "no more results after drop"
        );
    }
}

#[cfg(test)]
mod measure {
    use super::*;

    /// Mesure d'un relevé complet de l'hôte (attendu : moins de 300 ms une fois les sondes HTTP faites), dans le pire cas :
    /// tout ce qui écoute est dans le périmètre de la sonde.
    #[test]
    #[ignore = "mesure sur l'hôte, à lancer à la main"]
    fn full_scan_duration() {
        let stop = AtomicBool::new(false);
        let mut memory = ProbeMemory::default();
        let listing = scan_once(&mut memory, &Mutex::new(ProbeScope::default()), &stop)
            .expect("listing scan");
        let scope = Mutex::new(ProbeScope {
            declared_ports: listing
                .listeners
                .iter()
                .map(|listener| listener.port)
                .collect(),
            ..ProbeScope::default()
        });
        let first_started = Instant::now();
        let first = scan_once(&mut memory, &scope, &stop).expect("first scan");
        let first_elapsed = first_started.elapsed();
        let started = Instant::now();
        let second = scan_once(&mut memory, &scope, &stop).expect("second scan");
        let elapsed = started.elapsed();
        println!(
            "scan listeners={} processes={} first_scan_ms={} steady_scan_ms={} http={}",
            second.listeners.len(),
            second.processes.len(),
            first_elapsed.as_millis(),
            elapsed.as_millis(),
            first.http.len()
        );
    }
}
