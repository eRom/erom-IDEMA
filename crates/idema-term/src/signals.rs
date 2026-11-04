//! Détecteur de signaux d'un pane (spec §5.1) : cloche, notifications, progression et marqueurs OSC 133.
//! Il lit la sortie brute du pane, à côté de l'émulateur, et ne dessine rien.

use alacritty_terminal::vte::{Params, Parser, Perform};

/// Taille maximale gardée d'une charge utile de passthrough `DCS tmux;` sans ESC (BEL seul, en pratique).
const MAX_PASSTHROUGH: usize = 64;

/// Ce que le programme d'un pane signale, hors du dessin de l'écran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// BEL hors d'une chaîne OSC ou DCS, OSC 9 (sauf 9;4), OSC 777;notify ou OSC 99.
    Notify,
    /// OSC 9;4 : vrai pour les états 1 à 4, faux pour l'état 0 (progression finie).
    Progress(bool),
    /// OSC 133;A : le prompt s'affiche.
    PromptStart,
    /// OSC 133;B : fin du prompt, début de la saisie.
    PromptEnd,
    /// OSC 133;C : une commande démarre.
    CommandStart,
    /// OSC 133;D : la commande est finie, avec son code s'il est donné.
    CommandEnd(Option<i32>),
}

#[derive(Default)]
struct Collector {
    signals: Vec<Signal>,
    /// Charge d'un `DCS tmux;` en cours : `Some` entre `hook` et `unhook`.
    dcs: Option<Vec<u8>>,
    /// Charges complètes à relire, ESC dédoublés.
    passthrough: Vec<Vec<u8>>,
}

impl Perform for Collector {
    fn execute(&mut self, byte: u8) {
        if byte == 0x07 {
            self.signals.push(Signal::Notify);
        }
    }

    fn hook(&mut self, _params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // `ESC P tmux;` : vte prend « t » pour l'action finale, « mux; » arrive par `put`.
        self.dcs = (intermediates.is_empty() && action == 't').then(Vec::new);
    }

    fn put(&mut self, byte: u8) {
        if let Some(buffer) = self.dcs.as_mut()
            && buffer.len() < MAX_PASSTHROUGH
        {
            buffer.push(byte);
        }
    }

    fn unhook(&mut self) {
        // vte clôt le DCS au premier ESC : une charge qui en contient (`ESC ESC ] 9 ; …`) se termine ici,
        // et la séquence intérieure est lue directement par le parseur. Reste la charge sans ESC (un BEL).
        if let Some(buffer) = self.dcs.take()
            && let Some(payload) = buffer.strip_prefix(b"mux;")
            && !payload.is_empty()
        {
            self.passthrough.push(payload.to_vec());
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if let Some(signal) = osc_signal(params) {
            self.signals.push(signal);
        }
    }
}

fn osc_signal(params: &[&[u8]]) -> Option<Signal> {
    match params {
        [b"9", b"4", state, ..] => Some(Signal::Progress(matches!(
            *state,
            b"1" | b"2" | b"3" | b"4"
        ))),
        [b"9", b"4"] => None,
        // OSC 9;<n> purement numérique : une sous-commande de ConEmu (9;9, répertoire courant…), pas un message.
        [b"9", code, ..] if !code.is_empty() && code.iter().all(u8::is_ascii_digit) => None,
        [b"9", ..] => Some(Signal::Notify),
        [b"777", b"notify", ..] => Some(Signal::Notify),
        [b"99", ..] => Some(Signal::Notify),
        [b"133", mark, rest @ ..] => match *mark {
            b"A" => Some(Signal::PromptStart),
            b"B" => Some(Signal::PromptEnd),
            b"C" => Some(Signal::CommandStart),
            b"D" => Some(Signal::CommandEnd(
                rest.first()
                    .and_then(|code| std::str::from_utf8(code).ok())
                    .and_then(|code| code.parse().ok()),
            )),
            _ => None,
        },
        _ => None,
    }
}

/// Un parseur par pane, à état : une séquence coupée entre deux `%output` est reprise au paquet suivant.
#[derive(Default)]
pub struct SignalDetector {
    parser: Parser,
    collector: Collector,
}

impl SignalDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lit des octets bruts du pane et rend les signaux qu'ils complètent, dans l'ordre.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Signal> {
        self.parser.advance(&mut self.collector, bytes);
        let mut signals = std::mem::take(&mut self.collector.signals);
        for payload in std::mem::take(&mut self.collector.passthrough) {
            // Un seul niveau de passthrough : la charge est relue par un parseur neuf, sans le déballer encore.
            let mut inner = Collector::default();
            Parser::new().advance(&mut inner, &undouble_escapes(&payload));
            signals.extend(inner.signals);
        }
        signals
    }
}

