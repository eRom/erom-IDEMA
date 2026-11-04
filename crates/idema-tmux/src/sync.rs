//! Relecture de l'état d'un pane dans tmux, et rejeu dans un émulateur (spec §6.2).

use crate::command::quote;
use crate::protocol::PaneId;

/// Lignes d'historique rechargées au plus (spec §6.2).
pub const HISTORY_REPLAY: u32 = 2000;

/// Format passé à `display-message -p`. `cursor_shape` est un mot, `pane_key_mode` vient en dernier :
/// sa valeur peut contenir une espace.
pub const STATE_FORMAT: &str = "#{cursor_x} #{cursor_y} #{cursor_flag} #{alternate_on} \
#{keypad_cursor_flag} #{keypad_flag} #{bracket_paste_flag} #{mouse_standard_flag} \
#{mouse_button_flag} #{mouse_all_flag} #{mouse_sgr_flag} #{pane_width} #{pane_height} \
#{scroll_region_upper} #{scroll_region_lower} #{cursor_blinking} #{cursor_shape} #{pane_key_mode}";

/// Forme du curseur, telle que tmux 3.8 la rapporte (`cursor_shape`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    /// Forme par défaut du terminal : DECSCUSR 0.
    Default,
    Block,
    Underline,
    Bar,
}

impl CursorShape {
    fn parse(word: &str) -> Self {
        match word {
            "block" => Self::Block,
            "underline" => Self::Underline,
            "bar" => Self::Bar,
            _ => Self::Default,
        }
    }

    /// Paramètre de DECSCUSR (`CSI Ps SP q`) : 1 et 2 pour le bloc, 3 et 4 le soulignement, 5 et 6 la barre ;
    /// l'impair clignote, le pair reste fixe.
    fn decscusr(self, blinking: bool) -> u8 {
        let base = match self {
            Self::Default => return 0,
            Self::Block => 1,
            Self::Underline => 3,
            Self::Bar => 5,
        };
        if blinking { base } else { base + 1 }
    }
}

/// État d'un pane relu dans tmux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneState {
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: bool,
    pub alternate_on: bool,
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    pub mouse_standard: bool,
    pub mouse_button: bool,
    pub mouse_all: bool,
    pub mouse_sgr: bool,
    pub width: u16,
    pub height: u16,
    /// Haut de la zone de défilement, rangée à partir de 0 (`scroll_region_upper`).
    pub scroll_region_upper: u16,
    /// Bas de la zone de défilement, rangée à partir de 0, incluse (`scroll_region_lower`).
    pub scroll_region_lower: u16,
    pub cursor_blinking: bool,
    pub cursor_shape: CursorShape,
    /// Valeur brute de `pane_key_mode`, à confirmer sur tmux 3.8 (tâche 5).
    pub key_mode: String,
}

pub fn parse_state(line: &str) -> Option<PaneState> {
    let mut tokens = line.splitn(18, ' ');
    let mut numbers = [0u16; 16];
    for slot in numbers.iter_mut() {
        *slot = tokens.next()?.parse().ok()?;
    }
    let cursor_shape = CursorShape::parse(tokens.next()?);
    let key_mode = tokens.next().unwrap_or("").to_string();
    let [
        cx,
        cy,
        cursor,
        alt,
        kcur,
        kpad,
        paste,
        standard,
        button,
        all,
        sgr,
        width,
        height,
        region_upper,
        region_lower,
        blinking,
    ] = numbers;
    Some(PaneState {
        cursor_x: cx,
        cursor_y: cy,
        cursor_visible: cursor != 0,
        alternate_on: alt != 0,
        app_cursor: kcur != 0,
        app_keypad: kpad != 0,
        bracketed_paste: paste != 0,
        mouse_standard: standard != 0,
        mouse_button: button != 0,
        mouse_all: all != 0,
        mouse_sgr: sgr != 0,
        width,
        height,
        scroll_region_upper: region_upper,
        scroll_region_lower: region_lower,
        cursor_blinking: blinking != 0,
        cursor_shape,
        key_mode,
    })
}

/// Relecture complète (attache, pause) ou de l'écran visible seul (après un redimensionnement).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncKind {
    /// Historique et écran, rejoués dans un émulateur neuf, construit hors du fil d'interface.
    Full,
    /// Écran visible seul, repeint dans l'émulateur existant, qui garde son historique.
    Visible,
}

/// Ce qu'il faut pour reconstruire ou repeindre un émulateur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub kind: SyncKind,
    pub state: PaneState,
    /// Écran principal : historique et écran, ou historique et écran sauvegardé si l'écran alterné est actif.
    pub primary: Vec<Vec<u8>>,
    /// Écran alterné visible, seul (il n'a pas d'historique), s'il est actif.
    pub alternate: Option<Vec<Vec<u8>>>,
}

