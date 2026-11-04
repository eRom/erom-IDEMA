//! Lecture du flux d'un client tmux en mode contrôle (`tmux -C`).

/// Identifiant d'un pane tmux : `%3` donne `PaneId(3)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub u32);

/// Identifiant d'une fenêtre tmux : `@3` donne `WindowId(3)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(pub u32);

/// Identifiant d'une session tmux : `$3` donne `SessionId(3)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u32);

/// Ce qu'apporte une ligne, ou un bloc de lignes, du flux de contrôle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    /// Réponse complète à une commande : `%begin` … `%end` (ok) ou `%error`.
    Reply {
        number: u64,
        ok: bool,
        lines: Vec<Vec<u8>>,
    },
    /// Octets bruts écrits par le programme du pane (`%output` ou `%extended-output`).
    Output {
        pane: PaneId,
        data: Vec<u8>,
    },
    LayoutChange {
        window: WindowId,
        rest: String,
    },
    Pause {
        pane: PaneId,
    },
    Continue {
        pane: PaneId,
    },
    WindowAdd {
        window: WindowId,
    },
    WindowClose {
        window: WindowId,
    },
    SubscriptionChanged {
        name: String,
        value: String,
    },
    /// `%session-changed $<session> <nom>` : la session à laquelle le client est attaché (envoyé à l'attache).
    SessionChanged {
        session: SessionId,
    },
    /// `%session-window-changed $<session> @<fenêtre>` : la fenêtre courante de la session a changé (la fenêtre
    /// visible s'est fermée et tmux est passé à la fenêtre cachée, par exemple). tmux l'envoie à tous les clients
    /// de contrôle, quelle que soit leur session : `session` dit laquelle est concernée.
    SessionWindowChanged {
        session: SessionId,
        window: WindowId,
    },
    /// `%window-pane-changed @<fenêtre> %<pane>` : le pane actif de la fenêtre a changé.
    WindowPaneChanged {
        window: WindowId,
        pane: PaneId,
    },
    Exit {
        reason: Option<String>,
    },
    /// `%sessions-changed` : une session a été créée ou tuée sur le serveur, par n'importe quel client. tmux l'envoie
    /// sans argument à tous les clients de contrôle, quelle que soit leur session.
    SessionsChanged,
    /// Notification que le squelette n'exploite pas encore, gardée telle quelle.
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Guard {
    Begin,
    End,
    Error,
}

/// Analyseur à état : il regroupe les lignes d'un bloc de réponse.
/// Il travaille sur des octets, car une ligne `%output` peut couper un caractère UTF-8.
#[derive(Debug, Default)]
pub struct ControlParser {
    open_block: Option<(u64, Vec<Vec<u8>>)>,
}

impl ControlParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reçoit une ligne du flux, sans son `\n` final.
    /// Renvoie un événement quand la ligne en complète un.
    pub fn feed_line(&mut self, line: &[u8]) -> Option<ControlEvent> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let guard = parse_guard(line);
        if let Some((number, mut lines)) = self.open_block.take() {
            return match guard {
                Some((Guard::End, n)) if n == number => Some(ControlEvent::Reply {
                    number,
                    ok: true,
                    lines,
                }),
                Some((Guard::Error, n)) if n == number => Some(ControlEvent::Reply {
                    number,
                    ok: false,
                    lines,
                }),
                _ => {
                    // Dans un bloc, toute autre ligne est du contenu, même si elle commence par `%`.
                    lines.push(line.to_vec());
                    self.open_block = Some((number, lines));
                    None
                }
            };
        }
        if let Some((Guard::Begin, number)) = guard {
            self.open_block = Some((number, Vec::new()));
            return None;
        }
        Some(parse_notification(line))
    }
}

