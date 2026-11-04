//! Tests d'intégration contre un vrai tmux 3.8, sur un serveur jetable par test.
//! Ils confirment aussi les contrats que le code tient pour acquis (apostrophes, `pane_key_mode`,
//! captures de l'écran alterné, octets UTF-8 par `send-keys -H`).

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use idema_tmux::client::ControlClient;
use idema_tmux::command::{
    BORDER_WATCH, PaneWatch, border_reset, border_subscription, color_report, quote, send_keys_hex,
    unset_pane_option,
};
use idema_tmux::env::server_env;
use idema_tmux::layout::{Preset, WindowLayout, preset_cell, preset_layout};
use idema_tmux::protocol::{ControlEvent, PaneId, SessionId};
use idema_tmux::server::{
    CreatedSession, NewServerSession, NewSession, RestorePane, RestoreSession, ServerError,
    TmuxServer, exact, locate_tmux,
};
use idema_tmux::shell::write_zsh_integration;
use idema_tmux::sync::{CursorShape, PaneLink, Snapshot, SyncKind, SyncRequest};
use idema_tmux::windows::{QueryStep, SessionWindows, WindowsQuery};

/// Serveur jetable : socket et dossier propres au test, `kill-server` à la fin.
struct Disposable {
    server: TmuxServer,
    dir: PathBuf,
}

impl Drop for Disposable {
    /// Arrête le serveur et retire le dossier du test : sans cela, chaque passage laissait un `idema-test-*`
    /// dans `$TMPDIR` (mineur du jalon 2).
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn disposable(name: &str) -> Disposable {
    let path = std::env::var("PATH").ok();
    let binary = locate_tmux(None, path.as_deref()).expect("tmux installed");
    let dir = std::env::temp_dir().join(format!("idema-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("test directory");
    // Le socket vit dans le dossier du test (`-S`) : `-L` laisserait un fichier dans le dossier de tmux à chaque passage.
    let socket = dir.join("sock").to_string_lossy().into_owned();
    // /bin/sh : pas de fichiers de démarrage, donc un prompt immédiat et des tests déterministes.
    let mut env = server_env(std::env::vars(), None);
    env.retain(|(key, _)| key != "SHELL");
    env.push(("SHELL".into(), "/bin/sh".into()));
    let server = TmuxServer::new(binary, socket, dir.join("tmux.conf"), env);
    server.write_config().expect("test config");
    Disposable { server, dir }
}

fn attach(test: &Disposable, session: &str) -> (ControlClient, Receiver<ControlEvent>) {
    let (sender, receiver) = channel();
    let client = ControlClient::spawn(&test.server, session, move |event| {
        let _ = sender.send(event);
    })
    .expect("control client");
    (client, receiver)
}

/// Session « Shell » dans le dossier du test.
fn shell_session(test: &Disposable) -> CreatedSession {
    test.server
        .create_session(&NewSession {
            project: &test.dir,
            display_name: "Shell",
            agent: "Shell",
            columns: 80,
            lines: 24,
            remote_name: None,
        })
        .expect("session created")
}

/// Attend une réponse de tmux à une commande : le client est alors attaché.
fn wait_attached(client: &mut ControlClient, events: &Receiver<ControlEvent>) {
    client.send("display-message -p idema-ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(ControlEvent::Reply { lines, .. }) = events.recv_timeout(left)
            && lines.iter().any(|line| line == b"idema-ready")
        {
            return;
        }
    }
    panic!("control client never answered");
}

/// Accumule la sortie du pane jusqu'à y trouver `needle` (10 s au plus), et la rend.
/// Tout ce qui précède l'aiguille est rendu avec elle : un marqueur arrivé dans le même paquet n'est pas perdu.
fn collect_output(events: &Receiver<ControlEvent>, pane: PaneId, needle: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(ControlEvent::Output { pane: p, data }) if p == pane => {
                seen.extend_from_slice(&data);
                let text = String::from_utf8_lossy(&seen);
                if text.contains(needle) {
                    return Some(text.into_owned());
                }
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

/// Attend la sortie du pane jusqu'à y trouver `needle` (10 s au plus).
fn wait_for_output(events: &Receiver<ControlEvent>, pane: PaneId, needle: &str) -> bool {
    collect_output(events, pane, needle).is_some()
}

/// Lance une relecture et attend l'instantané (10 s au plus).
fn sync(
    client: &mut ControlClient,
    events: &Receiver<ControlEvent>,
    pane: PaneId,
    kind: SyncKind,
) -> Snapshot {
    let mut link = PaneLink::default();
    client
        .send(&link.start_sync(SyncRequest::new(pane, kind, "it")))
        .expect("sending the resync");
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(ControlEvent::Reply { lines, .. }) = events.recv_timeout(left)
            && let Some(snapshot) = link.on_reply(&lines)
        {
            return snapshot;
        }
    }
    panic!("resync got no reply");
}

fn type_in(client: &mut ControlClient, pane: PaneId, text: &str) {
    for line in send_keys_hex(pane, text.as_bytes()) {
        client.send(&line).expect("send-keys");
    }
}

#[test]
fn local_tmux_is_supported() {
    assert!(disposable("version").server.check_version().is_ok());
}

#[test]
fn create_session_sets_every_option_in_one_invocation() {
    let test = disposable("create");
    // Un nom libre : tiret en tête, apostrophe, point-virgule isolé et final.
    let tricky = "-x l'atelier ; fin;";
    let session = test
        .server
        .create_session(&NewSession {
            project: &test.dir,
            display_name: tricky,
            agent: "Claude",
            columns: 80,
            lines: 24,
            remote_name: None,
        })
        .unwrap();
    let listed = test.server.sessions().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, session.name);
    assert_eq!(listed[0].project, test.dir.to_string_lossy());
    assert_eq!(listed[0].display_name, tricky);
    assert_eq!(listed[0].created, session.created);
    let target = format!("={}:", session.name);
    assert_eq!(
        test.server
            .run(&["show-options", "-t", &target, "-v", "@idema_layout"])
            .unwrap(),
        "single"
    );
    let panes = test.server.panes().unwrap();
    assert_eq!(panes.len(), 1);
    assert_eq!(
        (panes[0].pane, panes[0].active, panes[0].agent.as_str()),
        (session.pane, true, "Claude")
    );
}

#[test]
fn create_session_marks_an_ideation_session() {
    let test = disposable("ideation");
    let ideation_session = test
        .server
        .create_session(&NewSession {
            project: &test.dir,
            display_name: "Fable",
            agent: "Claude",
            columns: 80,
            lines: 24,
            remote_name: Some("erom-ideation-2"),
        })
        .unwrap();
    let plain = test
        .server
        .create_session(&NewSession {
            project: &test.dir,
            display_name: "Shell",
            agent: "Shell",
            columns: 80,
            lines: 24,
            remote_name: None,
        })
        .unwrap();
    let listed = test.server.sessions().unwrap();
    let find = |name: &str| listed.iter().find(|session| session.name == name).unwrap();
    assert!(find(&ideation_session.name).ideation);
    assert_eq!(find(&ideation_session.name).remote_name, "erom-ideation-2");
    assert_eq!(find(&ideation_session.name).display_name, "Fable");
    assert!(!find(&plain.name).ideation);
    assert_eq!(find(&plain.name).remote_name, "");
}

#[test]
fn listings_without_a_server_are_empty() {
    let test = disposable("noserver");
    assert!(test.server.sessions().unwrap().is_empty());
    assert!(test.server.panes().unwrap().is_empty());
    test.server
        .set_global_environment("IDEMA_PROBE", "x")
        .unwrap();
}

#[test]
fn a_project_folder_with_a_hash_opens_there() {
    let test = disposable("hash");
    let project = test.dir.join("projet#S");
    std::fs::create_dir_all(&project).unwrap();
    let session = test
        .server
        .create_session(&NewSession {
            project: &project,
            display_name: "Shell",
            agent: "Shell",
            columns: 80,
            lines: 24,
            remote_name: None,
        })
        .unwrap();
    let opened = test
        .server
        .run(&[
            "display-message",
            "-p",
            "-t",
            &format!("={}:", session.name),
            "#{pane_current_path}",
        ])
        .unwrap();
    assert_eq!(
        std::fs::canonicalize(opened).unwrap(),
        std::fs::canonicalize(&project).unwrap()
    );
    assert_eq!(
        test.server.sessions().unwrap()[0].project,
        project.to_string_lossy()
    );
}

#[test]
fn foreign_sessions_are_not_listed() {
    let test = disposable("foreign");
    test.server
        .run(&["new-session", "-d", "-s", "hors-projet"])
        .unwrap();
    shell_session(&test);
    assert_eq!(test.server.sessions().unwrap().len(), 1);
}

#[test]
fn options_and_kill_target_the_exact_session() {
    let test = disposable("exact");
    let first = shell_session(&test);
    let second = shell_session(&test);
    assert_ne!(first.name, second.name);
    // Sans `=`, tmux prendrait « idema- » pour un préfixe de nom.
    assert!(!test.server.has_session("idema-"));
    test.server
        .set_session_option(&first.name, "@idema_name", "Renommée")
        .unwrap();
    test.server
        .set_pane_option(second.pane, "@idema_agent", "Codex")
        .unwrap();
    // Ce que `idnma run codex` pose sur son pane.
    test.server
        .set_pane_option(second.pane, "@idnma_agent", "codex")
        .unwrap();
    assert!(
        test.server
            .panes()
            .unwrap()
            .iter()
            .any(|pane| pane.pane == second.pane
                && pane.agent == "Codex"
                && pane.idnma_agent == "codex")
    );
    test.server.kill_session(&second.name).unwrap();
    assert!(test.server.has_session(&first.name));
    assert!(!test.server.has_session(&second.name));
    let names: Vec<String> = test
        .server
        .sessions()
        .unwrap()
        .into_iter()
        .map(|session| session.display_name)
        .collect();
    assert_eq!(names, vec!["Renommée".to_string()]);
}

#[test]
fn the_capture_reads_the_resume_option_of_a_pane() {
    let test = disposable("resume-option");
    let with_option = shell_session(&test);
    let without_option = shell_session(&test);
    let value = "aaaa1111-bbbb-4ccc-8ddd-eeeeffff0000 idnma run 'claude' --model 'haiku' --remote-control 'it'\\''s me'";
    test.server
        .set_pane_option(with_option.pane, "@idema_resume", value)
        .unwrap();
    let (_, captured) = test.server.capture().unwrap();
    let find = |id: PaneId| captured.iter().find(|pane| pane.pane == id).unwrap();
    assert_eq!(find(with_option.pane).resume, value);
    assert_eq!(find(without_option.pane).resume, "");
    // Les champs d'après ne bougent pas : le dossier reste lu en dernier.
    assert!(find(with_option.pane).cwd.is_absolute());
}

#[test]
fn restore_session_writes_the_resume_option_of_each_pane_that_has_one() {
    let test = disposable("restore-resume");
    let value = "aaaa1111-bbbb-4ccc-8ddd-eeeeffff0000 idnma run 'claude' --model 'haiku'";
    let panes = [
        RestorePane {
            resume: Some(value),
            ..restore_pane(&test.dir, "Opus 5.5", None, true)
        },
        restore_pane(&test.dir, "Shell seul", None, false),
        RestorePane {
            resume: Some(value),
            ..restore_pane(&test.dir, "Opus 5.5", Some(777), false)
        },
    ];
    let ids = test
        .server
        .restore_session(&restore_request(&test.dir, "columns-2", &panes))
        .expect("session restored");
    let (_, captured) = test.server.capture().unwrap();
    let resume_of = |id: PaneId| {
        captured
            .iter()
            .find(|c| c.pane == id)
            .unwrap()
            .resume
            .clone()
    };
    assert_eq!(resume_of(ids[0]), value);
    assert_eq!(resume_of(ids[1]), "");
    // Le pane caché a rejoint sa fenêtre après la pose : l'option l'a suivi.
    assert_eq!(resume_of(ids[2]), value);
}

#[test]
fn the_control_client_clears_the_idnma_agent_of_a_pane_before_its_keys() {
    let test = disposable("unset");
    let session = shell_session(&test);
    test.server
        .set_pane_option(session.pane, "@idnma_agent", "glm")
        .unwrap();
    let (mut client, events) = attach(&test, &session.name);
    wait_attached(&mut client, &events);
    // Comme une relance d'IdemA : l'option retirée, puis la commande tapée, sur le même client de contrôle.
    client
        .send(&unset_pane_option(session.pane, "@idnma_agent"))
        .unwrap();
    for line in send_keys_hex(session.pane, b"true\r") {
        client.send(&line).unwrap();
    }
    let idnma_agent = |test: &Disposable| {
        test.server
            .panes()
            .unwrap()
            .into_iter()
            .find(|pane| pane.pane == session.pane)
            .map(|pane| pane.idnma_agent)
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while idnma_agent(&test).as_deref() != Some("") && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(idnma_agent(&test).as_deref(), Some(""));
    // Retirer une option absente n'est pas une erreur : le client de contrôle reste en vie.
    client
        .send(&unset_pane_option(session.pane, "@idnma_agent"))
        .unwrap();
    assert!(test.server.has_session(&session.name));
}

#[test]
fn pane_watch_reports_the_agent_and_the_command() {
    let test = disposable("watch");
    let session = shell_session(&test);
    let (mut client, events) = attach(&test, &session.name);
    wait_attached(&mut client, &events);
    client
        .send(&PaneWatch::Agent.subscribe(session.pane))
        .unwrap();
    client
        .send(&PaneWatch::Command.subscribe(session.pane))
        .unwrap();
    test.server
        .set_pane_option(session.pane, "@idema_agent", "GLM")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut agent, mut command) = (false, false);
    while !(agent && command) {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        if let Ok(ControlEvent::SubscriptionChanged { name, value }) = events.recv_timeout(left) {
            match PaneWatch::parse(&name) {
                Some((PaneWatch::Agent, pane)) if pane == session.pane => {
                    agent |= value == "GLM";
                }
                Some((PaneWatch::Command, pane)) if pane == session.pane => {
                    command |= !value.is_empty();
                }
                _ => {}
            }
        }
    }
    assert!(agent, "the agent subscription never reported the new value");
    assert!(command, "the command subscription never reported a value");
}

#[test]
fn control_client_exits_when_its_session_is_killed() {
    let test = disposable("killed");
    let session = shell_session(&test);
    let (mut client, events) = attach(&test, &session.name);
    wait_attached(&mut client, &events);
    test.server.kill_session(&session.name).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(ControlEvent::Exit { .. }) = events.recv_timeout(left) {
            exited = true;
            break;
        }
    }
    assert!(exited);
}

/// Attend un `SessionsChanged` (2 s au plus, la borne d'AC-37) ; vrai s'il est venu.
fn sessions_changed_within_two_seconds(events: &Receiver<ControlEvent>) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(ControlEvent::SessionsChanged) = events.recv_timeout(left) {
            return true;
        }
    }
    false
}

