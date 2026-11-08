//! Le volet Terminal (spec §7.1, AC-23) : un shell local par projet, dans un volet à droite de la colonne principale.
//! Ce module tient l'état du volet, ses PTY et sa peinture ; `Workspace` l'ouvre, le ferme et le rattache au projet de
//! contexte. Les cotes sont celles de la maquette (`IDEWindow.dc.html`, lignes 173 à 196).

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    App, Context, Entity, MouseButton, MouseDownEvent, Pixels, Point, Subscription, Window, canvas,
    div, prelude::*, px, svg,
};

use crate::assets::Icon;
use crate::pane_view::PaneView;
use crate::terminal_element::{TerminalFont, cell_metrics, grid_for};
use crate::theme::{
    HOVER_STRONG, ICON_HEADER, PANE_SEPARATOR, PANEL, TEXT_HOVER, TEXT_LABEL, TEXT_MIN, Theme,
    color, white,
};
use crate::workspace::{RightPanel, Workspace, drag_window};

/// Hauteur de l'en-tête, comme celle de la colonne principale.
pub const HEADER_HEIGHT: f32 = 48.;
/// Marges du corps : en haut et en bas, puis sur les côtés.
pub const BODY_MARGIN_VERTICAL: f32 = 8.;
pub const BODY_MARGIN_SIDES: f32 = 14.;

/// Le volet droit affiché est-il agrandi ? Le drapeau est partagé par les deux volets (Terminal et Fichiers) : lequel
/// est ouvert, c'est `Workspace::right_panel`, et le drapeau ne compte que volet ouvert. Agrandi, le volet prend la
/// place de la colonne principale.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PanelState {
    maximized: bool,
}

impl PanelState {
    /// Agrandi et ouvert : la colonne principale (barre d'outils comprise) disparaît.
    pub fn is_maximized(&self, shown: Option<RightPanel>) -> bool {
        shown.is_some() && self.maximized
    }

    /// Le volet affiché passe de `current` à `next` : fermer ou changer de volet rend la taille normale, la prochaine
    /// ouverture (ou l'autre volet) ne commence pas agrandie.
    pub fn switch(&mut self, current: Option<RightPanel>, next: Option<RightPanel>) {
        if current != next {
            self.maximized = false;
        }
    }

    /// Agrandit ou rend la taille normale. Sans effet volet fermé.
    pub fn toggle_maximized(&mut self, shown: Option<RightPanel>) {
        if shown.is_some() {
            self.maximized = !self.maximized;
        }
    }
}

/// À quoi un shell du volet appartient : un projet de ce Mac, ou une machine distante (un shell ssh par machine, sa
/// connexion pour clé).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TerminalKey {
    Project(PathBuf),
    Machine(String),
}

/// Le shell d'un projet ou d'une machine, avec l'abonnement qui apprend sa fin.
struct LocalTerminal {
    view: Entity<PaneView>,
    _subscription: Subscription,
}

/// Les shells locaux : pour chaque clé de projet, le PID de son shell. Les shells d'une machine n'y sont jamais : leur
/// processus est un client ssh, pas un shell de ce Mac sous lequel les serveurs repérés se reconnaissent.
fn project_shell_pids(shells: impl IntoIterator<Item = (TerminalKey, i32)>) -> Vec<(PathBuf, i32)> {
    shells
        .into_iter()
        .filter_map(|(key, pid)| match key {
            TerminalKey::Project(project) => Some((project, pid)),
            TerminalKey::Machine(_) => None,
        })
        .collect()
}

/// L'état du volet et ses shells : un par projet (local) ou par machine (ssh), créé à la première ouverture, gardé
/// tant qu'Idema vit (ou que son shell ne sort pas).
#[derive(Default)]
pub struct TerminalPanel {
    pub state: PanelState,
    terminals: HashMap<TerminalKey, LocalTerminal>,
}

impl TerminalPanel {
    pub fn view_of(&self, key: &TerminalKey) -> Option<Entity<PaneView>> {
        self.terminals
            .get(key)
            .map(|terminal| terminal.view.clone())
    }

