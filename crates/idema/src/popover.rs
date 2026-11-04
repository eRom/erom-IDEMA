//! Le menu flottant fait main (maquette `IDEWindow.dc.html`, lignes 104 à 117) : titre, entrées avec coche et
//! raccourci, notes, séparateurs et sous-menus. Il se rend dans une couche différée, ancrée à la souris et rabattue dans
//! la fenêtre ; un appui à l'extérieur (avalé) ou Échap le ferme. Toutes les cotes passent par `Theme::px`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Anchor, AnchoredPositionMode, AnyElement, Bounds, BoxShadow, Context, Entity, FocusHandle,
    FontWeight, IntoElement, Pixels, Point, SharedString, Size, Stateful, Task, Window, anchored,
    black, canvas, deferred, div, img, point, prelude::*, px, svg,
};
use idema_term::activity::ActivityState;
use idema_tmux::layout::{Preset, WindowLayout, preset_cell};

use crate::assets::Icon;
use crate::changes::panel::ChangesPanel;
use crate::files::panel::FilesPanel;
use crate::theme::{
    CONTROL, ERROR, FOCUS, HOVER_SHORTCUT, POPOVER, TEXT_DISABLED, TEXT_FULL, TEXT_LABEL, TEXT_MIN,
    THUMB_BACKGROUND, THUMB_BORDER, THUMB_BORDER_HOVER, THUMB_PANE, THUMB_PANE_HOVER, Theme,
    WAITING, color, color_alpha, white,
};
use crate::workspace::{CloseMenu, Workspace};

/// Contexte de touches du menu : `escape` s'y accroche (`CloseMenu`).
pub const KEY_CONTEXT: &str = "Menu";

/// Largeur minimale du menu : celle de la maquette.
pub(crate) const MENU_WIDTH: f32 = 220.;
/// Marge de la fenêtre que le menu ne franchit pas.
const WINDOW_MARGIN: f32 = 8.;
/// La marge interne du panneau.
const PANEL_PADDING: f32 = 5.;
/// Le sous-menu mord d'autant sur le bord de son menu, comme ceux de macOS.
const SUBMENU_OVERLAP: f32 = 2.;
/// Délai avant de changer de sous-menu : le temps de traverser en diagonale les entrées voisines pour rejoindre celui qui
/// est ouvert.
pub(crate) const SUBMENU_DELAY: Duration = Duration::from_millis(150);
/// Décalage des entrées d'un sous-menu dans les noms de groupes de survol, uniques entre les deux panneaux.
const SUBMENU_ITEM_BASE: usize = 1000;

