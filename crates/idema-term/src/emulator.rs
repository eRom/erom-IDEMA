//! L'émulateur d'un pane : `alacritty_terminal`, alimenté en octets, sans boucle PTY. Il porte aussi la sélection,
//! les liens sous une cellule (OSC 8 et URL détectées) et les écritures OSC 52 vers le presse-papier (spec §6.3).

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
use alacritty_terminal::term::{
    Config, RenderableContent, Term, TermMode, point_to_viewport, viewport_to_point,
};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use alacritty_terminal::vte::{Params, Parser, Perform};

use crate::palette;

/// D'où viennent les octets d'un pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Pane tmux : tmux répond déjà aux requêtes du terminal, l'émulateur se tait (spec §6.2).
    TmuxPane,
    /// PTY local (volet Terminal) : les réponses de l'émulateur partent au programme.
    LocalPty,
}

/// Reçoit les événements de l'émulateur : les réponses d'un PTY local, et les écritures OSC 52 de toute source
/// (tmux transmet la séquence brute dans `%output`). La lecture OSC 52 est refusée par la config (`OnlyCopy`).
#[derive(Clone)]
pub struct Listener {
    source: Source,
    writes: Arc<Mutex<Vec<u8>>>,
    clipboard: Arc<Mutex<Option<String>>>,
    /// La zone de texte en cellules et en pixels, pour répondre à `CSI 14 t` ; `None` tant que la cellule en pixels
    /// n'est pas connue (l'émulateur ne devine pas).
    area: Arc<Mutex<Option<TextArea>>>,
}

/// La zone de texte d'un PTY local. La cellule garde sa fraction de pixel : la taille annoncée est arrondie sur le
/// total, pas cellule arrondie fois nombre de cellules (E36).
#[derive(Debug, Clone, Copy)]
struct TextArea {
    columns: usize,
    lines: usize,
    cell_width: f32,
    cell_height: f32,
}

impl TextArea {
    /// La réponse à `CSI 14 t` : `CSI 4 ; hauteur ; largeur t`, en pixels.
    fn reply(&self) -> String {
        let height = (self.lines as f32 * self.cell_height).round() as u32;
        let width = (self.columns as f32 * self.cell_width).round() as u32;
        format!("\x1b[4;{height};{width}t")
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match (self.source, event) {
            (Source::LocalPty, Event::PtyWrite(text)) => self
                .writes
                .lock()
                .expect("replies lock")
                .extend_from_slice(text.as_bytes()),
            // OSC 4, 10, 11, 12 avec `?` : un PTY local répond avec la palette d'Idema (spec §6.2) ; un pane tmux
            // se tait, tmux répond pour lui. Sans réponse, un programme qui interroge le fond attend son délai.
            (Source::LocalPty, Event::ColorRequest(index, format)) => {
                if let Some([r, g, b]) = requested_color(index) {
                    self.writes
                        .lock()
                        .expect("replies lock")
                        .extend_from_slice(format(Rgb { r, g, b }).as_bytes());
                }
            }
            // `CSI 14 t` : la taille de la zone de texte en pixels, un PTY local seulement (tmux répond pour ses panes).
            (Source::LocalPty, Event::TextAreaSizeRequest(_)) => {
                if let Some(area) = *self.area.lock().expect("area lock") {
                    self.writes
                        .lock()
                        .expect("replies lock")
                        .extend_from_slice(area.reply().as_bytes());
                }
            }
            // `c`, `p` ou `s` : macOS n'a qu'un presse-papier, la dernière écriture gagne.
            (_, Event::ClipboardStore(_, text)) => {
                *self.clipboard.lock().expect("clipboard lock") = Some(text);
            }
            _ => {}
        }
    }
}

/// La couleur que demande une requête OSC : index de la palette (0 à 255), texte, fond ou curseur par défaut.
fn requested_color(index: usize) -> Option<[u8; 3]> {
    match index {
        0..=255 => Some(palette::rgb(Color::Indexed(index as u8), true)),
        _ if index == NamedColor::Foreground as usize => Some(palette::DEFAULT_FOREGROUND),
        _ if index == NamedColor::Background as usize => Some(palette::DEFAULT_BACKGROUND),
        _ if index == NamedColor::Cursor as usize => {
            Some(palette::rgb(Color::Named(NamedColor::Cursor), true))
        }
        _ => None,
    }
}

/// Une cellule de l'écran tel qu'il est affiché : rangée depuis le haut de la vue (défilement compris), colonne,
/// et moitié droite de la cellule (une sélection qui y commence ne la prend pas).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewPoint {
    pub row: usize,
    pub column: usize,
    pub right_half: bool,
}

impl ViewPoint {
    pub fn new(row: usize, column: usize) -> Self {
        Self {
            row,
            column,
            right_half: false,
        }
    }
}

/// Ce que sélectionne un geste : un glisser, un double-clic (le mot), un triple-clic (la ligne).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    Word,
    Line,
}

/// Un lien sous une cellule, avec sa plage dans la vue (pour le souligner) : de `start` à `end` inclus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub uri: String,
    pub start: (usize, usize),
    pub end: (usize, usize),
}

/// URL reconnues dans le texte (spec §6.3) : web, fichier et courriel. Le motif s'arrête aux blancs, aux
/// guillemets, aux chevrons et aux caractères de contrôle ; la ponctuation finale est retirée par `trim_url`.
const URL_PATTERN: &str = "(?:https?://|file://|mailto:)[^\\s<>\"'`{}|\\\\^\\x00-\\x1f\\x7f]+";

/// Retire d'une URL détectée la ponctuation qui la termine dans une phrase (`.`, `,`, `;`, `:`, `!`, `?`,
/// guillemets) et une parenthèse ou un crochet fermant sans ouvrant.
pub fn trim_url(url: &str) -> &str {
    let mut end = url.len();
    loop {
        let text = &url[..end];
        let Some(last) = text.chars().last() else {
            return text;
        };
        let unbalanced = |open: char, close: char| {
            last == close && text.matches(open).count() < text.matches(close).count()
        };
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"')
            || unbalanced('(', ')')
            || unbalanced('[', ']')
        {
            end -= last.len_utf8();
        } else {
            return text;
        }
    }
}

/// Un chemin de fichier sous une cellule : le texte sans son suffixe `:ligne[:colonne]`, la position qu'il donne,
/// et la plage complète dans la vue (suffixe compris) pour le souligner, de `start` à `end` inclus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMatch {
    pub text: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub start: (usize, usize),
    pub end: (usize, usize),
}

/// Chemins reconnus dans le texte (spec §4.7) : absolu, relatif ou `~/`, caractères courants des noms de fichier,
/// suivis facultativement de `:ligne` ou `:ligne:colonne`. La ponctuation finale est retirée par `trim_url`.
const PATH_PATTERN: &str = "(?:~/)?[A-Za-z0-9._+@/-]+(?::[0-9]+(?::[0-9]+)?)?";

/// Un nombre fait seulement de chiffres.
fn digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Sépare un chemin détecté de son suffixe `:ligne[:colonne]`.
fn split_location(text: &str) -> (&str, Option<u32>, Option<u32>) {
    let Some((head, last)) = text.rsplit_once(':') else {
        return (text, None, None);
    };
    let Some(last) = digits(last) else {
        return (text, None, None);
    };
    if let Some((path, line)) = head.rsplit_once(':')
        && let Some(line) = digits(line)
    {
        return (path, Some(line), Some(last));
    }
    (head, Some(last), None)
}

/// Ce que fait un ⌘-clic sur un lien (écart E21). Un lien OSC 8 peut afficher un texte anodin et viser autre chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// `http`, `https`, `mailto` : le navigateur ou la messagerie par défaut.
    Open(String),
    /// `file://` local : montré dans le Finder, jamais ouvert (un `.command` ou une app s'exécuterait).
    Reveal(PathBuf),
    /// Tout autre schéma, ou une adresse de fichier illisible : rien, sinon le journal.
    Refused,
}

