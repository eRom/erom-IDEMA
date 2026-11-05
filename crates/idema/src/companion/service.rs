//! Le fil du service : un runtime tokio à lui (courant, sans pool), une prise sur `127.0.0.1` seulement, et un arrêt
//! qui rend toujours le port en 1,5 s au plus, même avec des flux ouverts.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use futures::future::{Either, select};
use idema_tmux::server::TmuxServer;
use tokio::sync::{mpsc::UnboundedReceiver, watch};

use super::server::{AppState, router, stopped};
use super::{ListenState, PaneCommand, ServiceEvent, ServiceStatus};
use crate::config::CompanionConfig;
use crate::hud;

/// Le temps que le serveur a pour finir ses échanges en cours une fois le signal d'arrêt posé. Au-delà, on l'abandonne.
const SERVER_STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// Après un redémarrage, la prise de l'ancien service peut rester tenue un instant par un process fils lancé au même
/// moment (il hérite du descripteur jusqu'à son exec) : on réessaie pendant ce délai avant de dire « port pris ».
const RESTART_BIND_RETRY: Duration = Duration::from_secs(1);

/// Le temps que le runtime a pour rendre ses tâches de fond (appels bloquants) avant d'être abandonné.
const RUNTIME_STOP_TIMEOUT: Duration = Duration::from_millis(200);

/// Tout ce que le fil du service reçoit de l'espace de travail. Chaque tâche suivante ajoute le champ dont elle a besoin.
// Les champs ci-dessous sont lus par les tâches suivantes (appairage, flotte, saisie, scratchpad) : ils sont posés
// ici une fois pour toutes pour que le contrôleur n'ait plus à changer de forme.
#[allow(dead_code, reason = "lus par les tâches 2 à 11 du compagnon")]
pub struct ServiceDeps {
    pub dir: PathBuf,
    pub version: &'static str,
    pub config: CompanionConfig,
    pub tmux: Arc<TmuxServer>,
    pub scratchpad: PathBuf,
    pub events: UnboundedReceiver<ServiceEvent>,
    pub commands: UnboundedSender<PaneCommand>,
    pub status: watch::Sender<ServiceStatus>,
    /// Le fil du service précédent, quand celui-ci le remplace : le nouveau le joint (sur son propre fil, jamais celui
    /// de l'interface) avant de lier la prise, pour ne pas la trouver encore occupée.
    pub previous: Option<JoinHandle<()>>,
}

/// La poignée que garde le contrôleur : de quoi demander l'arrêt, puis joindre le fil.
pub struct ServiceHandle {
    stop: watch::Sender<bool>,
    thread: JoinHandle<()>,
}

/// Lance le fil du service. Il lie la prise, puis sert jusqu'au signal d'arrêt.
pub fn start(deps: ServiceDeps) -> ServiceHandle {
    let (stop, stop_receiver) = watch::channel(false);
    let thread = std::thread::Builder::new()
        .name("companion".into())
        .spawn(move || run(deps, stop_receiver))
        .expect("the companion thread spawns");
    ServiceHandle { stop, thread }
}

impl ServiceHandle {
    /// Envoie le signal et rend aussitôt le fil à joindre : le fil d'interface ne bloque jamais. Le fil se termine en
    /// 1,5 s au plus, port libéré ; le contrôleur le passe au service suivant, qui le joint avant de lier la prise.
    pub fn stop(self) -> JoinHandle<()> {
        // Aucun récepteur ne reste si le fil a déjà fini (prise refusée) : le signal n'a alors rien à faire.
        self.stop.send_replace(true);
        self.thread
    }
}

/// Le corps du fil.
fn run(deps: ServiceDeps, stop: watch::Receiver<bool>) {
    let ServiceDeps {
        version,
        config,
        status,
        previous,
        ..
    } = deps;
    let restarted = previous.is_some();
    if let Some(previous) = previous {
        let _ = previous.join();
    }
    // Arrêté pendant l'attente (retrait de la section aussitôt après son ajout) : rien à lier, rien à dire.
    if *stop.borrow() {
        return;
    }
    let port = config.port;
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            fail(&status, port, format!("runtime_failed: {error}"));
            return;
        }
    };
    let bind_retry = if restarted {
        RESTART_BIND_RETRY
    } else {
        Duration::ZERO
    };
    let listened = runtime.block_on(serve(port, version, &status, stop, bind_retry));
    // Le runtime est abandonné ici : ses tâches restantes (flux en retard, appel bloquant) ne retiennent plus le fil.
    runtime.shutdown_timeout(RUNTIME_STOP_TIMEOUT);
    if listened {
        hud::log_event("companion stopped");
    }
}