/// Où en est une relecture après avoir reçu des lignes de réponse.
#[derive(Debug, PartialEq, Eq)]
pub enum SyncProgress {
    /// Le marqueur de fin n'est pas encore passé.
    Pending,
    /// Le marqueur de fin est passé, la réponse est exploitable.
    Done(Snapshot),
    /// Le marqueur de fin est passé, mais la réponse est inexploitable (état illisible, séparateur absent).
    Failed,
}

/// Une relecture en cours. Les marqueurs encadrent les réponses, quel que soit le découpage
/// de tmux en blocs (un par commande, ou un pour toute la ligne).
#[derive(Debug)]
pub struct SyncRequest {
    pane: PaneId,
    kind: SyncKind,
    start: Vec<u8>,
    screen: Vec<u8>,
    separator: Vec<u8>,
    end: Vec<u8>,
    collected: Option<Vec<Vec<u8>>>,
}

impl SyncRequest {
    /// `token` doit être unique par relecture (identifiant de process, pane et compteur : toutes les sessions de panes reçoivent les mêmes réponses).
    pub fn new(pane: PaneId, kind: SyncKind, token: &str) -> Self {
        let marker = |part: &str| format!("idema-sync-{token}-{part}").into_bytes();
        Self {
            pane,
            kind,
            start: marker("start"),
            screen: marker("screen"),
            separator: marker("sep"),
            end: marker("end"),
            collected: None,
        }
    }

    /// Une seule ligne de commandes. tmux exécute une ligne d'un trait, sans lire la sortie
    /// des panes entre deux commandes : l'état et les captures décrivent le même instant.
    pub fn command_line(&self) -> String {
        let pane = format!("%{}", self.pane.0);
        let text = |bytes: &[u8]| quote(&String::from_utf8_lossy(bytes));
        // Sans -J : une rangée par ligne, les cellules vides gardent leur fond (tmux les retire avec -J, même avec -N).
        let screen = format!("capture-pane -p -e -N -t {pane}");
        // Sépare l'historique de l'écran dans la réponse (voir `build`).
        let mark = format!("display-message -p {}", text(&self.screen));
        let (visible, saved) = match self.kind {
            // L'historique est recollé par -J (l'émulateur neuf replie les lignes à sa largeur), l'écran visible
            // est lu sans -J. `-E -1` s'arrête à la dernière ligne d'historique ; avec un historique vide, tmux
            // rendrait la rangée 0 de l'écran (sonde du 16/10) : la capture n'a lieu que s'il y a un historique.
            SyncKind::Full => {
                let history =
                    format!("capture-pane -p -e -J -N -S -{HISTORY_REPLAY} -E -1 -t {pane}");
                (
                    format!(
                        "if-shell -F -t {pane} {} {} ; {mark} ; {screen}",
                        quote("#{history_size}"),
                        quote(&history)
                    ),
                    format!("capture-pane -p -e -N -a -q -t {pane}"),
                )
            }
            SyncKind::Visible => (
                format!("{mark} ; {screen}"),
                format!("capture-pane -p -e -N -a -q -t {pane}"),
            ),
        };
        [
            format!("display-message -p {}", text(&self.start)),
            format!("display-message -p -t {pane} {}", quote(STATE_FORMAT)),
            visible,
            format!("display-message -p {}", text(&self.separator)),
            saved,
            format!("display-message -p {}", text(&self.end)),
        ]
        .join(" ; ")
    }

    /// Le marqueur de début est passé, pas encore celui de fin. tmux exécute une ligne de commandes d'un client d'un
    /// trait : une erreur reçue maintenant vient d'une commande de cette relecture.
    pub fn has_started(&self) -> bool {
        self.collected.is_some()
    }

    /// Reçoit les lignes de chaque réponse. Renvoie l'instantané quand le marqueur de fin passe.
    pub fn accept(&mut self, lines: &[Vec<u8>]) -> Option<Snapshot> {
        match self.advance(lines) {
            SyncProgress::Done(snapshot) => Some(snapshot),
            SyncProgress::Pending | SyncProgress::Failed => None,
        }
    }

    /// Comme `accept`, mais distingue une relecture inachevée d'une relecture terminée sans instantané.
    pub fn advance(&mut self, lines: &[Vec<u8>]) -> SyncProgress {
        for line in lines {
            if *line == self.start {
                self.collected = Some(Vec::new());
            } else if *line == self.end {
                return match self.collected.take().and_then(|body| self.build(body)) {
                    Some(snapshot) => SyncProgress::Done(snapshot),
                    None => SyncProgress::Failed,
                };
            } else if let Some(body) = self.collected.as_mut() {
                body.push(line.clone());
            }
        }
        SyncProgress::Pending
    }

