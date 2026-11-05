//! La saisie venue du téléphone, côté pur : ce que le corps d'une requête peut demander, les règles qui la refusent
//! (péremption, doublon, caractères de contrôle, pane qui n'accepte rien) et l'encodage des touches. Rien ici ne
//! touche un pane : l'espace de travail applique, ce module décide.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use idema_term::input::{KeyModes, Mods, encode_key};
use serde::Deserialize;

use super::InputOutcome;

/// Le corps d'une saisie ne dépasse pas 16 Kio.
const MAX_BODY: usize = 16 * 1024;
/// Un texte fait 1 à 1 024 octets.
const MAX_TEXT: usize = 1024;
/// Un `command_id` fait 1 à 64 octets (un UUID en fait 36).
const MAX_COMMAND_ID: usize = 64;
/// Une commande traitée plus tard que cela après sa réception HTTP est jetée.
const EXPIRY: Duration = Duration::from_millis(1500);
/// Un `command_id` déjà vu dans cette fenêtre n'est pas retapé.
const DUPLICATE_WINDOW: Duration = Duration::from_secs(60);

/// Entre un texte (ou une lettre) et son Entrée : une interface Ink (Claude Code, Codex) prendrait une rafale pour un
/// collage et garderait le retour comme saut de ligne.
pub const ENTER_DELAY: Duration = Duration::from_millis(80);
/// Le côté HTTP attend la réponse du fil d'interface au plus ce temps. Plus long que la péremption (1,5 s) avec une
/// marge d'un second : une touche ne part jamais après une réponse d'échec.
pub const HTTP_WAIT: Duration = Duration::from_millis(2500);

/// Au-delà de cet âge (depuis la réception HTTP), l'Entrée d'une saisie n'est plus tapée : il reste au moins 300 ms avant
/// que le côté HTTP ne renonce (`HTTP_WAIT`), pour taper l'Entrée et répondre.
pub const ENTER_DEADLINE: Duration = Duration::from_millis(2200);

/// Une touche que le téléphone sait envoyer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKey {
    /// Un chiffre de 1 à 9 (les choix numérotés).
    Digit(u8),
    Y,
    N,
    Enter,
    Escape,
    Tab,
    Up,
    Down,
    CtrlC,
}

/// Ce que la requête demande de taper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteInput {
    /// Une touche, suivie d'Entrée (après `ENTER_DELAY`) si `enter`.
    Key { key: RemoteKey, enter: bool },
    /// Un texte d'une ligne, suivi d'Entrée (après `ENTER_DELAY`) si `enter`.
    Text { text: String, enter: bool },
}

/// Pourquoi un corps est refusé (400).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadInput {
    /// Pas du JSON de la forme attendue, ou plus de 16 Kio.
    NotJson,
    NeitherKeyNorText,
    BothKeyAndText,
    UnknownKey,
    EmptyText,
    TextTooLong,
    /// Un caractère de contrôle (C0, DEL, C1) : un collage de plusieurs lignes ne doit pas exécuter de commande.
    ControlChar,
    /// `command_id` absent, vide ou de plus de 64 octets.
    MissingCommandId,
}

impl BadInput {
    /// Le code d'erreur de la réponse JSON.
    pub fn error_code(self) -> &'static str {
        match self {
            Self::NotJson => "not_json",
            Self::NeitherKeyNorText => "neither_key_nor_text",
            Self::BothKeyAndText => "both_key_and_text",
            Self::UnknownKey => "unknown_key",
            Self::EmptyText => "empty_text",
            Self::TextTooLong => "text_too_long",
            Self::ControlChar => "control_char",
            Self::MissingCommandId => "missing_command_id",
        }
    }
}

#[derive(Deserialize)]
struct Body {
    key: Option<String>,
    text: Option<String>,
    enter: Option<bool>,
    command_id: Option<String>,
}