#[test]
fn sessions_changed_reaches_a_client_of_another_session() {
    let test = disposable("sessions-changed");
    let session = shell_session(&test);
    let (mut client, events) = attach(&test, &session.name);
    wait_attached(&mut client, &events);
    // Une session créée puis tuée par la CLI, hors d'Idema : le client de la session A l'apprend les deux fois.
    test.server
        .run(&["new-session", "-d", "-s", "outside"])
        .expect("outside session");
    assert!(
        sessions_changed_within_two_seconds(&events),
        "no notification for a creation"
    );
    test.server
        .kill_session("outside")
        .expect("outside session killed");
    assert!(
        sessions_changed_within_two_seconds(&events),
        "no notification for a kill"
    );
}

#[test]
fn a_session_can_be_created_after_kill_server() {
    let test = disposable("after-kill-server");
    shell_session(&test);
    test.server.kill().expect("kill-server");
    // Sans serveur, la liste est vide : la barre d'Idema se vide.
    assert!(eventually(|| test.server.sessions().unwrap().is_empty()));
    // `new-session` relance le serveur avec la config et l'environnement d'Idema.
    let session = shell_session(&test);
    let names: Vec<String> = test
        .server
        .sessions()
        .unwrap()
        .into_iter()
        .map(|listed| listed.name)
        .collect();
    assert_eq!(names, vec![session.name]);
}

#[test]
fn global_environment_reaches_the_panes_created_afterwards() {
    let test = disposable("environment");
    shell_session(&test);
    test.server
        .set_global_environment("IDEMA_PROBE", "posee")
        .unwrap();
    let session = shell_session(&test);
    let (mut client, events) = attach(&test, &session.name);
    type_in(
        &mut client,
        session.pane,
        "printf 'env-%s' \"$IDEMA_PROBE\"\r",
    );
    assert!(wait_for_output(&events, session.pane, "env-posee"));
}

#[test]
fn zsh_integration_marks_commands_and_keeps_the_user_files() {
    let path = std::env::var("PATH").ok();
    let binary = locate_tmux(None, path.as_deref()).expect("tmux installed");
    let dir = std::env::temp_dir().join(format!("idema-test-{}-zsh", std::process::id()));
    let socket = dir.join("sock").to_string_lossy().into_owned();
    let home = dir.join("home");
    let user_zdotdir = home.join("zdot");
    let integration = dir.join("shell");
    std::fs::create_dir_all(&user_zdotdir).unwrap();
    // Un utilisateur dont le ~/.zshenv pose ZDOTDIR et dont un crochet precmd change `$?`.
    std::fs::write(
        home.join(".zshenv"),
        "export ZDOTDIR=\"$HOME/zdot\"\nexport IDEMA_MARK_ENV=env\n",
    )
    .unwrap();
    std::fs::write(
        user_zdotdir.join(".zshrc"),
        "PS1='$ '\nexport IDEMA_MARK_RC=rc\n_user_precmd() { true }\nprecmd_functions+=(_user_precmd)\n",
    )
    .unwrap();
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
    let test = Disposable { server, dir };
    let session = shell_session(&test);
    let (mut client, events) = attach(&test, &session.name);
    // Le premier prompt peut précéder l'attache : on juge sur la commande suivante.
    type_in(&mut client, session.pane, "false\r");
    let output = collect_output(&events, session.pane, "\x1b]133;D;1\x07\x1b]133;A\x07")
        .expect("the command end and the next prompt are marked");
    assert!(output.contains("\x1b]133;C\x07"));
    // B (ZLE a la ligne) suit A au même prompt, dans ce paquet ou le suivant.
    let after_prompt = output
        .rsplit_once("\x1b]133;A\x07")
        .map(|(_, rest)| rest.to_string())
        .expect("the prompt mark is in the output");
    assert!(
        after_prompt.contains("\x1b]133;B\x07")
            || wait_for_output(&events, session.pane, "\x1b]133;B\x07"),
        "the line-init hook marks the end of the prompt"
    );
    type_in(
        &mut client,
        session.pane,
        "printf 'z-%s-%s-%s' \"$ZDOTDIR\" \"$IDEMA_MARK_ENV\" \"$IDEMA_MARK_RC\"\r",
    );
    let expected = format!("z-{}-env-rc", user_zdotdir.display());
    assert!(wait_for_output(&events, session.pane, &expected));
}