    fn build(&self, body: Vec<Vec<u8>>) -> Option<Snapshot> {
        let (state_line, rest) = body.split_first()?;
        let state = parse_state(&String::from_utf8_lossy(state_line))?;
        let at = rest.iter().position(|line| *line == self.separator)?;
        let (history, screen) = {
            let block = &rest[..at];
            let boundary = block.iter().position(|line| *line == self.screen)?;
            (&block[..boundary], &block[boundary + 1..])
        };
        // Avec -e, tmux ne répète pas les attributs d'une ligne à l'autre dans UNE capture, mais la capture de
        // l'écran repart de l'état par défaut : elle n'émet rien pour une première rangée sans attribut, et
        // l'émulateur garderait ceux de la fin de l'historique (sonde du 16/10, rouge encore actif).
        let after_history = |screen: &[Vec<u8>]| {
            let mut lines = history.to_vec();
            lines.extend_from_slice(screen);
            if !history.is_empty()
                && let Some(first) = lines.get_mut(history.len())
            {
                first.splice(0..0, b"\x1b[0m".iter().copied());
            }
            lines
        };
        let kind = self.kind;
        Some(if state.alternate_on {
            // L'historique est celui de l'écran principal (sonde du 16/10 : `capture-pane -S -E` le rend aussi en
            // écran alterné) et l'écran alterné n'en a pas : `primary` = historique + écran sauvegardé,
            // `alternate` = l'écran alterné seul.
            Snapshot {
                kind,
                state,
                primary: after_history(&rest[at + 1..]),
                alternate: Some(screen.to_vec()),
            }
        } else {
            Snapshot {
                kind,
                state,
                primary: after_history(screen),
                alternate: None,
            }
        })
    }
}

/// Relie un pane à son émulateur pendant une relecture. Tant qu'elle est en vol, la sortie
/// est jetée : la capture l'inclut déjà. Après le marqueur de fin, la sortie passe de nouveau.
#[derive(Debug, Default)]
pub struct PaneLink {
    pending: Option<SyncRequest>,
}

impl PaneLink {
    /// Arme une relecture et renvoie la ligne de commandes à envoyer à tmux.
    pub fn start_sync(&mut self, request: SyncRequest) -> String {
        let line = request.command_line();
        self.pending = Some(request);
        line
    }

    pub fn is_syncing(&self) -> bool {
        self.pending.is_some()
    }

    /// La sortie à donner à l'émulateur, ou `None` pendant une relecture.
    pub fn on_output<'a>(&self, data: &'a [u8]) -> Option<&'a [u8]> {
        if self.pending.is_some() {
            None
        } else {
            Some(data)
        }
    }

    /// Les lignes d'une réponse. Renvoie l'instantané quand la relecture se termine.
    pub fn on_reply(&mut self, lines: &[Vec<u8>]) -> Option<Snapshot> {
        match self.on_reply_progress(lines) {
            SyncProgress::Done(snapshot) => Some(snapshot),
            SyncProgress::Pending | SyncProgress::Failed => None,
        }
    }

    /// Comme `on_reply`, mais signale aussi une relecture terminée sans instantané.
    /// Dans les deux cas de fin, la relecture est levée et la sortie repasse.
    pub fn on_reply_progress(&mut self, lines: &[Vec<u8>]) -> SyncProgress {
        let Some(request) = self.pending.as_mut() else {
            return SyncProgress::Pending;
        };
        let progress = request.advance(lines);
        if progress != SyncProgress::Pending {
            self.pending = None;
        }
        progress
    }

    /// Abandonne la relecture en cours. Renvoie `true` s'il y en avait une.
    pub fn abort_sync(&mut self) -> bool {
        self.pending.take().is_some()
    }

    /// Une réponse en erreur (`%error`). Elle n'arrête la relecture que si son marqueur de début est passé, avant
    /// ou dans cette réponse : sinon l'erreur est celle d'une autre commande du client (celle d'un autre pane de la
    /// session, par exemple), et la relecture continue. Vrai si la relecture est abandonnée.
    pub fn on_failed_reply(&mut self, lines: &[Vec<u8>]) -> bool {
        let Some(request) = self.pending.as_mut() else {
            return false;
        };
        if request.advance(lines) == SyncProgress::Pending && !request.has_started() {
            return false;
        }
        self.pending = None;
        true
    }
}

/// Octets à donner à un émulateur neuf, de la taille du pane, pour reproduire l'état relu.
/// Les lignes s'enchaînent sans remise à zéro des attributs : avec `-e`, tmux ne les répète pas d'une ligne à l'autre
/// (seul le fond est retiré le temps du saut de ligne, voir `append_lines`).
pub fn replay_bytes(snapshot: &Snapshot) -> Vec<u8> {
    let mut out = Vec::new();
    append_lines(&mut out, &snapshot.primary);
    if let Some(screen) = &snapshot.alternate {
        out.extend_from_slice(b"\x1b[0m\x1b[?1049h\x1b[H\x1b[2J");
        append_lines(&mut out, screen);
    }
    out.extend_from_slice(&cursor_and_modes(&snapshot.state));
    out
}