/// Lie la prise et sert jusqu'au signal d'arrêt. Vrai quand le service a écouté.
async fn serve(
    port: u16,
    version: &'static str,
    status: &watch::Sender<ServiceStatus>,
    stop: watch::Receiver<bool>,
    bind_retry: Duration,
) -> bool {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = match bind(address, bind_retry).await {
        Ok(listener) => listener,
        Err(error) => {
            fail(status, port, bind_reason(&error));
            return false;
        }
    };
    status.send_replace(ServiceStatus {
        listen: ListenState::Listening { port },
    });
    hud::log_event(&format!("companion listening addr={address}"));
    let state = AppState {
        version,
        stop: stop.clone(),
    };
    let server = axum::serve(listener, router(state))
        .with_graceful_shutdown(stopped(stop.clone()))
        .into_future();
    let mut server = tokio::spawn(server);
    // Le signal d'arrêt, ou la fin du serveur de lui-même (ce qu'axum ne fait qu'en cas d'erreur grave).
    let waiting = Box::pin(stopped(stop));
    match select(waiting, &mut server).await {
        Either::Left(_) => {
            // Le signal a aussi fermé les flux : le serveur finit ses échanges, mais pas au-delà d'une seconde.
            if tokio::time::timeout(SERVER_STOP_TIMEOUT, &mut server)
                .await
                .is_err()
            {
                server.abort();
                hud::log_event("companion stop timed out");
            }
        }
        Either::Right((outcome, _)) => {
            hud::log_event(&format!("companion server ended unexpectedly: {outcome:?}"));
        }
    }
    true
}

/// Lie la prise ; un port pris est réessayé pendant `retry_for` avant d'être rendu comme une erreur.
async fn bind(address: SocketAddr, retry_for: Duration) -> io::Result<tokio::net::TcpListener> {
    let deadline = tokio::time::Instant::now() + retry_for;
    loop {
        match tokio::net::TcpListener::bind(address).await {
            Err(error)
                if error.kind() == io::ErrorKind::AddrInUse
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            outcome => return outcome,
        }
    }
}

/// La prise n'a pas pu être liée, ou le runtime n'a pas pu naître : le service ne tourne pas, et le dit.
fn fail(status: &watch::Sender<ServiceStatus>, port: u16, reason: String) {
    hud::log_event(&format!("companion disabled reason={reason} port={port}"));
    status.send_replace(ServiceStatus {
        listen: ListenState::Failed { port, reason },
    });
}