pub fn link_target(uri: &str) -> LinkTarget {
    let lower = uri.to_ascii_lowercase();
    if ["http://", "https://", "mailto:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return LinkTarget::Open(uri.to_string());
    }
    let Some(rest) = lower
        .starts_with("file://")
        .then(|| &uri["file://".len()..])
    else {
        return LinkTarget::Refused;
    };
    // `file:///chemin` ou `file://localhost/chemin` ; un autre hôte n'est pas un fichier de ce Mac.
    let path = match rest.strip_prefix("localhost") {
        Some(path) => path,
        None => rest,
    };
    if !path.starts_with('/') {
        return LinkTarget::Refused;
    }
    percent_decode(path).map_or(LinkTarget::Refused, |path| {
        LinkTarget::Reveal(PathBuf::from(path))
    })
}

/// Défait l'encodage `%XX` d'un chemin d'URL. `None` si un `%` est mal formé ou si le résultat n'est pas de l'UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Une occurrence d'une recherche, de sa première à sa dernière cellule incluse, en (ligne, colonne) comptées depuis
/// le haut de l'historique. Ces lignes ne bougent pas quand la sortie défile la grille, historique plein compris, tant
/// que `PaneEmulator::track_scroll` est actif ; une occurrence dont la ligne est partie du haut n'est plus nulle part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    pub start: (usize, usize),
    pub end: (usize, usize),
}

/// Échappe les métacaractères de regex d'une requête : la recherche est en texte simple.
pub fn escape_literal(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for c in query.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Le motif d'une recherche en texte simple. `RegexSearch` ignore la casse quand le motif n'a aucune majuscule : la
/// requête passe donc en minuscules, ce qui rend toute recherche insensible à la casse.
fn literal_regex(query: &str) -> Option<RegexSearch> {
    if query.is_empty() {
        return None;
    }
    RegexSearch::new(&escape_literal(&query.to_lowercase())).ok()
}

/// Taille des tranches de sortie pendant une recherche (voir `PaneEmulator::advance_tracked`).
const TRACKED_SLICE: usize = 4096;

/// Rangées parcourues au plus de part et d'autre de la souris pour trouver une URL : une ligne repliée sans fin
/// (un JSON d'un mégaoctet) ne fait pas parcourir tout l'historique à chaque mouvement avec ⌘.
const URL_SEARCH_LINES: i32 = 100;

/// Taille d'un pane, en cellules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub columns: usize,
    pub lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Suit le niveau de touches étendues demandé par le programme : `CSI > 4 ; n m`.
/// `alacritty_terminal` ne suit pas cette demande ; tmux, lui, la suit pour ses propres clients.
#[derive(Debug, Default)]
struct KeyModeTracker {
    level: u16,
}

impl Perform for KeyModeTracker {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        if action != 'm' || intermediates != b">" {
            return;
        }
        let mut values = params
            .iter()
            .map(|param| param.first().copied().unwrap_or(0));
        if values.next() == Some(4) {
            self.level = values.next().unwrap_or(0);
        }
    }
}

/// Longueur maximale d'un titre `ESC k … ST` avalé. Au-delà, le filtre cesse d'avaler : un `ESC k`
/// sans terminateur ne doit pas manger le flux du programme jusqu'à la fin de la session.
const MAX_TITLE_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum TitleState {
    #[default]
    Ground,
    /// Un `ESC` est en attente : on ignore encore s'il ouvre un titre.
    Escape,
    /// Dans un titre `ESC k`, avec le nombre d'octets déjà avalés.
    Title(usize),
    /// Un `ESC` a été vu dans le titre : `\` le termine (ST).
    TitleEscape(usize),
}

/// Retire les titres de fenêtre « à la screen » (`ESC k titre ESC \`) que zsh émet sous `tmux-256color`.
/// tmux les consomme pour ses clients normaux, mais le mode contrôle transmet les octets bruts du pane,
/// et `vte` laisserait le titre s'imprimer à l'écran. L'état survit entre deux appels : une séquence
/// peut être coupée entre deux blocs. Toute autre séquence passe intacte et dans l'ordre.
#[derive(Debug, Default)]
struct TitleFilter {
    state: TitleState,
}

impl TitleFilter {
    fn filter<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        if self.state == TitleState::Ground && !bytes.contains(&0x1b) {
            return Cow::Borrowed(bytes);
        }
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            self.step(byte, &mut out);
        }
        Cow::Owned(out)
    }

    fn step(&mut self, byte: u8, out: &mut Vec<u8>) {
        self.state = match self.state {
            TitleState::Ground => self.ground(byte, out),
            TitleState::Escape => match byte {
                b'k' => TitleState::Title(0),
                _ => {
                    // Pas un titre : l'ESC retenu repart, puis l'octet suit les règles du repos.
                    out.push(0x1b);
                    self.ground(byte, out)
                }
            },
            TitleState::Title(swallowed) => match byte {
                0x1b => TitleState::TitleEscape(swallowed),
                _ => Self::swallow(swallowed + 1, byte, out),
            },
            TitleState::TitleEscape(swallowed) => match byte {
                b'\\' => TitleState::Ground,
                // Un ESC sans `\` derrière fait partie du titre.
                _ => Self::swallow(swallowed + 2, byte, out),
            },
        };
    }

    fn ground(&self, byte: u8, out: &mut Vec<u8>) -> TitleState {
        if byte == 0x1b {
            TitleState::Escape
        } else {
            out.push(byte);
            TitleState::Ground
        }
    }

    /// Continue d'avaler, ou abandonne passé `MAX_TITLE_BYTES` : l'octet courant repart alors comme du texte.
    fn swallow(swallowed: usize, byte: u8, out: &mut Vec<u8>) -> TitleState {
        if swallowed > MAX_TITLE_BYTES {
            let mut ground = TitleFilter::default();
            ground.step(byte, out);
            ground.state
        } else {
            TitleState::Title(swallowed)
        }
    }
}

pub struct PaneEmulator {
    term: Term<Listener>,
    processor: Processor,
    key_parser: Parser,
    key_modes: KeyModeTracker,
    title_filter: TitleFilter,
    writes: Arc<Mutex<Vec<u8>>>,
    clipboard: Arc<Mutex<Option<String>>>,
    /// Zone de texte lue par le `Listener` (voir là).
    area: Arc<Mutex<Option<TextArea>>>,
    /// Motif des URL, construit à la première recherche (son automate est paresseux).
    url_regex: Option<RegexSearch>,
    /// Motif des chemins de fichier, construit de même.
    path_regex: Option<RegexSearch>,
    /// Lignes d'historique que l'émulateur garde au plus.
    history_cap: usize,
    /// Une recherche est ouverte : `feed` compte les lignes que l'historique plein laisse partir (voir `dropped`).
    track_scroll: bool,
    /// Lignes parties du haut de l'historique plein depuis que le suivi est actif. Ajoutées aux lignes de `SearchMatch`,
    /// elles gardent les coordonnées d'une occurrence exactes quand la sortie fait défiler un historique plein.
    dropped: usize,
    /// L'historique du grand écran a gardé une limite relevée par `feed` (un programme est passé en écran alternatif en
    /// cours de route) : le prochain `feed` qui revient à l'écran normal la rétablit.
    history_raised: bool,
}

