//! Le fil du service : un runtime tokio à lui (courant, sans pool), une prise sur `127.0.0.1` seulement, et un arrêt
//! qui rend toujours le port en 1,5 s au plus, même avec des flux ouverts.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use futures::future::{Either, select};
use idema_tmux::server::TmuxServer;
use tokio::sync::{mpsc::UnboundedReceiver, watch};

use super::access::Access;
use super::devices::DeviceStore;
use super::fleet::Fleet;
use super::server::{AppState, router, stopped};
use super::timeline::Timeline;
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
pub struct ServiceDeps {
    pub dir: PathBuf,
    pub version: &'static str,
    pub config: CompanionConfig,
    #[allow(
        dead_code,
        reason = "lu par la tâche 4 (écran, saisie et acquittement d'un pane)"
    )]
    pub tmux: Arc<TmuxServer>,
    #[allow(dead_code, reason = "lu par la tâche 9 (Scratchpad)")]
    pub scratchpad: PathBuf,
    pub events: UnboundedReceiver<ServiceEvent>,
    #[allow(dead_code, reason = "lu par la tâche 4 (commandes sur un pane)")]
    pub commands: UnboundedSender<PaneCommand>,
    pub status: watch::Sender<ServiceStatus>,
    /// Le dernier instantané de la flotte, publié par l'espace de travail (`CompanionController::publish`).
    pub fleet: watch::Receiver<Arc<Fleet>>,
    /// La chronologie, qui appartient au contrôleur : elle vit autant qu'IdemA, pas autant que ce service (un
    /// redémarrage du service sur un autre port la garde). Le service la lit ; l'espace de travail y écrit.
    pub timeline: Arc<Mutex<Timeline>>,
    /// Le fil du service précédent, quand celui-ci le remplace : le nouveau le joint (sur son propre fil, jamais celui
    /// de l'interface) avant de lier la prise, pour ne pas la trouver encore occupée.
    pub previous: Option<JoinHandle<()>>,
    /// Les routes de test existent-elles ? Décidé par le contrôleur (`auth::test_routes_enabled`).
    pub test_routes: bool,
}

/// La poignée que garde le contrôleur : de quoi demander l'arrêt, puis joindre le fil.
pub struct ServiceHandle {
    stop: watch::Sender<bool>,
    thread: JoinHandle<()>,
}

/// Lance le fil du service. Il lie la prise, puis sert jusqu'au signal d'arrêt. `None` si le système refuse le fil :
/// la section reste sans effet, et le journal le dit.
pub fn start(deps: ServiceDeps) -> Option<ServiceHandle> {
    let (stop, stop_receiver) = watch::channel(false);
    let spawned = std::thread::Builder::new()
        .name("companion".into())
        .spawn(move || run(deps, stop_receiver));
    match spawned {
        Ok(thread) => Some(ServiceHandle { stop, thread }),
        Err(_) => {
            hud::log_event("companion disabled reason=thread_spawn_failed");
            None
        }
    }
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

/// Comment `serve` s'est terminé.
enum Served {
    /// La prise n'a jamais été liée.
    NeverListened,
    /// Arrêt demandé, serveur fini à temps.
    Stopped,
    /// Arrêt demandé, mais le serveur n'a pas fini en une seconde : il a été abandonné.
    TimedOut,
    /// Le serveur s'est arrêté de lui-même.
    ServerError,
}

/// Ce que `serve` a besoin de savoir, hors l'état et les canaux.
struct Serving {
    port: u16,
    version: &'static str,
    known_origin: Option<String>,
    test_routes: bool,
    bind_retry: Duration,
    fleet: watch::Receiver<Arc<Fleet>>,
    timeline: Arc<Mutex<Timeline>>,
}

/// Le corps du fil.
fn run(deps: ServiceDeps, stop: watch::Receiver<bool>) {
    let ServiceDeps {
        dir,
        version,
        config,
        events,
        status,
        fleet,
        timeline,
        previous,
        test_routes,
        ..
    } = deps;
    let status = Arc::new(status);
    let restarted = previous.is_some();
    if let Some(previous) = previous {
        let _ = previous.join();
    }
    // Arrêté pendant l'attente (retrait de la section aussitôt après son ajout) : rien à lier, rien à dire.
    if *stop.borrow() {
        return;
    }
    let port = config.port;
    // Les appareils sont lus une fois le fil précédent fini : il n'y a jamais deux écrivains de `devices.json`.
    let devices = match DeviceStore::open(&dir) {
        Ok(devices) => devices,
        Err(error) => {
            fail(
                &status,
                port,
                "devices_unavailable",
                format!("devices_unavailable: {error}"),
            );
            return;
        }
    };
    let access = Arc::new(Access::new(devices, Arc::clone(&status)));
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            fail(
                &status,
                port,
                "runtime_failed",
                format!("runtime_failed: {error}"),
            );
            return;
        }
    };
    let serving = Serving {
        port,
        version,
        known_origin: config.public_url,
        test_routes,
        bind_retry: if restarted {
            RESTART_BIND_RETRY
        } else {
            Duration::ZERO
        },
        fleet,
        timeline,
    };
    let served = runtime.block_on(serve(serving, &status, access, events, stop));
    // Le runtime est abandonné ici : ses tâches restantes (flux en retard, appel bloquant) ne retiennent plus le fil.
    runtime.shutdown_timeout(RUNTIME_STOP_TIMEOUT);
    match served {
        Served::NeverListened => {}
        Served::Stopped => hud::log_event("companion stopped"),
        Served::TimedOut => hud::log_event("companion stopped reason=timeout"),
        Served::ServerError => hud::log_event("companion stopped reason=server_error"),
    }
}

