//! Traduction du clavier, du collage, de la souris et des fichiers déposés en octets pour le programme du pane.

/// Modificateurs d'une touche. `command` est la touche ⌘, réservée aux raccourcis d'Idema.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub command: bool,
}

/// Modes du programme qui changent l'encodage des touches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyModes {
    /// DECCKM : flèches en `ESC O A` au lieu de `ESC [ A`.
    pub app_cursor: bool,
    /// Touches étendues demandées par le programme (`CSI > 4 ; n m`) : `CSI <code> ; <mod> u`.
    pub extended_keys: bool,
}

/// Code de modificateur xterm : 1 + Maj + 2 × Option + 4 × Ctrl.
fn modifier_code(mods: Mods) -> u8 {
    1 + mods.shift as u8 + 2 * mods.alt as u8 + 4 * mods.control as u8
}

/// Traduit une touche nommée par GPUI (« enter », « left », « a »…) en octets.
/// `None` : la touche part par la saisie de texte (lettres, accents, Option+touche en AZERTY),
/// ou elle appartient à Idema (⌘, ⌃⇥). Seuls ⌘← et ⌘→ partent au programme : début et fin de ligne, comme Ghostty
/// et iTerm2 (`cmd+arrow_left=text:\x01`), que Claude Code, zsh et readline comprennent.
pub fn encode_key(key: &str, mods: Mods, modes: KeyModes) -> Option<Vec<u8>> {
    if mods.command {
        return match key {
            "left" if !mods.control && !mods.alt && !mods.shift => Some(b"\x01".to_vec()),
            "right" if !mods.control && !mods.alt && !mods.shift => Some(b"\x05".to_vec()),
            _ => None,
        };
    }
    let code = modifier_code(mods);
    match key {
        "enter" => Some(encode_enter(mods, modes, code)),
        "tab" if mods.control => None,
        "tab" if mods.shift => Some(b"\x1b[Z".to_vec()),
        "tab" => Some(b"\t".to_vec()),
        "backspace" if mods.alt => Some(b"\x1b\x7f".to_vec()),
        "backspace" if mods.control => Some(b"\x08".to_vec()),
        "backspace" => Some(b"\x7f".to_vec()),
        "escape" => Some(b"\x1b".to_vec()),
        "space" if mods.control => Some(b"\x00".to_vec()),
        // ⌥← et ⌥→ : saut de mot, comme Ghostty (`alt+arrow_left=esc:b`).
        "left" if mods.alt && !mods.control && !mods.shift => Some(b"\x1bb".to_vec()),
        "right" if mods.alt && !mods.control && !mods.shift => Some(b"\x1bf".to_vec()),
        "up" => Some(cursor_key(b'A', code, modes.app_cursor)),
        "down" => Some(cursor_key(b'B', code, modes.app_cursor)),
        "right" => Some(cursor_key(b'C', code, modes.app_cursor)),
        "left" => Some(cursor_key(b'D', code, modes.app_cursor)),
        "home" => Some(cursor_key(b'H', code, modes.app_cursor)),
        "end" => Some(cursor_key(b'F', code, modes.app_cursor)),
        "insert" => Some(tilde_key(2, code)),
        "delete" => Some(tilde_key(3, code)),
        "pageup" => Some(tilde_key(5, code)),
        "pagedown" => Some(tilde_key(6, code)),
        "f1" => Some(ss3_key(b'P', code)),
        "f2" => Some(ss3_key(b'Q', code)),
        "f3" => Some(ss3_key(b'R', code)),
        "f4" => Some(ss3_key(b'S', code)),
        "f5" => Some(tilde_key(15, code)),
        "f6" => Some(tilde_key(17, code)),
        "f7" => Some(tilde_key(18, code)),
        "f8" => Some(tilde_key(19, code)),
        "f9" => Some(tilde_key(20, code)),
        "f10" => Some(tilde_key(21, code)),
        "f11" => Some(tilde_key(23, code)),
        "f12" => Some(tilde_key(24, code)),
        _ if mods.control => encode_control(key, mods, modes, code),
        _ => None,
    }
}