/// Ce qu'une entrée fait au clic, une fois le menu fermé.
pub type OnSelect = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// Ce qu'une vignette de layout fait au clic, une fois le menu fermé.
pub type OnSelectLayout = Rc<dyn Fn(Preset, &mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// Grille de cellules dans laquelle une vignette dessine son preset (écart E22) : 28 colonnes sur 16 lignes, une
/// cellule valant `Theme::px(2.)`, soit 56 × 32 dans une vignette de 64 × 40.
const THUMB_COLUMNS: u16 = 28;
const THUMB_LINES: u16 = 16;

#[derive(Clone)]
pub struct MenuSpec {
    pub title: Option<SharedString>,
    pub entries: Vec<MenuEntry>,
}

#[derive(Clone)]
pub enum MenuEntry {
    Item(MenuItemSpec),
    Separator,
    Note(SharedString),
    /// Un titre de section, du style du titre de menu.
    Heading(SharedString),
    /// Une entrée qui déplie un panneau à côté d'elle, au survol ou au clic.
    ///
    /// `openable` : l'entrée s'ouvre et reste éclairée même quand aucun de ses items n'est actif (les sessions d'un
    /// autre projet restent visibles, grisées). Faux pour le lanceur : un harness sans agent actif reste fermé.
    Submenu {
        label: SharedString,
        items: Vec<MenuItemSpec>,
        openable: bool,
    },
    /// Les neuf presets en vignettes (trois par rangée) ; `current` est cerclé de `FOCUS`.
    Layouts {
        current: Preset,
        on_select: OnSelectLayout,
    },
    /// « N panes masqués », avec la pastille de leur état s'il attend ou est en erreur.
    HiddenPanes {
        count: usize,
        dot: Option<ActivityState>,
    },
}

/// Un pane de vignette : son rectangle, en cellules de la grille de la vignette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbCell {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Les panes du preset dans la grille de la vignette, dans l'ordre du layout : la géométrie de tmux pour une
/// fenêtre de 28 × 16, une cellule d'écart entre deux panes.
pub fn thumbnail_cells(preset: Preset) -> Vec<ThumbCell> {
    let Some(root) = preset_cell(preset, THUMB_COLUMNS, THUMB_LINES) else {
        return Vec::new();
    };
    WindowLayout { root, active: None }
        .panes()
        .iter()
        .map(|rect| ThumbCell {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        })
        .collect()
}

/// Le libellé de la ligne des panes cachés.
pub fn hidden_panes_label(count: usize) -> String {
    if count == 1 {
        "1 pane masqué".to_owned()
    } else {
        format!("{count} panes masqués")
    }
}

/// La pastille de « N panes masqués » d'après le plus urgent des panes cachés : seuls « attend » et « erreur »
/// en portent une (écart E18).
pub fn hidden_panes_dot(state: ActivityState) -> Option<ActivityState> {
    matches!(state, ActivityState::Waiting | ActivityState::Error).then_some(state)
}

/// Hauteur maximale du panneau : la fenêtre moins ses marges, et moins la place prise au-dessus du panneau quand
/// il s'ancre sous un bouton (`top`, sinon la marge de la fenêtre).
fn panel_max_height(viewport: f32, top: f32) -> f32 {
    (viewport - top.max(WINDOW_MARGIN) - WINDOW_MARGIN).max(0.)
}

#[derive(Clone)]
pub struct MenuItemSpec {
    pub label: SharedString,
    pub checked: bool,
    pub shortcut: Option<SharedString>,
    /// Un texte secondaire, peint en `TEXT_MIN` après le libellé et coupé par une ellipse (le chemin d'un worktree).
    pub detail: Option<SharedString>,
    /// La colonne du logo d'agent, avant le libellé : `Some(None)` la réserve vide pour garder l'alignement d'un agent
    /// sans logo, `None` l'omet.
    pub logo: Option<Option<&'static str>>,
    pub enabled: bool,
    pub on_select: OnSelect,
}

impl MenuItemSpec {
    /// Une entrée active, sans coche ni raccourci.
    pub fn new(
        label: impl Into<SharedString>,
        on_select: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            checked: false,
            shortcut: None,
            detail: None,
            logo: None,
            enabled: true,
            on_select: Rc::new(on_select),
        }
    }

    pub fn detail(mut self, text: impl Into<SharedString>) -> Self {
        self.detail = Some(text.into());
        self
    }

    /// Le logo d'un agent (chemin pour `img()`), ou sa place vide quand l'agent n'en a pas.
    pub fn logo(mut self, path: Option<&'static str>) -> Self {
        self.logo = Some(path);
        self
    }

    pub fn shortcut(mut self, shortcut: impl Into<SharedString>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// Où le menu s'ancre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Placement {
    /// À la souris, en coordonnées de la fenêtre.
    Pointer(Point<Pixels>),
    /// Sous le bouton `⋮` de la barre d'outils : le menu est un enfant de ce bouton.
    Toolbar,
}

/// Le menu ouvert : sa description, où il s'ancre, et le focus qu'il prend.
pub struct OpenMenu {
    pub spec: MenuSpec,
    pub placement: Placement,
    pub focus: FocusHandle,
    /// Le volet Fichiers dont l'arbre a ouvert ce menu : à la fermeture (entrée, Échap, clic dehors) le focus revient à
    /// son arbre, pas à son éditeur. `None` pour tout autre menu.
    pub tree_of: Option<Entity<FilesPanel>>,
    /// Le volet Changes dont la liste a ouvert ce menu : à la fermeture le focus revient à sa liste. `None` pour tout
    /// autre menu.
    pub changes_of: Option<Entity<ChangesPanel>>,
    /// Le menu vient du diff de `changes_of` : à la fermeture le focus revient au diff (`⌘⌥C` n'y vaut que là) et non à
    /// la liste. Porté par le menu, il meurt avec lui, quelle que soit la façon dont le menu disparaît.
    pub changes_to_diff: bool,
    /// Le rang dans `spec.entries` du sous-menu ouvert.
    pub submenu: Option<usize>,
    /// Le changement de sous-menu qui attend `SUBMENU_DELAY`.
    pub submenu_switch: Option<Task<()>>,
    /// Les rectangles des entrées à sous-menu, relevés à chaque image : le panneau s'ancre à côté de la sienne.
    pub submenu_rows: SubmenuRows,
}

/// Le rectangle de chaque entrée à sous-menu, par rang dans `MenuSpec::entries`.
pub type SubmenuRows = Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>;

/// Le menu en couche flottante, en deux couches différées : un fond transparent de la taille de la fenêtre qui
/// avale tout appui hors du menu (il le ferme sans que rien dessous ne le reçoive, ce que ne fait pas une écoute
/// de phase de capture), puis le panneau au-dessus. Un clic sur une entrée active ferme le menu, puis appelle
/// son `on_select`.
pub fn render_menu(
    menu: &OpenMenu,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let theme = *theme;
    let viewport = window.viewport_size();
    let backdrop = deferred(
        anchored()
            .position(point(px(0.), px(0.)))
            .position_mode(AnchoredPositionMode::Window)
            .child(
                div()
                    .w(viewport.width)
                    .h(viewport.height)
                    .occlude()
                    .on_any_mouse_down(
                        cx.listener(|this, _, window, cx| this.close_menu(window, cx)),
                    ),
            ),
    )
    .with_priority(1);
    // Le haut du panneau, quand il est sous le bouton `⋮` (centré dans l'en-tête de 48, le menu 32 plus bas). Un menu
    // à la souris est rabattu dans la fenêtre par l'ancre : il peut occuper toute la hauteur moins les marges.
    let top = match menu.placement {
        Placement::Toolbar => f32::from((theme.px(48.) - theme.px(28.)) / 2. + theme.px(32.)),
        Placement::Pointer(_) => 0.,
    };
    let max_height = panel_max_height(f32::from(viewport.height), top);
    let mut panel = panel_chrome(
        div()
            .id("menu-panel")
            .key_context(KEY_CONTEXT)
            .track_focus(&menu.focus)
            .on_action(cx.listener(|this, _: &CloseMenu, window, cx| this.close_menu(window, cx))),
        viewport,
        max_height,
        &theme,
    );

    if let Some(title) = &menu.spec.title {
        panel = panel.child(heading(title, &theme));
    }
    for (index, entry) in menu.spec.entries.iter().enumerate() {
        panel = match entry {
            MenuEntry::Item(item) => {
                let row = render_item(item, index, &theme, cx).on_hover(cx.listener(
                    |this, hovered: &bool, _, cx| {
                        if *hovered {
                            this.hover_submenu(None, cx);
                        }
                    },
                ));
                panel.child(row)
            }
            MenuEntry::Separator => panel.child(
                div()
                    .h(px(1.))
                    .my(theme.px(4.))
                    .flex_none()
                    .bg(white(CONTROL)),
            ),
            MenuEntry::Note(text) => panel.child(
                div()
                    .pt(theme.px(2.))
                    .pr(theme.px(9.))
                    .pb(theme.px(4.))
                    .pl(theme.px(26.))
                    .text_size(theme.text(11.))
                    .text_color(white(TEXT_MIN))
                    .child(text.clone()),
            ),
            MenuEntry::Heading(text) => panel.child(heading(text, &theme)),
            MenuEntry::Submenu {
                label,
                items,
                openable,
            } => panel.child(render_submenu_row(
                SubmenuRow {
                    label,
                    items,
                    openable: *openable,
                },
                index,
                menu.submenu == Some(index),
                &menu.submenu_rows,
                &theme,
                cx,
            )),
            MenuEntry::Layouts { current, on_select } => {
                panel.child(render_layouts(*current, on_select, &theme, cx))
            }
            MenuEntry::HiddenPanes { count, dot } => {
                panel.child(render_hidden_panes(*count, *dot, &theme))
            }
        };
    }

    let anchor = anchored().snap_to_window_with_margin(px(WINDOW_MARGIN));
    let anchor = match menu.placement {
        Placement::Pointer(position) => anchor.position(position),
        // Sans `position`, l'ancre est l'origine du bouton parent : le décalage porte le coin haut-droit du menu au
        // coin droit du bouton, 32 plus bas (la maquette place ses popovers à 32 sous un bouton de 28).
        Placement::Toolbar => anchor
            .anchor(Anchor::TopRight)
            .offset(point(theme.px(28.), theme.px(32.))),
    };
    let panel = deferred(anchor.child(panel)).with_priority(2);
    let submenu = menu.submenu.and_then(|index| {
        let MenuEntry::Submenu { items, .. } = menu.spec.entries.get(index)? else {
            return None;
        };
        let row = *menu.submenu_rows.borrow().get(&index)?;
        Some(render_submenu(items, row, viewport, &theme, cx))
    });
    // Un seul élément pour l'appelant : les couches sont rendues à la suite, sans boîte qui ajoute de la mise en page.
    div()
        .absolute()
        .size_0()
        .child(backdrop)
        .child(panel)
        .children(submenu)
        .into_any_element()
}

/// L'habillage commun d'un panneau de menu, et sa mise en page en colonne.
fn panel_chrome(
    panel: Stateful<gpui::Div>,
    viewport: Size<Pixels>,
    max_height: f32,
    theme: &Theme,
) -> Stateful<gpui::Div> {
    panel
        .occlude()
        .flex()
        .flex_col()
        // Un libellé plus long que la largeur de la maquette élargit le menu au lieu d'être coupé (« Ouvrir dans le
        // navigateur par défaut », spec P4 §4.2), comme un menu de macOS, sans dépasser la fenêtre.
        .min_w(theme.px(MENU_WIDTH))
        .max_w(viewport.width - px(2. * WINDOW_MARGIN))
        // Un menu qui dépasse la fenêtre défile, sans que la grille ni les agents ne sortent de l'écran.
        .max_h(px(max_height))
        .overflow_y_scroll()
        // Le bord de 1 px est dessiné dans la boîte, la marge interne de 5 le compte.
        .p(theme.px(PANEL_PADDING) - px(1.))
        .rounded(theme.px(9.))
        .bg(color_alpha(POPOVER))
        .border_1()
        .border_color(white(CONTROL))
        .shadow(vec![
            BoxShadow {
                color: black().opacity(0.7),
                offset: point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(0.5),
                inset: false,
            },
            BoxShadow {
                color: black().opacity(0.5),
                offset: point(px(0.), theme.px(14.)),
                blur_radius: theme.px(36.),
                spread_radius: px(0.),
                inset: false,
            },
        ])
        .text_size(theme.text(13.))
}

/// Le coin par lequel s'ancre un sous-menu, et sa position : à droite de son entrée, ou à gauche quand la largeur du menu
/// n'y tient plus (le menu `⋮` colle au bord droit de la fenêtre). La première entrée s'aligne sur celle du parent.
fn submenu_anchor(
    row: Bounds<Pixels>,
    viewport_width: Pixels,
    menu_width: Pixels,
    padding: Pixels,
) -> (Anchor, Point<Pixels>) {
    let top = row.top() - padding;
    let right = row.right() + padding - px(SUBMENU_OVERLAP);
    if right + menu_width + px(WINDOW_MARGIN) <= viewport_width {
        (Anchor::TopLeft, point(right, top))
    } else {
        (
            Anchor::TopRight,
            point(row.left() - padding + px(SUBMENU_OVERLAP), top),
        )
    }
}

/// Le panneau d'un sous-menu, au-dessus de celui du menu. Y entrer annule un changement de sous-menu en attente.
fn render_submenu(
    items: &[MenuItemSpec],
    row: Bounds<Pixels>,
    viewport: Size<Pixels>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let (corner, position) = submenu_anchor(
        row,
        viewport.width,
        theme.px(MENU_WIDTH),
        theme.px(PANEL_PADDING),
    );
    let mut panel = panel_chrome(
        div()
            .id("menu-submenu")
            .on_hover(cx.listener(|this, hovered: &bool, _, _| {
                if *hovered {
                    this.keep_submenu();
                }
            })),
        viewport,
        panel_max_height(f32::from(viewport.height), 0.),
        theme,
    );
    for (index, item) in items.iter().enumerate() {
        panel = panel.child(render_item(item, SUBMENU_ITEM_BASE + index, theme, cx));
    }
    deferred(
        anchored()
            .anchor(corner)
            .position(position)
            .position_mode(AnchoredPositionMode::Window)
            .snap_to_window_with_margin(px(WINDOW_MARGIN))
            .child(panel),
    )
    .with_priority(3)
    .into_any_element()
}

/// Vrai si une entrée à sous-menu s'ouvre (et s'éclaire) : elle le déclare, ou un de ses items est actif.
pub(crate) fn submenu_opens(items: &[MenuItemSpec], openable: bool) -> bool {
    openable || items.iter().any(|item| item.enabled)
}

/// Ce qui définit une entrée à sous-menu, groupé pour que `render_submenu_row` garde un nombre d'arguments raisonnable.
struct SubmenuRow<'a> {
    label: &'a SharedString,
    items: &'a [MenuItemSpec],
    openable: bool,
}

