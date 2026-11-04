//! De la sortie réelle d'un pane à son état (spec §9, intégration) : des agents fictifs dans un serveur tmux
//! jetable, un zsh avec l'intégration d'Idema, puis l'émulateur, le détecteur et la machine d'états branchés
//! comme dans l'app. La session n'est jamais vue : les drapeaux peuvent se poser.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use idema_term::activity::{ActivityState, PaneActivity};
use idema_term::emulator::{GridSize, PaneEmulator, Source};
use idema_term::signals::SignalDetector;
use idema_tmux::client::ControlClient;
use idema_tmux::command::send_keys_hex;
use idema_tmux::protocol::{ControlEvent, PaneId};
use idema_tmux::server::{NewSession, TmuxServer, locate_tmux};
use idema_tmux::shell::write_zsh_integration;

/// Faux agent : collage encadré comme Claude Code, 2 s d'écriture, puis le silence.
const FAKE_AGENT: &str = "printf '\\033[?2004h'; for i in $(seq 1 20); do printf 'ligne %02d du faux agent, assez longue pour compter\\n' $i; sleep 0.1; done; sleep 30";

/// Faux serveur : la même écriture, sans collage encadré.
const FAKE_SERVER: &str = "for i in $(seq 1 20); do printf 'ligne %02d du faux serveur, assez longue pour compter\\n' $i; sleep 0.1; done; sleep 30";

struct Scenario {
    server: TmuxServer,
    dir: PathBuf,
}

impl Drop for Scenario {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Serveur jetable dont les panes lancent zsh avec l'intégration d'Idema, sur un `HOME` vide.
fn scenario(name: &str) -> Scenario {
    let path = std::env::var("PATH").ok();
    let binary = locate_tmux(None, path.as_deref()).expect("tmux installed");
    let dir =
        std::env::temp_dir().join(format!("idema-test-{}-pipeline-{name}", std::process::id()));
    // Le socket vit dans le dossier du test (`-S`) : le nettoyage du dossier le retire avec le reste.
    let socket = dir.join("sock").to_string_lossy().into_owned();
    let home = dir.join("home");
    let integration = dir.join("shell");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".zshrc"), "PS1='$ '\n").unwrap();
    write_zsh_integration(&integration).unwrap();
    let env = vec![
        ("HOME".to_string(), home.to_string_lossy().to_string()),
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("SHELL".to_string(), "/bin/zsh".to_string()),
        ("LANG".to_string(), "en_US.UTF-8".to_string()),
        (
            "ZDOTDIR".to_string(),
            integration.to_string_lossy().to_string(),
        ),
    ];
    let server = TmuxServer::new(binary, socket, dir.join("tmux.conf"), env);
    server.write_config().unwrap();
    Scenario { server, dir }
}

/// Le pane tel que l'app le suit : émulateur (pour le mode 2004), détecteur et machine d'états.
struct Watched {
    pane: PaneId,
    emulator: PaneEmulator,
    detector: SignalDetector,
    activity: PaneActivity,
}

impl Watched {
    /// Traite les événements jusqu'à `until`, avec un pas de la machine toutes les 100 ms.
    fn pump(&mut self, events: &Receiver<(Instant, ControlEvent)>, until: Instant) {
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            if let Ok((at, ControlEvent::Output { pane, data })) =
                events.recv_timeout(left.min(Duration::from_millis(100)))
                && pane == self.pane
            {
                self.emulator.feed(&data);
                for signal in self.detector.feed(&data) {
                    self.activity.on_signal(signal, false);
                }
                self.activity.on_output(data.len(), at);
            }
            let bracketed = self.emulator.mode().contains(TermMode::BRACKETED_PASTE);
            self.activity.tick(Instant::now(), bracketed, false);
        }
    }
}

/// Lance `command` dans une session neuve, non vue, et rend son état au bout de `watch`.
fn state_after(name: &str, command: &str, watch: Duration) -> ActivityState {
    let test = scenario(name);
    let created = test
        .server
        .create_session(&NewSession {
            project: &test.dir,
            display_name: "Test",
            agent: "Test",
            columns: 80,
            lines: 24,
            remote_name: None,
        })
        .unwrap();
    let (sender, events) = channel();
    let mut client = ControlClient::spawn(&test.server, &created.name, move |event| {
        let _ = sender.send((Instant::now(), event));
    })
    .unwrap();
    let mut watched = Watched {
        pane: created.pane,
        emulator: PaneEmulator::new(
            GridSize {
                columns: 80,
                lines: 24,
            },
            1000,
            Source::TmuxPane,
        ),
        detector: SignalDetector::new(),
        activity: PaneActivity::new(),
    };
    // Le premier prompt peut précéder l'attache : au pire, on tape après 5 s, et zsh lira la frappe.
    let ready = Instant::now() + Duration::from_secs(5);
    while !watched.activity.at_prompt() && Instant::now() < ready {
        watched.pump(&events, Instant::now() + Duration::from_millis(100));
    }
    for line in send_keys_hex(created.pane, format!("{command}\r").as_bytes()) {
        client.send(&line).unwrap();
    }
    watched.pump(&events, Instant::now() + watch);
    watched.activity.state(Instant::now())
}

#[test]
fn a_bell_from_a_background_session_waits() {
    let state = state_after("bell", "sleep 1; printf '\\a'", Duration::from_secs(3));
    assert_eq!(state, ActivityState::Waiting);
}

#[test]
fn an_osc_9_notification_waits() {
    let state = state_after("osc9", "printf '\\033]9;fini\\007'", Duration::from_secs(2));
    assert_eq!(state, ActivityState::Waiting);
}

#[test]
fn a_command_failing_with_code_3_is_an_error() {
    let state = state_after("exit3", "sh -c 'exit 3'", Duration::from_secs(2));
    assert_eq!(state, ActivityState::Error);
}

#[test]
fn a_command_interrupted_with_code_130_is_not_an_error() {
    // 4 s : la frappe et le prompt qui suit sont de la sortie ; le pane retombe au repos 2 s après.
    let state = state_after("exit130", "sh -c 'exit 130'", Duration::from_secs(4));
    assert_eq!(state, ActivityState::Idle);
}

#[test]
fn the_fake_agent_ends_its_turn_and_waits() {
    let state = state_after("agent", FAKE_AGENT, Duration::from_secs(7));
    assert_eq!(state, ActivityState::Waiting);
}

#[test]
fn the_fake_server_without_bracketed_paste_stays_idle() {
    let state = state_after("server", FAKE_SERVER, Duration::from_secs(7));
    assert_eq!(state, ActivityState::Idle);
}
