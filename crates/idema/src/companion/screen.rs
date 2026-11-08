//! L'écran d'un pane tel que le téléphone le lit : la sortie de `tmux capture-pane -p -e -J` (séquences de couleur
//! comprises), la taille du pane et une empreinte pour le 304. La capture est bloquante (un process tmux) : l'appelant
//! la lance sur `spawn_blocking`, jamais sur le fil d'interface.

use std::fmt::Write as _;
use std::time::Instant;

use idema_tmux::server::TmuxServer;
use sha2::{Digest, Sha256};

use crate::machines::sessions::{SessionServers, with_session_server};

/// Un écran capturé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    /// Le texte de `capture-pane`, non coupé : la fin de ligne finale et les lignes d'espaces sont conservées, de sorte
    /// que la comparaison avec une capture directe se fait octet à octet. Les octets qui ne sont pas de l'UTF-8 sont
    /// remplacés par U+FFFD.
    pub text: String,
    pub cols: u16,
    pub rows: u16,
    /// L'empreinte du texte et de la taille (16 chiffres hexadécimaux, sans guillemets) : la page la compare, le
    /// service en tire l'`ETag`.
    pub etag: String,
    /// Le temps pris par la capture, taille comprise.
    pub took_ms: u64,
}

/// Pur : la sortie brute de `capture-pane` (non coupée) en `Screen`.
pub fn screen_from_output(stdout: &[u8], cols: u16, rows: u16, took_ms: u64) -> Screen {
    let text = String::from_utf8_lossy(stdout).into_owned();
    let mut hash = Sha256::new();
    hash.update(cols.to_be_bytes());
    hash.update(rows.to_be_bytes());
    hash.update(text.as_bytes());
    let etag = hash
        .finalize()
        .iter()
        .take(8)
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    Screen {
        text,
        cols,
        rows,
        etag,
        took_ms,
    }
}

/// Lit « <largeur> <hauteur> » rendu par `display-message`.
fn parse_size(stdout: &[u8]) -> Option<(u16, u16)> {
    let text = std::str::from_utf8(stdout).ok()?;
    let mut words = text.split_whitespace();
    let cols = words.next()?.parse().ok()?;
    let rows = words.next()?.parse().ok()?;
    words.next().is_none().then_some((cols, rows))
}

/// Capture le pane `%<pane>` avec `history` lignes d'historique au-dessus de l'écran : le texte a donc `history + rows`
/// lignes au plus. Bloquant. Les deux appels passent par `tmux.command(..).output()` : `TmuxServer::run` coupe la fin de
/// la sortie et casserait la comparaison octet à octet.
pub fn capture(tmux: &TmuxServer, pane: u32, history: u16) -> Result<Screen, String> {
    let started = Instant::now();
    let target = format!("%{pane}");
    let size = tmux
        .command(&[
            "display-message",
            "-p",
            "-t",
            &target,
            "#{pane_width} #{pane_height}",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("display-message: {error}"))?;
    if !size.status.success() {
        return Err("display-message failed".into());
    }
    let (cols, rows) = parse_size(&size.stdout).ok_or("unreadable pane size")?;
    let start = format!("-{history}");
    let captured = tmux
        .command(&[
            "capture-pane",
            "-p",
            "-e",
            "-J",
            "-t",
            &target,
            "-S",
            &start,
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("capture-pane: {error}"))?;
    if !captured.status.success() {
        return Err("capture-pane failed".into());
    }
    let took_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(screen_from_output(&captured.stdout, cols, rows, took_ms))
}

/// `capture` sur le serveur de la session `session_key` : le tmux du Mac pour une session locale, celui de sa machine
/// (par ssh, derrière la porte des appels ponctuels) pour une session distante. Un pane ne se lit jamais sur un autre
/// serveur que celui de sa session. Bloquant.
pub fn capture_for(
    servers: &SessionServers,
    session_key: &str,
    pane: u32,
    history: u16,
) -> Result<Screen, String> {
    with_session_server(servers, session_key, |tmux, _| capture(tmux, pane, history))
        .unwrap_or_else(|| Err("no server for the session".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_from_output_handles_hostile_bytes() {
        // Octets qui ne sont pas de l'UTF-8 : remplacés, rien ne plante.
        let screen = screen_from_output(b"ok \xff\xfe\xc3 end\n", 80, 24, 3);
        assert_eq!(screen.text, "ok \u{fffd}\u{fffd}\u{fffd} end\n");

        // Une ligne de 300 colonnes passe entière, sans coupe.
        let wide = "x".repeat(300);
        let screen = screen_from_output(format!("{wide}\n").as_bytes(), 80, 24, 0);
        assert_eq!(screen.text, format!("{wide}\n"));

        // La fin `\n` suivie d'une ligne d'espaces est gardée telle quelle (un `trim` casserait l'octet à octet).
        let raw = b"line\n   \n";
        assert_eq!(screen_from_output(raw, 80, 24, 0).text.as_bytes(), raw);

        // Une séquence SGR tronquée, du HTML et un caractère de contrôle restent du texte.
        let hostile = "\u{1b}[38;5;1\u{1b}[<img onerror=x>\u{7}\n";
        assert_eq!(
            screen_from_output(hostile.as_bytes(), 1, 1, 0).text,
            hostile
        );

        let empty = screen_from_output(b"", 0, 0, 0);
        assert_eq!(empty.text, "");
    }

    #[test]
    fn the_fingerprint_follows_text_and_size_not_timing() {
        let first = screen_from_output(b"a\n", 80, 24, 5);
        assert_eq!(first, screen_from_output(b"a\n", 80, 24, 5));
        assert_eq!(first.etag, screen_from_output(b"a\n", 80, 24, 900).etag);
        assert_ne!(first.etag, screen_from_output(b"b\n", 80, 24, 5).etag);
        assert_ne!(first.etag, screen_from_output(b"a\n", 81, 24, 5).etag);
        assert_ne!(first.etag, screen_from_output(b"a\n", 80, 25, 5).etag);
        assert_ne!(first.etag, screen_from_output(b"a", 80, 24, 5).etag);
        assert_eq!(first.etag.len(), 16);
        assert!(first.etag.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn the_pane_size_is_read_strictly() {
        assert_eq!(parse_size(b"80 24\n"), Some((80, 24)));
        assert_eq!(parse_size(b"80\n"), None);
        assert_eq!(parse_size(b"80 24 1\n"), None);
        assert_eq!(parse_size(b"a b\n"), None);
        assert_eq!(parse_size(b"\xff 2\n"), None);
    }

    #[test]
    fn capturing_a_pane_of_an_absent_server_is_an_error_not_a_panic() {
        let tmux = TmuxServer::new(
            "/nonexistent/tmux".into(),
            "idema-test-screen",
            "/nonexistent/tmux.conf".into(),
            Vec::new(),
        );
        assert!(capture(&tmux, 1, 200).is_err());
    }
}