impl PaneEmulator {
    pub fn new(size: GridSize, history: usize, source: Source) -> Self {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let clipboard = Arc::new(Mutex::new(None));
        let area = Arc::new(Mutex::new(None));
        // `osc52` reste à `OnlyCopy` (défaut) : un programme écrit le presse-papier, il ne le lit jamais.
        let config = Config {
            scrolling_history: history,
            ..Config::default()
        };
        let term = Term::new(
            config,
            &size,
            Listener {
                source,
                writes: writes.clone(),
                clipboard: clipboard.clone(),
                area: area.clone(),
            },
        );
        Self {
            term,
            processor: Processor::new(),
            key_parser: Parser::new(),
            key_modes: KeyModeTracker::default(),
            title_filter: TitleFilter::default(),
            writes,
            clipboard,
            area,
            url_regex: None,
            path_regex: None,
            history_cap: history,
            track_scroll: false,
            dropped: 0,
            history_raised: false,
        }
    }

    /// Donne des octets bruts du programme. Une séquence coupée entre deux appels est reprise.
    pub fn feed(&mut self, bytes: &[u8]) {
        let bytes = self.title_filter.filter(bytes);
        if self.track_scroll || self.history_raised {
            // Pendant une recherche, le bloc passe par tranches : la marge d'historique reste petite.
            for slice in bytes.chunks(TRACKED_SLICE) {
                self.advance_tracked(slice);
            }
        } else {
            self.processor.advance(&mut self.term, &bytes);
        }
        self.key_parser.advance(&mut self.key_modes, &bytes);
    }

    /// Une tranche de la sortie pendant une recherche : un historique plein ne perd plus ses lignes en silence. On lui
    /// laisse de la marge le temps de la tranche (au plus un défilement par octet, ou la hauteur de l'écran par
    /// séquence), puis on rogne à la limite et on compte ce qui est parti dans `dropped`. Restent sous-comptés : un
    /// `CSI n S` géant, un retour de l'écran alternatif au milieu d'une tranche, un émulateur remplacé par une relecture.
    fn advance_tracked(&mut self, slice: &[u8]) {
        let room = slice.len() + self.term.screen_lines();
        let raise = self.track_scroll
            && !self.term.mode().contains(TermMode::ALT_SCREEN)
            && self.term.grid().history_size() + room > self.history_cap;
        if raise {
            self.term.grid_mut().update_history(self.history_cap + room);
            self.history_raised = true;
        }
        self.processor.advance(&mut self.term, slice);
        if self.history_raised && !self.term.mode().contains(TermMode::ALT_SCREEN) {
            self.history_raised = false;
            let excess = self
                .term
                .grid()
                .history_size()
                .saturating_sub(self.history_cap);
            self.term.grid_mut().update_history(self.history_cap);
            self.dropped += excess;
        }
    }

    /// Une recherche est ouverte ou fermée : tant qu'elle l'est, les coordonnées des occurrences restent exactes
    /// quand la sortie fait défiler un historique plein.
    pub fn track_scroll(&mut self, on: bool) {
        self.track_scroll = on;
    }

    pub fn resize(&mut self, size: GridSize) {
        self.term.resize(size);
        if let Some(area) = self.area.lock().expect("area lock").as_mut() {
            area.lines = size.lines;
            area.columns = size.columns;
        }
    }

    /// La cellule en pixels de l'appareil (échelle de l'écran comprise, fraction comprise) : sans elle, `CSI 14 t`
    /// reste sans réponse.
    pub fn set_cell_pixels(&mut self, width: f32, height: f32) {
        let size = self.size();
        *self.area.lock().expect("area lock") = Some(TextArea {
            columns: size.columns,
            lines: size.lines,
            cell_width: width,
            cell_height: height,
        });
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Le programme a demandé les touches étendues (niveau 1 ou 2).
    pub fn extended_keys(&self) -> bool {
        self.key_modes.level > 0
    }

    /// Niveau relu dans tmux à l'attache (`pane_key_mode`), puisque la demande d'origine est passée.
    pub fn set_extended_keys_level(&mut self, level: u16) {
        self.key_modes.level = level;
    }

    /// Taille de la grille, en cellules.
    pub fn size(&self) -> GridSize {
        let grid = self.term.grid();
        GridSize {
            columns: grid.columns(),
            lines: grid.screen_lines(),
        }
    }

    /// Lignes d'historique au-dessus de l'écran.
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// Position du curseur : (ligne, colonne), à partir de 0.
    pub fn cursor(&self) -> (usize, usize) {
        let point = self.term.grid().cursor.point;
        (point.line.0.max(0) as usize, point.column.0)
    }

    /// Fait défiler la vue dans l'historique (positif : vers le haut).
    pub fn scroll_history(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    /// Ramène la vue en bas, sur l'écran courant (à la frappe, par exemple).
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Lignes de défilement de la vue au-dessus de l'écran courant.
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Ce qu'il faut pour dessiner la grille.
    pub fn renderable(&self) -> RenderableContent<'_> {
        self.term.renderable_content()
    }

    /// Réponses à renvoyer au programme (PTY local seulement).
    pub fn take_pty_writes(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.writes.lock().expect("replies lock"))
    }

