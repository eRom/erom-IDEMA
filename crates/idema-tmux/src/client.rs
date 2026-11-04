//! Client tmux en mode contrôle : un process `tmux -C attach-session` et un fil de lecture (spec §3.1).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::protocol::{ControlEvent, ControlParser};
use crate::server::{ServerError, TmuxServer, exact};

/// L'entrée d'un client de contrôle, partagée entre les panes de sa session : chacun y écrit ses relectures, ses
/// abonnements et sa saisie. Une ligne part d'un bloc, sous le verrou.
#[derive(Clone)]
pub struct ClientWriter {
    stdin: Arc<Mutex<ChildStdin>>,
}

impl ClientWriter {
    /// Envoie une ligne de commandes, sans `\n`.
    pub fn send(&self, line: &str) -> std::io::Result<()> {
        debug_assert!(!line.contains('\n'), "a command must fit on one line");
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|_| std::io::Error::other("control client stdin lock poisoned"))?;
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()
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
        let stdout = child.stdout.take().expect("control client stdout");
        let reader = std::thread::Builder::new()
            .name(format!("tmux-control-{session}"))
            .spawn(move || read_events(stdout, on_event))?;
        Ok(Self {
            child,
            writer: ClientWriter {
                stdin: Arc::new(Mutex::new(stdin)),
            },
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

    #[test]
    fn read_events_does_not_duplicate_a_real_exit() {
        let mut events = Vec::new();
        read_events(&b"%exit\n"[..], |event| events.push(event));
        assert_eq!(events, vec![ControlEvent::Exit { reason: None }]);
    }
}