#[test]
fn control_client_replies_and_quote_round_trips() {
    let test = disposable("client");
    let session = shell_session(&test);
    let (mut client, events) = attach(&test, &session.name);
    client
        .send(&format!("set -g @idema_test {}", quote("l'atelier ok")))
        .unwrap();
    client.send("display-message -p ping-idema").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut answered = false;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(ControlEvent::Reply { lines, .. }) = events.recv_timeout(left)
            && lines.iter().any(|line| line == b"ping-idema")
        {
            answered = true;
            break;
        }
    }
    assert!(answered);
    assert_eq!(
        test.server
            .run(&["show-options", "-gv", "@idema_test"])
            .unwrap(),
        "l'atelier ok"
    );
}

#[test]
fn pane_output_flows_and_full_sync_matches_the_pane() {
    let test = disposable("output");
    let session = shell_session(&test);
    let pane = session.pane;
    let (mut client, events) = attach(&test, &session.name);
    client.send("refresh-client -C 80x24").unwrap();
    type_in(&mut client, pane, "printf 'idema-%s\\n' sortie\r");
    assert!(wait_for_output(&events, pane, "idema-sortie"));
    let snapshot = sync(&mut client, &events, pane, SyncKind::Full);
    assert_eq!((snapshot.state.width, snapshot.state.height), (80, 24));
    assert!(
        snapshot
            .primary
            .iter()
            .any(|line| String::from_utf8_lossy(line).contains("idema-sortie"))
    );
    assert_eq!(snapshot.alternate, None);
}

#[test]
fn a_full_resync_keeps_the_background_of_empty_cells() {
    let test = disposable("fullbackground");
    let session = shell_session(&test);
    let pane = session.pane;
    let (mut client, events) = attach(&test, &session.name);
    client.send("refresh-client -C 80x24").unwrap();
    // 40 lignes de plus que l'écran ne tient : l'historique est non vide, la frontière est franchie.
    type_in(
        &mut client,
        pane,
        "seq 1 40; printf '\\033[44mfond\\033[K\\033[0m\\n'; printf 'fin-%s' ok\r",
    );
    assert!(wait_for_output(&events, pane, "fin-ok"));
    let snapshot = sync(&mut client, &events, pane, SyncKind::Full);
    let background = snapshot
        .primary
        .iter()
        .find(|row| row.starts_with(b"\x1b[44mfond"))
        .expect("the painted row is in the snapshot");
    // Fond bleu puis le texte, et les cellules vides jusqu'à la 80e colonne : sans -J, tmux les rend.
    assert_eq!(background.len(), b"\x1b[44m".len() + 80, "{background:?}");
    // Chaque ligne d'historique et d'écran présente une seule fois, dans l'ordre, frontière comprise.
    let numbers: Vec<u32> = snapshot
        .primary
        .iter()
        .filter_map(|row| {
            // La première rangée de l'écran porte le retour aux attributs par défaut, posé à la frontière.
            let row = row.strip_prefix(b"\x1b[0m").unwrap_or(row);
            std::str::from_utf8(row.trim_ascii()).ok()?.parse().ok()
        })
        .collect();
    assert_eq!(numbers, (1..=40).collect::<Vec<u32>>());
}

#[test]
fn a_full_resync_of_an_empty_history_does_not_repeat_the_first_row() {
    // Contrat sondé le 16/10 : `-S -2000 -E -1` rend la rangée 0 de l'écran quand l'historique est vide.
    let test = disposable("fullempty");
    let session = shell_session(&test);
    let pane = session.pane;
    let (mut client, events) = attach(&test, &session.name);
    wait_attached(&mut client, &events);
    let snapshot = sync(&mut client, &events, pane, SyncKind::Full);
    assert_eq!(snapshot.primary.len(), usize::from(snapshot.state.height));
}

#[test]
fn alternate_screen_sync_separates_both_screens() {
    let test = disposable("alternate");
    let session = shell_session(&test);
    let pane = session.pane;
    let (mut client, events) = attach(&test, &session.name);
    // `%s` : l'aiguille n'apparaît que dans la sortie, jamais dans l'écho de la commande tapée.
    type_in(
        &mut client,
        pane,
        "printf 'avant-%s\\n\\033[?1049h' ok && printf 'ecran-%s' alterne\r",
    );
    assert!(wait_for_output(&events, pane, "ecran-alterne"));
    let snapshot = sync(&mut client, &events, pane, SyncKind::Full);
    assert!(snapshot.state.alternate_on);
    let alternate = snapshot.alternate.expect("alternate screen captured");
    assert!(
        alternate
            .iter()
            .any(|line| String::from_utf8_lossy(line).contains("ecran-alterne"))
    );
    assert!(
        snapshot
            .primary
            .iter()
            .any(|line| String::from_utf8_lossy(line).contains("avant-ok"))
    );
}

#[test]
fn a_full_resync_in_the_alternate_screen_keeps_the_history_with_the_primary_screen() {
    // Sonde du 16/10 (tmux 3.8) : en écran alterné, `capture-pane -S -2000 -E -1` rend l'historique de l'écran
    // principal et `-a` son écran sauvegardé. L'historique appartient donc à `primary`, jamais à `alternate`.
    let test = disposable("alternatehistory");
    let session = shell_session(&test);
    let pane = session.pane;
    let (mut client, events) = attach(&test, &session.name);
    wait_attached(&mut client, &events);
    type_in(
        &mut client,
        pane,
        "seq 1 3000 && printf '\\033[?1049h' && printf 'ecran-%s' alterne\r",
    );
    assert!(wait_for_output(&events, pane, "ecran-alterne"));
    let snapshot = sync(&mut client, &events, pane, SyncKind::Full);
    assert!(snapshot.state.alternate_on);
    let contains = |lines: &[Vec<u8>], needle: &str| {
        lines
            .iter()
            .any(|line| String::from_utf8_lossy(line).trim_end() == needle)
    };
    assert!(contains(&snapshot.primary, "1500"), "old history line lost");
    assert!(contains(&snapshot.primary, "3000"), "saved screen lost");
    let alternate = snapshot.alternate.expect("alternate screen captured");
    assert_eq!(alternate.len(), usize::from(snapshot.state.height));
    assert!(!contains(&alternate, "1500"));
    assert!(
        alternate
            .iter()
            .any(|line| String::from_utf8_lossy(line).contains("ecran-alterne"))
    );
}

#[test]
fn key_mode_reports_an_extended_keys_request() {
    // Contrat de `extended_level` : « Ext 2 » après `CSI > 4 ; 2 m`.
    let test = disposable("keymode");
    let session = shell_session(&test);
    let pane = session.pane;
    let (mut client, events) = attach(&test, &session.name);
    type_in(
        &mut client,
        pane,
        "printf '\\033[>4;2m' && printf 'mode-%s' pose\r",
    );
    assert!(wait_for_output(&events, pane, "mode-pose"));
    let snapshot = sync(&mut client, &events, pane, SyncKind::Visible);
    assert_eq!(snapshot.state.key_mode, "Ext 2");
}

#[test]
fn sync_reports_the_scroll_region_and_the_cursor_shape() {
    // Contrat de `STATE_FORMAT` : tmux 3.8 rapporte la zone (rangées à partir de 0) et la forme du curseur.
    let test = disposable("region");
    let session = shell_session(&test);
    let pane = session.pane;
    let (mut client, events) = attach(&test, &session.name);
    type_in(
        &mut client,
        pane,
        "printf '\\033[5;10r\\033[6 q' && printf 'zone-%s' posee\r",
    );
    assert!(wait_for_output(&events, pane, "zone-posee"));
    let snapshot = sync(&mut client, &events, pane, SyncKind::Visible);
    assert_eq!(
        (
            snapshot.state.scroll_region_upper,
            snapshot.state.scroll_region_lower
        ),
        (4, 9)
    );
    assert_eq!(snapshot.state.cursor_shape, CursorShape::Bar);
    assert!(!snapshot.state.cursor_blinking);
}

#[test]
fn utf8_bytes_reach_the_pane_through_hex_send_keys() {
    let test = disposable("utf8");
    let session = shell_session(&test);
    let pane = session.pane;
    let target = test.dir.join("saisie.txt");
    let (mut client, events) = attach(&test, &session.name);
    type_in(
        &mut client,
        pane,
        &format!("cat > {} && printf 'cat-%s\\n' fini\r", target.display()),
    );
    type_in(&mut client, pane, "àéèçùœ\r\u{4}");
    assert!(wait_for_output(&events, pane, "cat-fini"));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "àéèçùœ\n");
}

