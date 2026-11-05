//! Ce que les tests du compagnon se partagent : un générateur à graine, des dossiers jetables, et un client HTTP brut.

use rand::RngCore;

/// Un générateur déterministe (xorshift64*) : les tests de codes et de jetons rejouent la même suite d'une graine.
pub struct SeededRng(u64);

impl SeededRng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
}

impl RngCore for SeededRng {
    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use idema_tmux::server::TmuxServer;
use tokio::sync::{mpsc, watch};

use super::fleet::Fleet;
use super::service::{self, ServiceDeps, ServiceHandle};
use super::timeline::Timeline;
use super::{ListenState, PaneCommand, ServiceEvent, ServiceStatus};
use crate::config::CompanionConfig;

/// Un dossier jetable, retiré en fin de test.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "idema-companion-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Un port libre, hors de la plage éphémère du système (dès 49152) : les connexions sortantes des autres tests
/// du processus y prendraient sinon leur port local. Le sondage est une connexion refusée : aucune prise n'est
/// créée sur le port, donc aucun process lancé par un autre test ne peut en hériter.
pub fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    loop {
        let offset = NEXT.fetch_add(1, Ordering::Relaxed) % 5000;
        let port = 30000 + (std::process::id() % 5000) as u16 + offset;
        if TcpStream::connect(("127.0.0.1", port)).is_err() {
            return port;
        }
    }
}

/// Les dépendances d'un service de test sur `dir`, et de quoi le piloter.
pub fn deps_in(
    port: u16,
    dir: PathBuf,
    previous: Option<JoinHandle<()>>,
    test_routes: bool,
) -> (
    ServiceDeps,
    mpsc::UnboundedSender<ServiceEvent>,
    watch::Receiver<ServiceStatus>,
    watch::Sender<Arc<Fleet>>,
) {
    let (events_sender, events) = mpsc::unbounded_channel();
    let (commands, _commands_receiver) = futures::channel::mpsc::unbounded();
    let (status, status_receiver) = watch::channel(ServiceStatus::default());
    let (fleet_sender, fleet) = watch::channel(Arc::new(Fleet::default()));
    let config: CompanionConfig = toml::from_str(&format!("port = {port}")).unwrap();
    let tmux = Arc::new(TmuxServer::new(
        "/nonexistent/tmux".into(),
        "idema-test-companion",
        "/nonexistent/tmux.conf".into(),
        Vec::new(),
    ));
    let deps = ServiceDeps {
        dir: dir.clone(),
        version: "test",
        config,
        tmux,
        scratchpad: dir.join("scratchpad.md"),
        events,
        commands,
        status,
        fleet,
        timeline: Arc::new(Mutex::new(Timeline::new(std::time::SystemTime::now()))),
        previous,
        test_routes,
        push_override: None,
    };
    (deps, events_sender, status_receiver, fleet_sender)
}

/// Comme `deps_in`, sur un dossier neuf que l'appelant garde le temps du test.
pub fn deps(
    port: u16,
    previous: Option<JoinHandle<()>>,
) -> (ServiceDeps, watch::Receiver<ServiceStatus>, TempDir) {
    let home = TempDir::new("deps");
    let (deps, _events, status, _fleet) = deps_in(port, home.0.clone(), previous, false);
    (deps, status, home)
}