fn encode_enter(mods: Mods, modes: KeyModes, code: u8) -> Vec<u8> {
    if (mods.shift || mods.control) && modes.extended_keys {
        format!("\x1b[13;{code}u").into_bytes()
    } else if mods.alt || mods.shift {
        // ESC CR : nouvelle ligne dans Claude Code comme dans zsh, sans touches étendues.
        b"\x1b\r".to_vec()
    } else {
        b"\r".to_vec()
    }
}

fn cursor_key(letter: u8, code: u8, app_cursor: bool) -> Vec<u8> {
    if code > 1 {
        let mut out = format!("\x1b[1;{code}").into_bytes();
        out.push(letter);
        out
    } else if app_cursor {
        vec![0x1b, b'O', letter]
    } else {
        vec![0x1b, b'[', letter]
    }
}

fn ss3_key(letter: u8, code: u8) -> Vec<u8> {
    if code > 1 {
        let mut out = format!("\x1b[1;{code}").into_bytes();
        out.push(letter);
        out
    } else {
        vec![0x1b, b'O', letter]
    }
}

fn tilde_key(number: u8, code: u8) -> Vec<u8> {
    if code > 1 {
        format!("\x1b[{number};{code}~").into_bytes()
    } else {
        format!("\x1b[{number}~").into_bytes()
    }
}

fn encode_control(key: &str, mods: Mods, modes: KeyModes, code: u8) -> Option<Vec<u8>> {
    let mut chars = key.chars();
    let c = chars.next()?.to_ascii_lowercase();
    if chars.next().is_some() {
        return None;
    }
    if c.is_ascii_lowercase() && mods.shift && modes.extended_keys {
        return Some(format!("\x1b[{};{code}u", c as u32).into_bytes());
    }
    let byte = match c {
        'a'..='z' => c as u8 & 0x1f,
        '@' | '2' => 0x00,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '/' => 0x1f,
        _ => return None,
    };
    Some(if mods.alt {
        vec![0x1b, byte]
    } else {
        vec![byte]
    })
}

/// Prépare un collage : fins de ligne en CR, tout autre caractère de contrôle retiré sauf la tabulation (ESC ne
/// peut ni fermer l'encadrement ni piloter le terminal, `U+009B` vaut CSI, `^C` et `^D` n'interrompent rien),
/// encadrement si demandé.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let clean: String = text
        .replace("\r\n", "\r")
        .replace('\n', "\r")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\t' | '\r'))
        .collect();
    if bracketed {
        [
            b"\x1b[200~".as_slice(),
            clean.as_bytes(),
            b"\x1b[201~".as_slice(),
        ]
        .concat()
    } else {
        clean.into_bytes()
    }
}

/// Modes souris du programme, lus dans l'émulateur.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseModes {
    /// Un des modes 1000, 1002 ou 1003 est actif.
    pub reporting: bool,
    /// Mode 1002 ou 1003 : les glissés sont rapportés.
    pub drag: bool,
    /// Mode 1003 : tout mouvement est rapporté, bouton enfoncé ou non.
    pub motion: bool,
    /// Encodage SGR (mode 1006).
    pub sgr: bool,
    pub alt_screen: bool,
    /// Mode 1007 : sur l'écran alterné, la molette devient des flèches.
    pub alternate_scroll: bool,
    pub app_cursor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButtonKind {
    Left,
    Middle,
    Right,
}

/// Un événement de souris au-dessus d'un pane, en cellules à partir de 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press(MouseButtonKind),
    Release(MouseButtonKind),
    /// Mouvement bouton enfoncé.
    Drag(MouseButtonKind),
    /// Mouvement sans bouton.
    Move,
}