#[test]
fn a_program_in_a_pane_reads_the_declared_colours() {
    let test = disposable("colours");
    let session = shell_session(&test);
    let pane = session.pane;
    let (mut client, events) = attach(&test, &session.name);
    wait_attached(&mut client, &events);
    client
        .send(&color_report(pane, 10, [0xE2, 0xE2, 0xE3]))
        .unwrap();
    client
        .send(&color_report(pane, 11, [0x15, 0x15, 0x16]))
        .unwrap();
    // Sonde du 16/10 : le programme interroge OSC 10 puis OSC 11 sur le terminal et imprime chaque réponse par `cat -v`.
    let script = r#"q(){ printf "\033]$1;?\033\\" >/dev/tty; IFS= read -rs -t 2 -d "\\" r; printf "%s\n" "$r" | cat -v; }; q 10; q 11"#;
    type_in(
        &mut client,
        pane,
        &format!("bash --norc --noprofile -c '{script}'\r"),
    );
    // La réponse au fond arrive en second : la collecte rend aussi celle du texte.
    let output = collect_output(&events, pane, "rgb:1515/1515/1616").expect("background reply");
    assert!(output.contains("rgb:e2e2/e2e2/e3e3"), "{output}");
}

/// AC-38 : le wrapper du swarm pose `pane-border-status top` sur une fenêtre ; l'abonnement le signale, la remise à
/// `off` part sur la valeur reçue, et tmux ne signale rien de plus (pas de boucle).
#[test]
fn a_border_set_by_the_wrapper_is_turned_off_within_two_seconds() {
    let test = disposable("border");
    let session = shell_session(&test);
    let (mut client, events) = attach(&test, &session.name);
    wait_attached(&mut client, &events);
    client.send(&border_subscription()).unwrap();
    let target = exact(&session.name);
    // Ce que fait le wrapper du swarm : la bordure en haut, puis son format.
    test.server
        .run(&[
            "set-option",
            "-w",
            "-t",
            &target,
            "pane-border-status",
            "top",
        ])
        .unwrap();
    test.server
        .run(&[
            "set-option",
            "-w",
            "-t",
            &target,
            "pane-border-format",
            " x ",
        ])
        .unwrap();
    let start = Instant::now();
    let mut resets = 0;
    let mut checked_at_two_seconds = false;
    while let Some(left) = (start + Duration::from_secs(3)).checked_duration_since(Instant::now()) {
        // Les 2 s passées, la bordure est éteinte et le pane a retrouvé la hauteur de la fenêtre.
        if !checked_at_two_seconds && start.elapsed() >= Duration::from_secs(2) {
            checked_at_two_seconds = true;
            assert_eq!(
                option(&test, &target, "-w", "pane-border-status"),
                "off",
                "the border is off within two seconds"
            );
            let height = |format: &str| {
                test.server
                    .run(&["display-message", "-p", "-t", &target, format])
                    .unwrap()
            };
            assert_eq!(height("#{pane_height}"), height("#{window_height}"));
        }
        let wait = left.min(Duration::from_millis(100));
        if let Ok(ControlEvent::SubscriptionChanged { name, value }) = events.recv_timeout(wait)
            && name == BORDER_WATCH
            && let Some(line) = border_reset(&value)
        {
            resets += 1;
            client.send(&line).unwrap();
        }
    }
    assert!(checked_at_two_seconds);
    assert_eq!(resets, 1, "one reset, no loop");
}

/// Un process est-il encore là (vivant ou zombie) ? `kill(pid, 0)` ne distingue pas les deux : c'est voulu.
fn process_exists(pid: i32) -> bool {
    // SAFETY : signal 0, aucun signal n'est envoyé, le noyau vérifie seulement l'existence du process.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Attend qu'une condition devienne vraie (5 s au plus).
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

/// S-1 : le fil qui lance tmux bloque tous les signaux (ce que fait un fil de GPUI). Le serveur ne doit pas
/// en hériter : `kill-server` l'arrête, et il récolte le shell d'un pane sorti au lieu d'en faire un zombie.
#[test]
fn a_server_started_from_a_blocked_signal_thread_still_dies_and_reaps_children() {
    let test = disposable("blocked-signals");
    let outcome = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                // SAFETY : on ne touche qu'au masque du fil courant, qui meurt avec ce fil.
                unsafe {
                    let mut all: libc::sigset_t = std::mem::zeroed();
                    libc::sigfillset(&mut all);
                    assert_eq!(
                        libc::pthread_sigmask(libc::SIG_SETMASK, &all, std::ptr::null_mut()),
                        0
                    );
                }
                let created = shell_session(&test);
                let server_pid: i32 = test
                    .server
                    .run(&["display-message", "-p", "#{pid}"])
                    .expect("server pid")
                    .parse()
                    .expect("numeric pid");
                // Un second pane dont le shell sort aussitôt : tmux doit le récolter.
                let short_lived: i32 = test
                    .server
                    .run(&[
                        "split-window",
                        "-d",
                        "-P",
                        "-F",
                        "#{pane_pid}",
                        "-t",
                        &idema_tmux::server::exact(&created.name),
                        "exit 0",
                    ])
                    .expect("short-lived pane")
                    .parse()
                    .expect("numeric pane pid");
                (server_pid, short_lived)
            })
            .join()
            .expect("launcher thread")
    });
    let (server_pid, short_lived) = outcome;
    assert!(
        eventually(|| !process_exists(short_lived)),
        "the exited pane shell {short_lived} stayed a zombie"
    );
    test.server.kill().expect("kill-server");
    assert!(
        eventually(|| !process_exists(server_pid)),
        "the tmux server {server_pid} ignored kill-server"
    );
}

/// Session « Shell » de `columns` × `lines`, sans client attaché : la fenêtre garde cette taille.
fn sized_session(test: &Disposable, columns: u16, lines: u16) -> CreatedSession {
    test.server
        .create_session(&NewSession {
            project: &test.dir,
            display_name: "Shell",
            agent: "Shell",
            columns,
            lines,
            remote_name: None,
        })
        .expect("session created")
}

/// Géométrie des panes de la fenêtre visible, dans l'ordre : (x, y, largeur, hauteur).
fn visible_geometry(test: &Disposable, session: &str) -> Vec<(u16, u16, u16, u16)> {
    test.server
        .session_panes(session)
        .unwrap()
        .iter()
        .filter(|pane| pane.window_active && !pane.stash)
        .map(|pane| pane.pane)
        .map(|pane| {
            let geometry = test
                .server
                .run(&[
                    "display-message",
                    "-p",
                    "-t",
                    &format!("%{}", pane.0),
                    "#{pane_left} #{pane_top} #{pane_width} #{pane_height}",
                ])
                .unwrap();
            let numbers: Vec<u16> = geometry.split(' ').map(|n| n.parse().unwrap()).collect();
            (numbers[0], numbers[1], numbers[2], numbers[3])
        })
        .collect()
}

fn expected_geometry(preset: Preset, columns: u16, lines: u16) -> Vec<(u16, u16, u16, u16)> {
    WindowLayout {
        root: preset_cell(preset, columns, lines).unwrap(),
        active: None,
    }
    .panes()
    .iter()
    .map(|rect| (rect.x, rect.y, rect.width, rect.height))
    .collect()
}

/// Spec §9 : pour chaque preset et plusieurs tailles, un vrai tmux accepte la chaîne et produit la géométrie attendue.
#[test]
fn every_preset_layout_is_accepted_with_the_expected_geometry() {
    let test = disposable("presets");
    for (columns, lines) in [(120, 40), (80, 24), (211, 57)] {
        for preset in Preset::ALL {
            let session = sized_session(&test, columns, lines);
            let target = exact(&session.name);
            for _ in 1..preset.panes() {
                test.server
                    .run(&["split-window", "-d", "-t", &target, "-c", "/tmp"])
                    .unwrap();
                test.server
                    .run(&["select-layout", "-t", &target, "tiled"])
                    .unwrap();
            }
            let layout = preset_layout(preset, columns, lines).unwrap();
            test.server
                .run(&["select-layout", "-t", &target, &layout])
                .unwrap_or_else(|error| panic!("{} at {columns}x{lines}: {error}", preset.id()));
            assert_eq!(
                visible_geometry(&test, &session.name),
                expected_geometry(preset, columns, lines),
                "{} at {columns}x{lines}",
                preset.id()
            );
            test.server.kill_session(&session.name).unwrap();
        }
    }
}

fn pane_pid(test: &Disposable, pane: PaneId) -> i32 {
    test.server
        .run(&[
            "display-message",
            "-p",
            "-t",
            &format!("%{}", pane.0),
            "#{pane_pid}",
        ])
        .unwrap()
        .parse()
        .unwrap()
}

/// Valeur d'une option utilisateur : `scope` vaut `-p` (pane), `-w` (fenêtre) ou `""` (session).
fn option(test: &Disposable, target: &str, scope: &str, name: &str) -> String {
    let mut args = vec!["show-options"];
    if !scope.is_empty() {
        args.push(scope);
    }
    args.extend(["-t", target, "-v", name]);
    test.server.run(&args).unwrap_or_default()
}