/// Une entrée à sous-menu : le gabarit d'une entrée, coche si le sous-menu porte le défaut, chevron à droite. Elle reste
/// en surbrillance tant que son panneau est ouvert. Sans entrée active, elle est éteinte et ne s'ouvre pas, sauf si
/// elle est `openable` (`submenu_opens`).
fn render_submenu_row(
    SubmenuRow {
        label,
        items,
        openable,
    }: SubmenuRow<'_>,
    index: usize,
    open: bool,
    rows: &SubmenuRows,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let enabled = submenu_opens(items, openable);
    let alpha = if enabled { TEXT_LABEL } else { TEXT_DISABLED };
    let highlighted = open && enabled;
    let foreground = if highlighted { TEXT_FULL } else { alpha };
    let target = enabled.then_some(index);
    let rows = rows.clone();
    let check = items.iter().any(|item| item.checked).then(|| {
        svg()
            .path(Icon::Check.path())
            .size(theme.px(15.))
            .text_color(white(foreground))
    });
    div()
        .id(("menu-submenu-row", index))
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .gap(theme.px(6.))
        .h(theme.px(26.))
        .pl(theme.px(5.))
        .pr(theme.px(5.))
        .rounded(theme.px(5.))
        .text_color(white(foreground))
        .when(highlighted, |row| row.bg(color(FOCUS)))
        .when(enabled, |row| {
            row.cursor_pointer()
                .hover(|style| style.bg(color(FOCUS)).text_color(white(TEXT_FULL)))
                .on_click(cx.listener(move |this, _, _, cx| this.open_submenu(index, cx)))
        })
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered {
                this.hover_submenu(target, cx);
            }
        }))
        .child(
            canvas(
                move |bounds, _, _| {
                    rows.borrow_mut().insert(index, bounds);
                },
                |_, (), _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(theme.px(16.))
                .children(check),
        )
        .child(div().flex_1().min_w_0().truncate().child(label.clone()))
        .child(
            svg()
                .flex_none()
                .path(Icon::ChevronRight.path())
                .size(theme.px(15.))
                .text_color(white(foreground)),
        )
}

