//! L'élément GPUI qui dessine la grille d'un pane : fonds, texte, curseur et texte en composition. Il dessine à
//! l'origine de ses bornes, sans marges : elles sont celles de la zone de la session (`content_bounds`).

use std::ops::Range;
use std::time::Instant;

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use gpui::{
    App, Bounds, CursorStyle, DispatchPhase, Element, ElementId, ElementInputHandler, Entity,
    Global, GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement,
    LayoutId, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Rgba, ShapedLine,
    SharedString, Size, Style, TextAlign, TextRun, UnderlineStyle, Window, fill, font, outline,
    point, px, relative, size,
};
use idema_term::emulator::{GridSize, Link, ViewPoint};
use idema_term::palette;

use crate::config::Config;
use crate::pane_view::PaneView;
use crate::theme::{self, Theme};

/// Police du terminal, tenue en global GPUI : posée au lancement, remplacée quand `config.toml` change.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalFont {
    pub family: SharedString,
    pub size: f32,
    pub line_height: f32,
}

impl Global for TerminalFont {}

impl TerminalFont {
    pub fn from_config(config: &Config) -> Self {
        Self {
            family: SharedString::from(config.terminal_font_family.clone()),
            size: config.terminal_font_size,
            line_height: config.terminal_line_height,
        }
    }
}

/// Les marges de la maquette (haut, côtés, bas), à l'échelle de l'interface.
const MARGIN_TOP: f32 = 4.;
pub const MARGIN_SIDES: f32 = 18.;
const MARGIN_BOTTOM: f32 = 16.;

/// Largeur et hauteur d'une cellule : avance du « m » dans la police du terminal, hauteur arrondie au pixel.
pub fn cell_metrics(window: &Window, terminal_font: &TerminalFont) -> (Pixels, Pixels) {
    let text_system = window.text_system();
    let font_id = text_system.resolve_font(&font(terminal_font.family.clone()));
    let width = match text_system.advance(font_id, px(terminal_font.size), 'm') {
        Ok(advance) => advance.width,
        Err(error) => {
            eprintln!("cannot measure the terminal cell, using 0.6 em: {error}");
            px(terminal_font.size * 0.6)
        }
    };
    (
        width,
        px((terminal_font.size * terminal_font.line_height).round()),
    )
}

/// Zone utile de la session : ses bornes moins les marges.
pub fn content_bounds(bounds: Bounds<Pixels>, theme: &Theme) -> Bounds<Pixels> {
    let (top, sides, bottom) = (
        theme.px(MARGIN_TOP),
        theme.px(MARGIN_SIDES),
        theme.px(MARGIN_BOTTOM),
    );
    Bounds::new(
        point(bounds.origin.x + sides, bounds.origin.y + top),
        Size {
            width: (bounds.size.width - sides * 2.).max(px(0.)),
            height: (bounds.size.height - top - bottom).max(px(0.)),
        },
    )
}

/// Grille tenant dans la zone utile, au minimum 2 colonnes sur 1 ligne.
pub fn grid_for(content: Bounds<Pixels>, cell: (Pixels, Pixels)) -> GridSize {
    let columns = (content.size.width / cell.0).floor() as usize;
    let lines = (content.size.height / cell.1).floor() as usize;
    GridSize {
        columns: columns.max(2),
        lines: lines.max(1),
    }
}

fn hsla(color: [u8; 3]) -> Hsla {
    Rgba {
        r: f32::from(color[0]) / 255.,
        g: f32::from(color[1]) / 255.,
        b: f32::from(color[2]) / 255.,
        a: 1.,
    }
    .into()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Underline {
    None,
    Straight,
    Wavy,
}

/// Ce qui fait changer de `TextRun` : la couleur du texte et les drapeaux de police.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    foreground: [u8; 3],
    bold: bool,
    italic: bool,
    underline: Underline,
    strikeout: bool,
}