/// AC-41 : un shell créé par une opération qui échoue ensuite reste en vie ; il porte quand même son agent.
#[test]
fn a_shell_created_by_a_failing_operation_still_gets_its_agent() {
    let test = disposable("failing-op");
    let session = shell_session(&test);
    let target = exact(&session.name);
    let pane = format!("%{}", session.pane.0);
    let result = test.server.run_creating_shells(
        &[
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &pane,
            ";",
            "select-layout",
            "-t",
            &target,
            "bogus-layout",
        ],
        "Shell",
    );
    assert!(result.is_err(), "the layout name is unknown to tmux");
    let listed = test
        .server
        .run(&[
            "list-panes",
            "-t",
            &target,
            "-F",
            "#{pane_id} #{@idema_agent}",
        ])
        .unwrap();
    let panes: Vec<(&str, &str)> = listed
        .lines()
        .map(|line| line.split_once(' ').unwrap_or((line, "")))
        .collect();
    assert_eq!(panes.len(), 2, "the split happened: {listed}");
    let created = panes
        .iter()
        .find(|(id, _)| *id != pane)
        .expect("a new pane");
    assert_eq!(
        created.1, "Shell",
        "the new pane carries its agent: {listed}"
    );
}

/// Spec §4.4 et AC-17 : réduire cache les derniers panes dans une fenêtre marquée, avec leur process ; agrandir
/// rejoint les cachés du plus récent au plus ancien, puis crée des shells dans le projet.
#[test]
fn presets_hide_and_rejoin_panes_with_their_processes() {
    let test = disposable("hide-rejoin");
    let session = sized_session(&test, 120, 40);
    let project = test.dir.to_string_lossy().into_owned();
    let created = test
        .server
        .apply_preset(
            &session.name,
            &project,
            Preset::Grid2x2,
            Some(session.pane),
            "Shell",
        )
        .unwrap();
    assert_eq!(created.len(), 3);
    assert_eq!(
        visible_geometry(&test, &session.name),
        expected_geometry(Preset::Grid2x2, 120, 40)
    );
    for pane in &created {
        assert_eq!(
            option(&test, &format!("%{}", pane.0), "-p", "@idema_agent"),
            "Shell"
        );
    }
    let order: Vec<PaneId> = test
        .server
        .session_panes(&session.name)
        .unwrap()
        .iter()
        .map(|pane| pane.pane)
        .collect();
    assert_eq!(order[0], session.pane);
    let pids: Vec<i32> = order.iter().map(|pane| pane_pid(&test, *pane)).collect();

    // Retour à un pane : les trois derniers passent dans la fenêtre cachée, vivants.
    test.server
        .apply_preset(
            &session.name,
            &project,
            Preset::Single,
            Some(session.pane),
            "Shell",
        )
        .unwrap();
    let panes = test.server.session_panes(&session.name).unwrap();
    let hidden: Vec<PaneId> = panes.iter().filter(|p| p.stash).map(|p| p.pane).collect();
    assert_eq!(hidden.len(), 3);
    assert!(
        panes
            .iter()
            .filter(|p| !p.stash)
            .all(|p| p.pane == session.pane)
    );
    let stash_window = panes.iter().find(|p| p.stash).unwrap().window;
    assert_eq!(
        option(&test, &format!("@{}", stash_window.0), "-w", "@idema_stash"),
        "1"
    );
    for (pane, pid) in order.iter().zip(&pids) {
        assert_eq!(
            pane_pid(&test, *pane),
            *pid,
            "pane %{} keeps its process",
            pane.0
        );
    }
    assert_eq!(
        option(&test, &exact(&session.name), "", "@idema_layout"),
        "single"
    );

    // Trois panes : les deux derniers cachés reviennent, dans l'ordre d'avant.
    test.server
        .apply_preset(
            &session.name,
            &project,
            Preset::Columns3,
            Some(session.pane),
            "Shell",
        )
        .unwrap();
    let visible: Vec<PaneId> = test
        .server
        .session_panes(&session.name)
        .unwrap()
        .iter()
        .filter(|p| !p.stash)
        .map(|p| p.pane)
        .collect();
    assert_eq!(visible, order[..3].to_vec());
    assert_eq!(
        option(&test, &format!("%{}", order[1].0), "-p", "@idema_hidden"),
        ""
    );

    // Cinq panes : le dernier caché revient et un shell neuf s'ajoute ; la fenêtre cachée, vide, disparaît.
    let created = test
        .server
        .apply_preset(
            &session.name,
            &project,
            Preset::ThreeTwo,
            Some(session.pane),
            "Shell",
        )
        .unwrap();
    assert_eq!(created.len(), 1);
    let panes = test.server.session_panes(&session.name).unwrap();
    assert!(panes.iter().all(|p| !p.stash));
    assert_eq!(panes.len(), 5);
    assert_eq!(
        visible_geometry(&test, &session.name),
        expected_geometry(Preset::ThreeTwo, 120, 40)
    );
    let new_shell = test
        .server
        .run(&[
            "display-message",
            "-p",
            "-t",
            &format!("%{}", created[0].0),
            "#{pane_current_path}",
        ])
        .unwrap();
    assert_eq!(
        std::fs::canonicalize(new_shell).unwrap(),
        std::fs::canonicalize(&test.dir).unwrap()
    );
}

/// Le pane qui a le focus n'est jamais caché, même s'il est le dernier.
#[test]
fn the_focused_pane_is_never_hidden() {
    let test = disposable("focus-kept");
    let session = sized_session(&test, 120, 40);
    let project = test.dir.to_string_lossy().into_owned();
    let created = test
        .server
        .apply_preset(
            &session.name,
            &project,
            Preset::Columns3,
            Some(session.pane),
            "Shell",
        )
        .unwrap();
    let last = *created.iter().max_by_key(|pane| pane.0).unwrap();
    let order: Vec<PaneId> = test
        .server
        .session_panes(&session.name)
        .unwrap()
        .iter()
        .map(|p| p.pane)
        .collect();
    let focused = *order.last().unwrap();
    test.server
        .apply_preset(
            &session.name,
            &project,
            Preset::Single,
            Some(focused),
            "Shell",
        )
        .unwrap();
    let visible: Vec<PaneId> = test
        .server
        .session_panes(&session.name)
        .unwrap()
        .iter()
        .filter(|p| !p.stash)
        .map(|p| p.pane)
        .collect();
    assert_eq!(visible, vec![focused], "created {last:?}");
}

/// Spec §4.4 : la fenêtre visible meurt, tmux montre la cachée ; Idema en sort le pane caché le plus récent.
#[test]
fn restore_brings_back_a_hidden_pane_when_the_visible_window_dies() {
    let test = disposable("restore");
    let session = sized_session(&test, 120, 40);
    let project = test.dir.to_string_lossy().into_owned();
    test.server
        .apply_preset(
            &session.name,
            &project,
            Preset::Columns3,
            Some(session.pane),
            "Shell",
        )
        .unwrap();
    test.server
        .apply_preset(
            &session.name,
            &project,
            Preset::Single,
            Some(session.pane),
            "Shell",
        )
        .unwrap();
    let before = test.server.session_panes(&session.name).unwrap();
    let latest = before
        .iter()
        .filter(|p| p.stash)
        .max_by_key(|p| p.hidden_at)
        .unwrap()
        .pane;
    test.server
        .run(&["kill-pane", "-t", &format!("%{}", session.pane.0)])
        .unwrap();
    assert!(test.server.restore_visible(&session.name).unwrap());
    let after = test.server.session_panes(&session.name).unwrap();
    let visible: Vec<PaneId> = after
        .iter()
        .filter(|p| p.window_active && !p.stash)
        .map(|p| p.pane)
        .collect();
    assert_eq!(visible, vec![latest]);
    assert_eq!(after.iter().filter(|p| p.stash).count(), 1);
    assert!(!test.server.restore_visible(&session.name).unwrap());
}

/// Noms des fenêtres d'une session (`list-windows`), pour vérifier où `break-pane` crée la sienne.
fn window_names(test: &Disposable, session: &str) -> Vec<String> {
    test.server
        .run(&[
            "list-windows",
            "-t",
            &exact(session),
            "-F",
            "#{window_name}",
        ])
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Hors client, `break-pane` sans cible créerait sa fenêtre dans la session la plus récemment active : une autre
/// session (créée en dernier) ne doit ni recevoir la fenêtre cachée ni celle de la restauration.
#[test]
fn breaking_a_pane_out_stays_in_its_session_when_another_one_is_more_recent() {
    let test = disposable("break-target");
    let first = sized_session(&test, 120, 40);
    // Créée en dernier : c'est la session que tmux choisirait sans cible.
    let other = sized_session(&test, 120, 40);
    assert_ne!(first.name, other.name);
    let other_before = window_names(&test, &other.name);
    let project = test.dir.to_string_lossy().into_owned();
    for preset in [Preset::Columns3, Preset::Single] {
        test.server
            .apply_preset(&first.name, &project, preset, Some(first.pane), "Shell")
            .unwrap();
    }
    let panes = test.server.session_panes(&first.name).unwrap();
    assert_eq!(panes.iter().filter(|p| p.stash).count(), 2);
    assert!(window_names(&test, &first.name).contains(&"idema-stash".to_owned()));
    assert_eq!(window_names(&test, &other.name), other_before);

    test.server
        .run(&["kill-pane", "-t", &format!("%{}", first.pane.0)])
        .unwrap();
    assert!(test.server.restore_visible(&first.name).unwrap());
    let after = test.server.session_panes(&first.name).unwrap();
    assert!(after.iter().any(|p| p.window_active && !p.stash));
    assert_eq!(after.iter().filter(|p| p.stash).count(), 1);
    assert_eq!(window_names(&test, &other.name), other_before);
}

/// Le client de contrôle demande les layouts en JSON (`new-layouts`) : la relecture des fenêtres et les
/// `%layout-change` qui suivent se lisent avec `SessionWindows`.
#[test]
fn the_control_client_reads_window_layouts_in_json() {
    let test = disposable("windows-json");
    let session = shell_session(&test);
    let (mut client, events) = attach(&test, &session.name);
    wait_attached(&mut client, &events);
    let mut query = WindowsQuery::new("it");
    client.send(&query.command_line()).unwrap();
    let mut windows = SessionWindows::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut listed = None;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(ControlEvent::Reply { ok, lines, .. }) = events.recv_timeout(left)
            && let QueryStep::Done(states) = query.advance(ok, &lines)
        {
            listed = Some(states);
            break;
        }
    }
    windows.replace(listed.expect("the windows query answered"));
    assert_eq!(
        windows
            .visible_panes()
            .iter()
            .map(|r| r.pane)
            .collect::<Vec<_>>(),
        vec![session.pane]
    );
    test.server
        .run(&[
            "split-window",
            "-d",
            "-t",
            &exact(&session.name),
            "-c",
            "/tmp",
        ])
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(event) = events.recv_timeout(left) {
            windows.on_event(&event);
            if windows.visible_panes().len() == 2 {
                break;
            }
        }
    }
    assert_eq!(windows.visible_panes().len(), 2);
    assert_eq!(windows.active_pane(), Some(session.pane));
}