/// Un titre de menu ou de section : 11, graisse 600, blanc à 55 %.
fn heading(text: &SharedString, theme: &Theme) -> impl IntoElement {
    div()
        .pt(theme.px(4.))
        .px(theme.px(9.))
        .pb(theme.px(5.))
        .text_size(theme.text(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(white(TEXT_MIN))
        .child(text.clone())
}

/// La grille des vignettes : trois par rangée dans l'ordre de `Preset::ALL`, réparties sur la largeur utile du menu
/// (l'écart nominal de 9 absorbe l'arrondi des cotes à l'échelle), marge `2 0 6` autour.
fn render_layouts(
    current: Preset,
    on_select: &OnSelectLayout,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let mut grid = div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(theme.px(8.))
        .mt(theme.px(2.))
        .mb(theme.px(6.));
    for (row, presets) in Preset::ALL.chunks(3).enumerate() {
        let mut line = div().flex().flex_none().justify_between();
        for (column, preset) in presets.iter().enumerate() {
            let index = row * 3 + column;
            line = line.child(render_thumbnail(
                *preset,
                *preset == current,
                index,
                on_select.clone(),
                theme,
                cx,
            ));
        }
        grid = grid.child(line);
    }
    grid
}

/// Une vignette de 64 × 40 : fond noir à 22 %, les panes du preset (blanc 30 %, 50 % au survol) centrés dans la
/// grille de cellules, et un bord posé par-dessus pour que le cercle de 2 px du preset courant ne décale rien.
fn render_thumbnail(
    preset: Preset,
    current: bool,
    index: usize,
    on_select: OnSelectLayout,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let group = format!("layout-thumb-{index}");
    let (width, height) = (theme.px(64.), theme.px(40.));
    let cell = theme.px(2.);
    let left = ((width - cell * f32::from(THUMB_COLUMNS)) / 2.).floor();
    let top = ((height - cell * f32::from(THUMB_LINES)) / 2.).floor();
    let panes = thumbnail_cells(preset).into_iter().map(|pane| {
        div()
            .absolute()
            .left(left + cell * f32::from(pane.x))
            .top(top + cell * f32::from(pane.y))
            .w(cell * f32::from(pane.width))
            .h(cell * f32::from(pane.height))
            .rounded(theme.px(2.))
            .bg(white(THUMB_PANE))
            .group_hover(group.clone(), |style| style.bg(white(THUMB_PANE_HOVER)))
    });
    let ring = div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .rounded(theme.px(5.));
    let ring = if current {
        ring.border(theme.px(2.)).border_color(color(FOCUS))
    } else {
        ring.border_1()
            .border_color(white(THUMB_BORDER))
            .group_hover(group.clone(), |style| {
                style.border_color(white(THUMB_BORDER_HOVER))
            })
    };
    div()
        .id(("layout-thumb", index))
        .group(group.clone())
        .relative()
        .flex_none()
        .w(width)
        .h(height)
        .rounded(theme.px(5.))
        .bg(black().opacity(THUMB_BACKGROUND))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, window, cx| {
            this.close_menu(window, cx);
            on_select(preset, this, window, cx);
        }))
        .children(panes)
        .child(ring)
}