/// Octets qui repeignent l'écran visible d'un émulateur existant, sans toucher à son historique.
/// Effacement ligne par ligne (EL 2) : ED 2 pousserait l'écran dans l'historique d'`alacritty_terminal`
/// (constaté le 14/10 par le test `repaint_replaces_the_screen_and_keeps_history`).
pub fn repaint_bytes(snapshot: &Snapshot) -> Vec<u8> {
    let screen = snapshot.alternate.as_ref().unwrap_or(&snapshot.primary);
    // Zone de défilement pleine et curseur par défaut pendant la peinture : un saut de ligne en bas d'une
    // zone réduite ferait défiler la zone au lieu de passer à la rangée suivante. `cursor_and_modes` rétablit l'état relu.
    let mut out = b"\x1b[0m\x1b[r\x1b[0 q".to_vec();
    for row in 1..=snapshot.state.height {
        out.extend_from_slice(format!("\x1b[{row};1H\x1b[2K").as_bytes());
    }
    out.extend_from_slice(b"\x1b[H");
    append_lines(&mut out, screen);
    out.extend_from_slice(&cursor_and_modes(&snapshot.state));
    out
}

/// Zone de défilement, forme du curseur, position du curseur, puis modes. DECSTBM ramène le curseur
/// en haut à gauche : la zone vient donc avant le positionnement. Une zone pleine hauteur est l'état par défaut.
fn cursor_and_modes(state: &PaneState) -> Vec<u8> {
    let mut out = b"\x1b[0m".to_vec();
    let (upper, lower) = (state.scroll_region_upper, state.scroll_region_lower);
    if upper < lower && lower < state.height && !(upper == 0 && lower + 1 == state.height) {
        out.extend_from_slice(format!("\x1b[{};{}r", upper + 1, lower + 1).as_bytes());
    }
    if state.cursor_shape != CursorShape::Default {
        let code = state.cursor_shape.decscusr(state.cursor_blinking);
        out.extend_from_slice(format!("\x1b[{code} q").as_bytes());
    }
    out.extend_from_slice(
        format!("\x1b[{};{}H", state.cursor_y + 1, state.cursor_x + 1).as_bytes(),
    );
    let modes: [(bool, &[u8]); 8] = [
        (!state.cursor_visible, b"\x1b[?25l"),
        (state.app_cursor, b"\x1b[?1h"),
        (state.app_keypad, b"\x1b="),
        (state.bracketed_paste, b"\x1b[?2004h"),
        (state.mouse_standard, b"\x1b[?1000h"),
        (state.mouse_button, b"\x1b[?1002h"),
        (state.mouse_all, b"\x1b[?1003h"),
        (state.mouse_sgr, b"\x1b[?1006h"),
    ];
    for (active, sequence) in modes {
        if active {
            out.extend_from_slice(sequence);
        }
    }
    out
}

/// Enchaîne les lignes par des sauts de ligne. Sondé le 16/10 (tmux 3.8) : l'état d'une ligne se prolonge sur la
/// suivante sans être répété, donc aucune remise à zéro générale ne convient. Le seul état que le saut de ligne
/// transmet est le fond : quand l'écran défile, l'émulateur remplit la rangée neuve avec le fond actif (BCE) et tmux
/// ne réécrit ensuite que les cellules qu'il a émises. Le fond est donc retiré avant le saut et rétabli juste après.
fn append_lines(out: &mut Vec<u8>, lines: &[Vec<u8>]) {
    let mut background: Option<Vec<u8>> = None;
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            match &background {
                Some(active) => {
                    out.extend_from_slice(b"\x1b[49m\r\n\x1b[");
                    out.extend_from_slice(active);
                    out.push(b'm');
                }
                None => out.extend_from_slice(b"\r\n"),
            }
        }
        out.extend_from_slice(line);
        track_background(line, &mut background);
    }
}

/// Met à jour le fond actif (les paramètres SGR qui le posent, tels que tmux les a écrits) après une ligne.
fn track_background(line: &[u8], background: &mut Option<Vec<u8>>) {
    let mut at = 0;
    while let Some(offset) = line[at..].windows(2).position(|pair| pair == b"\x1b[") {
        let params_start = at + offset + 2;
        let Some(length) = line[params_start..]
            .iter()
            .position(|byte| (0x40..=0x7e).contains(byte))
        else {
            return;
        };
        let params = &line[params_start..params_start + length];
        if line[params_start + length] == b'm'
            && params
                .iter()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b':'))
        {
            apply_sgr(params, background);
        }
        at = params_start + length + 1;
    }
}