/// tmux envoie `%session-window-changed` de toutes les sessions à chaque client de contrôle : le client de A reçoit
/// celui de B, et `SessionWindows` ne doit pas en faire la fenêtre courante de A (écran vide le temps d'une
/// relecture, I-1 de la revue finale du jalon 3).
#[test]
fn a_window_change_in_another_session_does_not_touch_this_sessions_windows() {
    let test = disposable("foreign-window");
    let first = shell_session(&test);
    let second = shell_session(&test);
    // Pas de `wait_attached` : il jetterait le `%session-changed` de l'attache, que la vue lit avant tout le reste.
    let (mut client, events) = attach(&test, &first.name);
    let mut query = WindowsQuery::new("foreign");
    client.send(&query.command_line()).unwrap();
    let mut windows = SessionWindows::new();
    let mut loaded = false;
    let mut own_session = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !loaded && let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(ControlEvent::Reply { ok, lines, .. }) => {
                if let QueryStep::Done(states) = query.advance(ok, &lines) {
                    windows.replace(states);
                    loaded = true;
                }
            }
            Ok(event) => {
                if let ControlEvent::SessionChanged { session } = &event {
                    own_session = Some(*session);
                }
                windows.on_event(&event);
            }
            Err(_) => break,
        }
    }
    assert!(loaded, "the windows query answered");
    assert!(own_session.is_some(), "tmux announces the attached session");
    let visible_before = windows.visible_panes();
    assert_eq!(visible_before.len(), 1);

    let target = exact(&second.name);
    let second_id: u32 = test
        .server
        .run(&["display-message", "-p", "-t", &target, "#{session_id}"])
        .unwrap()
        .trim_start_matches('$')
        .parse()
        .unwrap();
    assert_ne!(Some(SessionId(second_id)), own_session);
    test.server
        .run(&["new-window", "-d", "-t", &target])
        .unwrap();
    test.server
        .run(&["select-window", "-t", &format!("{}1", target)])
        .unwrap();

    let mut seen_foreign = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !seen_foreign && let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = events.recv_timeout(left) else {
            break;
        };
        if let ControlEvent::SessionWindowChanged { session, .. } = &event
            && *session == SessionId(second_id)
        {
            seen_foreign = true;
            assert!(!windows.on_event(&event), "the foreign change is ignored");
        }
    }
    assert!(
        seen_foreign,
        "the client of the first session received the second session's window change"
    );
    assert_eq!(windows.visible_panes(), visible_before);
    assert!(!windows.needs_query(), "no unknown window was created");
}

/// `resize-pane -Z` zoome le pane puis le rétablit : `#{window_zoomed_flag}` vaut `1` puis `0`.
#[test]
fn toggle_zoom_zooms_and_unzooms() {
    let test = disposable("zoom");
    let session = shell_session(&test);
    let target = exact(&session.name);
    test.server
        .run(&["split-window", "-d", "-t", &target, "-c", "/tmp"])
        .unwrap();
    let flag = || {
        test.server
            .run(&[
                "display-message",
                "-p",
                "-t",
                &target,
                "#{window_zoomed_flag}",
            ])
            .unwrap()
    };
    assert_eq!(flag(), "0");
    test.server.toggle_zoom(session.pane).unwrap();
    assert_eq!(flag(), "1");
    test.server.toggle_zoom(session.pane).unwrap();
    assert_eq!(flag(), "0");
}

/// La rafale réelle de tmux 3.8 pour un redimensionnement d'une fenêtre zoomée (un layout non zoomé, puis deux
/// zoomés) ne lève jamais le zoom côté Idema, alors qu'un vrai dézoom le lève une fois l'attente écoulée.
#[test]
fn a_resize_of_a_zoomed_window_never_lifts_the_zoom_but_a_real_unzoom_does() {
    use idema_tmux::windows::UNZOOM_SETTLE;

    let test = disposable("zoom-burst");
    let session = shell_session(&test);
    let target = exact(&session.name);
    test.server
        .run(&["split-window", "-d", "-t", &target, "-c", "/tmp"])
        .unwrap();
    test.server.toggle_zoom(session.pane).unwrap();
    let (mut client, events) = attach(&test, &session.name);
    wait_attached(&mut client, &events);

    let mut query = WindowsQuery::new("burst");
    client.send(&query.command_line()).unwrap();
    let mut windows = SessionWindows::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut listed = None;
    while listed.is_none()
        && let Some(left) = deadline.checked_duration_since(Instant::now())
    {
        if let Ok(ControlEvent::Reply { ok, lines, .. }) = events.recv_timeout(left)
            && let QueryStep::Done(states) = query.advance(ok, &lines)
        {
            listed = Some(states);
        }
    }
    windows.replace(listed.expect("the windows query answered"));
    assert_eq!(windows.zoomed_pane(), Some(session.pane));

    // Chaque `%layout-change` du redimensionnement passe par le chemin du flux : le zoom tient à chaque pas.
    client.send("refresh-client -C 100x30").unwrap();
    client.send("display-message -p idema-burst-end").unwrap();
    let mut layout_changes = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    'burst: while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = events.recv_timeout(left) else {
            break;
        };
        if matches!(event, ControlEvent::LayoutChange { .. }) {
            layout_changes += 1;
        }
        windows.on_notification(&event, Instant::now());
        assert_eq!(
            windows.zoomed_pane(),
            Some(session.pane),
            "the zoom held after {layout_changes} layout changes"
        );
        assert_eq!(windows.displayed_panes().len(), 1, "one pane displayed");
        if let ControlEvent::Reply { lines, .. } = &event
            && lines.iter().any(|line| line == b"idema-burst-end")
        {
            break 'burst;
        }
    }
    assert!(layout_changes >= 2, "tmux sent the burst: {layout_changes}");
    assert!(
        !windows.has_held_layout(),
        "nothing left held after the burst"
    );

    // Un vrai dézoom n'émet qu'un layout non zoomé : il attend, puis s'applique.
    test.server.toggle_zoom(session.pane).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while windows.zoomed_pane().is_some()
        && let Some(left) = deadline.checked_duration_since(Instant::now())
    {
        if let Ok(event) = events.recv_timeout(left.min(UNZOOM_SETTLE)) {
            windows.on_notification(&event, Instant::now());
        }
        windows.release_due(Instant::now());
    }
    assert_eq!(windows.zoomed_pane(), None, "the real unzoom got through");
}

#[test]
fn a_session_in_a_missing_folder_is_refused_before_tmux() {
    let test = disposable("gone");
    let gone = test.dir.join("gone");
    let error = test
        .server
        .create_session(&NewSession {
            project: &gone,
            display_name: "Shell",
            agent: "Shell",
            columns: 80,
            lines: 24,
            remote_name: None,
        })
        .unwrap_err();
    assert!(matches!(&error, ServerError::ProjectGone(path) if *path == gone));
    assert_eq!(
        error.to_string(),
        format!("no such directory: {}", gone.display())
    );
    // tmux 3.8 aurait ouvert le shell dans `$HOME` (sonde du 16/10) : aucune session ne doit exister.
    assert!(test.server.sessions().unwrap().is_empty());
}

