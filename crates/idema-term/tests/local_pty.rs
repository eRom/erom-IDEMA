//! Le PTY local du volet Terminal, contre de vrais shells (AC-23 : rien ne survit à Idema).

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use idema_term::emulator::GridSize;
use idema_term::local_pty::{LocalPty, PtyCommand, PtyEvent};

const SIZE: GridSize = GridSize {
    columns: 80,
    lines: 24,
};

fn env() -> Vec<(String, String)> {
    ["HOME", "USER", "PATH", "LANG"]
        .iter()
        .filter_map(|key| Some((key.to_string(), std::env::var(key).ok()?)))
        .chain([("IDEMA_PTY_TEST".to_string(), "1".to_string())])
        .collect()
}

/// Un zsh interactif sans fichiers de démarrage : un prompt immédiat, et l'option HUP par défaut.
fn spawn_zsh(env: &[(String, String)]) -> (LocalPty, Receiver<PtyEvent>) {
    let (sender, receiver) = channel();
    let pty = LocalPty::spawn(
        &PtyCommand {
            program: Path::new("/bin/zsh"),
            args: &["-f"],
            directory: Path::new("/tmp"),
            env,
            size: SIZE,
        },
        move |event| {
            let _ = sender.send(event);
        },
    )
    .expect("pty spawned");
    (pty, receiver)
}

/// Accumule la sortie jusqu'à `needle` (10 s au plus) et la rend.
fn read_until(events: &Receiver<PtyEvent>, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(PtyEvent::Output(bytes)) => {
                seen.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&seen).contains(needle) {
                    return String::from_utf8_lossy(&seen).into_owned();
                }
            }
            Ok(PtyEvent::Closed) | Err(_) => break,
        }
    }
    panic!(
        "never saw {needle:?} in {:?}",
        String::from_utf8_lossy(&seen)
    );
}

fn process_exists(pid: i32) -> bool {
    // SAFETY : signal 0, le noyau vérifie seulement l'existence du process.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition()
}

/// Lance une commande au premier plan qui écrit son pid puis devient `sleep 1000`.
fn start_foreground_sleep(pty: &LocalPty, events: &Receiver<PtyEvent>) -> i32 {
    pty.write(b"sh -c 'echo SLEEP=$$:; exec sleep 1000'\r");
    let output = read_until(events, ":\r\n");
    let start = output.rfind("SLEEP=").expect("pid line") + "SLEEP=".len();
    output[start..]
        .split(':')
        .next()
        .unwrap()
        .parse()
        .expect("numeric pid")
}

#[test]
fn the_shell_sees_only_the_given_environment_and_an_xterm_term() {
    let (pty, events) = spawn_zsh(&env());
    // `$((40+2))` : l'écho de la ligne tapée ne contient pas encore la marque, seule la sortie la contient.
    pty.write(b"env | sort; echo END-OF-$((40+2))\r");
    let output = read_until(&events, "END-OF-42\r\n");
    assert!(output.contains("IDEMA_PTY_TEST=1"));
    assert!(output.contains("TERM=xterm-256color"));
    // Le process de test a des variables CARGO_* : aucune ne doit passer.
    assert!(!output.contains("CARGO_"), "{output}");
}

#[test]
fn a_resize_reaches_the_shell() {
    let (pty, events) = spawn_zsh(&env());
    pty.resize(GridSize {
        columns: 100,
        lines: 30,
    })
    .unwrap();
    pty.write(b"stty size; echo END-OF-$((40+2))\r");
    let output = read_until(&events, "END-OF-42\r\n");
    assert!(output.contains("30 100"), "{output}");
}

#[test]
fn terminate_takes_down_the_shell_and_its_foreground_command() {
    let (mut pty, events) = spawn_zsh(&env());
    let sleep = start_foreground_sleep(&pty, &events);
    let shell = pty.shell_pid() as i32;
    pty.terminate(Duration::from_secs(2));
    assert!(
        eventually(|| !process_exists(sleep)),
        "sleep {sleep} survived"
    );
    assert!(
        eventually(|| pty.has_exited()),
        "the shell {shell} survived"
    );
}

/// Écart E27 : une commande au premier plan qui ignore SIGHUP survit au raccrochage, puis meurt du SIGKILL que le
/// guetteur détaché envoie une fois le délai passé.
#[test]
fn a_command_ignoring_hangup_is_killed_after_the_grace() {
    let (mut pty, events) = spawn_zsh(&env());
    pty.write(b"sh -c 'trap \"\" HUP; echo SLEEP=$$:; exec sleep 1000'\r");
    let output = read_until(&events, ":\r\n");
    let start = output.rfind("SLEEP=").expect("pid line") + "SLEEP=".len();
    let sleep: i32 = output[start..].split(':').next().unwrap().parse().unwrap();
    pty.terminate(Duration::from_millis(1500));
    std::thread::sleep(Duration::from_millis(500));
    assert!(process_exists(sleep), "SIGHUP alone must not stop it");
    assert!(
        eventually(|| !process_exists(sleep)),
        "sleep {sleep} survived SIGKILL"
    );
}