/// Ce que la recherche (⌘F) fait d'une cellule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    None,
    /// Une occurrence : un voile `WAITING` léger sous le texte.
    Match,
    /// L'occurrence courante : un fond `WAITING` plein, le texte passe en `WINDOW`.
    Current,
}

struct StyledCell {
    text: String,
    style: CellStyle,
    background: [u8; 3],
    /// La cellule fait partie de la sélection : un voile `FOCUS` se peint dessus.
    selected: bool,
    mark: Mark,
}

impl StyledCell {
    /// Un blanc sans trait ne se dessine pas : on peut le couper en fin de rangée.
    fn is_blank(&self) -> bool {
        self.text == " " && self.style.underline == Underline::None && !self.style.strikeout
    }
}

/// Résout une couleur de cellule : surcharge du programme (OSC 4, 10, 11), sinon palette d'Idema.
fn resolve(
    color: Color,
    foreground: bool,
    overrides: &alacritty_terminal::term::color::Colors,
) -> [u8; 3] {
    let overridden = match color {
        Color::Named(named) => overrides[named],
        Color::Indexed(index) => overrides[usize::from(index)],
        Color::Spec(_) => None,
    };
    overridden.map_or_else(
        || palette::rgb(color, foreground),
        |rgb| [rgb.r, rgb.g, rgb.b],
    )
}

struct PositionedLine {
    line: ShapedLine,
    origin: Point<Pixels>,
}