/// Reconnaît `%begin <temps> <numéro> <drapeaux>`, et de même `%end` et `%error`.
fn parse_guard(line: &[u8]) -> Option<(Guard, u64)> {
    let text = std::str::from_utf8(line).ok()?;
    let mut tokens = text.split(' ');
    let guard = match tokens.next()? {
        "%begin" => Guard::Begin,
        "%end" => Guard::End,
        "%error" => Guard::Error,
        _ => return None,
    };
    let _time = tokens.next()?;
    let number = tokens.next()?.parse().ok()?;
    Some((guard, number))
}

fn parse_notification(line: &[u8]) -> ControlEvent {
    let (head, tail) = split_token(line);
    match head {
        b"%output" => {
            let (pane, data) = split_token(tail);
            match parse_id(pane, b'%') {
                Some(id) => ControlEvent::Output {
                    pane: PaneId(id),
                    data: unescape_output(data),
                },
                None => other(line),
            }
        }
        b"%extended-output" => {
            // `%extended-output %<pane> <âge> … : <données>` : on ignore tout jusqu'au ` : `.
            let (pane, rest) = split_token(tail);
            match (parse_id(pane, b'%'), find(rest, b" : ")) {
                (Some(id), Some(at)) => ControlEvent::Output {
                    pane: PaneId(id),
                    data: unescape_output(&rest[at + 3..]),
                },
                _ => other(line),
            }
        }
        b"%layout-change" => {
            let (window, rest) = split_token(tail);
            match parse_id(window, b'@') {
                Some(id) => ControlEvent::LayoutChange {
                    window: WindowId(id),
                    rest: String::from_utf8_lossy(rest).into_owned(),
                },
                None => other(line),
            }
        }
        b"%pause" | b"%continue" => match parse_id(split_token(tail).0, b'%') {
            Some(id) if head == b"%pause" => ControlEvent::Pause { pane: PaneId(id) },
            Some(id) => ControlEvent::Continue { pane: PaneId(id) },
            None => other(line),
        },
        b"%window-add" | b"%window-close" => match parse_id(split_token(tail).0, b'@') {
            Some(id) if head == b"%window-add" => ControlEvent::WindowAdd {
                window: WindowId(id),
            },
            Some(id) => ControlEvent::WindowClose {
                window: WindowId(id),
            },
            None => other(line),
        },
        b"%subscription-changed" => {
            let (name, rest) = split_token(tail);
            match find(rest, b" : ") {
                Some(at) => ControlEvent::SubscriptionChanged {
                    name: String::from_utf8_lossy(name).into_owned(),
                    value: String::from_utf8_lossy(&rest[at + 3..]).into_owned(),
                },
                None => other(line),
            }
        }
        b"%session-changed" => match parse_id(split_token(tail).0, b'$') {
            Some(id) => ControlEvent::SessionChanged {
                session: SessionId(id),
            },
            None => other(line),
        },
        b"%session-window-changed" => {
            let (session, rest) = split_token(tail);
            match (parse_id(session, b'$'), parse_id(split_token(rest).0, b'@')) {
                (Some(session), Some(window)) => ControlEvent::SessionWindowChanged {
                    session: SessionId(session),
                    window: WindowId(window),
                },
                _ => other(line),
            }
        }
        b"%window-pane-changed" => {
            let (window, rest) = split_token(tail);
            match (parse_id(window, b'@'), parse_id(split_token(rest).0, b'%')) {
                (Some(window), Some(pane)) => ControlEvent::WindowPaneChanged {
                    window: WindowId(window),
                    pane: PaneId(pane),
                },
                _ => other(line),
            }
        }
        b"%sessions-changed" => ControlEvent::SessionsChanged,
        b"%exit" => ControlEvent::Exit {
            reason: (!tail.is_empty()).then(|| String::from_utf8_lossy(tail).into_owned()),
        },
        _ => other(line),
    }
}

fn other(line: &[u8]) -> ControlEvent {
    ControlEvent::Other(String::from_utf8_lossy(line).into_owned())
}

/// Coupe au premier espace : (mot, reste). Sans espace, le reste est vide.
fn split_token(bytes: &[u8]) -> (&[u8], &[u8]) {
    match bytes.iter().position(|b| *b == b' ') {
        Some(at) => (&bytes[..at], &bytes[at + 1..]),
        None => (bytes, &[]),
    }
}