/// Encode un événement de souris pour un programme qui a activé un mode souris (spec §6.3) : SGR (1006) si le
/// programme l'a demandé, sinon l'encodage historique, limité aux 223 premières cellules. `None` quand le mode du
/// programme ne rapporte pas cet événement (glissé sans 1002 ni 1003, mouvement sans 1003).
pub fn encode_mouse(
    action: MouseAction,
    column: u16,
    row: u16,
    mods: Mods,
    modes: MouseModes,
) -> Option<Vec<u8>> {
    if !modes.reporting {
        return None;
    }
    let button_code = |button: MouseButtonKind| match button {
        MouseButtonKind::Left => 0u16,
        MouseButtonKind::Middle => 1,
        MouseButtonKind::Right => 2,
    };
    let (base, release) = match action {
        MouseAction::Press(button) => (button_code(button), false),
        MouseAction::Release(button) => (button_code(button), true),
        MouseAction::Drag(button) if modes.drag => (button_code(button) + 32, false),
        MouseAction::Move if modes.motion => (3 + 32, false),
        MouseAction::Drag(_) | MouseAction::Move => return None,
    };
    let modifiers =
        4 * u16::from(mods.shift) + 8 * u16::from(mods.alt) + 16 * u16::from(mods.control);
    if modes.sgr {
        let code = base + modifiers;
        let last = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{};{}{last}", column + 1, row + 1).into_bytes());
    }
    // L'encodage historique ne dit pas quel bouton est relâché : 3.
    let code = if release { 3 } else { base } + modifiers;
    Some(vec![
        0x1b,
        b'[',
        b'M',
        (32 + code).min(255) as u8,
        (33 + column.min(222)) as u8,
        (33 + row.min(222)) as u8,
    ])
}

/// Caractères ASCII qu'un chemin déposé garde tels quels ; tout autre caractère ASCII est échappé à l'antislash.
fn is_plain_in_path(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '/' | '.' | '_' | '-' | '+' | ',' | ':' | '@' | '%' | '=')
}

/// Un chemin déposé, échappé pour le shell comme dans Terminal.app (`/tmp/a\ b.txt`, spec §6.3) : chaque caractère
/// ASCII spécial pour zsh (espace, guillemets, `$`, `*`, parenthèses…) prend un antislash ; les lettres
/// accentuées passent telles quelles.
pub fn escape_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii() && !is_plain_in_path(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Les chemins de fichiers déposés sur un pane, prêts à être insérés comme une saisie : échappés et séparés par une
/// espace. Un chemin qui contient un caractère de contrôle (retour à la ligne compris) refuse tout le dépôt : il
/// est rendu pour le message.
pub fn dropped_paths_text(paths: &[&str]) -> Result<String, String> {
    if let Some(rejected) = paths.iter().find(|path| path.chars().any(char::is_control)) {
        return Err((*rejected).to_string());
    }
    Ok(paths
        .iter()
        .map(|path| escape_path(path))
        .collect::<Vec<_>>()
        .join(" "))
}

/// Ce que fait un cran de molette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WheelAction {
    Send(Vec<u8>),
    /// Défile l'historique de l'émulateur (positif : vers le haut).
    ScrollHistory(i32),
}

