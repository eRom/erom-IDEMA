//! Client tmux en mode contrôle : un process `tmux -C attach-session` et un fil de lecture (spec §3.1).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::protocol::{ControlEvent, ControlParser};
use crate::server::{ServerError, TmuxServer, exact};

/// L'entrée d'un client de contrôle, partagée entre les panes de sa session : chacun y écrit ses relectures, ses
/// abonnements et sa saisie. Une ligne part d'un bloc, sous le verrou.
#[derive(Clone)]
pub struct ClientWriter {
    sink: Sink,
}

/// Où part une ligne : directement dans le tuyau du client (tmux du Mac), ou dans la file d'un fil d'écriture (ssh).
#[derive(Clone)]
enum Sink {
    Direct(Arc<Mutex<ChildStdin>>),
    Queued(Sender<String>),
}

impl ClientWriter {
    /// Écrit dans le tuyau du client, sous verrou : le tmux du Mac le vide toujours.
    fn direct(stdin: ChildStdin) -> Self {
        Self {
            sink: Sink::Direct(Arc::new(Mutex::new(stdin))),
        }
    }

    /// Écrit par un fil dédié. Sur une machine distante, un lien figé ne vide plus le tuyau de ssh : une écriture directe
    /// bloquerait l'appelant (le fil d'interface) dès que le tuyau est plein, 64 Kio, soit un collage de 21 Kio (mesuré le
    /// 08/11 sur un maître ssh arrêté). Le fil prend la file, bloque seul, et s'arrête à la première écriture qui échoue
    /// (ssh mort) ou quand plus personne ne tient d'`Sender`.
    fn queued(mut stdin: ChildStdin, name: String) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<String>();
        std::thread::Builder::new().name(name).spawn(move || {
            while let Ok(line) = receiver.recv() {
                let written = stdin
                    .write_all(line.as_bytes())
                    .and_then(|()| stdin.write_all(b"\n"))
                    .and_then(|()| stdin.flush());
                if written.is_err() {
                    break;
                }
            }
        })?;
        Ok(Self {
            sink: Sink::Queued(sender),
        })
    }

    /// Envoie une ligne de commandes, sans `\n`. Sur une machine, la ligne part dans la file et l'appel ne bloque jamais ;
    /// une erreur dit que le fil d'écriture s'est arrêté, c'est-à-dire que le client est mort.
    pub fn send(&self, line: &str) -> std::io::Result<()> {
        debug_assert!(!line.contains('\n'), "a command must fit on one line");
        match &self.sink {
            Sink::Direct(stdin) => {
                let mut stdin = stdin
                    .lock()
                    .map_err(|_| std::io::Error::other("control client stdin lock poisoned"))?;
                stdin.write_all(line.as_bytes())?;
                stdin.write_all(b"\n")?;
                stdin.flush()
            }
            Sink::Queued(sender) => sender
                .send(line.to_owned())
                .map_err(|_| std::io::Error::other("control client writer stopped")),
        }
    }
}

pub struct ControlClient {
    child: Child,
    writer: ClientWriter,
    reader: Option<JoinHandle<()>>,
}