fn key_named(name: &str) -> Option<RemoteKey> {
    Some(match name {
        "1" => RemoteKey::Digit(1),
        "2" => RemoteKey::Digit(2),
        "3" => RemoteKey::Digit(3),
        "4" => RemoteKey::Digit(4),
        "5" => RemoteKey::Digit(5),
        "6" => RemoteKey::Digit(6),
        "7" => RemoteKey::Digit(7),
        "8" => RemoteKey::Digit(8),
        "9" => RemoteKey::Digit(9),
        "y" => RemoteKey::Y,
        "n" => RemoteKey::N,
        "enter" => RemoteKey::Enter,
        "escape" => RemoteKey::Escape,
        "tab" => RemoteKey::Tab,
        "up" => RemoteKey::Up,
        "down" => RemoteKey::Down,
        "ctrl_c" => RemoteKey::CtrlC,
        _ => return None,
    })
}

/// Lit le corps d'une saisie : `{"key": …}` ou `{"text": …}`, `enter` (faux par défaut) et le `command_id` tiré par le
/// client. Les clés inconnues sont ignorées ; rien n'est deviné.
pub fn parse_input(body: &[u8]) -> Result<(RemoteInput, String), BadInput> {
    if body.len() > MAX_BODY {
        return Err(BadInput::NotJson);
    }
    let Body {
        key,
        text,
        enter,
        command_id,
    } = serde_json::from_slice(body).map_err(|_| BadInput::NotJson)?;
    let enter = enter.unwrap_or(false);
    let input = match (key, text) {
        (Some(_), Some(_)) => return Err(BadInput::BothKeyAndText),
        (None, None) => return Err(BadInput::NeitherKeyNorText),
        (Some(name), None) => RemoteInput::Key {
            key: key_named(&name).ok_or(BadInput::UnknownKey)?,
            enter,
        },
        (None, Some(text)) => {
            if text.is_empty() {
                return Err(BadInput::EmptyText);
            }
            if text.len() > MAX_TEXT {
                return Err(BadInput::TextTooLong);
            }
            if text.chars().any(char::is_control) {
                return Err(BadInput::ControlChar);
            }
            RemoteInput::Text { text, enter }
        }
    };
    let command_id = command_id
        .filter(|id| (1..=MAX_COMMAND_ID).contains(&id.len()))
        .ok_or(BadInput::MissingCommandId)?;
    Ok((input, command_id))
}

/// La commande est-elle arrivée trop tard (plus de 1,5 s après sa réception HTTP) pour qu'on la tape encore ?
pub fn is_expired(received: Instant, now: Instant) -> bool {
    now.saturating_duration_since(received) > EXPIRY
}

/// L'Entrée différée d'une saisie peut-elle encore partir ? Oui jusqu'à `ENTER_DEADLINE` après la réception HTTP. Passé ce
/// délai, le téléphone a reçu (ou va recevoir) un échec : une Entrée tapée après lui pourrait valider une invite qu'il
/// n'a pas vue.
pub fn enter_still_allowed(received: Instant, now: Instant) -> bool {
    now.saturating_duration_since(received) <= ENTER_DEADLINE
}

/// Les `command_id` tapés dans la dernière minute.
#[derive(Debug, Default)]
pub struct RecentCommands {
    typed: HashMap<String, Instant>,
}

impl RecentCommands {
    /// Cet identifiant a-t-il été retenu depuis moins d'une minute ? Ne retient rien.
    pub fn seen(&self, id: &str, now: Instant) -> bool {
        self.typed
            .get(id)
            .is_some_and(|at| now.saturating_duration_since(*at) < DUPLICATE_WINDOW)
    }

    /// Retient l'identifiant. Vrai si c'est la première fois dans la minute ; faux s'il est déjà retenu (le délai ne
    /// repart pas). Les identifiants plus vieux qu'une minute sont oubliés au passage.
    pub fn first_seen(&mut self, id: &str, now: Instant) -> bool {
        self.typed
            .retain(|_, at| now.saturating_duration_since(*at) < DUPLICATE_WINDOW);
        if self.typed.contains_key(id) {
            return false;
        }
        self.typed.insert(id.to_owned(), now);
        true
    }
}