    /// Texte de l'écran courant (pas la vue défilée), une ligne par rangée, comme `capture-pane -p` : seules les
    /// espaces finales sont retirées, une espace insécable reste (Claude Code en met une après `❯`), et les
    /// marques combinantes suivent leur caractère (vidage de grille, AC-22).
    pub fn dump_text(&self) -> String {
        let grid = self.term.grid();
        let rows: Vec<String> = (0..grid.screen_lines())
            .map(|line| {
                let row = &grid[Line(line as i32)];
                let mut text = String::new();
                for cell in (0..grid.columns()).map(|column| &row[Column(column)]) {
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        continue;
                    }
                    text.push(cell.c);
                    if let Some(marks) = cell.zerowidth() {
                        text.extend(marks);
                    }
                }
                text.trim_end_matches(' ').to_string()
            })
            .collect();
        rows.join("\n")
    }

    /// La dernière écriture OSC 52 du programme, à mettre dans le presse-papier de macOS.
    pub fn take_clipboard_store(&mut self) -> Option<String> {
        self.clipboard.lock().expect("clipboard lock").take()
    }

    /// Point de la grille sous une cellule de la vue, borné à la grille.
    fn grid_point(&self, at: ViewPoint) -> Point {
        let size = self.size();
        let row = at.row.min(size.lines.saturating_sub(1));
        let column = at.column.min(size.columns.saturating_sub(1));
        viewport_to_point(self.display_offset(), Point::new(row, Column(column)))
    }

    /// Commence une sélection à la cellule `at` (spec §6.3 : glisser, double-clic pour un mot, triple-clic pour
    /// une ligne).
    pub fn start_selection(&mut self, at: ViewPoint, kind: SelectionKind) {
        let point = self.grid_point(at);
        let side = if at.right_half {
            Side::Right
        } else {
            Side::Left
        };
        let ty = match kind {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Word => SelectionType::Semantic,
            SelectionKind::Line => SelectionType::Lines,
        };
        self.term.selection = Some(Selection::new(ty, point, side));
    }

    /// Étend la sélection en cours jusqu'à la cellule `at`.
    pub fn extend_selection(&mut self, at: ViewPoint) {
        let point = self.grid_point(at);
        let side = if at.right_half {
            Side::Right
        } else {
            Side::Left
        };
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Sélectionne tout l'historique et l'écran (⌘A).
    pub fn select_all(&mut self) {
        let start = Point::new(self.term.topmost_line(), Column(0));
        let end = Point::new(self.term.bottommost_line(), self.term.last_column());
        let mut selection = Selection::new(SelectionType::Lines, start, Side::Left);
        selection.update(end, Side::Right);
        self.term.selection = Some(selection);
    }

    /// Une sélection qui couvre au moins une cellule (un clic sans glisser n'en fait pas).
    pub fn has_selection(&self) -> bool {
        self.term
            .selection
            .as_ref()
            .is_some_and(|selection| !selection.is_empty())
    }

    /// Le texte sélectionné, à copier (⌘C). Une sélection de lignes (triple-clic, ⌘A) perd son saut de ligne
    /// final, comme Ghostty et Terminal.app : collée dans un programme sans collage encadré, la ligne ne
    /// s'exécute pas d'elle-même (décision de la porte du jalon 3).
    pub fn selection_text(&self) -> Option<String> {
        let selection = self.term.selection.as_ref()?;
        if selection.is_empty() {
            return None;
        }
        let lines = selection.ty == SelectionType::Lines;
        let mut text = self.term.selection_to_string()?;
        if lines && text.ends_with('\n') {
            text.pop();
        }
        Some(text)
    }

    /// Toutes les occurrences de `query`, du haut de l'historique au bas de l'écran, en texte simple et sans
    /// distinction de casse. Les lignes repliées comptent pour une seule ligne.
    pub fn search(&mut self, query: &str) -> Vec<SearchMatch> {
        let Some(mut regex) = literal_regex(query) else {
            return Vec::new();
        };
        let start = Point::new(self.term.topmost_line(), Column(0));
        let end = Point::new(self.term.bottommost_line(), self.term.last_column());
        RegexIter::new(start, end, Direction::Right, &self.term, &mut regex)
            .map(|found| SearchMatch {
                start: (
                    self.absolute_line(found.start().line),
                    found.start().column.0,
                ),
                end: (self.absolute_line(found.end().line), found.end().column.0),
            })
            .collect()
    }

    /// La ligne `line` de la grille (négative dans l'historique) comptée depuis le haut de l'historique, lignes déjà
    /// parties comprises : elle ne change pas quand la sortie défile la grille.
    fn absolute_line(&self, line: Line) -> usize {
        (line.0 + self.term.grid().history_size() as i32) as usize + self.dropped
    }

    /// La dernière ligne de la vue (défilement compris), dans les coordonnées de `SearchMatch`.
    pub fn view_bottom_line(&self) -> usize {
        let last = self.term.screen_lines() as i32 - 1 - self.display_offset() as i32;
        self.absolute_line(Line(last))
    }

    /// L'inverse de `absolute_line` : la ligne de la grille, négative si elle est dans l'historique. `None` quand la
    /// ligne est déjà partie du haut de l'historique.
    fn grid_line(&self, absolute: usize) -> Option<Line> {
        let line = absolute as i32 - self.dropped as i32 - self.term.grid().history_size() as i32;
        (line >= self.term.topmost_line().0).then_some(Line(line))
    }

    /// Les occurrences de la partie affichée, cherchées à chaque appel (écart E29) : la sortie qui défile l'écran
    /// entre deux peintures ne laisse jamais un surlignage sur d'autres cellules. Une occurrence qui déborde de la
    /// vue est coupée à son bord.
    pub fn visible_matches(&mut self, query: &str) -> Vec<(ViewPoint, ViewPoint)> {
        let Some(mut regex) = literal_regex(query) else {
            return Vec::new();
        };
        let offset = self.display_offset() as i32;
        let lines = self.term.screen_lines() as i32;
        // Une occurrence repliée peut commencer au-dessus de la vue ou finir au-dessous : on cherche cette marge en plus.
        let spill = (query.chars().count() / self.term.columns().max(1) + 1) as i32;
        let first = (-offset - spill).max(self.term.topmost_line().0);
        let last = (lines - 1 - offset + spill).min(self.term.bottommost_line().0);
        let start = Point::new(Line(first), Column(0));
        let end = Point::new(Line(last), self.term.last_column());
        let found: Vec<_> =
            RegexIter::new(start, end, Direction::Right, &self.term, &mut regex).collect();
        found
            .into_iter()
            .filter_map(|found| self.clip_to_view(*found.start(), *found.end()))
            .collect()
    }

    /// Les cellules que `found` occupe dans la vue, ou `None` quand elle est hors de la vue.
    pub fn match_in_view(&self, found: &SearchMatch) -> Option<(ViewPoint, ViewPoint)> {
        let point = |(line, column): (usize, usize)| {
            Some(Point::new(self.grid_line(line)?, Column(column)))
        };
        self.clip_to_view(point(found.start)?, point(found.end)?)
    }

    /// De `start` à `end` (inclus) en cellules de la vue, coupées aux bords de la vue.
    fn clip_to_view(&self, start: Point, end: Point) -> Option<(ViewPoint, ViewPoint)> {
        let offset = self.display_offset() as i32;
        let lines = self.term.screen_lines() as i32;
        let (first_row, last_row) = (start.line.0 + offset, end.line.0 + offset);
        if last_row < 0 || first_row >= lines {
            return None;
        }
        let last_column = self.term.last_column().0;
        let from = if first_row < 0 {
            ViewPoint::new(0, 0)
        } else {
            ViewPoint::new(first_row as usize, start.column.0)
        };
        let to = if last_row >= lines {
            ViewPoint::new(lines as usize - 1, last_column)
        } else {
            ViewPoint::new(last_row as usize, end.column.0)
        };
        Some((from, to))
    }

    /// Met la ligne de `found` dans la vue : la vue ne défile que si l'occurrence en est hors.
    pub fn scroll_to_match(&mut self, found: &SearchMatch) {
        // Une occurrence déjà partie du haut de l'historique n'est plus nulle part où défiler.
        if let Some(line) = self.grid_line(found.start.0) {
            self.term
                .scroll_to_point(Point::new(line, Column(found.start.1)));
        }
    }

    /// Le lien sous la cellule `at` : un lien OSC 8 d'abord, sinon une URL du texte (spec §6.3).
    pub fn link_at(&mut self, at: ViewPoint) -> Option<Link> {
        let point = self.grid_point(at);
        self.hyperlink_at(point).or_else(|| self.url_at(point))
    }

    fn hyperlink_at(&self, point: Point) -> Option<Link> {
        let grid = self.term.grid();
        let link = grid[point].hyperlink()?;
        let row = &grid[point.line];
        let same = |column: usize| row[Column(column)].hyperlink().as_ref() == Some(&link);
        let mut first = point.column.0;
        while first > 0 && same(first - 1) {
            first -= 1;
        }
        let mut last = point.column.0;
        while last + 1 < grid.columns() && same(last + 1) {
            last += 1;
        }
        let offset = self.display_offset();
        let start = point_to_viewport(offset, Point::new(point.line, Column(first)))?;
        let end = point_to_viewport(offset, Point::new(point.line, Column(last)))?;
        Some(Link {
            uri: link.uri().to_string(),
            start: (start.line, start.column.0),
            end: (end.line, end.column.0),
        })
    }

    fn url_at(&mut self, point: Point) -> Option<Link> {
        if self.url_regex.is_none() {
            self.url_regex = RegexSearch::new(URL_PATTERN).ok();
        }
        let found = trimmed_match_at(&self.term, self.url_regex.as_mut()?, point)?;
        Some(Link {
            uri: found.text,
            start: found.start,
            end: found.end,
        })
    }

    /// Le chemin de fichier sous la cellule `at` (spec §4.7), avec sa ligne et sa colonne s'il en porte. Un lien
    /// ou une URL sous la cellule l'emporte : alors rien ici.
    pub fn path_at(&mut self, at: ViewPoint) -> Option<PathMatch> {
        if self.link_at(at).is_some() {
            return None;
        }
        let point = self.grid_point(at);
        if self.path_regex.is_none() {
            self.path_regex = RegexSearch::new(PATH_PATTERN).ok();
        }
        let found = trimmed_match_at(&self.term, self.path_regex.as_mut()?, point)?;
        let (text, line, column) = split_location(&found.text);
        Some(PathMatch {
            text: text.to_string(),
            line,
            column,
            start: found.start,
            end: found.end,
        })
    }
}