impl ControlClient {
    /// Attache un client de contrôle à la session. `on_event` est appelé depuis le fil de lecture
    /// pour chaque événement, puis une fois avec `Exit` si le flux se ferme sans `%exit`.
    pub fn spawn(
        server: &TmuxServer,
        session: &str,
        on_event: impl FnMut(ControlEvent) + Send + 'static,
    ) -> Result<Self, ServerError> {
        let target = exact(session);
        let mut child = server
            .command(&[
                "-C",
                "attach-session",
                "-f",
                // `new-layouts` : les layouts arrivent en JSON, avec l'identifiant de chaque pane (spec §3.1).
                "pause-after=10,new-layouts",
                "-t",
                &target,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("control client stdin");
        let writer = if server.is_remote() {
            ClientWriter::queued(stdin, format!("tmux-control-write-{session}"))?
        } else {
            ClientWriter::direct(stdin)
        };
        let stdout = child.stdout.take().expect("control client stdout");
        let reader = std::thread::Builder::new()
            .name(format!("tmux-control-{session}"))
            .spawn(move || read_events(stdout, on_event))?;
        Ok(Self {
            child,
            writer,
            reader: Some(reader),
        })
    }

    /// Envoie une ligne de commandes, sans `\n`.
    pub fn send(&mut self, line: &str) -> std::io::Result<()> {
        self.writer.send(line)
    }

    /// Une entrée de plus sur ce client, pour un pane de la session.
    pub fn writer(&self) -> ClientWriter {
        self.writer.clone()
    }
}

impl Drop for ControlClient {
    /// Détache le client : la session continue dans tmux (AC-1).
    fn drop(&mut self) {
        let _ = self.send("detach-client");
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Filet : un client qui n'a pas quitté en 500 ms est arrêté ; une erreur ici veut dire qu'il est déjà parti.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Découpe le flux en lignes d'octets, les donne au parseur, transmet les événements.
fn read_events(stdout: impl Read, mut on_event: impl FnMut(ControlEvent)) {
    let mut reader = BufReader::new(stdout);
    let mut parser = ControlParser::new();
    let mut line = Vec::new();
    let mut exited = false;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let content = line.strip_suffix(b"\n").unwrap_or(&line);
                if let Some(event) = parser.feed_line(content) {
                    exited |= matches!(event, ControlEvent::Exit { .. });
                    on_event(event);
                }
            }
        }
    }
    if !exited {
        on_event(ControlEvent::Exit {
            reason: Some("client stream closed".into()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PaneId;

    #[test]
    fn read_events_parses_lines_and_reports_a_closed_stream() {
        let stream: &[u8] = b"%output %1 hi\\012\n%begin 1 2 1\nbody\n%end 1 2 1\n";
        let mut events = Vec::new();
        read_events(stream, |event| events.push(event));
        assert_eq!(
            events,
            vec![
                ControlEvent::Output {
                    pane: PaneId(1),
                    data: b"hi\n".to_vec()
                },
                ControlEvent::Reply {
                    number: 2,
                    ok: true,
                    lines: vec![b"body".to_vec()]
                },
                ControlEvent::Exit {
                    reason: Some("client stream closed".into())
                },
            ]
        );
    }

    /// Un client dont le tuyau n'est jamais vidé (ssh derrière un lien figé), figé ici par un `sleep` qui ne lit rien.
    fn undrained_child() -> (Child, ChildStdin) {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .spawn()
            .expect("sleep starts");
        let stdin = child.stdin.take().expect("piped stdin");
        (child, stdin)
    }

    #[test]
    fn a_remote_writer_never_blocks_its_caller_on_a_pipe_nobody_drains() {
        let (mut child, stdin) = undrained_child();
        let writer = ClientWriter::queued(stdin, "test-writer".into()).expect("thread starts");
        let started = std::time::Instant::now();
        // 1 Mio de lignes, seize fois ce que le tuyau tient : une écriture directe resterait bloquée.
        let line = "send-keys -t %1 -H ".to_owned() + &"61 ".repeat(100);
        for _ in 0..3_500 {
            writer.send(&line).expect("the queue accepts the line");
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "sending blocked for {:?}",
            started.elapsed()
        );
        // Le client meurt : le fil d'écriture s'arrête sur l'écriture qui échoue, et l'appelant l'apprend.
        child.kill().expect("sleep killed");
        child.wait().expect("sleep reaped");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while writer.send("x").is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "the writer never reported the dead client"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_remote_writer_keeps_lines_in_order() {
        let mut child = std::process::Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("cat starts");
        let stdin = child.stdin.take().expect("piped stdin");
        let writer = ClientWriter::queued(stdin, "test-writer".into()).expect("thread starts");
        for number in 0..200 {
            writer.send(&format!("line {number}")).unwrap();
        }
        drop(writer);
        let output = child.wait_with_output().expect("cat ends");
        let expected: String = (0..200).map(|number| format!("line {number}\n")).collect();
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    }

    #[test]
    fn read_events_does_not_duplicate_a_real_exit() {
        let mut events = Vec::new();
        read_events(&b"%exit\n"[..], |event| events.push(event));
        assert_eq!(events, vec![ControlEvent::Exit { reason: None }]);
    }
}