/// Lie la prise et sert jusqu'au signal d'arrêt.
async fn serve(
    serving: Serving,
    status: &watch::Sender<ServiceStatus>,
    access: Arc<Access>,
    events: UnboundedReceiver<ServiceEvent>,
    stop: watch::Receiver<bool>,
) -> Served {
    let Serving {
        port,
        version,
        known_origin,
        test_routes,
        bind_retry,
        fleet,
        timeline,
    } = serving;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = match bind(address, bind_retry).await {
        Ok(listener) => listener,
        Err(error) => {
            let (token, detail) = bind_reason(&error);
            fail(status, port, token, detail);
            return Served::NeverListened;
        }
    };
    status.send_modify(|status| status.listen = ListenState::Listening { port });
    hud::log_event(&format!("companion listening addr={address}"));
    if test_routes {
        hud::log_event("companion test routes active");
    }
    // Les gestes de Personnaliser (code, oubli) et l'échéance du code : une tâche du runtime, qui s'arrête avec lui.
    tokio::spawn(control_loop(events, Arc::clone(&access)));
    let state = AppState {
        version,
        stop: stop.clone(),
        access,
        known_origin,
        port,
        test_routes,
        fleet,
        timeline,
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
                return Served::TimedOut;
            }
            Served::Stopped
        }
        Either::Right((outcome, _)) => {
            status.send_modify(|status| {
                status.listen = ListenState::Failed {
                    port,
                    reason: format!("server_error: {outcome:?}"),
                };
            });
            Served::ServerError
        }
    }
}

/// Applique les gestes de Personnaliser à l'état d'accès, et fait disparaître le code d'appairage à son échéance.
/// S'arrête quand l'espace de travail ferme la file (service retiré). Sans code en cours, la tâche dort sur la file :
/// aucune minuterie.
async fn control_loop(mut events: UnboundedReceiver<ServiceEvent>, access: Arc<Access>) {
    loop {
        let event = match access.pairing_deadline() {
            Some(deadline) => {
                let waiting = tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    events.recv(),
                );
                match waiting.await {
                    Ok(event) => event,
                    Err(_) => {
                        let access = Arc::clone(&access);
                        let _ = tokio::task::spawn_blocking(move || access.expire_pairing()).await;
                        continue;
                    }
                }
            }
            None => events.recv().await,
        };
        let Some(event) = event else {
            return;
        };
        let access = Arc::clone(&access);
        // L'oubli d'un appareil écrit sur disque : hors du runtime.
        let _ = tokio::task::spawn_blocking(move || access.handle(event)).await;
    }
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

