//! Layouts d'une fenêtre tmux (spec §4.4) : les neuf presets et leur chaîne `select-layout` avec sa somme de
//! contrôle, la lecture des layouts JSON de tmux 3.8 (`new-layouts`), les séparateurs à dessiner entre les panes,
//! le voisin d'un pane dans une direction, et le plan d'un changement de preset.

use std::fmt::Write as _;

use serde::Deserialize;

use crate::protocol::PaneId;

/// Largeur minimale d'un pane, en cellules : celle qu'`alacritty_terminal` accepte (`MIN_COLUMNS`).
const MIN_COLUMNS: u16 = 2;
/// Hauteur minimale d'un pane, en lignes.
const MIN_LINES: u16 = 1;

/// Un preset du menu « Layout », dans l'ordre du menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Preset {
    Single,
    Columns2,
    Rows2,
    MainLeft,
    MainRight,
    Columns3,
    Grid2x2,
    ThreeTwo,
    Grid3x2,
}

/// Forme d'un preset : un pane, ou une division égale en colonnes (`{…}`) ou en lignes (`[…]`).
enum Shape {
    Pane,
    Columns(Vec<Shape>),
    Rows(Vec<Shape>),
}

impl Shape {
    /// Voir `WindowLayout::column_gaps`.
    fn column_gaps(&self) -> u16 {
        match self {
            Self::Pane => 0,
            Self::Columns(children) => columns_gaps(children.iter().map(Self::column_gaps)),
            Self::Rows(children) => children.iter().map(Self::column_gaps).max().unwrap_or(0),
        }
    }
}

/// Gouttières d'une rangée de colonnes : celles de chaque enfant, plus une par séparateur entre deux enfants.
fn columns_gaps(children: impl ExactSizeIterator<Item = u16>) -> u16 {
    let separators = u16::try_from(children.len().saturating_sub(1)).unwrap_or(u16::MAX);
    children.sum::<u16>() + separators
}

impl Preset {
    pub const ALL: [Preset; 9] = [
        Preset::Single,
        Preset::Columns2,
        Preset::Rows2,
        Preset::MainLeft,
        Preset::MainRight,
        Preset::Columns3,
        Preset::Grid2x2,
        Preset::ThreeTwo,
        Preset::Grid3x2,
    ];

    /// Identifiant écrit dans `@idema_layout`.
    pub fn id(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Columns2 => "columns-2",
            Self::Rows2 => "rows-2",
            Self::MainLeft => "main-left",
            Self::MainRight => "main-right",
            Self::Columns3 => "columns-3",
            Self::Grid2x2 => "grid-2x2",
            Self::ThreeTwo => "three-two",
            Self::Grid3x2 => "grid-3x2",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.id() == id)
    }

    pub fn panes(self) -> usize {
        match self {
            Self::Single => 1,
            Self::Columns2 | Self::Rows2 => 2,
            Self::MainLeft | Self::MainRight | Self::Columns3 => 3,
            Self::Grid2x2 => 4,
            Self::ThreeTwo => 5,
            Self::Grid3x2 => 6,
        }
    }

    /// Le premier preset du menu qui a `panes` panes : celui qu'Idema applique quand un pane se ferme.
    pub fn first_with(panes: usize) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.panes() == panes)
    }

    /// Le plus grand nombre de séparateurs verticaux qu'une ligne horizontale traverse dans ce preset : celui de
    /// `preset_cell`, calculé sur la forme sans connaître la taille de la fenêtre.
    pub fn column_gaps(self) -> u16 {
        self.shape().column_gaps()
    }

    fn shape(self) -> Shape {
        let panes = |count: usize| (0..count).map(|_| Shape::Pane).collect::<Vec<_>>();
        match self {
            Self::Single => Shape::Pane,
            Self::Columns2 => Shape::Columns(panes(2)),
            Self::Rows2 => Shape::Rows(panes(2)),
            Self::MainLeft => Shape::Columns(vec![Shape::Pane, Shape::Rows(panes(2))]),
            Self::MainRight => Shape::Columns(vec![Shape::Rows(panes(2)), Shape::Pane]),
            Self::Columns3 => Shape::Columns(panes(3)),
            Self::Grid2x2 => Shape::Rows(vec![Shape::Columns(panes(2)), Shape::Columns(panes(2))]),
            Self::ThreeTwo => Shape::Rows(vec![Shape::Columns(panes(3)), Shape::Columns(panes(2))]),
            Self::Grid3x2 => Shape::Rows(vec![Shape::Columns(panes(3)), Shape::Columns(panes(3))]),
        }
    }
}

/// Somme de contrôle d'une chaîne de layout, sur tout ce qui suit la première virgule (`layout-custom.c`).
pub fn layout_checksum(layout: &str) -> u16 {
    layout.bytes().fold(0u16, |sum, byte| {
        (sum >> 1)
            .wrapping_add((sum & 1) << 15)
            .wrapping_add(u16::from(byte))
    })
}