/// Une détection du texte de la grille : sa chaîne sans la ponctuation finale, et sa plage dans la vue.
struct TextMatch {
    text: String,
    start: (usize, usize),
    end: (usize, usize),
}

/// Cherche avec `regex` la détection qui contient `point`, sur la ligne repliée qui l'entoure (bornée à
/// `URL_SEARCH_LINES`), puis retire la ponctuation finale par `trim_url`.
fn trimmed_match_at(
    term: &Term<Listener>,
    regex: &mut RegexSearch,
    point: Point,
) -> Option<TextMatch> {
    let mut start = term.line_search_left(point);
    let mut end = term.line_search_right(point);
    if point.line.0 - start.line.0 > URL_SEARCH_LINES {
        start = Point::new(Line(point.line.0 - URL_SEARCH_LINES), Column(0));
    }
    if end.line.0 - point.line.0 > URL_SEARCH_LINES {
        end = Point::new(Line(point.line.0 + URL_SEARCH_LINES), term.last_column());
    }
    let found = RegexIter::new(start, end, Direction::Right, term, regex)
        .find(|candidate| candidate.contains(&point))?;
    let text = term.bounds_to_string(*found.start(), *found.end());
    let kept = trim_url(&text).to_string();
    // La ponctuation retirée occupe une cellule par caractère : la fin de la détection recule d'autant.
    let trimmed = text.chars().count() - kept.chars().count();
    let last = found
        .end()
        .sub(term, alacritty_terminal::index::Boundary::Grid, trimmed);
    if point > last {
        return None;
    }
    let offset = term.grid().display_offset();
    let first = point_to_viewport(offset, *found.start())
        .map_or((0, 0), |start| (start.line, start.column.0));
    let last = point_to_viewport(offset, last)?;
    Some(TextMatch {
        text: kept,
        start: first,
        end: (last.line, last.column.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use idema_tmux::sync::{Snapshot, SyncKind, parse_state, repaint_bytes, replay_bytes};

    fn emulator(source: Source) -> PaneEmulator {
        PaneEmulator::new(
            GridSize {
                columns: 20,
                lines: 5,
            },
            100,
            source,
        )
    }

    #[test]
    fn feed_writes_text_on_screen() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"hello\r\nworld");
        assert_eq!(pane.dump_text(), "hello\nworld\n\n\n");
    }

    #[test]
    fn utf8_split_between_two_feeds_renders_once() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"caf\xc3");
        pane.feed(b"\xa9");
        assert!(pane.dump_text().starts_with("café\n"));
    }

    #[test]
    fn tmux_pane_never_answers_queries() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"\x1b[c\x1b[6n");
        assert!(pane.take_pty_writes().is_empty());
    }

    #[test]
    fn local_pty_answers_cursor_position_query() {
        let mut pane = emulator(Source::LocalPty);
        pane.feed(b"\x1b[6n");
        assert_eq!(pane.take_pty_writes(), b"\x1b[1;1R".to_vec());
    }

    #[test]
    fn replay_restores_text_cursor_and_bracketed_paste() {
        let mut pane = emulator(Source::TmuxPane);
        let snapshot = Snapshot {
            kind: SyncKind::Full,
            state: parse_state("4 2 1 0 0 0 1 0 0 0 0 20 5 0 4 0 default VT10x").unwrap(),
            primary: vec![b"$ ls".to_vec(), b"a  b".to_vec(), b"$".to_vec()],
            alternate: None,
        };
        pane.feed(&replay_bytes(&snapshot));
        assert_eq!(pane.dump_text(), "$ ls\na  b\n$\n\n");
        assert_eq!(pane.cursor(), (2, 4));
        assert!(pane.mode().contains(TermMode::BRACKETED_PASTE));
    }

    #[test]
    fn replay_puts_alternate_screen_on_top() {
        let mut pane = emulator(Source::TmuxPane);
        let snapshot = Snapshot {
            kind: SyncKind::Full,
            state: parse_state("0 0 1 1 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x").unwrap(),
            primary: vec![b"shell".to_vec()],
            alternate: Some(vec![b"tui".to_vec()]),
        };
        pane.feed(&replay_bytes(&snapshot));
        assert!(pane.mode().contains(TermMode::ALT_SCREEN));
        assert!(pane.dump_text().starts_with("tui\n"));
        pane.feed(b"\x1b[?1049l");
        assert!(pane.dump_text().starts_with("shell\n"));
    }

    #[test]
    fn repaint_replaces_the_screen_and_keeps_history() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7");
        assert_eq!(pane.history_size(), 2);
        let snapshot = Snapshot {
            kind: SyncKind::Visible,
            state: parse_state("1 4 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x").unwrap(),
            primary: vec![
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
                b"d".to_vec(),
                b"e".to_vec(),
            ],
            alternate: None,
        };
        pane.feed(&repaint_bytes(&snapshot));
        assert_eq!(pane.dump_text(), "a\nb\nc\nd\ne");
        assert_eq!(pane.history_size(), 2);
        assert_eq!(pane.cursor(), (4, 1));
    }

    #[test]
    fn scroll_moves_the_view_within_history_and_back() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8");
        pane.scroll_history(2);
        assert_eq!(pane.display_offset(), 2);
        pane.scroll_history(10);
        assert_eq!(pane.display_offset(), 3);
        pane.scroll_to_bottom();
        assert_eq!(pane.display_offset(), 0);
    }

    #[test]
    fn extended_keys_follow_program_requests_across_chunks() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"\x1b[>4;2m");
        assert!(pane.extended_keys());
        pane.feed(b"\x1b[>4");
        pane.feed(b";0m");
        assert!(!pane.extended_keys());
    }

    #[test]
    fn resize_keeps_text() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"abc");
        pane.resize(GridSize {
            columns: 30,
            lines: 6,
        });
        assert!(pane.dump_text().starts_with("abc\n"));
    }

    #[test]
    fn wide_characters_dump_once() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed("日本".as_bytes());
        assert!(pane.dump_text().starts_with("日本\n"));
    }

    #[test]
    fn screen_title_sequence_is_stripped() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"\x1bkecho\x1b\\hello");
        assert_eq!(pane.dump_text(), "hello\n\n\n\n");
    }

    #[test]
    fn screen_title_split_across_feeds_is_stripped() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"ab\x1bkec");
        pane.feed(b"ho\x1b");
        pane.feed(b"\\cd");
        assert_eq!(pane.dump_text(), "abcd\n\n\n\n");
    }

    #[test]
    fn unterminated_screen_title_stops_swallowing_after_the_cap() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"\x1bk");
        pane.feed(&[b'x'; 300]);
        pane.feed(b"\r\nlater");
        let dump = pane.dump_text();
        assert!(
            dump.trim_end().ends_with("later"),
            "later text must render: {dump:?}"
        );
        let swallowed = 300 - dump.matches('x').count();
        assert_eq!(swallowed, super::MAX_TITLE_BYTES);
    }

    #[test]
    fn other_escapes_and_text_around_a_title_are_untouched() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"\x1b[31mred\x1b[0m \x1bkt\x1b\\ok");
        assert_eq!(pane.dump_text(), "red ok\n\n\n\n");
        let first = pane.renderable().display_iter.next().expect("a first cell");
        assert_eq!(
            first.cell.fg,
            alacritty_terminal::vte::ansi::Color::Named(
                alacritty_terminal::vte::ansi::NamedColor::Red
            )
        );
    }

    fn pane(columns: usize, lines: usize, source: Source) -> PaneEmulator {
        PaneEmulator::new(GridSize { columns, lines }, 100, source)
    }

    #[test]
    fn a_drag_selects_across_rows_and_copies_the_text() {
        let mut pane = pane(20, 5, Source::TmuxPane);
        pane.feed(b"hello world\r\nsecond line");
        pane.start_selection(ViewPoint::new(0, 6), SelectionKind::Simple);
        assert!(!pane.has_selection(), "a click alone selects nothing");
        pane.extend_selection(ViewPoint {
            row: 1,
            column: 5,
            right_half: true,
        });
        assert_eq!(pane.selection_text().as_deref(), Some("world\nsecond"));
        pane.clear_selection();
        assert_eq!(pane.selection_text(), None);
    }

    #[test]
    fn double_click_takes_the_word_and_triple_click_the_line() {
        let mut pane = pane(30, 3, Source::TmuxPane);
        pane.feed(b"cargo test --workspace");
        pane.start_selection(ViewPoint::new(0, 14), SelectionKind::Word);
        assert_eq!(pane.selection_text().as_deref(), Some("--workspace"));
        pane.start_selection(ViewPoint::new(0, 2), SelectionKind::Line);
        assert_eq!(
            pane.selection_text().as_deref(),
            Some("cargo test --workspace")
        );
    }

    #[test]
    fn a_selection_in_a_scrolled_view_takes_the_history_shown() {
        let mut pane = pane(10, 2, Source::TmuxPane);
        pane.feed(b"one\r\ntwo\r\nthree\r\nfour");
        pane.scroll_history(2);
        pane.start_selection(ViewPoint::new(0, 0), SelectionKind::Line);
        assert_eq!(pane.selection_text().as_deref(), Some("one"));
    }

    #[test]
    fn select_all_copies_the_history_and_the_screen() {
        let mut pane = pane(10, 2, Source::TmuxPane);
        pane.feed(b"one\r\ntwo\r\nthree");
        pane.select_all();
        assert_eq!(pane.selection_text().as_deref(), Some("one\ntwo\nthree"));
    }

    #[test]
    fn an_osc8_link_is_found_with_its_extent() {
        let mut pane = pane(40, 2, Source::TmuxPane);
        pane.feed(b"go \x1b]8;;https://example.com/doc\x07click\x1b]8;;\x07 now");
        let link = pane.link_at(ViewPoint::new(0, 5)).unwrap();
        assert_eq!(
            link,
            Link {
                uri: "https://example.com/doc".into(),
                start: (0, 3),
                end: (0, 7),
            }
        );
        assert_eq!(pane.link_at(ViewPoint::new(0, 9)), None);
    }

    #[test]
    fn a_plain_url_is_detected_without_its_trailing_punctuation() {
        let mut pane = pane(60, 2, Source::TmuxPane);
        pane.feed(b"voir (https://example.com/a?b=1). merci");
        let link = pane.link_at(ViewPoint::new(0, 12)).unwrap();
        assert_eq!(link.uri, "https://example.com/a?b=1");
        assert_eq!((link.start, link.end), ((0, 6), (0, 30)));
        // La parenthèse fermante et le point ne font pas partie du lien.
        assert_eq!(pane.link_at(ViewPoint::new(0, 31)), None);
        assert_eq!(pane.link_at(ViewPoint::new(0, 2)), None);
    }

    #[test]
    fn a_url_wrapped_on_two_rows_is_found_from_either_row() {
        let mut pane = pane(20, 3, Source::TmuxPane);
        pane.feed(b"x https://example.com/very/long/path y");
        let from_second_row = pane.link_at(ViewPoint::new(1, 3)).unwrap();
        assert_eq!(from_second_row.uri, "https://example.com/very/long/path");
        assert_eq!(from_second_row.start, (0, 2));
    }

    #[test]
    fn a_path_with_line_and_column_is_found_under_the_cell() {
        let mut pane = pane(60, 2, Source::TmuxPane);
        pane.feed(b"voir crates/idema/src/theme.rs:42:5 merci");
        let found = pane.path_at(ViewPoint::new(0, 12)).unwrap();
        assert_eq!(
            found,
            PathMatch {
                text: "crates/idema/src/theme.rs".into(),
                line: Some(42),
                column: Some(5),
                start: (0, 5),
                end: (0, 34),
            }
        );
        // Un mot qui n'est pas contre la cellule visée n'est pas le chemin.
        assert_eq!(pane.path_at(ViewPoint::new(0, 2)).unwrap().text, "voir");
        assert_eq!(pane.path_at(ViewPoint::new(0, 4)), None);
    }

    #[test]
    fn a_path_may_carry_only_a_line_or_nothing() {
        let mut pane = pane(60, 2, Source::TmuxPane);
        pane.feed(b"~/dev/a.rs:7 /tmp/b.txt ../c.md");
        let line_only = pane.path_at(ViewPoint::new(0, 3)).unwrap();
        assert_eq!(
            (line_only.text.as_str(), line_only.line, line_only.column),
            ("~/dev/a.rs", Some(7), None)
        );
        let bare = pane.path_at(ViewPoint::new(0, 15)).unwrap();
        assert_eq!(
            (bare.text.as_str(), bare.line, bare.column),
            ("/tmp/b.txt", None, None)
        );
        assert_eq!(pane.path_at(ViewPoint::new(0, 28)).unwrap().text, "../c.md");
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_path() {
        let mut pane = pane(60, 2, Source::TmuxPane);
        pane.feed(b"(crates/idema/src/theme.rs:42). puis a/b.rs: fin");
        let found = pane.path_at(ViewPoint::new(0, 10)).unwrap();
        assert_eq!(found.text, "crates/idema/src/theme.rs");
        assert_eq!(found.line, Some(42));
        assert_eq!(found.end, (0, 28));
        // La parenthèse fermante n'en fait pas partie.
        assert_eq!(pane.path_at(ViewPoint::new(0, 29)), None);
        let colon = pane.path_at(ViewPoint::new(0, 38)).unwrap();
        // Le point et le deux-points finaux ne se soulignent pas.
        assert_eq!((colon.text.as_str(), colon.line), ("a/b.rs", None));
        assert_eq!(colon.end, (0, 42));
    }

    #[test]
    fn an_url_wins_over_a_path() {
        let mut pane = pane(60, 2, Source::TmuxPane);
        pane.feed(b"voir https://example.com/a/b ici");
        assert!(pane.link_at(ViewPoint::new(0, 20)).is_some());
        assert_eq!(pane.path_at(ViewPoint::new(0, 20)), None);
    }

    #[test]
    fn trim_url_keeps_balanced_parentheses() {
        assert_eq!(
            trim_url("https://en.wikipedia.org/wiki/Rust_(langage)"),
            "https://en.wikipedia.org/wiki/Rust_(langage)"
        );
        assert_eq!(trim_url("https://a.b/c),"), "https://a.b/c");
        assert_eq!(trim_url("mailto:r@e.fr."), "mailto:r@e.fr");
    }

    #[test]
    fn web_and_mail_links_open_and_local_files_are_only_revealed() {
        assert_eq!(
            link_target("https://example.com"),
            LinkTarget::Open("https://example.com".into())
        );
        assert_eq!(
            link_target("HTTP://example.com"),
            LinkTarget::Open("HTTP://example.com".into())
        );
        assert_eq!(
            link_target("mailto:r@e.fr"),
            LinkTarget::Open("mailto:r@e.fr".into())
        );
        assert_eq!(
            link_target("file:///Users/r/Mes%20notes.md"),
            LinkTarget::Reveal(PathBuf::from("/Users/r/Mes notes.md"))
        );
        assert_eq!(
            link_target("file://localhost/tmp/Caf%C3%A9.command"),
            LinkTarget::Reveal(PathBuf::from("/tmp/Café.command"))
        );
        for refused in [
            "file://serveur/partage",
            "file:///x%zz",
            "x-man-page://ls",
            "javascript:alert(1)",
            "vscode://file/x",
        ] {
            assert_eq!(link_target(refused), LinkTarget::Refused, "{refused}");
        }
    }

    #[test]
    fn a_url_at_the_end_of_a_huge_wrapped_line_is_still_found() {
        let mut pane = PaneEmulator::new(
            GridSize {
                columns: 10,
                lines: 5,
            },
            1000,
            Source::TmuxPane,
        );
        let mut line = vec![b'x'; 3000];
        line.extend_from_slice(b" https://e.com/a");
        pane.feed(&line);
        let link = pane.link_at(ViewPoint::new(4, 3)).unwrap();
        assert_eq!(link.uri, "https://e.com/a");
    }

    #[test]
    fn osc52_writes_reach_the_clipboard_and_reads_are_refused() {
        let mut pane = pane(20, 2, Source::LocalPty);
        // « idema » en base64.
        pane.feed(b"\x1b]52;c;aWRlbWE=\x07");
        assert_eq!(pane.take_clipboard_store().as_deref(), Some("idema"));
        assert_eq!(pane.take_clipboard_store(), None);
        pane.feed(b"\x1b]52;c;?\x07");
        assert!(
            pane.take_pty_writes().is_empty(),
            "the clipboard is never read back"
        );
        let mut tmux = pane_with_tmux();
        tmux.feed(b"\x1b]52;c;aWRlbWE=\x07");
        assert_eq!(tmux.take_clipboard_store().as_deref(), Some("idema"));
    }

    #[test]
    fn a_local_pty_answers_color_queries_and_a_tmux_pane_stays_silent() {
        let mut local = pane(20, 2, Source::LocalPty);
        local.feed(b"\x1b]11;?\x07");
        assert_eq!(
            String::from_utf8(local.take_pty_writes()).unwrap(),
            "\x1b]11;rgb:1515/1515/1616\x07"
        );
        local.feed(b"\x1b]4;1;?\x1b\\");
        assert_eq!(
            String::from_utf8(local.take_pty_writes()).unwrap(),
            "\x1b]4;1;rgb:ffff/6969/6161\x1b\\"
        );
        let mut tmux = pane_with_tmux();
        tmux.feed(b"\x1b]10;?\x07");
        assert!(tmux.take_pty_writes().is_empty());
    }

    #[test]
    fn a_local_pty_answers_the_text_area_size_in_pixels() {
        let mut local = pane(80, 24, Source::LocalPty);
        // Sans taille de cellule connue, l'émulateur ne devine pas : le programme attend son délai.
        local.feed(b"\x1b[14t");
        assert!(local.take_pty_writes().is_empty());
        local.set_cell_pixels(9., 25.);
        local.feed(b"\x1b[14t");
        assert_eq!(local.take_pty_writes(), b"\x1b[4;600;720t".to_vec());
        // La réponse suit la grille : 100 x 40 cellules de 9 x 25.
        local.resize(GridSize {
            columns: 100,
            lines: 40,
        });
        local.feed(b"\x1b[14t");
        assert_eq!(local.take_pty_writes(), b"\x1b[4;1000;900t".to_vec());
        // Tmux répond pour ses panes.
        let mut tmux = pane_with_tmux();
        tmux.set_cell_pixels(9., 25.);
        tmux.feed(b"\x1b[14t");
        assert!(tmux.take_pty_writes().is_empty());
    }

    #[test]
    fn the_text_area_in_pixels_is_rounded_on_the_total_not_per_cell() {
        // Une cellule de 9,63 px sur 62 colonnes fait 597 px : arrondir la cellule à 10 en annoncerait 620.
        let mut local = pane(62, 24, Source::LocalPty);
        local.set_cell_pixels(9.63, 25.);
        local.feed(b"\x1b[14t");
        assert_eq!(local.take_pty_writes(), b"\x1b[4;600;597t".to_vec());
    }

    fn pane_with_tmux() -> PaneEmulator {
        pane(20, 2, Source::TmuxPane)
    }

    #[test]
    fn the_grid_dump_keeps_no_break_spaces_and_combining_marks() {
        let mut pane = pane(10, 2, Source::TmuxPane);
        pane.feed("❯\u{a0}  \r\ne\u{301}".as_bytes());
        assert_eq!(pane.dump_text(), "❯\u{a0}\ne\u{301}");
    }

    /// Une sortie de `seq 1 n` dans un pane de 20 lignes et 10 000 d'historique.
    fn pane_after_seq(count: usize) -> PaneEmulator {
        let mut pane = PaneEmulator::new(
            GridSize {
                columns: 20,
                lines: 20,
            },
            10_000,
            Source::TmuxPane,
        );
        let text: String = (1..=count).map(|number| format!("{number}\r\n")).collect();
        pane.feed(text.as_bytes());
        pane
    }

    /// Le texte des cellules de la vue (défilement compris) entre `start` et `end` inclus, sur une même rangée.
    fn view_text(pane: &PaneEmulator, start: ViewPoint, end: ViewPoint) -> String {
        assert_eq!(start.row, end.row, "une occurrence sur une seule rangée");
        let content = pane.renderable();
        let offset = content.display_offset as i32;
        content
            .display_iter
            .filter(|cell| {
                cell.point.line.0 + offset == start.row as i32
                    && (start.column..=end.column).contains(&cell.point.column.0)
            })
            .map(|cell| cell.c)
            .collect()
    }

    #[test]
    fn search_finds_text_in_the_history_and_on_screen() {
        let mut pane = pane_after_seq(5000);
        assert!(pane.history_size() > 4000, "l'historique tient la sortie");
        let found = pane.search("4242");
        assert_eq!(found.len(), 1);
        // « 4242 » est la ligne 4242 de la sortie, soit la ligne 4241 depuis le haut de l'historique.
        assert_eq!(found[0].start, (4241, 0));
        assert_eq!(found[0].end, (4241, 3));
        let ones = pane.search("1");
        assert_eq!(
            ones[0].start,
            (0, 0),
            "la première occurrence est en tête de l'historique"
        );
        assert!(ones.windows(2).all(|pair| pair[0].start < pair[1].start));
    }

    #[test]
    fn search_ignores_case() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"IDEMA\r\nautre");
        assert_eq!(pane.search("idema").len(), 1);
        assert_eq!(pane.search("Idema").len(), 1);
        assert_eq!(pane.search("IDEMA").len(), 1);
    }

    #[test]
    fn special_characters_are_searched_literally() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"axb\r\na.b\r\n(x) [1] \\d+ $^*?");
        let dot = pane.search("a.b");
        assert_eq!(dot.len(), 1);
        assert_eq!(dot[0].start.0, pane.history_size() + 1);
        assert_eq!(pane.search("(x)").len(), 1);
        assert_eq!(pane.search("[1]").len(), 1);
        assert_eq!(pane.search("\\d+").len(), 1);
        assert_eq!(pane.search("$^*?").len(), 1);
        assert!(pane.search("").is_empty());
    }

    #[test]
    fn a_match_split_by_the_wrap_of_a_long_line_is_found() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed(b"0123456789012345needle-wrapped");
        let found = pane.search("needle-wrapped");
        assert_eq!(found.len(), 1);
        assert_ne!(
            found[0].start.0, found[0].end.0,
            "la ligne repliée porte l'occurrence sur deux rangées"
        );
    }

    #[test]
    fn visible_matches_follow_output_that_scrolls_the_screen() {
        let mut pane = pane(20, 10, Source::TmuxPane);
        pane.feed(b"l0\r\nl1\r\nl2\r\nl3\r\nl4\r\nneedle\r\nl6\r\nl7\r\nl8");
        assert_eq!(pane.search("needle").len(), 1);
        let before = pane.visible_matches("needle");
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].0.row, 5);
        // Cinq lignes de plus poussent l'écran vers le haut : l'occurrence change de rangée.
        pane.feed(b"\r\nx1\r\nx2\r\nx3\r\nx4\r\nx5");
        let after = pane.visible_matches("needle");
        assert_eq!(after.len(), 1);
        assert_ne!(after[0].0.row, before[0].0.row);
        assert_eq!(view_text(&pane, after[0].0, after[0].1), "needle");
    }

    #[test]
    fn visible_matches_only_report_the_part_shown() {
        let mut pane = pane_after_seq(5000);
        // En bas : « 4999 » est dans la vue, « 4242 » est dans l'historique.
        assert_eq!(pane.visible_matches("4999").len(), 1);
        assert!(pane.visible_matches("4242").is_empty());
        let found = pane.search("4242");
        pane.scroll_to_match(&found[0]);
        let shown = pane.visible_matches("4242");
        assert_eq!(shown.len(), 1);
        assert_eq!(view_text(&pane, shown[0].0, shown[0].1), "4242");
        assert_eq!(pane.match_in_view(&found[0]), Some(shown[0]));
    }

    #[test]
    fn scroll_to_match_shows_the_line() {
        let mut pane = pane(20, 5, Source::TmuxPane);
        let text: String = (0..30)
            .map(|line| {
                if line == 3 {
                    "target\r\n".to_owned()
                } else {
                    format!("line{line}\r\n")
                }
            })
            .collect();
        pane.feed(text.as_bytes());
        let found = pane.search("target");
        assert_eq!(found.len(), 1);
        assert!(
            pane.match_in_view(&found[0]).is_none(),
            "hors de la vue avant le défilement"
        );
        pane.scroll_to_match(&found[0]);
        let shown = pane.match_in_view(&found[0]).expect("the match is in view");
        assert_eq!(view_text(&pane, shown.0, shown.1), "target");
        // La vue n'est défilée que de ce qu'il faut : elle ne repart pas en bas ni ne dépasse l'historique.
        assert!(pane.display_offset() > 0);
        assert!(pane.display_offset() <= pane.history_size());
    }

    /// Le texte de la rangée `row` de la vue (défilement compris), sans les espaces finales.
    fn view_row(pane: &PaneEmulator, row: usize) -> String {
        let content = pane.renderable();
        let offset = content.display_offset as i32;
        let text: String = content
            .display_iter
            .filter(|cell| cell.point.line.0 + offset == row as i32)
            .map(|cell| cell.c)
            .collect();
        text.trim_end().to_owned()
    }

    /// Une ligne sur trois contient « hit », les autres « xxx » : le voisin d'une occurrence est trois lignes plus loin.
    fn feed_numbered(pane: &mut PaneEmulator, numbers: std::ops::Range<usize>) {
        let text: String = numbers
            .map(|n| {
                let word = if n % 3 == 0 { "hit" } else { "xxx" };
                format!("{word}{n:04}\r\n")
            })
            .collect();
        pane.feed(text.as_bytes());
    }

    #[test]
    fn the_current_match_stays_on_its_text_when_output_scrolls_a_full_history() {
        let cap = 50;
        let mut pane = PaneEmulator::new(
            GridSize {
                columns: 20,
                lines: 5,
            },
            cap,
            Source::TmuxPane,
        );
        feed_numbered(&mut pane, 0..120);
        assert_eq!(pane.history_size(), cap, "l'historique est plein");
        pane.track_scroll(true);
        let found = pane.search("hit");
        // Une occurrence ancienne, dans l'historique : la vue y va.
        let current = found[found.len() - 8];
        pane.scroll_to_match(&current);
        let shown = pane.match_in_view(&current).expect("the match is in view");
        let line = view_row(&pane, shown.0.row);
        assert!(line.starts_with("hit"), "{line}");

        // Dix-huit lignes de plus : l'historique plein perd autant de lignes du haut.
        feed_numbered(&mut pane, 120..138);
        assert_eq!(pane.history_size(), cap, "la limite d'historique est tenue");
        let shown = pane
            .match_in_view(&current)
            .expect("the match is still in history");
        assert_eq!(view_row(&pane, shown.0.row), line, "toujours sur sa ligne");
        assert_eq!(view_text(&pane, shown.0, shown.1), "hit");

        // La recherche refaite retrouve l'occurrence telle quelle, et son voisin est la suivante dans le texte.
        let again = pane.search("hit");
        let at = again
            .iter()
            .position(|found| *found == current)
            .expect("the same match, same coordinates");
        let neighbour = again[at + 1];
        assert_eq!(neighbour.start.0, current.start.0 + 3);
        pane.scroll_to_match(&neighbour);
        let shown = pane
            .match_in_view(&neighbour)
            .expect("the neighbour is in view");
        let number: usize = line["hit".len()..].parse().unwrap();
        assert_eq!(
            view_row(&pane, shown.0.row),
            format!("hit{:04}", number + 3)
        );
    }

    #[test]
    fn a_new_query_after_a_scrolled_full_history_starts_at_the_most_recent_match_in_view() {
        let mut pane = PaneEmulator::new(
            GridSize {
                columns: 20,
                lines: 5,
            },
            50,
            Source::TmuxPane,
        );
        feed_numbered(&mut pane, 0..120);
        pane.track_scroll(true);
        // Beaucoup plus de lignes que l'historique n'en tient, par blocs de plus de 4 Kio : `dropped` grandit.
        let end = 1320;
        for start in (120..end).step_by(600) {
            feed_numbered(&mut pane, start..start + 600);
        }
        assert_eq!(pane.history_size(), 50);
        let found = pane.search("hit");
        let mut recent_first = found.clone();
        recent_first.reverse();
        let bottom = pane.view_bottom_line();
        // Sans les lignes parties du haut, le bas de la vue serait compté trop tôt et manquerait les plus récentes.
        assert!(pane.history_size() + pane.size().lines - 1 < found.last().unwrap().start.0);
        // La vue est en bas : la plus récente occurrence de la vue est la première, et c'est la dernière « hit » écrite.
        let first = recent_first
            .iter()
            .position(|found| found.start.0 <= bottom)
            .expect("a match at or above the bottom");
        assert_eq!(first, 0);
        let shown = pane.match_in_view(&recent_first[first]).expect("in view");
        let last_hit = (0..end).rev().find(|n| n % 3 == 0).unwrap();
        let rows = pane.size().lines;
        let text: Vec<String> = (0..rows).map(|row| view_row(&pane, row)).collect();
        assert_eq!(text[shown.0.row], format!("hit{last_hit:04}"));
    }

    #[test]
    fn a_selection_survives_output_that_scrolls_a_full_history_during_a_search() {
        let mut pane = PaneEmulator::new(
            GridSize {
                columns: 20,
                lines: 5,
            },
            50,
            Source::TmuxPane,
        );
        feed_numbered(&mut pane, 0..120);
        pane.track_scroll(true);
        pane.start_selection(ViewPoint::new(2, 0), SelectionKind::Line);
        pane.extend_selection(ViewPoint::new(3, 0));
        feed_numbered(&mut pane, 120..123);
        assert!(pane.has_selection());
        assert!(pane.selection_text().is_some());
    }

    #[test]
    fn a_match_that_left_the_history_is_nowhere() {
        let mut pane = PaneEmulator::new(
            GridSize {
                columns: 20,
                lines: 5,
            },
            20,
            Source::TmuxPane,
        );
        feed_numbered(&mut pane, 0..40);
        pane.track_scroll(true);
        let oldest = pane.search("hit")[0];
        feed_numbered(&mut pane, 40..80);
        assert!(pane.match_in_view(&oldest).is_none());
        let before = pane.display_offset();
        pane.scroll_to_match(&oldest);
        assert_eq!(pane.display_offset(), before, "rien où défiler");
    }

    #[test]
    fn utf8_inside_a_screen_title_does_not_leak() {
        let mut pane = emulator(Source::TmuxPane);
        pane.feed("\x1bk日本語\x1b\\x".as_bytes());
        assert_eq!(pane.dump_text(), "x\n\n\n\n");
    }
}