fn apply_sgr(params: &[u8], background: &mut Option<Vec<u8>>) {
    let tokens: Vec<&[u8]> = params.split(|byte| *byte == b';').collect();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        index += 1;
        // Forme à deux-points : le paramètre porte toute la couleur (`48:2::r:g:b`), les autres (`38:…`, `4:3`) sont sans effet.
        if token.starts_with(b"48:") {
            *background = Some(token.to_vec());
            continue;
        }
        let code = if token.is_empty() {
            Some(0)
        } else {
            std::str::from_utf8(token)
                .ok()
                .and_then(|text| text.parse::<u16>().ok())
        };
        match code {
            // Paramètre vide, `0` et `00` : remise à zéro ; `49` : fond par défaut.
            Some(0 | 49) => *background = None,
            Some(40..=47 | 100..=107) => {
                *background = code.map(|code| code.to_string().into_bytes());
            }
            // 38;5;n, 38;2;r;g;b et les mêmes pour 58 : leurs arguments (qui peuvent valoir 40 à 49) ne sont pas des fonds.
            Some(38 | 58 | 48) => {
                let extra = match tokens
                    .get(index)
                    .and_then(|next| std::str::from_utf8(next).ok())
                {
                    Some("5") => 2,
                    Some("2") => 4,
                    _ => 0,
                };
                if code == Some(48) {
                    let end = (index + extra).min(tokens.len());
                    *background = Some(tokens[index - 1..end].join(&b';'));
                }
                index += extra;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<Vec<u8>> {
        items.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    const STATE_LINE: &str = "4 9 1 0 0 0 1 0 0 0 0 80 24 0 23 0 default VT10x";

    #[test]
    fn parse_state_reads_numbers_and_key_mode_with_space() {
        let state = parse_state("4 9 0 1 1 1 1 1 0 0 1 120 40 0 39 0 default Ext 2").unwrap();
        assert_eq!(
            (state.cursor_x, state.cursor_y, state.width, state.height),
            (4, 9, 120, 40)
        );
        assert!(!state.cursor_visible && state.alternate_on && state.app_cursor && state.mouse_sgr);
        assert_eq!(state.key_mode, "Ext 2");
        assert_eq!(parse_state("not a state"), None);
    }

    #[test]
    fn full_command_line_is_a_single_ordered_line() {
        let line = SyncRequest::new(PaneId(7), SyncKind::Full, "42").command_line();
        assert!(line.starts_with(
            "display-message -p 'idema-sync-42-start' ; display-message -p -t %7 '#{cursor_x} "
        ));
        assert!(line.ends_with(
            " ; capture-pane -p -e -N -a -q -t %7 ; display-message -p 'idema-sync-42-end'"
        ));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn full_command_line_joins_the_history_and_keeps_the_screen_rows() {
        let line = SyncRequest::new(PaneId(7), SyncKind::Full, "44").command_line();
        let before_separator = line.split("idema-sync-44-sep").next().unwrap();
        // L'historique (recollé par -J) d'abord, puis l'écran visible sans -J, avant le séparateur.
        let history = before_separator
            .find("capture-pane -p -e -J -N -S -2000 -E -1 -t %7")
            .expect("history capture with -J");
        let screen = before_separator
            .find(" ; display-message -p 'idema-sync-44-screen' ; capture-pane -p -e -N -t %7 ; ")
            .expect("screen marker then screen capture without -J");
        assert!(history < screen);
        // Un historique vide ferait rendre la première rangée de l'écran par -E -1 : la capture est gardée.
        assert!(before_separator.contains("if-shell -F -t %7 '#{history_size}' 'capture-pane"));
        // Aucune autre capture ne recolle les rangées : le fond des cellules vides serait perdu.
        assert_eq!(line.matches("-J").count(), 1);
    }

    #[test]
    fn visible_command_line_keeps_rows_and_skips_history() {
        let line = SyncRequest::new(PaneId(7), SyncKind::Visible, "43").command_line();
        assert!(line.contains(" ; capture-pane -p -e -N -t %7 ; "));
        assert!(!line.contains("-S -"));
        assert!(!line.contains(" -J "));
    }

    #[test]
    fn accept_works_with_one_block_per_command() {
        let mut request = SyncRequest::new(PaneId(1), SyncKind::Full, "t");
        assert_eq!(request.accept(&lines(&["idema-sync-t-start"])), None);
        assert_eq!(request.accept(&lines(&[STATE_LINE])), None);
        assert_eq!(request.accept(&lines(&["idema-sync-t-screen"])), None);
        assert_eq!(request.accept(&lines(&["$ ls", "a b"])), None);
        assert_eq!(request.accept(&lines(&["idema-sync-t-sep"])), None);
        assert_eq!(request.accept(&[]), None);
        let snapshot = request.accept(&lines(&["idema-sync-t-end"])).unwrap();
        assert_eq!(snapshot.kind, SyncKind::Full);
        assert_eq!(snapshot.primary, lines(&["$ ls", "a b"]));
        assert_eq!(snapshot.alternate, None);
    }

    #[test]
    fn accept_works_with_one_block_for_the_whole_line() {
        let mut request = SyncRequest::new(PaneId(1), SyncKind::Full, "t");
        let alt_state = "0 0 1 1 0 0 0 0 0 0 0 80 24 0 23 0 default VT10x";
        let snapshot = request
            .accept(&lines(&[
                "idema-sync-t-start",
                alt_state,
                "idema-sync-t-screen",
                "claude ui",
                "idema-sync-t-sep",
                "$ claude",
                "idema-sync-t-end",
            ]))
            .unwrap();
        assert_eq!(snapshot.alternate, Some(lines(&["claude ui"])));
        assert_eq!(snapshot.primary, lines(&["$ claude"]));
    }

    #[test]
    fn replies_before_the_start_marker_are_ignored() {
        let mut request = SyncRequest::new(PaneId(1), SyncKind::Full, "t");
        assert_eq!(
            request.accept(&lines(&["output of an older command"])),
            None
        );
        let snapshot = request
            .accept(&lines(&[
                "idema-sync-t-start",
                STATE_LINE,
                "idema-sync-t-screen",
                "idema-sync-t-sep",
                "idema-sync-t-end",
            ]))
            .unwrap();
        assert!(snapshot.primary.is_empty());
    }

    #[test]
    fn output_during_a_resync_is_dropped_then_flows_again() {
        let mut link = PaneLink::default();
        assert_eq!(link.on_output(b"before"), Some(b"before".as_slice()));
        let line = link.start_sync(SyncRequest::new(PaneId(2), SyncKind::Full, "z"));
        assert!(line.contains("idema-sync-z-start"));
        assert_eq!(link.on_output(b"during"), None);
        assert_eq!(
            link.on_reply(&lines(&[
                "idema-sync-z-start",
                STATE_LINE,
                "idema-sync-z-screen",
                "idema-sync-z-sep"
            ])),
            None
        );
        assert_eq!(link.on_output(b"still during"), None);
        let snapshot = link.on_reply(&lines(&["idema-sync-z-end"]));
        assert!(snapshot.is_some());
        assert!(!link.is_syncing());
        assert_eq!(link.on_output(b"after"), Some(b"after".as_slice()));
    }

    #[test]
    fn failed_build_clears_the_pending_sync_and_reports_it() {
        let mut link = PaneLink::default();
        link.start_sync(SyncRequest::new(PaneId(2), SyncKind::Full, "f"));
        let progress = link.on_reply_progress(&lines(&[
            "idema-sync-f-start",
            "garbage",
            "idema-sync-f-end",
        ]));
        assert_eq!(progress, SyncProgress::Failed);
        assert!(!link.is_syncing());
        assert_eq!(link.on_output(b"after"), Some(b"after".as_slice()));
    }

    #[test]
    fn missing_separator_is_a_failed_sync() {
        let mut request = SyncRequest::new(PaneId(1), SyncKind::Full, "m");
        let progress = request.advance(&lines(&[
            "idema-sync-m-start",
            STATE_LINE,
            "idema-sync-m-end",
        ]));
        assert_eq!(progress, SyncProgress::Failed);
    }

    #[test]
    fn a_missing_screen_marker_is_a_failed_sync() {
        let mut request = SyncRequest::new(PaneId(1), SyncKind::Full, "n");
        let progress = request.advance(&lines(&[
            "idema-sync-n-start",
            STATE_LINE,
            "idema-sync-n-sep",
            "idema-sync-n-end",
        ]));
        assert_eq!(progress, SyncProgress::Failed);
    }

    #[test]
    fn the_first_screen_row_resets_the_attributes_left_by_the_history() {
        // Sonde du 16/10 : l'historique finit sur du rouge encore actif, la capture de l'écran n'émet aucun SGR.
        let mut request = SyncRequest::new(PaneId(1), SyncKind::Full, "h");
        let snapshot = request
            .accept(&lines(&[
                "idema-sync-h-start",
                STATE_LINE,
                "\x1b[31mred1",
                "red2",
                "idema-sync-h-screen",
                "plain1",
                "plain2",
                "idema-sync-h-sep",
                "idema-sync-h-end",
            ]))
            .unwrap();
        assert_eq!(
            snapshot.primary,
            lines(&["\x1b[31mred1", "red2", "\x1b[0mplain1", "plain2"])
        );
    }

    #[test]
    fn the_screen_is_left_untouched_when_there_is_no_history() {
        let mut request = SyncRequest::new(PaneId(1), SyncKind::Full, "e");
        let snapshot = request
            .accept(&lines(&[
                "idema-sync-e-start",
                STATE_LINE,
                "idema-sync-e-screen",
                "plain1",
                "idema-sync-e-sep",
                "idema-sync-e-end",
            ]))
            .unwrap();
        assert_eq!(snapshot.primary, lines(&["plain1"]));
    }

    #[test]
    fn an_error_before_the_start_marker_belongs_to_another_command() {
        let mut link = PaneLink::default();
        link.start_sync(SyncRequest::new(PaneId(2), SyncKind::Full, "e"));
        assert!(!link.on_failed_reply(&lines(&["can't find pane: %9"])));
        assert!(link.is_syncing());
        assert!(link.on_failed_reply(&lines(&["idema-sync-e-start", "can't find pane: %2"])));
        assert!(!link.is_syncing());
        assert!(!link.on_failed_reply(&[]));
    }

    #[test]
    fn an_error_after_the_start_marker_ends_the_sync() {
        let mut link = PaneLink::default();
        link.start_sync(SyncRequest::new(PaneId(2), SyncKind::Full, "s"));
        assert_eq!(link.on_reply(&lines(&["idema-sync-s-start"])), None);
        assert!(link.on_failed_reply(&[]));
        assert_eq!(link.on_output(b"flows"), Some(b"flows".as_slice()));
    }

    #[test]
    fn aborting_a_sync_lets_output_flow() {
        let mut link = PaneLink::default();
        assert!(!link.abort_sync());
        link.start_sync(SyncRequest::new(PaneId(2), SyncKind::Visible, "a"));
        assert!(link.abort_sync());
        assert!(!link.is_syncing());
    }

    /// Le fond actif après `line`, en partant de `start`.
    fn background_after(start: Option<&str>, line: &str) -> Option<String> {
        let mut background = start.map(|value| value.as_bytes().to_vec());
        track_background(line.as_bytes(), &mut background);
        background.map(|value| String::from_utf8(value).unwrap())
    }

    /// Chaque cas porte son entrée dans le message : un échec dit quelle valeur a échoué.
    fn check_backgrounds(cases: &[(Option<&str>, &str, Option<&str>)]) {
        for &(start, line, expected) in cases {
            assert_eq!(
                background_after(start, line).as_deref(),
                expected,
                "line {line:?} from {start:?}"
            );
        }
    }

    #[test]
    fn a_background_is_tracked_in_every_form_tmux_writes() {
        check_backgrounds(&[
            (None, "\x1b[40mx", Some("40")),
            (None, "\x1b[44mx", Some("44")),
            (None, "\x1b[47mx", Some("47")),
            (None, "\x1b[100mx", Some("100")),
            (None, "\x1b[104mx", Some("104")),
            (None, "\x1b[107mx", Some("107")),
            (None, "\x1b[48;5;12mx", Some("48;5;12")),
            (None, "\x1b[48;2;1;2;3mx", Some("48;2;1;2;3")),
            (None, "\x1b[48:2::10:20:30mx", Some("48:2::10:20:30")),
            (None, "\x1b[1;4;44;3mx", Some("44")),
            // Plusieurs séquences : la dernière gagne.
            (None, "\x1b[44ma\x1b[42mb", Some("42")),
            // Pas des fonds : premier plan, hors plage, séquence qui n'est pas un SGR.
            (None, "\x1b[31mx", None),
            (None, "\x1b[39mx", None),
            (None, "\x1b[108mx", None),
            (Some("44"), "\x1b[44Hx", Some("44")),
            (Some("44"), "\x1b[?44hx", Some("44")),
            // Une séquence coupée en fin de ligne ne change rien.
            (Some("44"), "text\x1b[4", Some("44")),
        ]);
    }

    #[test]
    fn a_reset_clears_the_background() {
        check_backgrounds(&[
            (Some("44"), "\x1b[0mx", None),
            (Some("44"), "\x1b[00mx", None),
            (Some("44"), "\x1b[mx", None),
            (Some("44"), "\x1b[;mx", None),
            (Some("44"), "\x1b[49mx", None),
            (Some("104"), "\x1b[49mx", None),
            (Some("48;5;12"), "\x1b[0mx", None),
            (Some("48:2::1:2:3"), "\x1b[0mx", None),
            // Dans une même séquence, l'ordre décide.
            (Some("44"), "\x1b[44;0mx", None),
            (Some("44"), "\x1b[0;42mx", Some("42")),
            (Some("44"), "\x1b[00;42mx", Some("42")),
            // Un attribut sans rapport laisse le fond.
            (Some("44"), "\x1b[1mx", Some("44")),
            (Some("44"), "\x1b[22;39mx", Some("44")),
        ]);
    }

    #[test]
    fn the_arguments_of_a_foreground_or_underline_colour_are_not_backgrounds() {
        check_backgrounds(&[
            (None, "\x1b[38;5;44mx", None),
            (None, "\x1b[38;2;44;44;44mx", None),
            (None, "\x1b[58;5;44mx", None),
            (None, "\x1b[58;2;44;44;44mx", None),
            (None, "\x1b[58;2;1;2;3mx", None),
            // Forme à deux-points : sans effet sur le fond, et un fond qui suit est vu.
            (None, "\x1b[38:5:44mx", None),
            (None, "\x1b[38:2::44:44:44mx", None),
            (None, "\x1b[58:5:44mx", None),
            (None, "\x1b[58:2::44:44:44mx", None),
            (Some("42"), "\x1b[38:2::44:44:44mx", Some("42")),
            (Some("42"), "\x1b[58:2::44:44:44mx", Some("42")),
            (None, "\x1b[38:5:44;45mx", Some("45")),
            (Some("42"), "\x1b[38;5;44mx", Some("42")),
            (Some("42"), "\x1b[38;2;44;44;44mx", Some("42")),
            (Some("42"), "\x1b[58;5;44mx", Some("42")),
            (Some("42"), "\x1b[58;2;44;44;44mx", Some("42")),
            // Un vrai fond qui suit ces arguments est vu.
            (None, "\x1b[38;5;44;45mx", Some("45")),
            (None, "\x1b[38;2;44;44;44;45mx", Some("45")),
            (None, "\x1b[58;5;44;48;5;9mx", Some("48;5;9")),
        ]);
    }

    #[test]
    fn a_background_is_cleared_before_a_line_break_and_restored_after_it() {
        let mut out = Vec::new();
        append_lines(
            &mut out,
            &lines(&["\x1b[104mblue", "next", "\x1b[49mplain", "end"]),
        );
        assert_eq!(
            out,
            b"\x1b[104mblue\x1b[49m\r\n\x1b[104mnext\x1b[49m\r\n\x1b[104m\x1b[49mplain\r\nend"
                .to_vec()
        );
    }

    #[test]
    fn replay_rebuilds_screen_cursor_and_modes() {
        let snapshot = Snapshot {
            kind: SyncKind::Full,
            state: parse_state(STATE_LINE).unwrap(),
            primary: lines(&["\x1b[31mred", "still red"]),
            alternate: None,
        };
        assert_eq!(
            replay_bytes(&snapshot),
            b"\x1b[31mred\r\nstill red\x1b[0m\x1b[10;5H\x1b[?2004h".to_vec()
        );
    }

    #[test]
    fn replay_enters_alternate_screen_after_primary() {
        let snapshot = Snapshot {
            kind: SyncKind::Full,
            state: parse_state("0 0 0 1 1 0 0 0 0 1 1 80 24 0 23 0 default VT10x").unwrap(),
            primary: lines(&["shell"]),
            alternate: Some(lines(&["tui"])),
        };
        assert_eq!(
            replay_bytes(&snapshot),
            b"shell\x1b[0m\x1b[?1049h\x1b[H\x1b[2Jtui\x1b[0m\x1b[1;1H\x1b[?25l\x1b[?1h\x1b[?1003h\x1b[?1006h".to_vec()
        );
    }

    #[test]
    fn repaint_clears_then_paints_the_visible_screen() {
        let snapshot = Snapshot {
            kind: SyncKind::Visible,
            state: parse_state("0 1 1 0 0 0 0 0 0 0 0 80 2 0 1 0 default VT10x").unwrap(),
            primary: lines(&["a", "b"]),
            alternate: None,
        };
        assert_eq!(
            repaint_bytes(&snapshot),
            b"\x1b[0m\x1b[r\x1b[0 q\x1b[1;1H\x1b[2K\x1b[2;1H\x1b[2K\x1b[Ha\r\nb\x1b[0m\x1b[2;1H"
                .to_vec()
        );
    }
    #[test]
    fn parse_state_reads_scroll_region_and_cursor_shape() {
        let state = parse_state("0 0 1 0 0 0 0 0 0 0 0 80 24 2 9 1 bar Ext 2").unwrap();
        assert_eq!(
            (state.scroll_region_upper, state.scroll_region_lower),
            (2, 9)
        );
        assert_eq!(state.cursor_shape, CursorShape::Bar);
        assert!(state.cursor_blinking);
        assert_eq!(state.key_mode, "Ext 2");
        for (word, expected) in [
            ("default", CursorShape::Default),
            ("block", CursorShape::Block),
            ("underline", CursorShape::Underline),
            ("bar", CursorShape::Bar),
            ("something-new", CursorShape::Default),
        ] {
            let line = format!("0 0 1 0 0 0 0 0 0 0 0 80 24 0 23 0 {word} VT10x");
            assert_eq!(parse_state(&line).unwrap().cursor_shape, expected);
        }
    }

    #[test]
    fn state_format_requests_every_field_the_parser_reads() {
        for field in [
            "scroll_region_upper",
            "scroll_region_lower",
            "cursor_shape",
            "cursor_blinking",
        ] {
            assert!(STATE_FORMAT.contains(field), "missing {field}");
        }
    }

    #[test]
    fn replay_restores_the_scroll_region_before_positioning_the_cursor() {
        let snapshot = Snapshot {
            kind: SyncKind::Full,
            state: parse_state("0 3 1 0 0 0 0 0 0 0 0 10 6 1 3 1 bar VT10x").unwrap(),
            primary: lines(&["top"]),
            alternate: None,
        };
        let bytes = String::from_utf8(replay_bytes(&snapshot)).unwrap();
        assert!(
            bytes.ends_with("top\x1b[0m\x1b[2;4r\x1b[5 q\x1b[4;1H"),
            "{bytes:?}"
        );
    }

    #[test]
    fn repaint_resets_the_region_while_painting_then_restores_it() {
        let snapshot = Snapshot {
            kind: SyncKind::Visible,
            state: parse_state("0 1 1 0 0 0 0 0 0 0 0 10 3 0 1 0 underline VT10x").unwrap(),
            primary: lines(&["a", "b", "c"]),
            alternate: None,
        };
        let bytes = String::from_utf8(repaint_bytes(&snapshot)).unwrap();
        assert!(bytes.starts_with("\x1b[0m\x1b[r"), "{bytes:?}");
        assert!(
            bytes.ends_with("a\r\nb\r\nc\x1b[0m\x1b[1;2r\x1b[4 q\x1b[2;1H"),
            "{bytes:?}"
        );
    }
}