/// `lines` positif fait défiler vers le haut. `col` et `row` partent de 0.
pub fn encode_wheel(lines: i32, col: u16, row: u16, modes: MouseModes) -> WheelAction {
    let count = lines.unsigned_abs() as usize;
    if count > 0 && modes.reporting {
        let button: u16 = if lines > 0 { 64 } else { 65 };
        let one = if modes.sgr {
            format!("\x1b[<{button};{};{}M", col + 1, row + 1).into_bytes()
        } else {
            // Encodage historique X10, limité aux 223 premières cellules.
            vec![
                0x1b,
                b'[',
                b'M',
                (32 + button) as u8,
                (33 + col.min(222)) as u8,
                (33 + row.min(222)) as u8,
            ]
        };
        return WheelAction::Send(one.repeat(count));
    }
    if count > 0 && modes.alt_screen && modes.alternate_scroll {
        let arrow: &[u8] = match (lines > 0, modes.app_cursor) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        return WheelAction::Send(arrow.repeat(count));
    }
    WheelAction::ScrollHistory(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods {
        control: false,
        alt: false,
        shift: false,
        command: false,
    };
    const SHIFT: Mods = Mods {
        shift: true,
        ..NONE
    };
    const CTRL: Mods = Mods {
        control: true,
        ..NONE
    };
    const ALT: Mods = Mods { alt: true, ..NONE };
    const PLAIN: KeyModes = KeyModes {
        app_cursor: false,
        extended_keys: false,
    };
    const EXTENDED: KeyModes = KeyModes {
        app_cursor: false,
        extended_keys: true,
    };

    fn key(name: &str, mods: Mods, modes: KeyModes) -> Option<Vec<u8>> {
        encode_key(name, mods, modes)
    }

    #[test]
    fn basic_keys() {
        assert_eq!(key("enter", NONE, PLAIN), Some(b"\r".to_vec()));
        assert_eq!(key("tab", NONE, PLAIN), Some(b"\t".to_vec()));
        assert_eq!(key("tab", SHIFT, PLAIN), Some(b"\x1b[Z".to_vec()));
        assert_eq!(key("backspace", NONE, PLAIN), Some(b"\x7f".to_vec()));
        assert_eq!(key("escape", NONE, PLAIN), Some(b"\x1b".to_vec()));
    }

    #[test]
    fn shift_enter_depends_on_extended_keys() {
        assert_eq!(key("enter", SHIFT, EXTENDED), Some(b"\x1b[13;2u".to_vec()));
        assert_eq!(key("enter", SHIFT, PLAIN), Some(b"\x1b\r".to_vec()));
        assert_eq!(key("enter", ALT, PLAIN), Some(b"\x1b\r".to_vec()));
        assert_eq!(key("enter", CTRL, EXTENDED), Some(b"\x1b[13;5u".to_vec()));
    }

    #[test]
    fn arrows_follow_cursor_mode_and_modifiers() {
        let app = KeyModes {
            app_cursor: true,
            extended_keys: false,
        };
        assert_eq!(key("up", NONE, PLAIN), Some(b"\x1b[A".to_vec()));
        assert_eq!(key("up", NONE, app), Some(b"\x1bOA".to_vec()));
        assert_eq!(key("right", SHIFT, PLAIN), Some(b"\x1b[1;2C".to_vec()));
        assert_eq!(key("left", CTRL, app), Some(b"\x1b[1;5D".to_vec()));
        assert_eq!(key("home", NONE, PLAIN), Some(b"\x1b[H".to_vec()));
    }

    #[test]
    fn option_arrows_jump_words_like_ghostty() {
        assert_eq!(key("left", ALT, PLAIN), Some(b"\x1bb".to_vec()));
        assert_eq!(key("right", ALT, PLAIN), Some(b"\x1bf".to_vec()));
    }

    #[test]
    fn option_letters_go_through_text_input_for_azerty() {
        // ⌥( donne « { » sur un Mac AZERTY : c'est la saisie de texte qui l'envoie.
        assert_eq!(key("5", ALT, PLAIN), None);
        assert_eq!(
            key(
                "l",
                Mods {
                    alt: true,
                    shift: true,
                    ..NONE
                },
                PLAIN
            ),
            None
        );
    }

    #[test]
    fn control_letters_and_punctuation() {
        assert_eq!(key("c", CTRL, PLAIN), Some(vec![0x03]));
        assert_eq!(
            key(
                "a",
                Mods {
                    control: true,
                    alt: true,
                    ..NONE
                },
                PLAIN
            ),
            Some(vec![0x1b, 0x01])
        );
        assert_eq!(key("[", CTRL, PLAIN), Some(vec![0x1b]));
        assert_eq!(key("space", CTRL, PLAIN), Some(vec![0x00]));
        assert_eq!(
            key(
                "a",
                Mods {
                    control: true,
                    shift: true,
                    ..NONE
                },
                EXTENDED
            ),
            Some(b"\x1b[97;6u".to_vec())
        );
    }

    #[test]
    fn idema_shortcuts_are_never_sent() {
        assert_eq!(
            key(
                "v",
                Mods {
                    command: true,
                    ..NONE
                },
                PLAIN
            ),
            None
        );
        assert_eq!(key("tab", CTRL, PLAIN), None);
    }

    #[test]
    fn command_arrows_go_to_the_line_start_and_end() {
        let command = Mods {
            command: true,
            ..NONE
        };
        assert_eq!(key("left", command, PLAIN), Some(b"\x01".to_vec()));
        assert_eq!(key("right", command, PLAIN), Some(b"\x05".to_vec()));
        // ⌘⌥← change de pane (IdemA) ; ⌘⇧← ne part pas au programme.
        let pane = Mods {
            alt: true,
            ..command
        };
        assert_eq!(key("left", pane, PLAIN), None);
        let select = Mods {
            shift: true,
            ..command
        };
        assert_eq!(key("right", select, PLAIN), None);
    }

    #[test]
    fn function_and_editing_keys() {
        assert_eq!(key("f1", NONE, PLAIN), Some(b"\x1bOP".to_vec()));
        assert_eq!(key("f5", NONE, PLAIN), Some(b"\x1b[15~".to_vec()));
        assert_eq!(key("f12", SHIFT, PLAIN), Some(b"\x1b[24;2~".to_vec()));
        assert_eq!(key("delete", NONE, PLAIN), Some(b"\x1b[3~".to_vec()));
        assert_eq!(key("pagedown", NONE, PLAIN), Some(b"\x1b[6~".to_vec()));
    }

    #[test]
    fn plain_letters_go_through_text_input() {
        assert_eq!(key("a", NONE, PLAIN), None);
        assert_eq!(key("space", NONE, PLAIN), None);
    }

    #[test]
    fn paste_normalizes_newlines_and_strips_escape() {
        assert_eq!(encode_paste("a\nb\r\nc", false), b"a\rb\rc".to_vec());
        assert_eq!(
            encode_paste("x\x1b[201~y", true),
            b"\x1b[200~x[201~y\x1b[201~".to_vec()
        );
    }

    #[test]
    fn paste_drops_control_characters_but_keeps_tabs_newlines_and_accents() {
        assert_eq!(
            encode_paste("a\u{9b}31mb é", false),
            "a31mb é".as_bytes().to_vec()
        );
        assert_eq!(
            encode_paste("a\x03b\x04c\x7fd\te\nf", false),
            b"abcd\te\rf".to_vec()
        );
    }

    const SGR: MouseModes = MouseModes {
        reporting: true,
        drag: false,
        motion: false,
        sgr: true,
        alt_screen: false,
        alternate_scroll: false,
        app_cursor: false,
    };

    #[test]
    fn sgr_mouse_reports_presses_releases_and_modifiers() {
        let left = MouseButtonKind::Left;
        assert_eq!(
            encode_mouse(MouseAction::Press(left), 4, 9, NONE, SGR),
            Some(b"\x1b[<0;5;10M".to_vec())
        );
        assert_eq!(
            encode_mouse(MouseAction::Release(left), 4, 9, NONE, SGR),
            Some(b"\x1b[<0;5;10m".to_vec())
        );
        assert_eq!(
            encode_mouse(MouseAction::Press(MouseButtonKind::Right), 0, 0, CTRL, SGR),
            Some(b"\x1b[<18;1;1M".to_vec())
        );
    }

    #[test]
    fn drags_and_moves_follow_the_tracking_mode() {
        let left = MouseButtonKind::Left;
        assert_eq!(encode_mouse(MouseAction::Drag(left), 1, 1, NONE, SGR), None);
        let drag = MouseModes { drag: true, ..SGR };
        assert_eq!(
            encode_mouse(MouseAction::Drag(left), 1, 1, NONE, drag),
            Some(b"\x1b[<32;2;2M".to_vec())
        );
        assert_eq!(encode_mouse(MouseAction::Move, 1, 1, NONE, drag), None);
        let motion = MouseModes {
            motion: true,
            ..drag
        };
        assert_eq!(
            encode_mouse(MouseAction::Move, 1, 1, NONE, motion),
            Some(b"\x1b[<35;2;2M".to_vec())
        );
        assert_eq!(
            encode_mouse(MouseAction::Press(left), 0, 0, NONE, MouseModes::default()),
            None
        );
    }

    #[test]
    fn legacy_mouse_encoding_offsets_and_clamps_cells() {
        let x10 = MouseModes {
            reporting: true,
            ..MouseModes::default()
        };
        assert_eq!(
            encode_mouse(MouseAction::Press(MouseButtonKind::Middle), 0, 0, NONE, x10),
            Some(vec![0x1b, b'[', b'M', 33, 33, 33])
        );
        assert_eq!(
            encode_mouse(
                MouseAction::Release(MouseButtonKind::Middle),
                300,
                2,
                NONE,
                x10
            ),
            Some(vec![0x1b, b'[', b'M', 35, 255, 35])
        );
    }

    #[test]
    fn dropped_paths_are_escaped_and_joined_by_a_space() {
        assert_eq!(
            dropped_paths_text(&["/tmp/idema test.txt", "/Users/r/Café (2).md"]).as_deref(),
            Ok("/tmp/idema\\ test.txt /Users/r/Café\\ \\(2\\).md")
        );
        assert_eq!(
            escape_path("/a/b-c_d.e,f:g@h%i=j+k"),
            "/a/b-c_d.e,f:g@h%i=j+k"
        );
        assert_eq!(
            escape_path("/x/$HOME*'\"`!&;|<>[]{}?#~^"),
            "/x/\\$HOME\\*\\'\\\"\\`\\!\\&\\;\\|\\<\\>\\[\\]\\{\\}\\?\\#\\~\\^"
        );
    }

    #[test]
    fn a_path_with_a_control_character_refuses_the_drop() {
        assert_eq!(
            dropped_paths_text(&["/tmp/ok", "/tmp/a\nb"]),
            Err("/tmp/a\nb".to_string())
        );
        assert!(dropped_paths_text(&["/tmp/tab\there"]).is_err());
        assert!(dropped_paths_text(&["/tmp/c1\u{85}"]).is_err());
    }

    #[test]
    fn wheel_goes_to_program_or_history() {
        let sgr = MouseModes {
            reporting: true,
            sgr: true,
            ..Default::default()
        };
        assert_eq!(
            encode_wheel(2, 4, 9, sgr),
            WheelAction::Send(b"\x1b[<64;5;10M\x1b[<64;5;10M".to_vec())
        );
        assert_eq!(
            encode_wheel(-1, 0, 0, sgr),
            WheelAction::Send(b"\x1b[<65;1;1M".to_vec())
        );
        let x10 = MouseModes {
            reporting: true,
            ..Default::default()
        };
        assert_eq!(
            encode_wheel(1, 0, 0, x10),
            WheelAction::Send(vec![0x1b, b'[', b'M', 96, 33, 33])
        );
        let alternate = MouseModes {
            alt_screen: true,
            alternate_scroll: true,
            ..Default::default()
        };
        assert_eq!(
            encode_wheel(-2, 0, 0, alternate),
            WheelAction::Send(b"\x1b[B\x1b[B".to_vec())
        );
        assert_eq!(
            encode_wheel(3, 0, 0, MouseModes::default()),
            WheelAction::ScrollHistory(3)
        );
    }
}