/// Chaîne `select-layout` du preset pour une fenêtre de `width` × `height` cellules : `<somme>,<L>x<H>,0,0…`.
/// `None` quand la fenêtre est trop petite pour donner à chaque pane 2 colonnes et 1 ligne.
pub fn preset_layout(preset: Preset, width: u16, height: u16) -> Option<String> {
    let mut body = String::new();
    write_cell(&mut body, &preset_cell(preset, width, height)?);
    Some(format!("{:04x},{body}", layout_checksum(&body)))
}

/// La géométrie du preset pour une fenêtre de `width` × `height` cellules. tmux réserve une cellule entre deux
/// panes ; les tailles se répartissent à parts égales, le reste aux premiers. Les panes sont numérotés dans
/// l'ordre (`PaneId(0)`, `PaneId(1)`…) : tmux ignore ces numéros et attribue les panes dans leur ordre (sonde du 15/10).
pub fn preset_cell(preset: Preset, width: u16, height: u16) -> Option<LayoutCell> {
    let mut next_pane = 0;
    build_cell(&preset.shape(), (0, 0, width, height), &mut next_pane)
}

fn build_cell(
    shape: &Shape,
    (x, y, width, height): (u16, u16, u16, u16),
    next_pane: &mut u32,
) -> Option<LayoutCell> {
    if width < MIN_COLUMNS || height < MIN_LINES {
        return None;
    }
    let kind = match shape {
        Shape::Pane => {
            *next_pane += 1;
            CellKind::Pane(PaneId(*next_pane - 1))
        }
        Shape::Columns(children) => {
            let mut at = x;
            let mut cells = Vec::with_capacity(children.len());
            for (child, size) in children.iter().zip(split_evenly(width, children.len())?) {
                cells.push(build_cell(child, (at, y, size, height), next_pane)?);
                at += size + 1;
            }
            CellKind::Columns(cells)
        }
        Shape::Rows(children) => {
            let mut at = y;
            let mut cells = Vec::with_capacity(children.len());
            for (child, size) in children.iter().zip(split_evenly(height, children.len())?) {
                cells.push(build_cell(child, (x, at, width, size), next_pane)?);
                at += size + 1;
            }
            CellKind::Rows(cells)
        }
    };
    Some(LayoutCell {
        x,
        y,
        width,
        height,
        kind,
    })
}

/// Écrit une cellule au format de `layout-custom.c` : `<L>x<H>,<x>,<y>` suivi de `,<pane>`, `{…}` ou `[…]`.
fn write_cell(out: &mut String, cell: &LayoutCell) {
    let _ = write!(out, "{}x{},{},{}", cell.width, cell.height, cell.x, cell.y);
    let (open, close, children) = match &cell.kind {
        CellKind::Pane(pane) => {
            let _ = write!(out, ",{}", pane.0);
            return;
        }
        CellKind::Columns(children) => ('{', '}', children),
        CellKind::Rows(children) => ('[', ']', children),
    };
    out.push(open);
    for (index, child) in children.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_cell(out, child);
    }
    out.push(close);
}

/// Partage `length` cellules entre `count` enfants séparés d'une cellule ; le reste va aux premiers.
fn split_evenly(length: u16, count: usize) -> Option<Vec<u16>> {
    let count = u16::try_from(count).ok()?;
    let available = length.checked_sub(count.checked_sub(1)?)?;
    let (base, extra) = (available / count, available % count);
    Some(
        (0..count)
            .map(|index| base + u16::from(index < extra))
            .collect(),
    )
}

/// Le contenu d'une cellule du layout lu chez tmux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellKind {
    Pane(PaneId),
    /// Enfants côte à côte, de gauche à droite (`"t":"h"`).
    Columns(Vec<LayoutCell>),
    /// Enfants empilés, de haut en bas (`"t":"v"`).
    Rows(Vec<LayoutCell>),
}

/// Une cellule du layout, en cellules de terminal depuis le coin haut gauche de la fenêtre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutCell {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub kind: CellKind,
}

/// Le layout d'une fenêtre tel que tmux le rapporte, avec son pane actif.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowLayout {
    pub root: LayoutCell,
    pub active: Option<PaneId>,
}

/// Un pane placé dans la fenêtre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneRect {
    pub pane: PaneId,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl PaneRect {
    /// Centre au double, pour rester en entiers.
    fn center2(&self) -> (i64, i64) {
        (
            2 * i64::from(self.x) + i64::from(self.width),
            2 * i64::from(self.y) + i64::from(self.height),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// Trait vertical, dans la colonne `at`.
    Vertical,
    /// Trait horizontal, dans la ligne `at`.
    Horizontal,
}

/// Un séparateur à dessiner dans la cellule que tmux réserve entre deux panes : colonne (ou ligne) `at`, à partir
/// de la ligne (ou colonne) `from`, sur `length` cellules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Separator {
    pub orientation: Orientation,
    pub at: u16,
    pub from: u16,
    pub length: u16,
}

/// Un séparateur avec les gouttières que l'affichage ajoute autour de lui (voir `WindowLayout::column_gaps`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GappedSeparator {
    pub separator: Separator,
    /// Gouttières strictement à gauche du trait ; pour un trait horizontal, à gauche de son nœud.
    pub gaps_before: u16,
    /// Gouttières que son nœud traverse : elles allongent un trait horizontal. Zéro pour un trait vertical.
    pub gaps_within: u16,
}