/// Lance une session serveur dans le dossier du test (projet et cwd identiques).
fn server_session(
    test: &Disposable,
    entry: &str,
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> CreatedSession {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    test.server
        .create_server_session(&NewServerSession {
            project: &test.dir,
            entry,
            cwd: &test.dir,
            program,
            args: &args,
            env: &env,
        })
        .expect("server session created")
}

/// Tout ce que le pane a affiché, historique compris : `remain-on-exit` ajoute une ligne qui fait remonter l'écran.
fn pane_text(test: &Disposable, session: &str) -> String {
    test.server
        .run(&["capture-pane", "-p", "-S", "-", "-t", &exact(session)])
        .expect("capture-pane")
}

#[test]
fn a_server_session_is_not_a_sidebar_session() {
    let test = disposable("srv-hidden");
    shell_session(&test);
    let session = server_session(&test, "web", "sleep", &["30"], &[]);
    assert!(session.name.starts_with("idema-srv-"));
    let listed = test.server.sessions().unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed.iter().all(|listed| listed.name != session.name));
    let servers = test.server.server_sessions().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, session.name);
    assert_eq!(servers[0].project, test.dir);
    assert_eq!(servers[0].entry, "web");
    assert_eq!(servers[0].pane, session.pane);
    assert!(!servers[0].dead);
    assert!(servers[0].pane_pid > 0);
    // Ni `@idema_project` ni autre option de la barre : la barre ne la reconnaît pas d'elle-même.
    let options = test
        .server
        .run(&["show-options", "-t", &exact(&session.name)])
        .unwrap();
    assert!(!options.contains("@idema_project"), "{options}");
}

#[test]
fn a_server_session_runs_its_arguments_verbatim() {
    let test = disposable("srv-verbatim");
    // Projet dans un dossier à espace, arguments avec espace, apostrophe, `$`, guillemets, `#{…}` et `;` final.
    let project = test.dir.join("dossier à espace");
    std::fs::create_dir_all(&project).unwrap();
    let args: Vec<String> = [
        "%s|%s|%s|%s|%s\n",
        "a b",
        "it's $HOME",
        "say \"hi\" `id`",
        "#{pane_id}",
        "fin;",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect();
    let session = test
        .server
        .create_server_session(&NewServerSession {
            project: &project,
            entry: "web",
            cwd: &project,
            program: "printf",
            args: &args,
            env: &[],
        })
        .unwrap();
    assert!(eventually(|| pane_text(&test, &session.name)
        .contains("a b|it's $HOME|say \"hi\" `id`|#{pane_id}|fin;")));
    let servers = test.server.server_sessions().unwrap();
    assert_eq!(servers[0].project, project);
}

#[test]
fn a_server_session_starts_in_its_cwd_even_with_a_hash_in_it() {
    let test = disposable("srv-cwd");
    let cwd = test.dir.join("a#b c");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = test
        .server
        .create_server_session(&NewServerSession {
            project: &test.dir,
            entry: "web",
            cwd: &cwd,
            program: "pwd",
            args: &[],
            env: &[],
        })
        .unwrap();
    // `pwd` rend le chemin résolu (`/private/var` pour `/var` sous macOS) : on compare le dernier composant.
    assert!(eventually(
        || pane_text(&test, &session.name).contains("a#b c")
    ));
}

#[test]
fn a_server_command_that_exits_keeps_its_output() {
    let test = disposable("srv-exit");
    // Vingt essais : la course (commande qui se termine avant que `remain-on-exit` soit posé) ne doit jamais
    // coûter ni la session ni sa sortie, même avec `remain-on-exit off` au niveau du serveur tmux.
    for attempt in 0..20 {
        let session = server_session(
            &test,
            &format!("run-{attempt}"),
            "sh",
            &["-c", "echo boom; exit 3"],
            &[],
        );
        assert!(
            eventually(|| test
                .server
                .server_sessions()
                .unwrap()
                .iter()
                .any(|listed| listed.name == session.name && listed.dead)),
            "attempt {attempt}: the session is gone or not dead"
        );
        assert!(
            pane_text(&test, &session.name).contains("boom"),
            "attempt {attempt}: the output is lost"
        );
    }
    // Une session de la barre garde `remain-on-exit off` : seul le pane serveur est concerné.
    assert_eq!(
        test.server
            .run(&["show-options", "-gv", "remain-on-exit"])
            .unwrap(),
        "off"
    );
}

#[test]
fn a_dead_server_session_is_listed_without_a_pid_and_missing_from_pane_pids() {
    let test = disposable("srv-dead");
    let session = server_session(&test, "web", "false", &[], &[]);
    assert!(eventually(|| test
        .server
        .server_sessions()
        .unwrap()
        .iter()
        .any(|listed| listed.name == session.name && listed.dead)));
    let listed = test.server.server_sessions().unwrap();
    assert_eq!(listed[0].pane_pid, 0);
    assert!(
        test.server
            .pane_pids()
            .unwrap()
            .iter()
            .all(|(name, _, _)| *name != session.name)
    );
}

#[test]
fn a_missing_program_leaves_an_error_in_the_pane() {
    let test = disposable("srv-missing");
    let session = server_session(&test, "web", "idema-no-such-binary", &[], &[]);
    assert!(eventually(
        || pane_text(&test, &session.name).contains("idema-no-such-binary")
    ));
    assert!(
        test.server
            .server_sessions()
            .unwrap()
            .iter()
            .any(|listed| listed.name == session.name && listed.dead)
    );
}

#[test]
fn the_server_session_gets_env_and_port() {
    let test = disposable("srv-env");
    let session = server_session(
        &test,
        "web",
        "sh",
        &["-c", "echo \"$PORT $FOO $TRICKY\""],
        &[
            ("PORT", "8899"),
            ("FOO", "bar"),
            ("TRICKY", "a #{x} 'b' c;"),
        ],
    );
    assert!(eventually(
        || pane_text(&test, &session.name).contains("8899 bar a #{x} 'b' c;")
    ));
}

#[test]
fn a_server_session_goes_through_the_login_shell_of_the_user() {
    // Un vrai zsh de connexion avec un HOME jetable : comme les sessions de la barre (shells interactifs de
    // connexion), il lit le `.zshrc`, où l'utilisateur pose son `PATH` (bun, node). Sans `-i`, le `.zshrc` est
    // sauté et la variable manque : le test est rouge avec `-l -c`, vert avec `-l -i -c`.
    let path = std::env::var("PATH").ok();
    let binary = locate_tmux(None, path.as_deref()).expect("tmux installed");
    let dir = std::env::temp_dir().join(format!("idema-test-{}-srv-zsh", std::process::id()));
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".zshrc"), "export IDEMA_FROM_ZSHRC=found\n").unwrap();
    let env = vec![
        ("HOME".to_string(), home.to_string_lossy().to_string()),
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("SHELL".to_string(), "/bin/zsh".to_string()),
        ("LANG".to_string(), "en_US.UTF-8".to_string()),
    ];
    let socket = dir.join("sock").to_string_lossy().into_owned();
    let server = TmuxServer::new(binary, socket, dir.join("tmux.conf"), env);
    server.write_config().unwrap();
    let test = Disposable { server, dir };
    let session = server_session(
        &test,
        "web",
        "sh",
        &[
            "-c",
            "echo \"[$IDEMA_FROM_ZSHRC] [$0] [$1]\"",
            "zero",
            "un deux",
        ],
        &[],
    );
    assert!(eventually(
        || pane_text(&test, &session.name).contains("[found] [zero] [un deux]")
    ));
}

#[test]
fn pane_pids_cover_sessions_and_server_sessions() {
    let test = disposable("srv-pids");
    let sidebar = shell_session(&test);
    let server = server_session(&test, "web", "sleep", &["30"], &[]);
    let pids = test.server.pane_pids().unwrap();
    assert_eq!(pids.len(), 2);
    for (name, pane) in [(&sidebar.name, sidebar.pane), (&server.name, server.pane)] {
        let (_, _, pid) = pids
            .iter()
            .find(|(listed, _, _)| listed == name)
            .unwrap_or_else(|| panic!("{name} missing from {pids:?}"));
        assert!(*pid > 0 && process_exists(*pid), "{name} {pane:?}");
    }
    // Le PID d'une session serveur est celui que `server_sessions` donne.
    let listed = test.server.server_sessions().unwrap();
    assert_eq!(
        pids.iter()
            .find(|(name, _, _)| *name == server.name)
            .map(|(_, _, pid)| *pid),
        Some(listed[0].pane_pid)
    );
}

#[test]
fn a_server_session_outlives_the_sidebar_sessions_and_their_clients() {
    // AC-93 : fermer IdemA (client de contrôle détaché, sessions de la barre fermées) ne touche pas un serveur.
    let test = disposable("srv-survives");
    let sidebar = shell_session(&test);
    let server = server_session(&test, "web", "sleep", &["30"], &[]);
    let (mut client, events) = attach(&test, &sidebar.name);
    wait_attached(&mut client, &events);
    drop(client);
    test.server.kill_session(&sidebar.name).unwrap();
    assert!(test.server.sessions().unwrap().is_empty());
    let servers = test.server.server_sessions().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, server.name);
    assert!(!servers[0].dead && process_exists(servers[0].pane_pid));
}

/// Un pane à restaurer, visible (`hidden_at: None`) ou caché.
fn restore_pane<'a>(
    cwd: &'a std::path::Path,
    agent: &'a str,
    hidden_at: Option<u128>,
    active: bool,
) -> RestorePane<'a> {
    RestorePane {
        cwd,
        agent,
        hidden_at,
        active,
        resume: None,
    }
}

/// Une demande de restauration au nom et au préfixe d'identifiants fixes.
fn restore_request<'a>(
    project: &'a std::path::Path,
    layout: &'a str,
    panes: &'a [RestorePane<'a>],
) -> RestoreSession<'a> {
    RestoreSession {
        name: "idema-0000beef",
        project,
        display_name: "Claude 2",
        created: 1234,
        layout,
        remote_name: None,
        columns: 120,
        lines: 40,
        panes,
    }
}