/// « N panes masqués » : le style d'une `Note`, avec la pastille de 7 dans la colonne de coche des entrées.
fn render_hidden_panes(
    count: usize,
    dot: Option<ActivityState>,
    theme: &Theme,
) -> impl IntoElement {
    let dot = dot.map(|state| {
        let tone = if state == ActivityState::Error {
            ERROR
        } else {
            WAITING
        };
        div()
            .flex_none()
            .size(theme.px(7.))
            .rounded_full()
            .bg(color(tone))
    });
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(theme.px(6.))
        .pt(theme.px(2.))
        .pr(theme.px(9.))
        .pb(theme.px(4.))
        .pl(theme.px(5.))
        .text_size(theme.text(11.))
        .text_color(white(TEXT_MIN))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(theme.px(16.))
                .children(dot),
        )
        .child(hidden_panes_label(count))
}

/// Une entrée de 26 : colonne de coche de 16, logo d'agent de 16 s'il y a lieu, libellé, raccourci. Survolée, elle passe en `FOCUS` ; désactivée,
/// elle est à 25 % et sans survol ni clic.
fn render_item(
    item: &MenuItemSpec,
    index: usize,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> Stateful<gpui::Div> {
    let group = format!("menu-item-{index}");
    let enabled = item.enabled;
    let label_alpha = if enabled { TEXT_LABEL } else { TEXT_DISABLED };
    let shortcut_alpha = if enabled { TEXT_MIN } else { TEXT_DISABLED };
    let on_select = item.on_select.clone();

    let check = item.checked.then(|| {
        let icon = svg()
            .path(Icon::Check.path())
            .size(theme.px(15.))
            .text_color(white(label_alpha));
        if enabled {
            icon.group_hover(group.clone(), |style| style.text_color(white(TEXT_FULL)))
        } else {
            icon
        }
    });
    let shortcut = item.shortcut.clone().map(|shortcut| {
        let text = div()
            .flex_none()
            .text_size(theme.text(12.))
            .text_color(white(shortcut_alpha))
            .child(shortcut);
        if enabled {
            text.group_hover(group.clone(), |style| {
                style.text_color(white(HOVER_SHORTCUT))
            })
        } else {
            text
        }
    });

    let detail = item.detail.clone().map(|detail| {
        // Largeur nulle avant de grandir : le chemin d'un worktree n'élargit pas le menu, il prend la place qui reste.
        let text = div()
            .w_0()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_size(theme.text(11.))
            .text_color(white(shortcut_alpha))
            .child(detail);
        if enabled {
            text.group_hover(group.clone(), |style| {
                style.text_color(white(HOVER_SHORTCUT))
            })
        } else {
            text
        }
    });

    div()
        .id(("menu-item", index))
        .group(group)
        .flex()
        .flex_none()
        .items_center()
        .gap(theme.px(6.))
        .h(theme.px(26.))
        .pl(theme.px(5.))
        .pr(theme.px(9.))
        .rounded(theme.px(5.))
        .text_color(white(label_alpha))
        .when(enabled, |row| {
            row.cursor_pointer()
                .hover(|style| {
                    style
                        .bg(crate::theme::color(FOCUS))
                        .text_color(white(TEXT_FULL))
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.close_menu(window, cx);
                    on_select(this, window, cx);
                }))
        })
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(theme.px(16.))
                .children(check),
        )
        .children(item.logo.map(|logo| {
            div()
                .flex_none()
                .size(theme.px(16.))
                .when(!enabled, |slot| slot.opacity(0.25))
                .children(logo.map(|path| img(path).size_full()))
        }))
        .child(match detail {
            // Avec un texte secondaire, le libellé garde sa largeur (coupé seulement s'il déborde seul) et le texte
            // secondaire prend le reste.
            Some(_) => div().min_w_0().truncate().child(item.label.clone()),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(item.label.clone()),
        })
        .children(detail)
        .children(shortcut)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlaps(a: &ThumbCell, b: &ThumbCell) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    #[test]
    fn every_preset_thumbnail_draws_its_panes_inside_the_grid_without_overlap() {
        for preset in Preset::ALL {
            let cells = thumbnail_cells(preset);
            assert_eq!(cells.len(), preset.panes(), "{}", preset.id());
            for cell in &cells {
                assert!(cell.width > 0 && cell.height > 0, "{}", preset.id());
                assert!(cell.x + cell.width <= THUMB_COLUMNS, "{}", preset.id());
                assert!(cell.y + cell.height <= THUMB_LINES, "{}", preset.id());
            }
            for (index, a) in cells.iter().enumerate() {
                for b in &cells[index + 1..] {
                    assert!(!overlaps(a, b), "{}: {a:?} overlaps {b:?}", preset.id());
                }
            }
        }
    }

    #[test]
    fn every_preset_thumbnail_fills_the_whole_grid() {
        // Les panes touchent les quatre bords de la grille : la vignette est centrée, sans marge d'un côté.
        for preset in Preset::ALL {
            let cells = thumbnail_cells(preset);
            let left = cells.iter().map(|cell| cell.x).min();
            let top = cells.iter().map(|cell| cell.y).min();
            let right = cells.iter().map(|cell| cell.x + cell.width).max();
            let bottom = cells.iter().map(|cell| cell.y + cell.height).max();
            assert_eq!(
                (left, top, right, bottom),
                (Some(0), Some(0), Some(THUMB_COLUMNS), Some(THUMB_LINES)),
                "{}",
                preset.id()
            );
        }
    }

    #[test]
    fn thumbnails_follow_the_shape_of_their_preset() {
        let columns3 = thumbnail_cells(Preset::Columns3);
        assert!(
            columns3
                .iter()
                .all(|cell| cell.y == 0 && cell.height == THUMB_LINES)
        );
        assert!(columns3.windows(2).all(|pair| pair[0].x < pair[1].x));

        let rows2 = thumbnail_cells(Preset::Rows2);
        assert!(
            rows2
                .iter()
                .all(|cell| cell.x == 0 && cell.width == THUMB_COLUMNS)
        );

        // « Grand pane à gauche » : le premier pane prend toute la hauteur, les deux autres sont à sa droite.
        let main_left = thumbnail_cells(Preset::MainLeft);
        assert_eq!(main_left[0].height, THUMB_LINES);
        assert!(
            main_left[1..]
                .iter()
                .all(|cell| cell.x >= main_left[0].x + main_left[0].width)
        );
    }

    #[test]
    fn the_hidden_panes_label_agrees_with_the_count() {
        assert_eq!(hidden_panes_label(1), "1 pane masqué");
        assert_eq!(hidden_panes_label(4), "4 panes masqués");
    }

    #[test]
    fn only_a_waiting_or_failed_hidden_pane_shows_a_dot() {
        assert_eq!(hidden_panes_dot(ActivityState::Idle), None);
        assert_eq!(hidden_panes_dot(ActivityState::Running), None);
        assert_eq!(
            hidden_panes_dot(ActivityState::Waiting),
            Some(ActivityState::Waiting)
        );
        assert_eq!(
            hidden_panes_dot(ActivityState::Error),
            Some(ActivityState::Error)
        );
    }

    fn row_at(left: f32, width: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(left), px(100.)),
            size: gpui::size(px(width), px(26.)),
        }
    }

    #[test]
    fn a_submenu_opens_right_of_its_row_when_it_fits() {
        let (corner, position) = submenu_anchor(row_at(100., 200.), px(1000.), px(220.), px(5.));
        assert!(matches!(corner, Anchor::TopLeft));
        assert_eq!(position, point(px(303.), px(95.)));
    }

    #[test]
    fn a_submenu_opens_left_of_its_row_against_the_window_edge() {
        let (corner, position) = submenu_anchor(row_at(770., 220.), px(1000.), px(220.), px(5.));
        assert!(matches!(corner, Anchor::TopRight));
        assert_eq!(position, point(px(767.), px(95.)));
    }

    fn item(enabled: bool) -> MenuItemSpec {
        MenuItemSpec::new("x", |_, _, _| {}).enabled(enabled)
    }

    #[test]
    fn an_openable_submenu_opens_with_only_disabled_items() {
        assert!(submenu_opens(&[item(false), item(false)], true));
        assert!(submenu_opens(&[], true));
    }

    #[test]
    fn a_launcher_submenu_without_an_enabled_item_stays_shut() {
        assert!(!submenu_opens(&[item(false), item(false)], false));
        assert!(!submenu_opens(&[], false));
        assert!(submenu_opens(&[item(false), item(true)], false));
    }

    #[test]
    fn the_panel_never_outgrows_the_window() {
        // À la souris : la fenêtre moins deux marges. Sous un bouton : la place restante sous le panneau.
        assert_eq!(panel_max_height(800., 0.), 800. - 2. * WINDOW_MARGIN);
        assert_eq!(panel_max_height(800., 60.), 800. - 60. - WINDOW_MARGIN);
        assert_eq!(panel_max_height(40., 60.), 0.);
    }
}