/// Le pane prend-il une saisie ? Fermé par défaut : seulement hors lecture seule et si une commande tourne. Au prompt
/// du shell, et tant qu'IdemA ne sait pas ce qui tourne (`foreground_is_shell` inconnu), `command_running` est faux.
pub fn accepts_input(read_only: bool, command_running: bool) -> bool {
    !read_only && command_running
}

/// Ce que le fil d'interface sait d'une saisie une fois la session et le pane trouvés.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    pub client_exited: bool,
    pub expired: bool,
    pub duplicate: bool,
    pub read_only: bool,
    pub command_running: bool,
}

/// Le premier refus, dans l'ordre : client de contrôle sorti, commande périmée, doublon, lecture seule, rien ne
/// tourne. `None` : la saisie peut partir.
pub fn gate(facts: Gate) -> Option<InputOutcome> {
    if facts.client_exited {
        Some(InputOutcome::Disconnected)
    } else if facts.expired {
        Some(InputOutcome::Expired)
    } else if facts.duplicate {
        Some(InputOutcome::Duplicate)
    } else if facts.read_only {
        Some(InputOutcome::ReadOnly)
    } else if !accepts_input(facts.read_only, facts.command_running) {
        Some(InputOutcome::NoProgram)
    } else {
        None
    }
}