/// Le passthrough de tmux double chaque ESC de la séquence transportée.
fn undouble_escapes(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut previous_escape = false;
    for &byte in payload {
        if byte == 0x1b && previous_escape {
            previous_escape = false;
            continue;
        }
        previous_escape = byte == 0x1b;
        out.push(byte);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(chunks: &[&[u8]]) -> Vec<Signal> {
        let mut detector = SignalDetector::new();
        chunks
            .iter()
            .flat_map(|chunk| detector.feed(chunk))
            .collect()
    }

    #[test]
    fn bell_outside_a_string_is_a_notification() {
        assert_eq!(detect(&[b"done\x07"]), vec![Signal::Notify]);
    }

    #[test]
    fn bell_that_terminates_an_osc_is_not_a_notification() {
        // Titre de fenêtre terminé par BEL : aucun signal.
        assert_eq!(detect(&[b"\x1b]0;titre\x07"]), vec![]);
    }

    #[test]
    fn osc_9_notifies_but_osc_9_4_reports_progress() {
        assert_eq!(detect(&[b"\x1b]9;fini\x07"]), vec![Signal::Notify]);
        assert_eq!(
            detect(&[
                b"\x1b]9;4;1;50\x07",
                b"\x1b]9;4;3;0\x07",
                b"\x1b]9;4;0;0\x07"
            ]),
            vec![
                Signal::Progress(true),
                Signal::Progress(true),
                Signal::Progress(false)
            ]
        );
        assert_eq!(detect(&[b"\x1b]9;4\x07"]), vec![]);
        // ConEmu : répertoire courant, message numéroté ; un texte qui commence par un chiffre reste un message.
        assert_eq!(detect(&[b"\x1b]9;9;/Users/r\x07"]), vec![]);
        assert_eq!(
            detect(&[b"\x1b]9;3 tests en \xc3\xa9chec\x07"]),
            vec![Signal::Notify]
        );
    }

    #[test]
    fn osc_777_and_osc_99_notify_with_either_terminator() {
        // Échantillons de la sonde du 15/10 (tmux 3.8) : terminés par ST.
        assert_eq!(
            detect(&[b"\x1b]777;notify;titre;corps\x1b\\"]),
            vec![Signal::Notify]
        );
        assert_eq!(
            detect(&[b"\x1b]99;i=1;bonjour\x1b\\"]),
            vec![Signal::Notify]
        );
        assert_eq!(detect(&[b"\x1b]777;other;x\x07"]), vec![]);
    }

    #[test]
    fn osc_133_marks_carry_the_exit_code() {
        assert_eq!(
            detect(&[b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;3\x07\x1b]133;D\x07"]),
            vec![
                Signal::PromptStart,
                Signal::PromptEnd,
                Signal::CommandStart,
                Signal::CommandEnd(Some(3)),
                Signal::CommandEnd(None)
            ]
        );
        assert_eq!(
            detect(&[b"\x1b]133;A;cl=line\x07"]),
            vec![Signal::PromptStart]
        );
    }

    #[test]
    fn sequences_split_between_two_outputs_are_completed() {
        assert_eq!(
            detect(&[b"\x1b]133;D", b";130\x07", b"\x1b]9;fi", b"ni\x1b", b"\\"]),
            vec![Signal::CommandEnd(Some(130)), Signal::Notify]
        );
    }

    #[test]
    fn tmux_passthrough_is_unwrapped() {
        // Échantillon de la sonde du 15/10 : la séquence intérieure, ESC doublé, puis ST.
        assert_eq!(
            detect(&[b"\x1bPtmux;\x1b\x1b]9;passthrough\x07\x1b\\"]),
            vec![Signal::Notify]
        );
        // Charge sans ESC : la cloche transportée compte, coupée ou non.
        assert_eq!(detect(&[b"\x1bPtmux;\x07\x1b\\"]), vec![Signal::Notify]);
        assert_eq!(
            detect(&[b"\x1bPtmu", b"x;\x07\x1b", b"\\"]),
            vec![Signal::Notify]
        );
    }

    #[test]
    fn bell_inside_another_dcs_is_ignored() {
        assert_eq!(detect(&[b"\x1bPq\x07\x1b\\"]), vec![]);
    }

    #[test]
    fn plain_output_yields_nothing() {
        assert_eq!(detect(&[b"\x1b[1;31mrouge\x1b[0m\r\n"]), vec![]);
    }
}