    pub fn insert(&mut self, key: TerminalKey, view: Entity<PaneView>, subscription: Subscription) {
        self.terminals.insert(
            key,
            LocalTerminal {
                view,
                _subscription: subscription,
            },
        );
    }

    /// Retire le shell `view` (il est parti) et rend la clé à laquelle il appartenait. Le PTY est lâché avec la vue.
    pub fn remove(&mut self, view: &Entity<PaneView>) -> Option<TerminalKey> {
        let key = self
            .terminals
            .iter()
            .find(|(_, terminal)| &terminal.view == view)
            .map(|(key, _)| key.clone())?;
        self.terminals.remove(&key);
        Some(key)
    }

    /// Retire le shell de la machine `connection` (elle n'est plus déclarée) : sa vue lâchée ferme le PTY et rend le bail.
    pub fn remove_machine(&mut self, connection: &str) -> Option<Entity<PaneView>> {
        let terminal = self
            .terminals
            .remove(&TerminalKey::Machine(connection.to_owned()))?;
        Some(terminal.view)
    }

    /// Le shell de chaque projet, avec son PID : les racines que les serveurs repérés reconnaissent.
    pub fn shell_pids(&self, cx: &App) -> Vec<(PathBuf, i32)> {
        project_shell_pids(self.terminals.iter().filter_map(|(key, terminal)| {
            let pid = terminal.view.read(cx).local_shell_pid()?;
            Some((key.clone(), pid))
        }))
    }

    pub fn views(&self) -> Vec<Entity<PaneView>> {
        self.terminals
            .values()
            .map(|terminal| terminal.view.clone())
            .collect()
    }
}

/// Un bouton de l'en-tête (celui du volet Terminal et celui du volet Fichiers) : 28 × 28, rayon 6, icône à 55 %, survol fond à 7 % et icône à 92 %.
pub(crate) fn header_button(
    theme: &Theme,
    id: &'static str,
    icon: Icon,
    icon_size: f32,
    on_press: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    header_button_at(
        theme,
        id,
        icon,
        icon_size,
        move |_, this, window, cx| on_press(this, window, cx),
        cx,
    )
}