/// Les octets d'une touche. Chiffres, `y` et `n` partent bruts (`encode_key` ne les encode pas sans Ctrl) ; les autres
/// passent par `encode_key` avec les modes du pane, comme le clavier d'IdemA (curseur applicatif compris).
pub fn encode_remote_key(key: RemoteKey, modes: KeyModes) -> Option<Vec<u8>> {
    let named = |name: &str| encode_key(name, Mods::default(), modes);
    match key {
        RemoteKey::Digit(digit @ 1..=9) => Some(vec![b'0' + digit]),
        RemoteKey::Digit(_) => None,
        RemoteKey::Y => Some(b"y".to_vec()),
        RemoteKey::N => Some(b"n".to_vec()),
        RemoteKey::Enter => named("enter"),
        RemoteKey::Escape => named("escape"),
        RemoteKey::Tab => named("tab"),
        RemoteKey::Up => named("up"),
        RemoteKey::Down => named("down"),
        RemoteKey::CtrlC => encode_key(
            "c",
            Mods {
                control: true,
                ..Mods::default()
            },
            modes,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(json: &str) -> Result<(RemoteInput, String), BadInput> {
        parse_input(json.as_bytes())
    }

    #[test]
    fn keys_and_text_parse() {
        let cases = [
            ("1", RemoteKey::Digit(1)),
            ("9", RemoteKey::Digit(9)),
            ("y", RemoteKey::Y),
            ("n", RemoteKey::N),
            ("enter", RemoteKey::Enter),
            ("escape", RemoteKey::Escape),
            ("tab", RemoteKey::Tab),
            ("up", RemoteKey::Up),
            ("down", RemoteKey::Down),
            ("ctrl_c", RemoteKey::CtrlC),
        ];
        for (name, key) in cases {
            assert_eq!(
                body(&format!(r#"{{"key":"{name}","command_id":"c1"}}"#)),
                Ok((RemoteInput::Key { key, enter: false }, "c1".to_owned())),
                "{name}"
            );
        }
        assert_eq!(
            body(r#"{"key":"y","enter":true,"command_id":"c2"}"#),
            Ok((
                RemoteInput::Key {
                    key: RemoteKey::Y,
                    enter: true
                },
                "c2".to_owned()
            ))
        );
        let one = body(r#"{"text":"a","command_id":"c3"}"#).unwrap();
        assert_eq!(
            one.0,
            RemoteInput::Text {
                text: "a".into(),
                enter: false
            }
        );
        let longest = "é".repeat(512);
        assert_eq!(longest.len(), 1024);
        assert!(
            body(&format!(
                r#"{{"text":"{longest}","enter":true,"command_id":"c4"}}"#
            ))
            .is_ok()
        );
        let too_long = format!("{longest}a");
        assert_eq!(
            body(&format!(r#"{{"text":"{too_long}","command_id":"c5"}}"#)),
            Err(BadInput::TextTooLong)
        );
        let ascii_1024 = "a".repeat(1024);
        assert!(body(&format!(r#"{{"text":"{ascii_1024}","command_id":"c6"}}"#)).is_ok());
        assert_eq!(
            body(&format!(r#"{{"text":"{ascii_1024}a","command_id":"c6"}}"#)),
            Err(BadInput::TextTooLong)
        );
    }

    #[test]
    fn malformed_bodies_are_refused_with_their_reason() {
        let cases = [
            ("nope", BadInput::NotJson),
            (r#"["key"]"#, BadInput::NotJson),
            (r#"{"key":3,"command_id":"c"}"#, BadInput::NotJson),
            (r#"{"command_id":"c"}"#, BadInput::NeitherKeyNorText),
            (
                r#"{"key":"y","text":"a","command_id":"c"}"#,
                BadInput::BothKeyAndText,
            ),
            (r#"{"key":"Y","command_id":"c"}"#, BadInput::UnknownKey),
            (r#"{"key":"0","command_id":"c"}"#, BadInput::UnknownKey),
            (r#"{"key":"10","command_id":"c"}"#, BadInput::UnknownKey),
            (r#"{"text":"","command_id":"c"}"#, BadInput::EmptyText),
            (r#"{"key":"y"}"#, BadInput::MissingCommandId),
            (r#"{"key":"y","command_id":""}"#, BadInput::MissingCommandId),
            (
                r#"{"text":"a","command_id":null}"#,
                BadInput::MissingCommandId,
            ),
        ];
        for (json, expected) in cases {
            assert_eq!(body(json), Err(expected), "{json}");
        }
        let long_id = "x".repeat(65);
        assert_eq!(
            body(&format!(r#"{{"key":"y","command_id":"{long_id}"}}"#)),
            Err(BadInput::MissingCommandId)
        );
        let huge = format!(
            r#"{{"text":"a","command_id":"c","pad":"{}"}}"#,
            "x".repeat(MAX_BODY)
        );
        assert_eq!(body(&huge), Err(BadInput::NotJson));
    }

    #[test]
    fn control_characters_are_refused() {
        for (label, text) in [
            ("newline", "\\n"),
            ("escape", "\\u001b"),
            ("delete", "\\u007f"),
            ("c1", "\\u0085"),
            ("c1 end", "\\u009f"),
            ("tab", "\\t"),
            ("nul", "\\u0000"),
            ("inside", "ok\\rok"),
        ] {
            assert_eq!(
                body(&format!(r#"{{"text":"{text}","command_id":"c"}}"#)),
                Err(BadInput::ControlChar),
                "{label}"
            );
        }
        // Les caractères imprimables hors ASCII, y compris juste après le bloc C1, passent.
        assert!(body(r#"{"text":"café   ☃","command_id":"c"}"#).is_ok());
    }

    #[test]
    fn expiry_is_one_and_a_half_seconds() {
        let received = Instant::now();
        assert!(!is_expired(
            received,
            received + Duration::from_millis(1400)
        ));
        assert!(is_expired(received, received + Duration::from_millis(1600)));
        assert!(!is_expired(received, received));
        // Une horloge qui recule ne périme rien.
        assert!(!is_expired(received + Duration::from_secs(5), received));
    }

    #[test]
    fn a_delayed_enter_leaves_300_ms_before_the_http_side_gives_up() {
        assert_eq!(ENTER_DEADLINE + Duration::from_millis(300), HTTP_WAIT);
        let received = Instant::now();
        assert!(enter_still_allowed(received, received));
        assert!(enter_still_allowed(
            received,
            received + ENTER_DEADLINE - Duration::from_millis(1)
        ));
        assert!(enter_still_allowed(received, received + ENTER_DEADLINE));
        assert!(!enter_still_allowed(
            received,
            received + ENTER_DEADLINE + Duration::from_millis(1)
        ));
        // Une commande prise à la limite de la péremption (1,5 s) et son délai de 80 ms tiennent dans la marge.
        assert!(enter_still_allowed(
            received,
            received + Duration::from_millis(1500) + ENTER_DELAY
        ));
    }

    #[test]
    fn a_command_id_is_typed_once_a_minute() {
        let start = Instant::now();
        let mut recent = RecentCommands::default();
        assert!(!recent.seen("a", start));
        assert!(recent.first_seen("a", start));
        assert!(recent.seen("a", start + Duration::from_secs(59)));
        assert!(!recent.first_seen("a", start + Duration::from_secs(59)));
        assert!(recent.first_seen("a", start + Duration::from_secs(61)));
        assert!(recent.first_seen("b", start + Duration::from_secs(61)));
    }

    #[test]
    fn a_replayed_id_does_not_extend_its_own_window() {
        let start = Instant::now();
        let mut recent = RecentCommands::default();
        assert!(recent.first_seen("a", start));
        assert!(!recent.first_seen("a", start + Duration::from_secs(50)));
        // Le rejeu à +50 s ne repousse pas l'oubli : à +61 s l'identifiant est de nouveau neuf.
        assert!(recent.first_seen("a", start + Duration::from_secs(61)));
    }

    #[test]
    fn input_is_closed_until_a_program_is_known() {
        // « Programme encore inconnu » (`foreground_is_shell == None`) et prompt du shell : `command_running` faux.
        assert!(!accepts_input(false, false));
        assert!(!accepts_input(true, true));
        assert!(!accepts_input(true, false));
        assert!(accepts_input(false, true));
    }

    fn facts() -> Gate {
        Gate {
            client_exited: false,
            expired: false,
            duplicate: false,
            read_only: false,
            command_running: true,
        }
    }

    #[test]
    fn the_gate_refuses_in_the_documented_order() {
        assert_eq!(gate(facts()), None);
        let everything = Gate {
            client_exited: true,
            expired: true,
            duplicate: true,
            read_only: true,
            command_running: false,
        };
        assert_eq!(gate(everything), Some(InputOutcome::Disconnected));
        let without = |f: fn(&mut Gate)| {
            let mut next = everything;
            f(&mut next);
            gate(next)
        };
        assert_eq!(
            without(|g| g.client_exited = false),
            Some(InputOutcome::Expired)
        );
        assert_eq!(
            without(|g| {
                g.client_exited = false;
                g.expired = false;
            }),
            Some(InputOutcome::Duplicate)
        );
        assert_eq!(
            without(|g| {
                g.client_exited = false;
                g.expired = false;
                g.duplicate = false;
            }),
            Some(InputOutcome::ReadOnly)
        );
        assert_eq!(
            without(|g| {
                g.client_exited = false;
                g.expired = false;
                g.duplicate = false;
                g.read_only = false;
            }),
            Some(InputOutcome::NoProgram)
        );
        // Au prompt du shell, ou programme encore inconnu : fermé.
        assert_eq!(
            gate(Gate {
                command_running: false,
                ..facts()
            }),
            Some(InputOutcome::NoProgram)
        );
    }

    #[test]
    fn remote_keys_encode_like_the_keyboard() {
        let plain = KeyModes::default();
        let application = KeyModes {
            app_cursor: true,
            extended_keys: false,
        };
        assert_eq!(
            encode_remote_key(RemoteKey::Digit(2), plain),
            Some(b"2".to_vec())
        );
        assert_eq!(
            encode_remote_key(RemoteKey::Digit(9), plain),
            Some(b"9".to_vec())
        );
        assert_eq!(encode_remote_key(RemoteKey::Digit(0), plain), None);
        assert_eq!(encode_remote_key(RemoteKey::Y, plain), Some(b"y".to_vec()));
        assert_eq!(encode_remote_key(RemoteKey::N, plain), Some(b"n".to_vec()));
        assert_eq!(
            encode_remote_key(RemoteKey::Up, application),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_remote_key(RemoteKey::Up, plain),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_remote_key(RemoteKey::Down, plain),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            encode_remote_key(RemoteKey::Enter, plain),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            encode_remote_key(RemoteKey::Escape, plain),
            Some(b"\x1b".to_vec())
        );
        assert_eq!(
            encode_remote_key(RemoteKey::Tab, plain),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            encode_remote_key(RemoteKey::CtrlC, plain),
            Some(b"\x03".to_vec())
        );
    }
}