/// AC-23, `kill -9` : le process qui tient le PTY meurt sans rien nettoyer ; le noyau ferme le maître, le terminal
/// raccroche et la commande au premier plan meurt. Le porteur lance aussi un autre process après le PTY, comme
/// Idema lance le serveur tmux : s'il héritait du maître, le terminal ne raccrocherait jamais.
#[test]
fn a_killed_owner_takes_its_foreground_command_down() {
    if std::env::var_os("IDEMA_PTY_HOLDER").is_some() {
        let (pty, events) = spawn_zsh(&env());
        let sleep = start_foreground_sleep(&pty, &events);
        let mut other = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("another child");
        println!("PIDS {sleep} {}", other.id());
        // Le test parent tue ce process avant la fin de cette attente.
        std::thread::sleep(Duration::from_secs(60));
        let _ = other.kill();
        let _ = other.wait();
        return;
    }
    let mut holder = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_killed_owner_takes_its_foreground_command_down",
            "--nocapture",
        ])
        .env("IDEMA_PTY_HOLDER", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("holder process");
    let stdout = holder.stdout.take().unwrap();
    let line = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .find(|line| line.starts_with("PIDS "))
        .expect("the holder reports its pids");
    let pids: Vec<i32> = line[5..].split(' ').map(|n| n.parse().unwrap()).collect();
    let (sleep, other) = (pids[0], pids[1]);
    // SAFETY : SIGKILL au porteur, notre enfant.
    unsafe {
        libc::kill(holder.id() as i32, libc::SIGKILL);
    }
    let _ = holder.wait();
    let gone = eventually(|| !process_exists(sleep));
    // SAFETY : on retire l'autre enfant du porteur, orphelin désormais.
    unsafe {
        libc::kill(other, libc::SIGKILL);
    }
    assert!(gone, "sleep {sleep} survived its owner");
}

/// L'état du process vu par `ps` ; `None` quand il n'existe plus (récolté).
fn process_state(pid: i32) -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps ran");
    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!state.is_empty()).then_some(state)
}

/// Un shell qui sort puis un PTY lâché ne laissent pas de zombie : le volet Terminal lâche le PTY à `Closed`,
/// et chaque shell parti resterait sinon `Z` jusqu'à la fin d'Idema.
#[test]
fn a_dropped_pty_leaves_no_zombie_after_its_shell_exits() {
    let (pty, events) = spawn_zsh(&env());
    let shell = pty.shell_pid() as i32;
    pty.write(b"exit\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(PtyEvent::Closed) => break,
            Ok(PtyEvent::Output(_)) => {}
            Err(_) => panic!("the shell never closed"),
        }
    }
    drop(pty);
    assert!(
        eventually(|| process_state(shell).is_none()),
        "the shell {shell} is still there: {:?}",
        process_state(shell)
    );
}

/// Un process qui attend `sleep <grace>` : le guetteur de SIGKILL que `terminate` arme, repéré par son délai.
fn watcher_running(grace: &str) -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-f", &format!("sleep {grace}")])
        .stdout(Stdio::null())
        .status()
        .expect("pgrep ran")
        .success()
}

/// Un shell déjà parti n'arme pas de guetteur de SIGKILL (son pgid est libre), un shell vivant en arme un.
#[test]
fn the_kill_watcher_is_armed_only_for_a_live_shell() {
    // Délais à trois décimales, rares : `pgrep -f` ne reconnaît que ces guetteurs-ci.
    let (mut gone, events) = spawn_zsh(&env());
    gone.write(b"exit\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Ok(event) = events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if event == PtyEvent::Closed {
            break;
        }
    }
    assert!(eventually(|| gone.has_exited()), "the shell never exited");
    gone.terminate(Duration::from_millis(7531));
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !watcher_running("7.531"),
        "a watcher was armed for an exited shell"
    );

    let (mut live, _events) = spawn_zsh(&env());
    live.terminate(Duration::from_millis(1234));
    assert!(
        eventually(|| watcher_running("1.234")),
        "no watcher for a live shell"
    );
    // Le guetteur du shell vivant finit seul après son délai.
    assert!(
        eventually(|| !watcher_running("1.234")),
        "the watcher never ended"
    );
}

/// Le dossier courant du groupe au premier plan suit un `cd` du shell (le clic sur un chemin relatif du volet
/// Terminal se résout contre lui).
#[test]
fn foreground_cwd_follows_a_cd() {
    let (sender, events) = channel();
    let pty = LocalPty::spawn(
        &PtyCommand {
            program: Path::new("/bin/sh"),
            args: &[],
            directory: Path::new("/"),
            env: &env(),
            size: SIZE,
        },
        move |event| {
            let _ = sender.send(event);
        },
    )
    .expect("pty spawned");
    pty.write(b"cd /tmp; echo END-OF-$((40+2))\r");
    read_until(&events, "END-OF-42\r\n");
    let expected = std::fs::canonicalize("/tmp").unwrap();
    assert!(
        eventually(|| pty.foreground_cwd().as_deref() == Some(expected.as_path())),
        "foreground_cwd is {:?}, expected {expected:?}",
        pty.foreground_cwd()
    );
}

/// Le nombre d'entrées de `/dev/fd` que voit une commande lancée par ce test.
fn descriptors_seen_by_a_child() -> usize {
    let output = Command::new("/bin/sh")
        .args(["-c", "ls /dev/fd | wc -l"])
        .output()
        .expect("sh ran");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("a count")
}

/// Garde-fou : le PTY naît fermé à l'exec, une commande lancée ensuite par Idema ne le voit pas.
#[test]
fn a_command_spawned_after_the_pty_does_not_inherit_it() {
    let before = descriptors_seen_by_a_child();
    let (_pty, _events) = spawn_zsh(&env());
    let after = descriptors_seen_by_a_child();
    assert_eq!(before, after, "the pty leaked into a later command");
}