#[test]
fn restore_session_rebuilds_names_options_panes_and_stash() {
    let test = disposable("restore-full");
    let sub = test.dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let panes = [
        restore_pane(&test.dir, "Claude", None, false),
        restore_pane(&sub, "Shell seul", None, true),
        restore_pane(&test.dir, "Shell seul", None, false),
        restore_pane(&test.dir, "Codex", Some(777), false),
    ];
    let ids = test
        .server
        .restore_session(&restore_request(&test.dir, "main-left", &panes))
        .expect("session restored");
    assert_eq!(ids.len(), 4);

    let sessions = test.server.sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    assert_eq!(session.name, "idema-0000beef");
    assert_eq!(session.project, test.dir.to_string_lossy());
    assert_eq!(session.created, 1234);
    assert_eq!(session.layout, "main-left");
    assert_eq!(session.display_name, "Claude 2");
    assert!(!session.ideation);

    let session_panes = test.server.session_panes("idema-0000beef").unwrap();
    let visible: Vec<_> = session_panes.iter().filter(|pane| !pane.stash).collect();
    assert_eq!(visible.len(), 3);
    assert_eq!(
        visible.iter().map(|pane| pane.pane).collect::<Vec<_>>(),
        ids[..3]
    );
    let hidden = session_panes
        .iter()
        .find(|pane| pane.pane == ids[3])
        .unwrap();
    assert!(hidden.stash);
    assert_eq!(hidden.hidden_at, 777);

    // Le preset s'est posé sur les trois panes visibles : le premier occupe toute la hauteur, à gauche.
    let geometry = visible_geometry(&test, "idema-0000beef");
    assert_eq!(geometry.len(), 3);
    assert_eq!(geometry[0].0, 0);
    assert_eq!(geometry[0].3, 40);
    assert!(geometry[1].0 > 0 && geometry[2].0 > 0);

    let (_, captured) = test.server.capture().unwrap();
    for (id, pane) in ids.iter().zip(&panes) {
        let found = captured.iter().find(|c| c.pane == *id).unwrap();
        assert_eq!(found.agent, pane.agent);
    }
    let by_id = |id: PaneId| captured.iter().find(|c| c.pane == id).unwrap();
    assert_eq!(
        std::fs::canonicalize(&by_id(ids[1]).cwd).unwrap(),
        std::fs::canonicalize(&sub).unwrap()
    );
    assert!(by_id(ids[1]).pane_active);
    assert!(!by_id(ids[0]).pane_active);
    assert!(by_id(ids[3]).stash);
}

#[test]
fn restore_session_keeps_the_order_and_stamps_of_hidden_panes() {
    let test = disposable("restore-hidden");
    let panes = [
        restore_pane(&test.dir, "Codex", Some(900), false),
        restore_pane(&test.dir, "Claude", None, true),
        restore_pane(&test.dir, "Shell seul", Some(500), false),
        restore_pane(&test.dir, "Shell seul", None, false),
        restore_pane(&test.dir, "Gemini", Some(100), false),
    ];
    let ids = test
        .server
        .restore_session(&restore_request(&test.dir, "columns-2", &panes))
        .expect("session restored");
    assert_eq!(ids.len(), 5);
    let session_panes = test.server.session_panes("idema-0000beef").unwrap();
    // La fenêtre cachée garde l'ordre d'origine, chaque pane son instant sauvé.
    let stash: Vec<_> = session_panes.iter().filter(|pane| pane.stash).collect();
    assert_eq!(
        stash.iter().map(|pane| pane.pane).collect::<Vec<_>>(),
        [ids[0], ids[2], ids[4]]
    );
    assert_eq!(
        stash.iter().map(|pane| pane.hidden_at).collect::<Vec<_>>(),
        [900, 500, 100]
    );
    // Les visibles gardent l'ordre aussi, et la fenêtre courante reste la visible.
    let visible: Vec<_> = session_panes.iter().filter(|pane| !pane.stash).collect();
    assert_eq!(
        visible.iter().map(|pane| pane.pane).collect::<Vec<_>>(),
        [ids[1], ids[3]]
    );
    assert!(visible.iter().all(|pane| pane.window_active));
    assert!(visible[0].pane_active);
}

#[test]
fn restore_session_keeps_hash_and_spaces_in_paths() {
    let test = disposable("restore-path");
    let project = test.dir.join("a#b c");
    std::fs::create_dir_all(&project).unwrap();
    let panes = [restore_pane(&project, "Shell seul", None, true)];
    let ids = test
        .server
        .restore_session(&restore_request(&project, "single", &panes))
        .expect("session restored");
    let (_, captured) = test.server.capture().unwrap();
    let found = captured.iter().find(|c| c.pane == ids[0]).unwrap();
    assert_eq!(
        std::fs::canonicalize(&found.cwd).unwrap(),
        std::fs::canonicalize(&project).unwrap()
    );
    assert!(found.cwd.to_string_lossy().ends_with("a#b c"));
}

#[test]
fn restore_session_survives_a_window_too_small_for_the_preset() {
    let test = disposable("restore-small");
    let panes: Vec<RestorePane> = (0..6)
        .map(|index| restore_pane(&test.dir, "Shell seul", None, index == 0))
        .collect();
    let request = RestoreSession {
        columns: 20,
        lines: 5,
        ..restore_request(&test.dir, "grid-3x2", &panes)
    };
    let ids = test.server.restore_session(&request).expect("restored");
    assert_eq!(ids.len(), 6);
    assert_eq!(test.server.sessions().unwrap()[0].layout, "grid-3x2");
    assert_eq!(
        test.server.session_panes("idema-0000beef").unwrap().len(),
        6
    );
}

#[test]
fn restore_session_of_a_missing_project_is_project_gone() {
    let test = disposable("restore-gone");
    let missing = test.dir.join("absent");
    let panes = [restore_pane(&missing, "Shell seul", None, true)];
    let error = test
        .server
        .restore_session(&restore_request(&missing, "single", &panes))
        .unwrap_err();
    assert!(matches!(error, ServerError::ProjectGone(_)));
    assert!(test.server.sessions().unwrap().is_empty());
}

#[test]
fn restore_session_without_panes_is_an_error() {
    let test = disposable("restore-empty");
    let error = test
        .server
        .restore_session(&restore_request(&test.dir, "single", &[]));
    assert!(error.is_err());
    assert!(test.server.sessions().unwrap().is_empty());
}

#[test]
fn restore_session_marks_an_ideation_session() {
    let test = disposable("restore-ideation");
    let panes = [restore_pane(&test.dir, "Fable", None, true)];
    let request = RestoreSession {
        remote_name: Some("erom-ideation-3"),
        ..restore_request(&test.dir, "single", &panes)
    };
    test.server.restore_session(&request).expect("restored");
    let sessions = test.server.sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].ideation);
    assert_eq!(sessions[0].remote_name, "erom-ideation-3");
}

#[test]
fn a_failed_restore_leaves_no_session() {
    let test = disposable("restore-fails");
    // tmux ouvre un shell dans `$HOME` pour un dossier absent ou un fichier (sonde du 28/10) : seul un octet nul,
    // refusé au lancement de la commande, fait échouer de façon sûre une création de pane après `new-session`.
    let poisoned = PathBuf::from("bad\0folder");
    let panes = [
        restore_pane(&test.dir, "Claude", None, true),
        restore_pane(&poisoned, "Shell seul", None, false),
    ];
    let error = test
        .server
        .restore_session(&restore_request(&test.dir, "columns-2", &panes));
    assert!(error.is_err());
    assert!(test.server.sessions().unwrap().is_empty());
    assert!(!test.server.has_session("idema-0000beef"));
}

#[test]
fn a_restore_onto_a_taken_name_keeps_the_existing_session() {
    let test = disposable("restore-taken");
    let panes = [restore_pane(&test.dir, "Claude", None, true)];
    let request = restore_request(&test.dir, "single", &panes);
    test.server
        .restore_session(&request)
        .expect("first restore");
    assert!(test.server.restore_session(&request).is_err());
    // L'échec de `new-session` ne doit pas tuer la session qui portait déjà ce nom.
    assert_eq!(test.server.sessions().unwrap().len(), 1);
}

#[test]
fn capture_without_server_is_an_error() {
    let test = disposable("capture-no-server");
    assert!(test.server.capture().is_err());
}

#[test]
fn the_server_outlives_its_last_session_and_capture_sees_it_empty() {
    // `exit-empty off` : sans lui, la dernière session tuée emporte le serveur et la capture échoue au lieu de voir « zéro session ».
    let test = disposable("outlives-last-session");
    let session = shell_session(&test);
    test.server.kill_session(&session.name).unwrap();
    assert!(
        test.server.is_running(),
        "the server died with its last session"
    );
    let (sessions, panes) = test.server.capture().expect("capture of an empty server");
    assert!(sessions.is_empty() && panes.is_empty());
    // Les autres listes de panes lisent aussi ce serveur vide sans erreur.
    assert!(test.server.panes().unwrap().is_empty());
    assert!(test.server.server_sessions().unwrap().is_empty());
    assert!(test.server.pane_pids().unwrap().is_empty());
}