/// Le service ne tourne pas : le journal dit pourquoi en un mot (`reason=`), l'état de la prise porte le détail.
fn fail(status: &watch::Sender<ServiceStatus>, port: u16, token: &str, detail: String) {
    hud::log_event(&format!("companion disabled reason={token} port={port}"));
    status.send_modify(|status| {
        status.listen = ListenState::Failed {
            port,
            reason: detail,
        };
    });
}

/// Pourquoi un bind est refusé : le mot du journal, et le détail de l'état. `port_in_use` est la raison que
/// Personnaliser sait dire en français.
fn bind_reason(error: &io::Error) -> (&'static str, String) {
    match error.kind() {
        io::ErrorKind::AddrInUse => ("port_in_use", "port_in_use".into()),
        _ => ("bind_failed", format!("bind_failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Instant;

    use crate::companion::test_support::{
        TempDir, deps, deps_in, free_port, pair_device, wait_for,
    };

    /// Une requête GET en HTTP/1.1, lue jusqu'à la fin de l'en-tête ; `cookie` est la paire `nom=valeur` à présenter.
    fn get(port: u16, path: &str, cookie: &str) -> (TcpStream, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\n\r\n"
        )
        .unwrap();
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
        let home = TempDir::new("stop");
        let (service_deps, _events, mut status, _fleet) = deps_in(port, home.0.clone(), None, true);
        let handle = start(service_deps).expect("the service thread spawns");
        wait_for(&mut status, |state| {
            *state == ListenState::Listening { port }
        });
        // Le flux `events` demande un cookie : un appareil est appairé par la route de test.
        let cookie = pair_device(port, "iPhone");

        let (mut health, head) = get(port, "/idema-api/health", "");
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

        let (mut stream, head) = get(port, "/idema-api/events", &cookie);
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

    /// Les appareils ne peuvent pas être lus (ici, le dossier d'IdemA est un fichier) : le service ne tourne pas, au
    /// lieu de repartir d'une liste vide qui défairait tous les appairages.
    #[test]
    fn unreadable_devices_stop_the_service_before_it_listens() {
        let port = free_port();
        let home = TempDir::new("no-folder");
        let not_a_folder = home.0.join("file");
        std::fs::write(&not_a_folder, b"x").unwrap();
        let (service_deps, _events, mut status, _fleet) = deps_in(port, not_a_folder, None, false);
        let handle = start(service_deps).expect("the service thread spawns");
        let state = wait_for(&mut status, |state| {
            matches!(state, ListenState::Failed { .. })
        });
        let ListenState::Failed { reason, .. } = state else {
            unreachable!()
        };
        assert!(reason.starts_with("devices_unavailable"), "{reason}");
        handle.stop().join().unwrap();
        assert!(
            TcpStream::connect(("127.0.0.1", port)).is_err(),
            "nothing listens"
        );
    }

    #[test]
    fn a_taken_port_is_reported_and_the_thread_ends() {
        let port = free_port();
        let _taken = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let (service_deps, mut status, _home) = deps(port, None);
        let handle = start(service_deps).expect("the service thread spawns");
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
        let (service_deps, status, _home) = deps(
            port,
            Some(std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
            })),
        );
        let handle = start(service_deps).expect("the service thread spawns");
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
        let (service_deps, mut status, _home) = deps(port, Some(std::thread::spawn(|| {})));
        let handle = start(service_deps).expect("the service thread spawns");
        wait_for(&mut status, |state| {
            *state == ListenState::Listening { port }
        });
        release.join().unwrap();
        handle.stop().join().unwrap();
    }

    #[test]
    fn a_restart_waits_for_the_previous_thread_before_binding() {
        let port = free_port();
        let (first_deps, mut first_status, _first_home) = deps(port, None);
        let first = start(first_deps).expect("the service thread spawns");
        wait_for(&mut first_status, |state| {
            *state == ListenState::Listening { port }
        });
        let previous = first.stop();
        let (second_deps, mut second_status, _second_home) = deps(port, Some(previous));
        let second = start(second_deps).expect("the service thread spawns");
        wait_for(&mut second_status, |state| {
            *state == ListenState::Listening { port }
        });
        let (_stream, head) = get(port, "/idema-api/health", "");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        second.stop().join().unwrap();
    }
}