/// Attend que l'état de la prise vérifie `done`.
pub fn wait_for(
    status: &mut watch::Receiver<ServiceStatus>,
    done: impl Fn(&ListenState) -> bool,
) -> ListenState {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let current = status.borrow().listen.clone();
        if done(&current) {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "service state stuck at {current:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Un vrai service sur un vrai port, arrêté et joint à la chute.
pub struct Harness {
    pub port: u16,
    pub home: TempDir,
    pub events: mpsc::UnboundedSender<ServiceEvent>,
    pub status: watch::Receiver<ServiceStatus>,
    /// De quoi publier la flotte comme le fait l'espace de travail.
    pub fleet: watch::Sender<Arc<Fleet>>,
    /// La chronologie du service, que l'espace de travail alimente en vrai.
    pub timeline: Arc<Mutex<Timeline>>,
    /// Les commandes sur un pane que le service envoie à l'espace de travail : un test joue le fil d'interface en la
    /// lisant (`take`) et en répondant.
    pub commands: Option<futures::channel::mpsc::UnboundedReceiver<PaneCommand>>,
    handle: Option<ServiceHandle>,
}

impl Harness {
    pub fn start(test_routes: bool) -> Self {
        Self::start_in(TempDir::new("harness"), test_routes)
    }

    /// Démarre sur ce dossier (un nouveau service sur les appareils d'un service arrêté).
    pub fn start_in(home: TempDir, test_routes: bool) -> Self {
        Self::start_with(home, test_routes, None)
    }

    /// Démarre avec un serveur tmux de remplacement (`fake_tmux`) : l'écran d'un pane se capture alors sans tmux.
    pub fn start_with(home: TempDir, test_routes: bool, tmux: Option<Arc<TmuxServer>>) -> Self {
        Self::start_full(home, test_routes, tmux, None)
    }

    /// Démarre avec les routes de test et un faux service push à la place de chaque point d'accès.
    pub fn start_with_push(home: TempDir, push_override: String) -> Self {
        Self::start_full(home, true, None, Some(push_override))
    }

    fn start_full(
        home: TempDir,
        test_routes: bool,
        tmux: Option<Arc<TmuxServer>>,
        push_override: Option<String>,
    ) -> Self {
        let port = free_port();
        let (mut deps, events, mut status, fleet) =
            deps_in(port, home.0.clone(), None, test_routes);
        if let Some(tmux) = tmux {
            deps.tmux = tmux;
        }
        deps.push_override = push_override;
        let (commands, commands_receiver) = futures::channel::mpsc::unbounded();
        deps.commands = commands;
        let timeline = Arc::clone(&deps.timeline);
        let handle = service::start(deps).expect("the service thread spawns");
        wait_for(&mut status, |state| {
            *state == ListenState::Listening { port }
        });
        Self {
            port,
            home,
            events,
            status,
            fleet,
            timeline,
            commands: Some(commands_receiver),
            handle: Some(handle),
        }
    }

    /// Arrête le service, attend la fin de son fil, et rend le dossier pour en démarrer un autre dessus.
    pub fn stop(mut self) -> TempDir {
        self.shutdown();
        TempDir(std::mem::take(&mut self.home.0))
    }

    fn shutdown(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.stop().join().unwrap();
        }
    }

    pub fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Reply {
        request(self.port, method, path, headers, body)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Une réponse HTTP lue en entier.
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|error| panic!("{error}: {}", self.text()))
    }
}