/// Les plages continues de `true`, en indices : la sélection d'une rangée donne un voile par plage.
fn flag_runs(flags: impl Iterator<Item = bool>) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    let mut count = 0;
    for (index, flag) in flags.enumerate() {
        count = index + 1;
        match (flag, start) {
            (true, None) => start = Some(index),
            (false, Some(from)) => {
                runs.push(from..index);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        runs.push(from..count);
    }
    runs
}

/// Les plages soulignées d'un lien : (rangée, première colonne, dernière colonne incluse). Les rangées sont bornées à
/// la grille visible : la fin d'une URL repliée peut tomber sous la dernière rangée affichée quand la vue défile.
fn link_segments(link: &Link, grid: GridSize) -> Vec<(usize, usize, usize)> {
    range_segments(link.start, link.end, grid)
}

/// Les plages d'une étendue de cellules repliée sur plusieurs rangées, de `start` à `end` inclus : (rangée, première
/// colonne, dernière colonne incluse), bornées à la grille.
fn range_segments(
    start: (usize, usize),
    end: (usize, usize),
    grid: GridSize,
) -> Vec<(usize, usize, usize)> {
    let (Some(last_row), Some(last_column)) =
        (grid.lines.checked_sub(1), grid.columns.checked_sub(1))
    else {
        return Vec::new();
    };
    let ((first_row, first_column), (end_row, end_column)) = (start, end);
    (first_row..=end_row.min(last_row))
        .map(|row| {
            let from = if row == first_row { first_column } else { 0 };
            let to = if row == end_row {
                end_column
            } else {
                last_column
            };
            (row, from.min(last_column), to.min(last_column))
        })
        .collect()
}

/// La marque de chaque cellule de la grille (rangée par rangée) d'après les occurrences affichées et la courante.
/// L'occurrence courante l'emporte sur le voile des autres. Vide quand il n'y a rien à marquer.
fn search_marks(
    grid: GridSize,
    matches: &[(ViewPoint, ViewPoint)],
    current: Option<(ViewPoint, ViewPoint)>,
) -> Vec<Mark> {
    if matches.is_empty() && current.is_none() {
        return Vec::new();
    }
    let mut marks = vec![Mark::None; grid.lines * grid.columns];
    let mut paint = |range: (ViewPoint, ViewPoint), mark: Mark| {
        let start = (range.0.row, range.0.column);
        let end = (range.1.row, range.1.column);
        for (row, from, to) in range_segments(start, end, grid) {
            let first = row * grid.columns + from;
            let last = row * grid.columns + to;
            marks[first..=last].fill(mark);
        }
    };
    for range in matches {
        paint(*range, Mark::Match);
    }
    if let Some(range) = current {
        paint(range, Mark::Current);
    }
    marks
}

/// Une couleur `0xRRGGBB` des jetons, en octets.
fn rgb_bytes(hex: u32) -> [u8; 3] {
    [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8]
}

pub struct PrepaintState {
    fills: Vec<PaintQuad>,
    /// Les fonds des occurrences de la recherche : peints après les fonds, sous la sélection, le curseur et le texte.
    search: Vec<PaintQuad>,
    /// Les voiles de la sélection : peints après les fonds, avant le curseur et le texte.
    selection: Vec<PaintQuad>,
    selection_color: Hsla,
    search_match_color: Hsla,
    search_current_color: Hsla,
    /// Zone du pane pour le curseur de la souris.
    hitbox: Hitbox,
    /// Un lien est désigné par `⌘` et la souris : le curseur devient une main.
    pointing: bool,
    lines: Vec<PositionedLine>,
    cursor: Option<PaintQuad>,
    composition: Option<(PaintQuad, PositionedLine)>,
    line_height: Pixels,
    /// Début de l'image (entrée de `prepaint`) : `paint` en tire la durée mesurée pour le HUD.
    started: Instant,
}

pub struct TerminalElement {
    view: Entity<PaneView>,
}

impl TerminalElement {
    pub fn new(view: Entity<PaneView>) -> Self {
        Self { view }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let started = Instant::now();
        let terminal_font = cx.global::<TerminalFont>().clone();
        let cell = cell_metrics(window, &terminal_font);
        // Le pane retient sa cellule et ses bornes pour la souris et l'IME ; la taille, elle, vient de la session.
        let content = bounds;
        self.view.update(cx, |view, _| {
            view.set_geometry(cell, content);
            // Les occurrences se cherchent à chaque peinture sur la partie affichée (écart E29) : la sortie qui
            // défile l'écran ne laisse jamais un surlignage sur d'autres cellules.
            view.refresh_search_highlights();
        });

        let view = self.view.read(cx);
        let focused = view.focus_handle_ref().is_focused(window) && window.is_window_active();
        let marked = view.marked_text().map(str::to_owned);
        // Le lien ne se souligne que tant que `⌘` est tenu : un relâché manqué ne laisse pas de trait.
        let command_held = window.modifiers().platform;
        let emulator = view.emulator();
        // La grille dessinée est celle de l'émulateur : tmux peut la garder plus petite que la cellule du layout.
        let grid = emulator.size();
        let renderable = emulator.renderable();
        let offset = renderable.display_offset as i32;
        let selection = renderable.selection;
        let link_segments = view
            .hovered_link()
            .filter(|_| command_held)
            .map(|link| link_segments(link, grid))
            .unwrap_or_default();
        let pointing = !link_segments.is_empty();
        let highlights = view.search_highlights();
        let marks = search_marks(grid, &highlights.matches, highlights.current);

        // Un curseur plein recolore le glyphe dessous avec le fond, pour qu'il reste lisible.
        let cursor_row = renderable.cursor.point.line.0 + offset;
        let cursor_col = renderable.cursor.point.column.0;
        let cursor_shape = renderable.cursor.shape;
        // Un journal en lecture seule n'a pas de curseur : personne n'y tape.
        let cursor_visible = cursor_shape != CursorShape::Hidden
            && !view.is_read_only()
            && (0..grid.lines as i32).contains(&cursor_row)
            && cursor_col < grid.columns;
        let solid_block = cursor_visible && focused && matches!(cursor_shape, CursorShape::Block);
        let cursor_rgb = resolve(Color::Named(NamedColor::Cursor), true, renderable.colors);

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let mut state = PrepaintState {
            fills: Vec::new(),
            search: Vec::new(),
            selection: Vec::new(),
            selection_color: Rgba {
                a: theme::SELECTION,
                ..theme::color(theme::FOCUS)
            }
            .into(),
            search_match_color: Rgba {
                a: theme::SEARCH_MATCH,
                ..theme::color(theme::WAITING)
            }
            .into(),
            search_current_color: theme::color(theme::SEARCH_CURRENT).into(),
            hitbox,
            pointing,
            lines: Vec::new(),
            cursor: None,
            composition: None,
            line_height: cell.1,
            started,
        };
        let mut row_cells: Vec<StyledCell> = Vec::with_capacity(grid.columns);
        let mut current_row: Option<usize> = None;
        let mut cursor_is_wide = false;

        let mut flush = |row: usize, cells: &mut Vec<StyledCell>, state: &mut PrepaintState| {
            let origin = point(content.origin.x, content.origin.y + cell.1 * row as f32);
            push_row(origin, cells, cell, &terminal_font, state, window);
            cells.clear();
        };

        for indexed in renderable.display_iter {
            let row = indexed.point.line.0 + offset;
            // Rien n'est calculé au-delà de la zone utile.
            if row < 0 || row as usize >= grid.lines {
                continue;
            }
            let row = row as usize;
            let column = indexed.point.column.0;
            if column >= grid.columns {
                continue;
            }
            if current_row != Some(row) {
                if let Some(previous) = current_row {
                    flush(previous, &mut row_cells, &mut state);
                }
                current_row = Some(row);
            }

            let cell_data = indexed.cell;
            let flags = cell_data.flags;
            if cursor_visible && row as i32 == cursor_row && column == cursor_col {
                cursor_is_wide = flags.contains(Flags::WIDE_CHAR);
            }
            let (mut foreground, mut background) = (
                resolve(cell_data.fg, true, renderable.colors),
                resolve(cell_data.bg, false, renderable.colors),
            );
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut foreground, &mut background);
            }
            if flags.contains(Flags::DIM) {
                foreground = palette::dim(foreground);
            }
            if flags.contains(Flags::HIDDEN) {
                foreground = background;
            }
            if solid_block && row as i32 == cursor_row && column == cursor_col {
                foreground = palette::DEFAULT_BACKGROUND;
            }
            // Une cellule large occupe deux colonnes : sa seconde moitié porte la marque de la première.
            let mark = if flags.contains(Flags::WIDE_CHAR_SPACER) {
                row_cells
                    .last()
                    .map_or(Mark::None, |previous| previous.mark)
            } else {
                marks
                    .get(row * grid.columns + column)
                    .copied()
                    .unwrap_or(Mark::None)
            };
            if mark == Mark::Current {
                foreground = rgb_bytes(theme::SEARCH_CURRENT_TEXT);
            }

            // Une cellule large occupe deux colonnes : la seconde porte un blanc, pour qu'un glyphe
            // corresponde toujours à une colonne et que la grille reste alignée.
            let mut text = if flags.contains(Flags::WIDE_CHAR_SPACER) {
                " ".to_string()
            } else {
                cell_data.c.to_string()
            };
            if let Some(marks) = cell_data.zerowidth() {
                text.extend(marks);
            }
            let linked = link_segments
                .iter()
                .any(|&(link_row, from, to)| link_row == row && (from..=to).contains(&column));
            let underline = if linked {
                Underline::Straight
            } else if flags.contains(Flags::UNDERCURL) {
                Underline::Wavy
            } else if flags.intersects(Flags::ALL_UNDERLINES) {
                Underline::Straight
            } else {
                Underline::None
            };
            row_cells.push(StyledCell {
                text,
                style: CellStyle {
                    foreground,
                    bold: flags.contains(Flags::BOLD),
                    italic: flags.contains(Flags::ITALIC),
                    underline,
                    strikeout: flags.contains(Flags::STRIKEOUT),
                },
                background,
                selected: selection.is_some_and(|range| range.contains(indexed.point)),
                mark,
            });
        }
        if let Some(previous) = current_row {
            flush(previous, &mut row_cells, &mut state);
        }

        if cursor_visible {
            let origin = point(
                content.origin.x + cell.0 * cursor_col as f32,
                content.origin.y + cell.1 * cursor_row as f32,
            );
            let width = if cursor_is_wide { cell.0 * 2. } else { cell.0 };
            let color = hsla(cursor_rgb);
            let rect = |size| Bounds::new(origin, size);
            state.cursor = Some(if !focused {
                outline(
                    rect(size(width, cell.1)),
                    color,
                    gpui::BorderStyle::default(),
                )
            } else {
                match cursor_shape {
                    CursorShape::Beam => fill(rect(size(px(2.), cell.1)), color),
                    CursorShape::Underline => fill(
                        Bounds::new(
                            point(origin.x, origin.y + cell.1 - px(2.)),
                            size(width, px(2.)),
                        ),
                        color,
                    ),
                    CursorShape::HollowBlock => outline(
                        rect(size(width, cell.1)),
                        color,
                        gpui::BorderStyle::default(),
                    ),
                    _ => fill(rect(size(width, cell.1)), color),
                }
            });

            if let Some(text) = marked.filter(|text| !text.is_empty()) {
                let style = CellStyle {
                    foreground: palette::DEFAULT_FOREGROUND,
                    bold: false,
                    italic: false,
                    underline: Underline::Straight,
                    strikeout: false,
                };
                let runs = [text_run(text.len(), style, &terminal_font)];
                let line = window.text_system().shape_line(
                    SharedString::from(text),
                    px(terminal_font.size),
                    &runs,
                    Some(cell.0),
                );
                let background = fill(
                    Bounds::new(origin, size(line.width.max(cell.0), cell.1)),
                    hsla(palette::DEFAULT_BACKGROUND),
                );
                state.composition = Some((background, PositionedLine { line, origin }));
            }
        }
        state
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.view.read(cx).focus_handle_ref().clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );

        // La souris au-delà du pane, voire de la fenêtre : un geste en cours suit la cellule bornée à la grille, et son
        // relâché le clôt. Les écoutes sont posées à chaque peinture et ne font rien tant que le pane n'a pas de geste.
        let view = self.view.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && view.read(cx).has_gesture() {
                view.update(cx, |view, cx| view.gesture_moved(event, cx));
            }
        });
        let view = self.view.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && view.read(cx).has_gesture() {
                view.update(cx, |view, cx| view.gesture_released(event, cx));
            }
        });
        // Le relâché d'un glisser de fichiers du Finder : le pane qui les tient les dépose (écart : `on_drop` de GPUI
        // ne se déclenche pas après une frappe, voir `PaneView::on_files_dragged`).
        // Le survol se mesure sur la zone du pane : `is_hovered_at` tient compte d'un menu ouvert par-dessus (son fond
        // occulte la souris) et ne dépend pas de la dernière entrée de la fenêtre, contrairement à `is_hovered`.
        let view = self.view.clone();
        let hitbox = prepaint.hitbox.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture && cx.has_active_drag() {
                let hit = hitbox.is_hovered_at(event.position, window);
                view.update(cx, |view, cx| view.drag_hit(hit, cx));
            }
        });
        let view = self.view.clone();
        let hitbox = prepaint.hitbox.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                let hit = hitbox.is_hovered_at(event.position, window);
                let dragging = cx.has_active_drag();
                view.update(cx, |view, cx| view.drop_files(hit, dragging, window, cx));
            }
        });
        if prepaint.pointing {
            window.set_cursor_style(CursorStyle::PointingHand, &prepaint.hitbox);
        }

        for quad in prepaint.fills.drain(..) {
            window.paint_quad(quad);
        }
        for quad in prepaint.search.drain(..) {
            window.paint_quad(quad);
        }
        for quad in prepaint.selection.drain(..) {
            window.paint_quad(quad);
        }
        if let Some(cursor) = prepaint.cursor.take() {
            window.paint_quad(cursor);
        }
        for positioned in prepaint.lines.drain(..) {
            paint_line(&positioned, prepaint.line_height, window, cx);
        }
        if let Some((background, positioned)) = prepaint.composition.take() {
            window.paint_quad(background);
            paint_line(&positioned, prepaint.line_height, window, cx);
        }
        let frame = prepaint.started.elapsed();
        let refresh_interval = window.refresh_interval(cx);
        self.view
            .update(cx, |view, _| view.record_frame(frame, refresh_interval));
    }
}