/// Comme `header_button`, et l'appui dit où il est tombé (en coordonnées de la fenêtre) : de quoi ancrer un menu.
pub(crate) fn header_button_at(
    theme: &Theme,
    id: &'static str,
    icon: Icon,
    icon_size: f32,
    on_press: impl Fn(Point<Pixels>, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .id(id)
        .group(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(theme.px(28.))
        .rounded(theme.px(6.))
        .occlude()
        .cursor_pointer()
        .hover(|style| style.bg(white(HOVER_STRONG)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                on_press(event.position, this, window, cx);
            }),
        )
        .child(
            svg()
                .path(icon.path())
                .size(theme.px(icon_size))
                .text_color(white(TEXT_MIN))
                .group_hover(id, |style| style.text_color(white(TEXT_HOVER))),
        )
}

/// La grille qui tient dans le corps du volet : les bornes mesurées, divisées par la cellule du terminal.
pub(crate) fn body_grid(
    body: gpui::Bounds<Pixels>,
    window: &Window,
    cx: &gpui::App,
) -> idema_term::emulator::GridSize {
    let font = cx.global::<TerminalFont>();
    grid_for(body, cell_metrics(window, font))
}

/// La cellule du terminal en pixels de l'appareil (échelle de l'écran comprise, E36), fraction comprise : l'émulateur
/// l'arrondit sur la taille totale de la zone, pas par cellule.
pub(crate) fn device_cell_pixels(window: &Window, cx: &gpui::App) -> (f32, f32) {
    let font = cx.global::<TerminalFont>();
    let (width, height) = cell_metrics(window, font);
    let scale = window.scale_factor();
    (f32::from(width) * scale, f32::from(height) * scale)
}

/// La grille d'un volet de largeur normale dans une fenêtre de `viewport`, pour créer le PTY à la bonne taille avant
/// la première mesure du corps.
pub fn initial_grid(
    viewport: gpui::Size<Pixels>,
    panel_width: Pixels,
    window: &Window,
    theme: &Theme,
    cx: &gpui::App,
) -> idema_term::emulator::GridSize {
    let body = gpui::Bounds::new(
        gpui::point(px(0.), px(0.)),
        gpui::size(
            (panel_width - theme.px(BODY_MARGIN_SIDES) * 2.).max(px(0.)),
            (viewport.height - theme.px(HEADER_HEIGHT) - theme.px(BODY_MARGIN_VERTICAL) * 2.)
                .max(px(0.)),
        ),
    );
    body_grid(body, window, cx)
}

/// L'en-tête commun aux deux volets droits : pastille (icône, `label`, nom du projet à 55 %), puis agrandir et fermer.
/// `ids` sont ceux des deux boutons (agrandir, fermer). Un appui dans l'en-tête déplace la fenêtre et appelle
/// `on_focus`, qui rend la saisie au volet. `header_inset` est le retrait gauche (les feux, barre repliée).
#[allow(clippy::too_many_arguments)]
pub(crate) fn panel_header(
    theme: &Theme,
    icon: Icon,
    label: &'static str,
    project: String,
    ids: (&'static str, &'static str),
    maximized: bool,
    header_inset: Pixels,
    on_focus: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    on_maximize: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    on_close: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> gpui::Div {
    let (maximize_icon, maximize_size) = if maximized {
        (Icon::CloseFullscreen, 16.)
    } else {
        (Icon::OpenInFull, 16.)
    };
    div()
        .h(theme.px(HEADER_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.px(4.))
        .pl(header_inset)
        .pr(theme.px(8.))
        // L'en-tête déplace la fenêtre, et un appui y rend la saisie au volet.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                on_focus(this, window, cx);
                drag_window(event, window, cx);
            }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(theme.px(7.))
                .min_w_0()
                .h(theme.px(28.))
                .pl(theme.px(9.))
                .pr(theme.px(10.))
                .rounded(theme.px(6.))
                .bg(white(HOVER_STRONG))
                .child(
                    svg()
                        .path(icon.path())
                        .flex_none()
                        .size(theme.px(15.))
                        .text_color(white(ICON_HEADER)),
                )
                .child(div().flex_none().text_color(white(TEXT_LABEL)).child(label))
                // Le nom du projet à 55 % (45 % dans la maquette : plancher de lisibilité, spec §7.2). Un nom vide (le
                // Scratchpad n'a pas de projet) n'ajoute pas d'enfant : un enfant vide garderait son écart de 7 px et
                // décentrerait la pastille.
                .children((!project.is_empty()).then(|| {
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_color(white(TEXT_MIN))
                        .child(project)
                })),
        )
        .child(div().flex_1())
        .child(header_button(
            theme,
            ids.0,
            maximize_icon,
            maximize_size,
            on_maximize,
            cx,
        ))
        .child(header_button(theme, ids.1, Icon::Close, 18., on_close, cx))
}

/// La coque commune aux deux volets droits : colonne sur le fond `PANEL`, séparateur gauche de 1 px à 7 %, et soit la
/// largeur `width`, soit toute la place restante quand le volet est agrandi.
pub(crate) fn panel_shell(
    id: &'static str,
    maximized: bool,
    width: Pixels,
) -> gpui::Stateful<gpui::Div> {
    let panel = div()
        .id(id)
        .flex()
        .flex_col()
        .h_full()
        .min_w_0()
        .overflow_hidden()
        .bg(color(PANEL))
        .border_l_1()
        .border_color(white(PANE_SEPARATOR));
    if maximized {
        panel.flex_1()
    } else {
        panel.flex_none().w(width)
    }
}

/// Le volet : en-tête (pastille « Terminal » et projet, agrandir, fermer) puis le corps où vit le shell. `project` est
/// le nom affiché du projet ; `header_inset` est le retrait gauche de l'en-tête.
pub fn render_panel(
    project: String,
    view: Entity<PaneView>,
    maximized: bool,
    header_inset: Pixels,
    width: Pixels,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let header = panel_header(
        theme,
        Icon::Terminal,
        "Terminal",
        project,
        ("terminal-maximize", "terminal-close"),
        maximized,
        header_inset,
        |this, window, cx| this.focus_terminal(window, cx),
        |this, window, cx| this.toggle_terminal_maximized(window, cx),
        |this, window, cx| this.close_terminal(window, cx),
        cx,
    );

    let fitted = view.clone();
    let body = div()
        .flex_1()
        .min_h_0()
        .px(theme.px(BODY_MARGIN_SIDES))
        .py(theme.px(BODY_MARGIN_VERTICAL))
        // Un appui dans les marges du corps rend la saisie au shell, comme dans l'en-tête : sans cela il
        // remonterait à la racine, qui donnerait le focus à la session.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.focus_terminal(window, cx);
            }),
        )
        .child(
            div()
                .relative()
                .size_full()
                // Mesure les bornes du corps à chaque image : la grille qui y tient devient celle du shell
                // (`fit` : l'émulateur et `TIOCSWINSZ`). Un `fit` à la même taille ne fait rien.
                .child(
                    canvas(
                        move |bounds, window, cx| {
                            let grid = body_grid(bounds, window, cx);
                            let (cell_width, cell_height) = device_cell_pixels(window, cx);
                            fitted.update(cx, |pane, cx| {
                                pane.set_cell_pixels(cell_width, cell_height);
                                pane.fit(grid, cx);
                            });
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .child(view),
        );

    let panel = panel_shell("terminal-panel", maximized, width);
    panel.child(header).child(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(state: &mut PanelState, shown: &mut Option<RightPanel>, next: Option<RightPanel>) {
        state.switch(*shown, next);
        *shown = next;
    }

    #[test]
    fn terminal_keys_keep_projects_and_machines_apart() {
        let project = TerminalKey::Project(PathBuf::from("/p"));
        let machine = TerminalKey::Machine("caserne-claude".into());
        assert_ne!(project, machine);
        // Une machine nommée comme un chemin n'est pas le projet de ce chemin.
        assert_ne!(
            TerminalKey::Machine("/p".into()),
            TerminalKey::Project(PathBuf::from("/p"))
        );
        let mut shells = HashMap::new();
        shells.insert(project.clone(), 101);
        shells.insert(machine.clone(), 202);
        assert_eq!(shells.len(), 2);
        assert_eq!(shells[&project], 101);
        assert_eq!(shells[&machine], 202);
        // Seul le shell du projet est un shell de ce Mac.
        assert_eq!(project_shell_pids(shells), vec![(PathBuf::from("/p"), 101)]);
    }

    #[test]
    fn closing_the_panel_also_drops_the_maximized_size() {
        let mut state = PanelState::default();
        let mut shown = None;
        show(&mut state, &mut shown, Some(RightPanel::Terminal));
        state.toggle_maximized(shown);
        assert!(state.is_maximized(shown));
        show(&mut state, &mut shown, None);
        assert!(shown.is_none() && !state.is_maximized(shown));
        show(&mut state, &mut shown, Some(RightPanel::Terminal));
        assert!(shown.is_some());
        assert!(
            !state.is_maximized(shown),
            "a reopened panel is not maximized"
        );
    }

    #[test]
    fn a_closed_panel_cannot_be_maximized() {
        let mut state = PanelState::default();
        let mut shown = None;
        state.toggle_maximized(shown);
        assert!(!state.is_maximized(shown));
        show(&mut state, &mut shown, Some(RightPanel::Files));
        assert!(!state.is_maximized(shown));
    }

    #[test]
    fn maximizing_toggles_back_to_the_normal_width() {
        let mut state = PanelState::default();
        let mut shown = None;
        show(&mut state, &mut shown, Some(RightPanel::Terminal));
        state.toggle_maximized(shown);
        state.toggle_maximized(shown);
        assert!(shown.is_some() && !state.is_maximized(shown));
    }

    #[test]
    fn switching_panels_drops_the_maximized_size() {
        let mut state = PanelState::default();
        let mut shown = None;
        show(&mut state, &mut shown, Some(RightPanel::Files));
        state.toggle_maximized(shown);
        show(&mut state, &mut shown, Some(RightPanel::Terminal));
        assert!(!state.is_maximized(shown));
    }
}