impl WindowLayout {
    pub fn width(&self) -> u16 {
        self.root.width
    }

    pub fn height(&self) -> u16 {
        self.root.height
    }

    /// Les panes dans l'ordre du layout, qui est l'ordre des panes de la fenêtre.
    pub fn panes(&self) -> Vec<PaneRect> {
        let mut out = Vec::new();
        collect_panes(&self.root, &mut out);
        out
    }

    pub fn separators(&self) -> Vec<Separator> {
        let mut out = Vec::new();
        collect_separators(&self.root, &mut out);
        out
    }

    /// Le plus grand nombre de séparateurs verticaux qu'une ligne horizontale traverse. Idema écarte deux panes
    /// côte à côte d'une gouttière de plus que la cellule que tmux réserve (voir `gutter.rs`) : c'est le nombre de
    /// gouttières à retrancher de la largeur avant de demander la taille de la fenêtre à tmux.
    pub fn column_gaps(&self) -> u16 {
        self.root.column_gaps()
    }

    /// Les panes dans l'ordre du layout, chacun avec le nombre de gouttières à sa gauche.
    pub fn gapped_panes(&self) -> Vec<(PaneRect, u16)> {
        let mut out = Vec::new();
        collect_gapped_panes(&self.root, 0, &mut out);
        out
    }

    /// Les séparateurs, avec les gouttières qui décalent et allongent leur trait.
    pub fn gapped_separators(&self) -> Vec<GappedSeparator> {
        let mut out = Vec::new();
        collect_gapped_separators(&self.root, 0, &mut out);
        out
    }
}

impl LayoutCell {
    fn column_gaps(&self) -> u16 {
        match &self.kind {
            CellKind::Pane(_) => 0,
            CellKind::Columns(children) => {
                columns_gaps(children.iter().map(LayoutCell::column_gaps))
            }
            CellKind::Rows(children) => children.iter().map(Self::column_gaps).max().unwrap_or(0),
        }
    }
}

/// Les enfants d'un nœud en colonnes avec leur décalage : `offset` plus, pour chaque enfant précédent, ses
/// gouttières et celle du séparateur qui le suit.
fn column_offsets(children: &[LayoutCell], offset: u16) -> Vec<u16> {
    let mut next = offset;
    children
        .iter()
        .map(|child| {
            let at = next;
            next += child.column_gaps() + 1;
            at
        })
        .collect()
}

fn collect_gapped_panes(cell: &LayoutCell, offset: u16, out: &mut Vec<(PaneRect, u16)>) {
    match &cell.kind {
        CellKind::Pane(pane) => out.push((
            PaneRect {
                pane: *pane,
                x: cell.x,
                y: cell.y,
                width: cell.width,
                height: cell.height,
            },
            offset,
        )),
        CellKind::Columns(children) => {
            for (child, at) in children.iter().zip(column_offsets(children, offset)) {
                collect_gapped_panes(child, at, out);
            }
        }
        CellKind::Rows(children) => {
            for child in children {
                collect_gapped_panes(child, offset, out);
            }
        }
    }
}

fn collect_gapped_separators(cell: &LayoutCell, offset: u16, out: &mut Vec<GappedSeparator>) {
    let (CellKind::Columns(children) | CellKind::Rows(children)) = &cell.kind else {
        return;
    };
    let vertical = matches!(cell.kind, CellKind::Columns(_));
    let offsets = if vertical {
        column_offsets(children, offset)
    } else {
        vec![offset; children.len()]
    };
    for (index, separator) in own_separators(cell).into_iter().enumerate() {
        out.push(if vertical {
            // À gauche du trait : tout ce qui précède l'enfant suivant, moins la gouttière de ce trait.
            GappedSeparator {
                separator,
                gaps_before: offsets[index + 1] - 1,
                gaps_within: 0,
            }
        } else {
            GappedSeparator {
                separator,
                gaps_before: offset,
                gaps_within: cell.column_gaps(),
            }
        });
    }
    for (child, at) in children.iter().zip(offsets) {
        collect_gapped_separators(child, at, out);
    }
}

fn collect_panes(cell: &LayoutCell, out: &mut Vec<PaneRect>) {
    match &cell.kind {
        CellKind::Pane(pane) => out.push(PaneRect {
            pane: *pane,
            x: cell.x,
            y: cell.y,
            width: cell.width,
            height: cell.height,
        }),
        CellKind::Columns(children) | CellKind::Rows(children) => {
            for child in children {
                collect_panes(child, out);
            }
        }
    }
}

/// Les séparateurs entre les enfants directs d'un nœud, sans ceux de ses descendants.
fn own_separators(cell: &LayoutCell) -> Vec<Separator> {
    let (children, orientation) = match &cell.kind {
        CellKind::Pane(_) => return Vec::new(),
        CellKind::Columns(children) => (children, Orientation::Vertical),
        CellKind::Rows(children) => (children, Orientation::Horizontal),
    };
    children
        .windows(2)
        .map(|pair| match orientation {
            Orientation::Vertical => Separator {
                orientation,
                at: pair[0].x + pair[0].width,
                from: cell.y,
                length: cell.height,
            },
            Orientation::Horizontal => Separator {
                orientation,
                at: pair[0].y + pair[0].height,
                from: cell.x,
                length: cell.width,
            },
        })
        .collect()
}