/// La raison courte d'un bind refusé : `port_in_use` est celle que Personnaliser sait dire en français.
fn bind_reason(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::AddrInUse => "port_in_use".into(),
        _ => format!("bind_failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Instant;

    /// Un port libre, hors de la plage éphémère du système (dès 49152) : les connexions sortantes des autres tests
    /// du processus y prendraient sinon leur port local. Le sondage est une connexion refusée : aucune prise n'est
    /// créée sur le port, donc aucun process lancé par un autre test ne peut en hériter.
    fn free_port() -> u16 {
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

    fn deps(
        port: u16,
        previous: Option<JoinHandle<()>>,
    ) -> (ServiceDeps, watch::Receiver<ServiceStatus>) {
        let (_events_sender, events) = tokio::sync::mpsc::unbounded_channel();
        let (commands, _commands_receiver) = futures::channel::mpsc::unbounded();
        let (status, status_receiver) = watch::channel(ServiceStatus {
            listen: ListenState::Starting,
        });
        let config: CompanionConfig = toml::from_str(&format!("port = {port}")).unwrap();
        let tmux = Arc::new(TmuxServer::new(
            "/nonexistent/tmux".into(),
            "idema-test-companion",
            "/nonexistent/tmux.conf".into(),
            Vec::new(),
        ));
        let deps = ServiceDeps {
            dir: std::env::temp_dir(),
            version: "test",
            config,
            tmux,
            scratchpad: std::env::temp_dir().join("scratchpad.md"),
            events,
            commands,
            status,
            previous,
        };
        (deps, status_receiver)
    }

    /// Attend que l'état de la prise vérifie `done`.
    fn wait_for(
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

    /// Une requête GET en HTTP/1.1, lue jusqu'à la fin de l'en-tête.
    fn get(port: u16, path: &str) -> (TcpStream, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        (stream, String::from_utf8(head).unwrap())
    }

    /// Lie le port une fois, comme le fera un nouveau service : `SO_REUSEADDR` posé (les échanges clos du service d'avant
    /// restent en TIME_WAIT), donc seule une prise encore à l'écoute le fait échouer.
    fn bind_once(port: u16) -> io::Result<()> {
        let socket = tokio::net::TcpSocket::new_v4()?;
        socket.set_reuseaddr(true)?;
        socket.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
    }

    /// Lie le port, réessayé une seconde : un process lancé par un autre test au moment de l'arrêt garde le descripteur
    /// de la prise jusqu'à son exec. Une prise qui fuit, elle, échoue encore au bout de la seconde.
    fn bind_again(port: u16) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match bind_once(port) {
                Err(error)
                    if Instant::now() < deadline && error.kind() == io::ErrorKind::AddrInUse =>
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                outcome => return outcome,
            }
        }
    }

    #[test]
    fn stop_frees_the_port_even_with_an_open_stream() {
        let port = free_port();
        let (service_deps, mut status) = deps(port, None);
        let handle = start(service_deps);
        wait_for(&mut status, |state| {
            *state == ListenState::Listening { port }
        });

        let (mut health, head) = get(port, "/idema-api/health");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let length: usize = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .expect("health has a content-length");
        let mut body = vec![0u8; length];
        health.read_exact(&mut body).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body, serde_json::json!({ "ok": true, "version": "test" }));

        let (mut stream, head) = get(port, "/idema-api/events");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("text/event-stream"), "{head}");
        assert!(
            bind_once(port).is_err(),
            "the port is held while the service listens"
        );

        let started = Instant::now();
        let thread = handle.stop();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "stop must not block the caller"
        );
        thread.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "joined in {:?}",
            started.elapsed()
        );
        bind_again(port).expect("the port is free after the stop");

        // Le flux ouvert a été fermé par le service : la lecture rend la fin du flux, pas une attente.
        let mut rest = Vec::new();
        stream
            .read_to_end(&mut rest)
            .expect("the stream ends cleanly");
        assert!(rest.ends_with(b"0\r\n\r\n"), "{rest:?}");
    }

    #[test]
    fn a_taken_port_is_reported_and_the_thread_ends() {
        let port = free_port();
        let _taken = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let (service_deps, mut status) = deps(port, None);
        let handle = start(service_deps);
        let state = wait_for(&mut status, |state| {
            matches!(state, ListenState::Failed { .. })
        });
        assert_eq!(
            state,
            ListenState::Failed {
                port,
                reason: "port_in_use".into()
            }
        );
        handle.stop().join().unwrap();
    }

    #[test]
    fn a_service_stopped_before_it_binds_never_listens() {
        let port = free_port();
        let (service_deps, status) = deps(
            port,
            Some(std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
            })),
        );
        let handle = start(service_deps);
        handle.stop().join().unwrap();
        assert_eq!(status.borrow().listen, ListenState::Starting);
        bind_again(port).expect("nothing ever listened on the port");
    }

    #[test]
    fn a_restart_retries_a_port_that_is_freed_a_moment_later() {
        let port = free_port();
        let holder = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(holder);
        });
        let (service_deps, mut status) = deps(port, Some(std::thread::spawn(|| {})));
        let handle = start(service_deps);
        wait_for(&mut status, |state| {
            *state == ListenState::Listening { port }
        });
        release.join().unwrap();
        handle.stop().join().unwrap();
    }

    #[test]
    fn a_restart_waits_for_the_previous_thread_before_binding() {
        let port = free_port();
        let (first_deps, mut first_status) = deps(port, None);
        let first = start(first_deps);
        wait_for(&mut first_status, |state| {
            *state == ListenState::Listening { port }
        });
        let previous = first.stop();
        let (second_deps, mut second_status) = deps(port, Some(previous));
        let second = start(second_deps);
        wait_for(&mut second_status, |state| {
            *state == ListenState::Listening { port }
        });
        let (_stream, head) = get(port, "/idema-api/health");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        second.stop().join().unwrap();
    }
}
