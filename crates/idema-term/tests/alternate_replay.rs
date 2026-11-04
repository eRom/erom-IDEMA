//! Relecture complète d'un pane en écran alterné (revue finale du jalon 4b, I-1) : tmux jetable, puis la relecture
//! appliquée à un émulateur neuf, comme à l'attache dans l'app. L'historique de l'écran principal doit survivre.

use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use idema_term::emulator::{GridSize, PaneEmulator, Source};
use idema_tmux::client::ControlClient;
use idema_tmux::command::send_keys_hex;
use idema_tmux::env::server_env;
use idema_tmux::protocol::ControlEvent;
use idema_tmux::server::{NewSession, TmuxServer, locate_tmux};
use idema_tmux::sync::{PaneLink, SyncKind, SyncRequest, replay_bytes};

#[test]
fn leaving_the_alternate_screen_after_a_full_resync_leaves_the_history() {
    let path = std::env::var("PATH").ok();
    let binary = locate_tmux(None, path.as_deref()).expect("tmux installed");
    let dir = std::env::temp_dir().join(format!(
        "idema-test-{}-alternate-replay",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // Le socket vit dans le dossier du test (`-S`) : le nettoyage du dossier le retire avec le reste.
    let socket = dir.join("sock").to_string_lossy().into_owned();
    let mut env = server_env(std::env::vars(), None);
    env.retain(|(key, _)| key != "SHELL");
    env.push(("SHELL".into(), "/bin/sh".into()));
    let server = TmuxServer::new(binary, socket, dir.join("tmux.conf"), env);
    server.write_config().unwrap();
    let created = server
        .create_session(&NewSession {
            project: &dir,
            display_name: "Shell",
            agent: "Shell",
            columns: 80,
            lines: 24,
            remote_name: None,
        })
        .unwrap();
    let (sender, events) = channel();
    let mut client = ControlClient::spawn(&server, &created.name, move |event| {
        let _ = sender.send(event);
    })
    .unwrap();
    // `%s` : l'aiguille n'apparaît que dans la sortie, jamais dans l'écho de la commande tapée.
    let command = "seq 1 3000 && printf '\\033[?1049h' && printf 'ecran-%s' alterne\r";
    for line in send_keys_hex(created.pane, command.as_bytes()) {
        client.send(&line).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("ecran-alterne") {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("the program never reached the alternate screen");
        if let Ok(ControlEvent::Output { pane, data }) = events.recv_timeout(left)
            && pane == created.pane
        {
            seen.extend_from_slice(&data);
        }
    }

    let mut link = PaneLink::default();
    client
        .send(&link.start_sync(SyncRequest::new(created.pane, SyncKind::Full, "it")))
        .unwrap();
    let snapshot = loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("the resync got no reply");
        if let Ok(ControlEvent::Reply { lines, .. }) = events.recv_timeout(left)
            && let Some(snapshot) = link.on_reply(&lines)
        {
            break snapshot;
        }
    };
    assert!(snapshot.state.alternate_on);

    let mut emulator = PaneEmulator::new(
        GridSize {
            columns: 80,
            lines: 24,
        },
        5000,
        Source::TmuxPane,
    );
    emulator.feed(&replay_bytes(&snapshot));
    assert!(emulator.search("ecran-alterne").len() == 1);
    emulator.feed(b"\x1b[?1049l");
    assert!(
        emulator.history_size() > 1000,
        "history after leaving the alternate screen: {}",
        emulator.history_size()
    );
    assert!(
        !emulator.search("1500").is_empty(),
        "an old line of the history is gone"
    );
    let _ = server.kill();
    let _ = std::fs::remove_dir_all(&dir);
}