fn parse_id(token: &[u8], sigil: u8) -> Option<u32> {
    let digits = token.strip_prefix(&[sigil])?;
    std::str::from_utf8(digits).ok()?.parse().ok()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Défait l'échappement de tmux : un octet sous 0x20, ou `\`, devient `\ooo` en octal.
pub fn unescape_output(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        // Le premier chiffre est limité à 0-3 : au-delà de `\377`, la séquence reste littérale.
        let octal = data.get(i + 1..i + 4).filter(|d| {
            (b'0'..=b'3').contains(&d[0]) && d[1..].iter().all(|b| (b'0'..=b'7').contains(b))
        });
        match (data[i], octal) {
            (b'\\', Some(d)) => {
                out.push((d[0] - b'0') * 64 + (d[1] - b'0') * 8 + (d[2] - b'0'));
                i += 4;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(parser: &mut ControlParser, lines: &[&[u8]]) -> Vec<ControlEvent> {
        lines
            .iter()
            .filter_map(|line| parser.feed_line(line))
            .collect()
    }

    #[test]
    fn output_carries_raw_bytes_from_real_capture() {
        // Lignes relevées sur tmux 3.8 le 14/10 (sonde de recherche).
        let mut parser = ControlParser::new();
        let events = feed_all(
            &mut parser,
            &[
                b"%output %0 \\007",
                b"%output %0 \\033]9;hello\\007",
                b"%output %0 \\033]133;D;3\\007",
                b"%output %0 plain\\015\\012",
            ],
        );
        assert_eq!(
            events,
            vec![
                ControlEvent::Output {
                    pane: PaneId(0),
                    data: b"\x07".to_vec()
                },
                ControlEvent::Output {
                    pane: PaneId(0),
                    data: b"\x1b]9;hello\x07".to_vec()
                },
                ControlEvent::Output {
                    pane: PaneId(0),
                    data: b"\x1b]133;D;3\x07".to_vec()
                },
                ControlEvent::Output {
                    pane: PaneId(0),
                    data: b"plain\r\n".to_vec()
                },
            ]
        );
    }

    #[test]
    fn extended_output_skips_age_and_keeps_backslash() {
        let mut parser = ControlParser::new();
        let event = parser.feed_line(b"%extended-output %3 12 : a b\\134x");
        assert_eq!(
            event,
            Some(ControlEvent::Output {
                pane: PaneId(3),
                data: b"a b\\x".to_vec()
            })
        );
    }

    #[test]
    fn utf8_split_across_two_lines_stays_bytes() {
        let mut parser = ControlParser::new();
        let first = parser.feed_line(b"%output %1 caf\xc3");
        let second = parser.feed_line(b"%output %1 \xa9!");
        assert_eq!(
            first,
            Some(ControlEvent::Output {
                pane: PaneId(1),
                data: b"caf\xc3".to_vec()
            })
        );
        assert_eq!(
            second,
            Some(ControlEvent::Output {
                pane: PaneId(1),
                data: b"\xa9!".to_vec()
            })
        );
    }

    #[test]
    fn reply_block_keeps_body_lines_even_when_they_start_with_percent() {
        let mut parser = ControlParser::new();
        let events = feed_all(
            &mut parser,
            &[
                b"%begin 1760000000 12 1",
                b"%output %9 not a notification",
                b"line two",
                b"%end 1760000000 12 1",
            ],
        );
        assert_eq!(
            events,
            vec![ControlEvent::Reply {
                number: 12,
                ok: true,
                lines: vec![
                    b"%output %9 not a notification".to_vec(),
                    b"line two".to_vec()
                ],
            }]
        );
    }

    #[test]
    fn error_block_is_a_failed_reply() {
        let mut parser = ControlParser::new();
        let events = feed_all(
            &mut parser,
            &[b"%begin 1 13 1", b"unknown command", b"%error 1 13 1"],
        );
        assert_eq!(
            events,
            vec![ControlEvent::Reply {
                number: 13,
                ok: false,
                lines: vec![b"unknown command".to_vec()]
            }]
        );
    }

    #[test]
    fn end_with_another_number_stays_inside_the_block() {
        let mut parser = ControlParser::new();
        let events = feed_all(
            &mut parser,
            &[b"%begin 1 20 1", b"%end 1 99 1", b"%end 1 20 1"],
        );
        assert_eq!(
            events,
            vec![ControlEvent::Reply {
                number: 20,
                ok: true,
                lines: vec![b"%end 1 99 1".to_vec()]
            }]
        );
    }

    #[test]
    fn notifications_are_typed() {
        let mut parser = ControlParser::new();
        let events = feed_all(
            &mut parser,
            &[
                b"%pause %4",
                b"%continue %4",
                b"%window-add @7",
                b"%window-close @7",
                b"%layout-change @18 b25d,80x24,0,0,1 b25d,80x24,0,0,1 *",
                b"%subscription-changed st $2 @3 0 %3 : %3=bash|t|dead=0/",
                b"%exit",
                b"%exit detached",
                b"%sessions-changed",
            ],
        );
        assert_eq!(
            events,
            vec![
                ControlEvent::Pause { pane: PaneId(4) },
                ControlEvent::Continue { pane: PaneId(4) },
                ControlEvent::WindowAdd {
                    window: WindowId(7)
                },
                ControlEvent::WindowClose {
                    window: WindowId(7)
                },
                ControlEvent::LayoutChange {
                    window: WindowId(18),
                    rest: "b25d,80x24,0,0,1 b25d,80x24,0,0,1 *".into()
                },
                ControlEvent::SubscriptionChanged {
                    name: "st".into(),
                    value: "%3=bash|t|dead=0/".into()
                },
                ControlEvent::Exit { reason: None },
                ControlEvent::Exit {
                    reason: Some("detached".into())
                },
                ControlEvent::SessionsChanged,
            ]
        );
    }

    #[test]
    fn window_changes_are_typed() {
        // Lignes relevées sur tmux 3.8 le 15/10 (sonde 1 du plan du jalon 3).
        let mut parser = ControlParser::new();
        let events = feed_all(
            &mut parser,
            &[
                b"%session-changed $0 A",
                b"%session-window-changed $1 @2",
                b"%window-pane-changed @0 %1",
                b"%window-pane-changed @0 garbage",
            ],
        );
        assert_eq!(
            events,
            vec![
                ControlEvent::SessionChanged {
                    session: SessionId(0)
                },
                // L'identifiant de session est gardé : c'est lui qui distingue la session du client.
                ControlEvent::SessionWindowChanged {
                    session: SessionId(1),
                    window: WindowId(2)
                },
                ControlEvent::WindowPaneChanged {
                    window: WindowId(0),
                    pane: PaneId(1)
                },
                ControlEvent::Other("%window-pane-changed @0 garbage".into()),
            ]
        );
    }

    #[test]
    fn crlf_line_ending_is_tolerated() {
        let mut parser = ControlParser::new();
        assert_eq!(
            parser.feed_line(b"%pause %2\r"),
            Some(ControlEvent::Pause { pane: PaneId(2) })
        );
    }

    #[test]
    fn unescape_leaves_out_of_range_octal_alone() {
        assert_eq!(unescape_output(br"\777"), br"\777".to_vec());
        assert_eq!(unescape_output(br"\400"), br"\400".to_vec());
        assert_eq!(unescape_output(br"\134"), b"\\".to_vec());
        assert_eq!(unescape_output(br"\377"), vec![0xff]);
    }

    #[test]
    fn unescape_leaves_lone_backslash_and_short_sequences_alone() {
        assert_eq!(unescape_output(b"a\\b"), b"a\\b".to_vec());
        assert_eq!(unescape_output(b"end\\01"), b"end\\01".to_vec());
        assert_eq!(unescape_output(b"\\134\\033"), b"\\\x1b".to_vec());
    }
}