fn collect_separators(cell: &LayoutCell, out: &mut Vec<Separator>) {
    out.extend(own_separators(cell));
    if let CellKind::Columns(children) | CellKind::Rows(children) = &cell.kind {
        for child in children {
            collect_separators(child, out);
        }
    }
}

#[derive(Deserialize)]
struct RawLayout {
    #[serde(rename = "L")]
    root: RawCell,
}

#[derive(Deserialize)]
struct RawCell {
    t: String,
    w: u16,
    h: u16,
    x: u16,
    y: u16,
    #[serde(default)]
    c: Vec<RawCell>,
    #[serde(rename = "I", default)]
    pane: Option<String>,
    #[serde(default)]
    a: bool,
}

/// Lit un layout JSON de tmux 3.8 : `{"V":2,"L":{"t":"h","w":120,…,"c":[{"t":"p",…,"I":"%0"},…]}}`.
/// `"t"` vaut `p` (pane), `h` (côte à côte) ou `v` (empilés) ; `"a"` marque le pane actif. `None` s'il est illisible.
pub fn parse_layout(json: &str) -> Option<WindowLayout> {
    let raw: RawLayout = serde_json::from_str(json).ok()?;
    let mut active = None;
    let root = convert(&raw.root, &mut active)?;
    Some(WindowLayout { root, active })
}

fn convert(raw: &RawCell, active: &mut Option<PaneId>) -> Option<LayoutCell> {
    let children = |raw: &RawCell, active: &mut Option<PaneId>| {
        (!raw.c.is_empty())
            .then(|| raw.c.iter().map(|child| convert(child, active)).collect())
            .flatten()
    };
    let kind = match raw.t.as_str() {
        "p" => {
            let pane = PaneId(raw.pane.as_deref()?.strip_prefix('%')?.parse().ok()?);
            if raw.a {
                *active = Some(pane);
            }
            CellKind::Pane(pane)
        }
        "h" => CellKind::Columns(children(raw, active)?),
        "v" => CellKind::Rows(children(raw, active)?),
        _ => return None,
    };
    Some(LayoutCell {
        x: raw.x,
        y: raw.y,
        width: raw.w,
        height: raw.h,
        kind,
    })
}

/// Ce que dit un `%layout-change` : `<layout> <layout visible> <drapeaux>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutChange {
    /// Le layout complet de la fenêtre, tous ses panes, zoomés ou non.
    pub layout: WindowLayout,
    /// Le layout affiché : le pane zoomé seul quand la fenêtre est zoomée, sinon le layout complet.
    pub visible: WindowLayout,
    /// Si les drapeaux sont là, si la fenêtre est la fenêtre courante de la session (`*`).
    pub current: Option<bool>,
    /// Drapeau `Z` : la fenêtre est zoomée.
    pub zoomed: bool,
}