/// Une requête HTTP/1.1 brute (`Connection: close`), réponse lue jusqu'à la fin.
pub fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Reply {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    match body {
        Some(body) => head.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len())),
        None => head.push_str("\r\n"),
    }
    stream.write_all(head.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no header end in {:?}", String::from_utf8_lossy(&raw)));
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Reply {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

impl Harness {
    /// Attend que l'état publié par le service vérifie `done`, et le rend.
    pub fn wait(&self, done: impl Fn(&ServiceStatus) -> bool) -> ServiceStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let current = self.status.borrow().clone();
            if done(&current) {
                return current;
            }
            assert!(
                Instant::now() < deadline,
                "service status stuck at {current:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Appaire un appareil par la route de test (service démarré avec `test_routes`) : le cookie à présenter.
pub fn pair_device(port: u16, user_agent: &str) -> String {
    let json = ("Content-Type", "application/json");
    let code = request(
        port,
        "POST",
        "/idema-api/test/pairing-code",
        &[json],
        Some("{}"),
    );
    assert_eq!(code.status, 200, "{}", code.text());
    let code = code.json()["code"].as_str().unwrap().to_owned();
    let body = serde_json::json!({ "code": code }).to_string();
    let reply = request(
        port,
        "POST",
        "/idema-api/pair",
        &[("User-Agent", user_agent), json],
        Some(&body),
    );
    assert_eq!(reply.status, 200, "{}", reply.text());
    let cookie = reply.header("set-cookie").expect("a cookie is set");
    cookie.split(';').next().unwrap().to_owned()
}

impl Harness {
    pub fn pair(&self, user_agent: &str) -> String {
        pair_device(self.port, user_agent)
    }

    /// Appaire un appareil sans route de test, par l'événement de Personnaliser : le cookie à présenter.
    pub fn pair_with_events(&self, user_agent: &str) -> String {
        self.events.send(ServiceEvent::StartPairing).unwrap();
        let code = self
            .wait(|status| status.pairing.is_some())
            .pairing
            .unwrap()
            .code;
        let body = serde_json::json!({ "code": code }).to_string();
        let reply = self.request(
            "POST",
            "/idema-api/pair",
            &[
                ("User-Agent", user_agent),
                ("Content-Type", "application/json"),
            ],
            Some(&body),
        );
        assert_eq!(reply.status, 200, "{}", reply.text());
        let cookie = reply.header("set-cookie").expect("a cookie is set");
        cookie.split(';').next().unwrap().to_owned()
    }
}

/// Un flux SSE ouvert, lu image par image. Le corps arrive en `Transfer-Encoding: chunked` : les morceaux sont
/// recollés avant de couper les images (séparées par une ligne vide).
pub struct EventStream {
    stream: TcpStream,
    /// Les octets reçus, pas encore décodés en morceaux.
    raw: Vec<u8>,
    /// Le corps décodé, pas encore rendu.
    text: String,
    pub head: String,
    ended: bool,
}

/// Ouvre `GET path` et lit l'en-tête de la réponse.
pub fn open_stream(port: u16, path: &str, headers: &[(&str, &str)]) -> EventStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    EventStream {
        stream,
        raw: Vec::new(),
        text: String::new(),
        head: String::from_utf8(head).unwrap(),
        ended: false,
    }
}

impl EventStream {
    /// Décode les morceaux complets de `raw` vers `text`.
    fn decode(&mut self) {
        loop {
            let Some(line_end) = self.raw.windows(2).position(|pair| pair == b"\r\n") else {
                return;
            };
            let size =
                usize::from_str_radix(String::from_utf8_lossy(&self.raw[..line_end]).trim(), 16)
                    .expect("a chunk size");
            if size == 0 {
                self.ended = true;
                return;
            }
            let end = line_end + 2 + size;
            if self.raw.len() < end + 2 {
                return;
            }
            self.text
                .push_str(&String::from_utf8_lossy(&self.raw[line_end + 2..end]));
            self.raw.drain(..end + 2);
        }
    }

    /// La prochaine image (sans sa ligne vide finale), ou `None` si rien n'arrive en `wait` ou si le flux est fini.
    pub fn next_frame(&mut self, wait: Duration) -> Option<String> {
        let deadline = Instant::now() + wait;
        loop {
            self.decode();
            if let Some(at) = self.text.find("\n\n") {
                let frame = self.text[..at].to_owned();
                self.text.drain(..at + 2);
                return Some(frame);
            }
            if self.ended {
                return None;
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            self.stream
                .set_read_timeout(Some(left.max(Duration::from_millis(1))))
                .unwrap();
            let mut buffer = [0u8; 4096];
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    self.ended = true;
                }
                Ok(read) => self.raw.extend_from_slice(&buffer[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return None;
                }
                Err(error) => panic!("stream read failed: {error}"),
            }
        }
    }
}

/// Un faux `tmux` : un script qui note ses arguments dans `log`, rend « 80 24 » à `display-message` et le contenu de
/// `capture` à `capture-pane`. Rend le serveur et le fichier du journal des appels.
pub fn fake_tmux(dir: &std::path::Path, capture: &[u8]) -> (Arc<TmuxServer>, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("tmux-calls.log");
    let capture_file = dir.join("capture.bin");
    std::fs::write(&capture_file, capture).unwrap();
    let script = dir.join("tmux");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$*\" in\n*display-message*) echo '80 24' ;;\n*capture-pane*) /bin/cat '{}' ;;\n*) exit 1 ;;\nesac\n",
            log.display(),
            capture_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server = TmuxServer::new(script, "idema-test-fake", dir.join("tmux.conf"), Vec::new());
    (Arc::new(server), log)
}

/// Joue le fil d'interface : chaque commande reçue passe par `handle`, dans un fil à part qui finit avec le service.
pub fn serve_commands(harness: &mut Harness, mut handle: impl FnMut(PaneCommand) + Send + 'static) {
    use futures::StreamExt;
    let mut commands = harness.commands.take().expect("commands not taken yet");
    std::thread::spawn(move || {
        while let Some(command) = futures::executor::block_on(commands.next()) {
            handle(command);
        }
    });
}

/// Une requête reçue par le faux service push.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    /// Chemin et requête, tels que curl les a envoyés.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Les fichiers (nom, droits) du dossier surveillé au moment où la requête est arrivée.
    pub watched: Vec<(String, u32)>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Un faux service push sur `127.0.0.1` : il note chaque requête, et répond ce qu'on lui a dit (`None` : ne répond
/// jamais, la connexion reste ouverte jusqu'à sa chute).
pub struct FakePush {
    pub port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakePush {
    pub fn start(status: u16, body: &str) -> Self {
        Self::start_watching(Some((status, body.to_owned())), None)
    }

    /// `reply` : le code et le corps à rendre, ou `None` pour se taire. `watched` : un dossier dont on note le contenu
    /// à chaque requête.
    pub fn start_watching(reply: Option<(u16, String)>, watched: Option<PathBuf>) -> Self {
        Self::start_inner(reply, watched, Duration::ZERO)
    }

    /// Répond 201 après `delay` : un envoi qui dure, pour jouer deux appels qui se chevauchent.
    pub fn start_slow(delay: Duration) -> Self {
        Self::start_inner(Some((201, String::new())), None, delay)
    }

    fn start_inner(
        reply: Option<(u16, String)>,
        watched: Option<PathBuf>,
        delay: Duration,
    ) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let reply = Arc::new(Mutex::new(reply));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (requests, reply, stop) =
                (Arc::clone(&requests), Arc::clone(&reply), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (requests, reply, stop, watched) = (
                                Arc::clone(&requests),
                                Arc::clone(&reply),
                                Arc::clone(&stop),
                                watched.clone(),
                            );
                            std::thread::spawn(move || {
                                serve_one(
                                    stream,
                                    &requests,
                                    &reply,
                                    &stop,
                                    watched.as_deref(),
                                    delay,
                                );
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            port,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    /// Attend d'avoir reçu `count` requêtes au moins, et les rend.
    pub fn wait_for(&self, count: usize) -> Vec<Recorded> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let seen = self.requests();
            if seen.len() >= count {
                return seen;
            }
            assert!(
                Instant::now() < deadline,
                "only {} request(s) reached the fake push service",
                seen.len()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for FakePush {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Lit une requête entière (en-têtes puis `Content-Length` octets), la note, puis répond.
fn serve_one(
    mut stream: TcpStream,
    requests: &Mutex<Vec<Recorded>>,
    reply: &Mutex<Option<(u16, String)>>,
    stop: &std::sync::atomic::AtomicBool,
    watched: Option<&std::path::Path>,
    delay: Duration,
) {
    use std::os::unix::fs::PermissionsExt;
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break at;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default().to_owned();
    let path = first.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = raw[head_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
        }
    }
    let listing = watched
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let mode = entry.metadata().ok()?.permissions().mode() & 0o777;
                    Some((entry.file_name().to_string_lossy().into_owned(), mode))
                })
                .collect()
        })
        .unwrap_or_default();
    // Un service push ne reçoit que des POST : le reste est le sondage d'un autre programme de la machine, qui visite
    // chaque port qui s'ouvre (`HEAD /` observé le 05/11) et ne doit pas fausser les comptes des tests.
    if method == "POST" {
        requests.lock().unwrap().push(Recorded {
            method,
            path,
            headers,
            body,
            watched: listing,
        });
    }
    std::thread::sleep(delay);
    let answer = reply.lock().unwrap().clone();
    match answer {
        Some((status, text)) => {
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
        // Le silence : la connexion reste ouverte tant que le faux service vit.
        None => {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}