fn paint_line(positioned: &PositionedLine, line_height: Pixels, window: &mut Window, cx: &mut App) {
    if let Err(error) = positioned.line.paint(
        positioned.origin,
        line_height,
        TextAlign::Left,
        None,
        window,
        cx,
    ) {
        eprintln!("cannot paint a terminal row: {error}");
    }
}

fn text_run(len: usize, style: CellStyle, terminal_font: &TerminalFont) -> TextRun {
    let mut run_font = font(terminal_font.family.clone());
    if style.bold {
        run_font = run_font.bold();
    }
    if style.italic {
        run_font = run_font.italic();
    }
    let color = hsla(style.foreground);
    TextRun {
        len,
        font: run_font,
        color,
        background_color: None,
        underline: match style.underline {
            Underline::None => None,
            Underline::Straight => Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(color),
                wavy: false,
            }),
            Underline::Wavy => Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(color),
                wavy: true,
            }),
        },
        strikethrough: style.strikeout.then(|| gpui::StrikethroughStyle {
            thickness: px(1.),
            color: Some(color),
        }),
    }
}

/// Transforme une rangée de cellules en fonds fusionnés et en une ligne de texte.
fn push_row(
    origin: Point<Pixels>,
    cells: &mut Vec<StyledCell>,
    cell: (Pixels, Pixels),
    terminal_font: &TerminalFont,
    state: &mut PrepaintState,
    window: &mut Window,
) {
    // Un `fill` par plage de fond différente du fond par défaut.
    let mut start = 0;
    while start < cells.len() {
        let background = cells[start].background;
        let mut end = start + 1;
        while end < cells.len() && cells[end].background == background {
            end += 1;
        }
        if background != palette::DEFAULT_BACKGROUND {
            let range = Bounds::new(
                point(origin.x + cell.0 * start as f32, origin.y),
                size(cell.0 * (end - start) as f32, cell.1),
            );
            state.fills.push(fill(range, hsla(background)));
        }
        start = end;
    }

    // Les occurrences de la recherche : un voile par plage, plein pour l'occurrence courante.
    for (mark, color) in [
        (Mark::Match, state.search_match_color),
        (Mark::Current, state.search_current_color),
    ] {
        for run in flag_runs(cells.iter().map(|styled| styled.mark == mark)) {
            let range = Bounds::new(
                point(origin.x + cell.0 * run.start as f32, origin.y),
                size(cell.0 * run.len() as f32, cell.1),
            );
            state.search.push(fill(range, color));
        }
    }

    // Un voile par plage de cellules sélectionnées, jusqu'au bord de la grille même si la fin de rangée est blanche.
    let selection_color = state.selection_color;
    for run in flag_runs(cells.iter().map(|styled| styled.selected)) {
        let range = Bounds::new(
            point(origin.x + cell.0 * run.start as f32, origin.y),
            size(cell.0 * run.len() as f32, cell.1),
        );
        state.selection.push(fill(range, selection_color));
    }

    while cells.last().is_some_and(StyledCell::is_blank) {
        cells.pop();
    }
    if cells.is_empty() {
        return;
    }

    // Les cellules voisines de même style partagent un `TextRun`.
    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut run_style = cells[0].style;
    let mut run_len = 0;
    for styled in cells.iter() {
        if styled.style != run_style {
            runs.push(text_run(run_len, run_style, terminal_font));
            run_style = styled.style;
            run_len = 0;
        }
        text.push_str(&styled.text);
        run_len += styled.text.len();
    }
    runs.push(text_run(run_len, run_style, terminal_font));

    let line = window.text_system().shape_line(
        SharedString::from(text),
        px(terminal_font.size),
        &runs,
        Some(cell.0),
    );
    state.lines.push(PositionedLine { line, origin });
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRID: GridSize = GridSize {
        columns: 80,
        lines: 24,
    };

    fn link(start: (usize, usize), end: (usize, usize)) -> Link {
        Link {
            uri: "https://example.com".to_owned(),
            start,
            end,
        }
    }

    #[test]
    fn a_link_on_one_row_is_underlined_from_its_start_to_its_end() {
        assert_eq!(link_segments(&link((3, 10), (3, 24)), GRID), [(3, 10, 24)]);
    }

    #[test]
    fn a_wrapped_link_is_underlined_to_the_edge_then_from_the_start_of_the_next_row() {
        assert_eq!(
            link_segments(&link((3, 70), (5, 8)), GRID),
            [(3, 70, 79), (4, 0, 79), (5, 0, 8)]
        );
    }

    #[test]
    fn a_link_ending_below_the_visible_grid_is_cut_at_the_last_row() {
        // La vue défile : la fin de l'URL est sous la dernière rangée affichée.
        assert_eq!(
            link_segments(&link((22, 70), (26, 5)), GRID),
            [(22, 70, 79), (23, 0, 79)]
        );
    }

    #[test]
    fn a_link_wholly_below_the_visible_grid_is_not_underlined() {
        assert!(link_segments(&link((24, 0), (25, 5)), GRID).is_empty());
    }

    #[test]
    fn a_match_wrapped_on_two_rows_is_marked_to_the_edge_then_from_the_start() {
        let wrapped = (ViewPoint::new(3, 78), ViewPoint::new(4, 2));
        let marks = search_marks(GRID, &[wrapped], None);
        let at = |row: usize, column: usize| marks[row * GRID.columns + column];
        assert_eq!(at(3, 77), Mark::None);
        assert_eq!((at(3, 78), at(3, 79)), (Mark::Match, Mark::Match));
        assert_eq!((at(4, 0), at(4, 2)), (Mark::Match, Mark::Match));
        assert_eq!(at(4, 3), Mark::None);
    }

    #[test]
    fn the_current_match_wins_over_the_veil_of_the_others() {
        let first = (ViewPoint::new(1, 0), ViewPoint::new(1, 4));
        let second = (ViewPoint::new(5, 10), ViewPoint::new(5, 14));
        let marks = search_marks(GRID, &[first, second], Some(second));
        let at = |row: usize, column: usize| marks[row * GRID.columns + column];
        assert_eq!(at(1, 2), Mark::Match);
        assert_eq!(at(5, 10), Mark::Current);
        assert_eq!(at(5, 14), Mark::Current);
        assert_eq!(at(5, 15), Mark::None);
    }

    #[test]
    fn nothing_is_marked_without_a_match() {
        assert!(search_marks(GRID, &[], None).is_empty());
    }

    #[test]
    fn selected_cells_make_one_run_per_contiguous_stretch() {
        let flags = [false, true, true, false, true, false, true, true];
        assert_eq!(flag_runs(flags.into_iter()), [1..3, 4..5, 6..8]);
        assert!(flag_runs([false, false].into_iter()).is_empty());
        assert_eq!(flag_runs([true, true, true].into_iter()), vec![0..3]);
    }
}