/// Lit le reste d'un `%layout-change`. Les layouts JSON ne contiennent pas d'espace.
pub fn parse_layout_change(rest: &str) -> Option<LayoutChange> {
    let mut parts = rest.splitn(3, ' ');
    let layout = parse_layout(parts.next()?)?;
    let visible = parse_layout(parts.next()?)?;
    let flags = parts.next();
    Some(LayoutChange {
        layout,
        visible,
        current: flags.map(|flags| flags.contains('*')),
        zoomed: flags.is_some_and(|flags| flags.contains('Z')),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Le pane voisin de `from` dans `direction` : parmi les panes situés entièrement au-delà du bord de `from` dans
/// cette direction, celui dont le centre est le plus proche du sien ; à égalité, le premier dans l'ordre du layout.
pub fn neighbor(panes: &[PaneRect], from: PaneId, direction: Direction) -> Option<PaneId> {
    let origin = panes.iter().find(|rect| rect.pane == from)?;
    let (ox, oy) = origin.center2();
    panes
        .iter()
        .filter(|rect| rect.pane != from)
        .filter(|rect| match direction {
            Direction::Right => rect.x >= origin.x + origin.width,
            Direction::Left => rect.x + rect.width <= origin.x,
            Direction::Down => rect.y >= origin.y + origin.height,
            Direction::Up => rect.y + rect.height <= origin.y,
        })
        .min_by_key(|rect| {
            let (cx, cy) = rect.center2();
            (cx - ox).pow(2) + (cy - oy).pow(2)
        })
        .map(|rect| rect.pane)
}

/// Ce qu'un changement de preset fait aux panes : lesquels cacher (dans l'ordre), lesquels rejoindre (dans
/// l'ordre), et combien de shells neufs créer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresetPlan {
    pub hide: Vec<PaneId>,
    pub rejoin: Vec<PaneId>,
    pub create: usize,
}

/// Plan pour passer à `target` panes visibles (spec §4.4). `visible` : les panes visibles dans l'ordre du layout ;
/// `hidden` : les panes cachés et l'instant où ils l'ont été. Réduire cache les derniers dans l'ordre du layout,
/// jamais `focused` ; agrandir rejoint les cachés du plus récent au plus ancien, puis crée des shells.
pub fn plan_preset(
    visible: &[PaneId],
    focused: Option<PaneId>,
    hidden: &[(PaneId, u128)],
    target: usize,
) -> PresetPlan {
    let target = target.max(1);
    if visible.len() > target {
        let hide = visible
            .iter()
            .rev()
            .filter(|pane| Some(**pane) != focused)
            .take(visible.len() - target)
            .copied()
            .collect();
        return PresetPlan {
            hide,
            ..PresetPlan::default()
        };
    }
    let needed = target - visible.len();
    let mut by_recency = hidden.to_vec();
    by_recency.sort_by_key(|(_, hidden_at)| std::cmp::Reverse(*hidden_at));
    let rejoin: Vec<PaneId> = by_recency
        .into_iter()
        .take(needed)
        .map(|(pane, _)| pane)
        .collect();
    PresetPlan {
        create: needed - rejoin.len(),
        rejoin,
        hide: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_matches_the_value_tmux_printed() {
        // Vérifié le 14/10 sur tmux 3.8 (spec §4.4), et le 15/10 par `select-layout`.
        assert_eq!(
            layout_checksum("120x40,0,0{60x40,0,0,0,59x40,61,0,1}"),
            0xf91d
        );
    }

    #[test]
    fn columns_two_reproduces_the_spec_sample() {
        assert_eq!(
            preset_layout(Preset::Columns2, 120, 40).as_deref(),
            Some("f91d,120x40,0,0{60x40,0,0,0,59x40,61,0,1}")
        );
    }

    #[test]
    fn every_preset_tiles_the_window_with_its_panes_and_one_cell_gaps() {
        for (width, height) in [(120, 40), (80, 24), (211, 57), (33, 9)] {
            for preset in Preset::ALL {
                let panes = WindowLayout {
                    root: preset_cell(preset, width, height).unwrap(),
                    active: None,
                }
                .panes();
                assert_eq!(
                    panes.len(),
                    preset.panes(),
                    "{} {width}x{height}",
                    preset.id()
                );
                // Chaque cellule de la fenêtre est couverte par un pane au plus ; les autres sont les séparateurs.
                let covered: usize = panes
                    .iter()
                    .map(|r| usize::from(r.width) * usize::from(r.height))
                    .sum();
                for (i, a) in panes.iter().enumerate() {
                    assert!(a.x + a.width <= width && a.y + a.height <= height);
                    for b in &panes[i + 1..] {
                        let apart = a.x + a.width <= b.x
                            || b.x + b.width <= a.x
                            || a.y + a.height <= b.y
                            || b.y + b.height <= a.y;
                        assert!(apart, "{} overlaps at {width}x{height}", preset.id());
                    }
                }
                assert!(
                    covered < usize::from(width) * usize::from(height) || preset == Preset::Single
                );
            }
        }
    }

    #[test]
    fn equal_panes_differ_by_one_cell_at_most_and_the_first_ones_get_the_rest() {
        let panes = WindowLayout {
            root: preset_cell(Preset::Columns3, 120, 40).unwrap(),
            active: None,
        }
        .panes();
        assert_eq!(
            panes.iter().map(|r| r.width).collect::<Vec<_>>(),
            [40, 39, 39]
        );
        assert_eq!(panes.iter().map(|r| r.x).collect::<Vec<_>>(), [0, 41, 81]);
    }

    #[test]
    fn three_two_puts_three_panes_on_top_and_two_below() {
        assert_eq!(
            preset_layout(Preset::ThreeTwo, 120, 40)
                .as_deref()
                .map(|s| &s[5..]),
            Some(
                "120x40,0,0[120x20,0,0{40x20,0,0,0,39x20,41,0,1,39x20,81,0,2},120x19,0,21{60x19,0,21,3,59x19,61,21,4}]"
            )
        );
    }

    #[test]
    fn main_left_and_main_right_mirror_each_other() {
        assert_eq!(
            preset_layout(Preset::MainLeft, 120, 40)
                .as_deref()
                .map(|s| &s[5..]),
            Some("120x40,0,0{60x40,0,0,0,59x40,61,0[59x20,61,0,1,59x19,61,21,2]}")
        );
        assert_eq!(
            preset_layout(Preset::MainRight, 120, 40)
                .as_deref()
                .map(|s| &s[5..]),
            Some("120x40,0,0{60x40,0,0[60x20,0,0,0,60x19,0,21,1],59x40,61,0,2}")
        );
    }

    #[test]
    fn a_window_too_small_for_the_preset_has_no_layout() {
        assert_eq!(preset_layout(Preset::Grid3x2, 7, 40), None);
        assert_eq!(preset_layout(Preset::Rows2, 120, 2), None);
        assert!(preset_layout(Preset::Grid3x2, 8, 3).is_some());
    }

    #[test]
    fn presets_round_trip_through_their_ids_and_counts() {
        for preset in Preset::ALL {
            assert_eq!(Preset::from_id(preset.id()), Some(preset));
            assert_eq!(
                Preset::first_with(preset.panes()).map(Preset::panes),
                Some(preset.panes())
            );
        }
        assert_eq!(Preset::first_with(3), Some(Preset::MainLeft));
        assert_eq!(Preset::first_with(7), None);
        assert_eq!(Preset::from_id("tiled"), None);
    }

    /// Layout relevé sur tmux 3.8 le 15/10 (sonde 1) : une colonne à gauche, deux panes empilés à droite.
    const MAIN_LEFT_JSON: &str = r#"{"V":2,"L":{"t":"h","w":120,"h":40,"x":0,"y":0,"c":[{"t":"p","w":60,"h":40,"x":0,"y":0,"l":1,"i":0,"I":"%0"},{"t":"v","w":59,"h":40,"x":61,"y":0,"c":[{"t":"p","w":59,"h":20,"x":61,"y":0,"l":0,"i":1,"I":"%1"},{"t":"p","w":59,"h":19,"x":61,"y":21,"a":true,"i":2,"I":"%2"}]}]}}"#;

    #[test]
    fn a_tmux_json_layout_gives_panes_in_order_and_the_active_one() {
        let layout = parse_layout(MAIN_LEFT_JSON).unwrap();
        assert_eq!((layout.width(), layout.height()), (120, 40));
        assert_eq!(layout.active, Some(PaneId(2)));
        assert_eq!(
            layout.panes(),
            vec![
                PaneRect {
                    pane: PaneId(0),
                    x: 0,
                    y: 0,
                    width: 60,
                    height: 40
                },
                PaneRect {
                    pane: PaneId(1),
                    x: 61,
                    y: 0,
                    width: 59,
                    height: 20
                },
                PaneRect {
                    pane: PaneId(2),
                    x: 61,
                    y: 21,
                    width: 59,
                    height: 19
                },
            ]
        );
    }

    #[test]
    fn separators_sit_in_the_cells_tmux_reserves() {
        let layout = parse_layout(MAIN_LEFT_JSON).unwrap();
        assert_eq!(
            layout.separators(),
            vec![
                Separator {
                    orientation: Orientation::Vertical,
                    at: 60,
                    from: 0,
                    length: 40
                },
                Separator {
                    orientation: Orientation::Horizontal,
                    at: 20,
                    from: 61,
                    length: 59
                },
            ]
        );
    }

    #[test]
    fn a_single_pane_layout_has_no_separator() {
        let layout = parse_layout(
            r#"{"V":2,"L":{"t":"p","w":120,"h":40,"x":0,"y":0,"a":true,"i":0,"I":"%7"}}"#,
        )
        .unwrap();
        assert_eq!(layout.panes().len(), 1);
        assert!(layout.separators().is_empty());
        assert_eq!(layout.active, Some(PaneId(7)));
    }

    #[test]
    fn unreadable_layouts_are_refused() {
        assert_eq!(
            parse_layout("f91d,120x40,0,0{60x40,0,0,0,59x40,61,0,1}"),
            None
        );
        assert_eq!(
            parse_layout(r#"{"V":2,"L":{"t":"p","w":1,"h":1,"x":0,"y":0}}"#),
            None
        );
        assert_eq!(
            parse_layout(r#"{"V":2,"L":{"t":"h","w":1,"h":1,"x":0,"y":0,"c":[]}}"#),
            None
        );
        assert_eq!(
            parse_layout(r#"{"V":2,"L":{"t":"z","w":1,"h":1,"x":0,"y":0}}"#),
            None
        );
    }

    #[test]
    fn layout_change_reads_the_current_window_flag() {
        let single = r#"{"V":2,"L":{"t":"p","w":120,"h":40,"x":0,"y":0,"a":true,"i":0,"I":"%0"}}"#;
        let current = parse_layout_change(&format!("{single} {single} *")).unwrap();
        assert_eq!(current.current, Some(true));
        assert!(!current.zoomed);
        // La fenêtre cachée n'a pas de drapeau : le champ est vide (sonde 1).
        let stash = parse_layout_change(&format!("{single} {single} ")).unwrap();
        assert_eq!(stash.current, Some(false));
        let bare = parse_layout_change(&format!("{single} {single}")).unwrap();
        assert_eq!(bare.current, None);
        assert!(!bare.zoomed);
        assert_eq!(parse_layout_change("garbage"), None);
    }

    #[test]
    fn layout_change_reads_the_zoom_flag_and_both_layouts() {
        // Ligne de la sonde du 16/10 : le layout complet à deux panes, puis le pane zoomé seul.
        let full = r#"{"V":2,"L":{"t":"h","w":120,"h":40,"x":0,"y":0,"c":[{"t":"p","w":60,"h":40,"x":0,"y":0,"a":true,"i":0,"I":"%0"},{"t":"p","w":59,"h":40,"x":61,"y":0,"l":0,"i":1,"I":"%1"}]}}"#;
        let alone = r#"{"V":2,"L":{"t":"p","w":120,"h":40,"x":0,"y":0,"a":true,"i":0,"I":"%0"}}"#;
        let zoom = parse_layout_change(&format!("{full} {alone} *Z")).unwrap();
        assert!(zoom.zoomed);
        assert_eq!(zoom.current, Some(true));
        assert_eq!(zoom.layout.panes().len(), 2);
        assert_eq!(zoom.visible.panes().len(), 1);
        let unzoom = parse_layout_change(&format!("{full} {full} *")).unwrap();
        assert!(!unzoom.zoomed);
    }

    fn rect(pane: u32, x: u16, y: u16, width: u16, height: u16) -> PaneRect {
        PaneRect {
            pane: PaneId(pane),
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn neighbor_follows_the_direction_in_two_columns() {
        let panes = [rect(0, 0, 0, 60, 40), rect(1, 61, 0, 59, 40)];
        assert_eq!(
            neighbor(&panes, PaneId(0), Direction::Right),
            Some(PaneId(1))
        );
        assert_eq!(
            neighbor(&panes, PaneId(1), Direction::Left),
            Some(PaneId(0))
        );
        assert_eq!(neighbor(&panes, PaneId(0), Direction::Left), None);
        assert_eq!(neighbor(&panes, PaneId(0), Direction::Up), None);
    }

    #[test]
    fn neighbor_picks_the_closest_center_in_three_two() {
        // Trois en haut (40, 39, 39), deux en bas (60, 59).
        let panes = [
            rect(0, 0, 0, 40, 20),
            rect(1, 41, 0, 39, 20),
            rect(2, 81, 0, 39, 20),
            rect(3, 0, 21, 60, 19),
            rect(4, 61, 21, 59, 19),
        ];
        assert_eq!(
            neighbor(&panes, PaneId(0), Direction::Down),
            Some(PaneId(3))
        );
        // Le pane du milieu (centre à 60,5) est à 30 du pane de droite (90,5) et à 30,5 de celui de gauche (30).
        assert_eq!(
            neighbor(&panes, PaneId(1), Direction::Down),
            Some(PaneId(4))
        );
        assert_eq!(
            neighbor(&panes, PaneId(2), Direction::Down),
            Some(PaneId(4))
        );
        assert_eq!(neighbor(&panes, PaneId(4), Direction::Up), Some(PaneId(2)));
        assert_eq!(
            neighbor(&panes, PaneId(3), Direction::Right),
            Some(PaneId(4))
        );
        assert_eq!(neighbor(&panes, PaneId(9), Direction::Right), None);
    }

    #[test]
    fn neighbor_from_the_main_pane_reaches_the_closest_stacked_one() {
        let panes = [
            rect(0, 0, 0, 60, 40),
            rect(1, 61, 0, 59, 20),
            rect(2, 61, 21, 59, 19),
        ];
        // Centre de 0 : (60, 40) au double ; 1 : (181, 20) ; 2 : (181, 61). 1 est le plus proche.
        assert_eq!(
            neighbor(&panes, PaneId(0), Direction::Right),
            Some(PaneId(1))
        );
        assert_eq!(
            neighbor(&panes, PaneId(2), Direction::Left),
            Some(PaneId(0))
        );
    }

    #[test]
    fn shrinking_hides_the_last_panes_but_never_the_focused_one() {
        let visible = [PaneId(0), PaneId(1), PaneId(2), PaneId(3)];
        assert_eq!(
            plan_preset(&visible, Some(PaneId(0)), &[], 1).hide,
            vec![PaneId(3), PaneId(2), PaneId(1)]
        );
        assert_eq!(
            plan_preset(&visible, Some(PaneId(3)), &[], 2).hide,
            vec![PaneId(2), PaneId(1)]
        );
        let plan = plan_preset(&visible, Some(PaneId(3)), &[], 1);
        assert_eq!(plan.hide, vec![PaneId(2), PaneId(1), PaneId(0)]);
        assert!(plan.rejoin.is_empty() && plan.create == 0);
    }

    #[test]
    fn growing_rejoins_the_most_recently_hidden_first_then_creates_shells() {
        let visible = [PaneId(0)];
        let hidden = [(PaneId(3), 100), (PaneId(1), 102), (PaneId(2), 101)];
        let plan = plan_preset(&visible, Some(PaneId(0)), &hidden, 3);
        assert_eq!(plan.rejoin, vec![PaneId(1), PaneId(2)]);
        assert_eq!(plan.create, 0);
        let plan = plan_preset(&visible, Some(PaneId(0)), &hidden, 6);
        assert_eq!(plan.rejoin, vec![PaneId(1), PaneId(2), PaneId(3)]);
        assert_eq!(plan.create, 2);
        assert_eq!(plan_preset(&visible, None, &[], 1), PresetPlan::default());
    }

    // Layouts écrits à la main pour les gouttières : seule la forme compte, la géométrie reste cohérente.
    fn pane_json(id: u32, x: u16, y: u16, width: u16, height: u16) -> String {
        format!(r#"{{"t":"p","w":{width},"h":{height},"x":{x},"y":{y},"i":{id},"I":"%{id}"}}"#)
    }

    fn group_json(
        kind: char,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        children: &[String],
    ) -> String {
        format!(
            r#"{{"t":"{kind}","w":{width},"h":{height},"x":{x},"y":{y},"c":[{}]}}"#,
            children.join(",")
        )
    }

    fn layout_of(root: &str) -> WindowLayout {
        parse_layout(&format!(r#"{{"V":2,"L":{root}}}"#)).expect("a valid layout")
    }

    fn single_json() -> String {
        pane_json(0, 0, 0, 120, 40)
    }

    fn columns_2_json() -> String {
        group_json(
            'h',
            0,
            0,
            120,
            40,
            &[pane_json(0, 0, 0, 60, 40), pane_json(1, 61, 0, 59, 40)],
        )
    }

    fn rows_2_json() -> String {
        group_json(
            'v',
            0,
            0,
            120,
            40,
            &[pane_json(0, 0, 0, 120, 20), pane_json(1, 0, 21, 120, 19)],
        )
    }

    fn columns_3_json() -> String {
        group_json(
            'h',
            0,
            0,
            120,
            40,
            &[
                pane_json(0, 0, 0, 40, 40),
                pane_json(1, 41, 0, 39, 40),
                pane_json(2, 81, 0, 39, 40),
            ],
        )
    }

    fn three_two_json() -> String {
        let top = group_json(
            'h',
            0,
            0,
            120,
            20,
            &[
                pane_json(0, 0, 0, 40, 20),
                pane_json(1, 41, 0, 39, 20),
                pane_json(2, 81, 0, 39, 20),
            ],
        );
        let bottom = group_json(
            'h',
            0,
            21,
            120,
            19,
            &[pane_json(3, 0, 21, 60, 19), pane_json(4, 61, 21, 59, 19)],
        );
        group_json('v', 0, 0, 120, 40, &[top, bottom])
    }

    /// `{ {a, b}, c }` : deux colonnes dont la première est elle-même coupée en deux.
    fn nested_columns_json() -> String {
        let inner = group_json(
            'h',
            0,
            0,
            79,
            40,
            &[pane_json(0, 0, 0, 39, 40), pane_json(1, 40, 0, 39, 40)],
        );
        group_json('h', 0, 0, 120, 40, &[inner, pane_json(2, 80, 0, 40, 40)])
    }

    #[test]
    fn column_gaps_counts_the_widest_row() {
        let gaps = |root: String| layout_of(&root).column_gaps();
        assert_eq!(gaps(single_json()), 0);
        assert_eq!(gaps(columns_2_json()), 1);
        assert_eq!(gaps(rows_2_json()), 0);
        assert_eq!(gaps(columns_3_json()), 2);
        assert_eq!(gaps(three_two_json()), 2);
        assert_eq!(gaps(nested_columns_json()), 2);
    }

    #[test]
    fn every_preset_has_the_gaps_of_its_own_layout() {
        for preset in Preset::ALL {
            let layout = WindowLayout {
                root: preset_cell(preset, 120, 40).unwrap(),
                active: None,
            };
            assert_eq!(
                preset.column_gaps(),
                layout.column_gaps(),
                "{}",
                preset.id()
            );
        }
    }

    #[test]
    fn panes_know_the_gutters_to_their_left() {
        let gaps = |root: String| -> Vec<u16> {
            layout_of(&root)
                .gapped_panes()
                .iter()
                .map(|(_, gaps)| *gaps)
                .collect()
        };
        assert_eq!(gaps(nested_columns_json()), [0, 1, 2]);
        assert_eq!(gaps(three_two_json()), [0, 1, 2, 0, 1]);
        assert_eq!(gaps(rows_2_json()), [0, 0]);
        // Les rectangles ne changent pas : seul le décompte s'y ajoute.
        let layout = layout_of(&three_two_json());
        let rects: Vec<PaneRect> = layout.gapped_panes().into_iter().map(|(r, _)| r).collect();
        assert_eq!(rects, layout.panes());
    }

    #[test]
    fn separators_know_their_gutters() {
        let layout = layout_of(&three_two_json());
        let between_rows = layout
            .gapped_separators()
            .into_iter()
            .find(|gapped| gapped.separator.orientation == Orientation::Horizontal)
            .expect("a horizontal separator");
        assert_eq!((between_rows.gaps_before, between_rows.gaps_within), (0, 2));
        let verticals: Vec<(u16, u16)> = layout
            .gapped_separators()
            .into_iter()
            .filter(|gapped| gapped.separator.orientation == Orientation::Vertical)
            .map(|gapped| (gapped.separator.at, gapped.gaps_before))
            .collect();
        assert_eq!(verticals, [(40, 0), (80, 1), (60, 0)]);

        // Le trait entre `{a, b}` et `c` a la gouttière de `{a, b}` à sa gauche, pas la sienne.
        let nested = layout_of(&nested_columns_json());
        let gaps: Vec<(u16, u16)> = nested
            .gapped_separators()
            .into_iter()
            .map(|gapped| (gapped.separator.at, gapped.gaps_before))
            .collect();
        assert_eq!(gaps, [(79, 1), (39, 0)]);
    }
}
