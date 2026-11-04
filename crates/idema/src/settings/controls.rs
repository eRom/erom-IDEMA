//! Les contrôles des lignes de réglage de « Personnaliser » : interrupteur, champ, bouton, bouton d'icône, radio, et la
//! ligne qui les porte (libellé et aide à gauche, contrôle à droite, erreur dessous). Les cotes sont celles de la maquette
//! A (`maquette/settings-window.html`), passées par `Theme::px`.

use gpui::{
    App, Div, ElementId, Entity, Focusable, FontWeight, MouseButton, SharedString, Stateful, div,
    prelude::*, relative, svg,
};
use gpui_component::input::{InputState, Textarea, TextareaState};

use crate::assets::Icon;
use crate::files::panel::{FIELD_HEIGHT, FIELD_PADDING, FIELD_RADIUS, field_input};
use crate::theme::{
    CHECKBOX_OUTLINE, CONTROL, CONTROL_HOVER, ERROR_TEXT, FIELD_BACKGROUND, FIELD_RING, FOCUS,
    HOVER, ICON_MUTED, ICON_SECONDARY, RUNNING, SEPARATOR, TEXT_DISABLED, TEXT_FULL, TEXT_MIN,
    TEXT_PRIMARY, TEXT_SECONDARY, TOGGLE_OFF, Theme, black, color, white,
};

/// Le corps du texte d'un champ de la fenêtre.
const FIELD_TEXT: f32 = 12.5;
/// Hauteur d'un bouton, et de ses jumeaux (maquette `.btn`).
const BUTTON_HEIGHT: f32 = 28.;
/// Côté d'un bouton d'icône d'une ligne de liste.
const ICON_BUTTON: f32 = 24.;

/// L'interrupteur de la maquette (`.tog`) : 30 sur 18, rayon 9, pouce de 14 ; allumé, la piste prend `RUNNING`. Le geste
/// (clic) est celui de l'appelant, qui l'enveloppe.
pub fn toggle(on: bool, theme: &Theme) -> impl IntoElement {
    div()
        .relative()
        .flex_none()
        .w(theme.px(30.))
        .h(theme.px(18.))
        .rounded(theme.px(9.))
        .bg(if on {
            color(RUNNING)
        } else {
            white(TOGGLE_OFF)
        })
        .child(
            div()
                .absolute()
                .top(theme.px(2.))
                .left(theme.px(if on { 14. } else { 2. }))
                .size(theme.px(14.))
                .rounded_full()
                .bg(white(1.)),
        )
}

/// La ligne d'un réglage : libellé (13/500) et aide en tertiaire à gauche, contrôle à droite, l'erreur sous le tout en
/// `ERROR_TEXT`. Un trait la sépare de la suivante.
pub fn setting_row(
    label: &str,
    help: Option<&str>,
    control: impl IntoElement,
    error: Option<String>,
    theme: &Theme,
) -> Div {
    div()
        .flex()
        .flex_col()
        .py(theme.px(10.))
        .border_b_1()
        .border_color(white(SEPARATOR))
        .child(
            div()
                .flex()
                .items_center()
                .gap(theme.px(16.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(theme.px(2.))
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(white(TEXT_PRIMARY))
                                .child(SharedString::from(label.to_string())),
                        )
                        .children(help.map(|text| {
                            div()
                                .text_size(theme.text(12.))
                                .text_color(white(TEXT_MIN))
                                .child(SharedString::from(text.to_string()))
                        })),
                )
                .child(div().flex_none().child(control)),
        )
        .children(error.map(|text| error_line(text, theme)))
}

/// Une erreur sous son contrôle.
pub fn error_line(text: String, theme: &Theme) -> Div {
    div()
        .mt(theme.px(6.))
        .text_size(theme.text(12.))
        .text_color(color(ERROR_TEXT))
        .child(SharedString::from(text))
}

/// La boîte d'un champ : fond noir à 28 %, contour blanc à 8 % (rouge si `invalid`), la saisie du kit dedans. Un appui
/// n'importe où dans la boîte donne le focus au champ. `width` : largeur fixe, ou `None` pour remplir la place.
pub fn field_box(
    id: impl Into<ElementId>,
    state: &Entity<InputState>,
    width: Option<f32>,
    invalid: bool,
    theme: &Theme,
    cx: &App,
) -> Stateful<Div> {
    let focus = state.focus_handle(cx);
    let mut boxed = div()
        .id(id)
        .flex()
        .items_center()
        .h(theme.px(FIELD_HEIGHT))
        .px(theme.px(FIELD_PADDING))
        .rounded(theme.px(FIELD_RADIUS))
        .border_1()
        .border_color(ring(invalid))
        .bg(black(FIELD_BACKGROUND))
        .cursor_text()
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            window.focus(&focus, cx);
            window.prevent_default();
        });
    boxed = match width {
        Some(width) => boxed.w(theme.px(width)).flex_none(),
        None => boxed.flex_1().min_w_0(),
    };
    boxed.child(field_input(state, theme))
}

/// La boîte d'un champ multiligne : même habillage, `rows` lignes de haut.
pub fn area_box(
    id: impl Into<ElementId>,
    state: &Entity<TextareaState>,
    width: f32,
    rows: f32,
    invalid: bool,
    theme: &Theme,
    cx: &App,
) -> Stateful<Div> {
    let focus = state.focus_handle(cx);
    div()
        .id(id)
        .w(theme.px(width))
        .h(theme.px(rows * 20. + 12.))
        .flex_none()
        .px(theme.px(FIELD_PADDING))
        .py(theme.px(5.))
        .rounded(theme.px(FIELD_RADIUS))
        .border_1()
        .border_color(ring(invalid))
        .bg(black(FIELD_BACKGROUND))
        .cursor_text()
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            window.focus(&focus, cx);
            window.prevent_default();
        })
        .child(
            Textarea::new(state)
                .appearance(false)
                .bordered(false)
                .h(relative(1.))
                .px_0()
                .py_0()
                .text_size(theme.text(FIELD_TEXT))
                .text_color(white(TEXT_FULL)),
        )
}

fn ring(invalid: bool) -> gpui::Rgba {
    if invalid {
        color(ERROR_TEXT)
    } else {
        white(FIELD_RING)
    }
}

/// Un bouton de texte (maquette `.btn`) : 28 de haut, rayon 6, icône facultative ; `primary` le peint en `FOCUS`.
pub fn button(
    id: impl Into<ElementId>,
    label: &str,
    icon: Option<Icon>,
    primary: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(theme.px(6.))
        .h(theme.px(BUTTON_HEIGHT))
        .px(theme.px(10.))
        .rounded(theme.px(6.))
        .text_size(theme.text(12.5))
        .cursor_pointer();
    let base = if primary {
        base.bg(color(FOCUS))
            .text_color(white(1.))
            .hover(|style| style.opacity(0.85))
    } else {
        base.bg(white(CONTROL))
            .text_color(white(TEXT_PRIMARY))
            .hover(|style| style.bg(white(CONTROL_HOVER)))
    };
    base.children(icon.map(|icon| {
        svg()
            .path(icon.path())
            .flex_none()
            .size(theme.px(16.))
            .text_color(white(if primary { 1. } else { ICON_SECONDARY }))
    }))
    .child(SharedString::from(label.to_string()))
}

/// Le style d'un bouton d'action de ligne dépliée (maquette `.btn`, `.btn.ghost`, `.btn.ghost.del`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// `FOCUS` plein.
    Primary,
    /// Sans fond, texte secondaire.
    Ghost,
    /// Sans fond, texte rouge, collé à gauche (« Supprimer »).
    Danger,
}

/// Un bouton d'action de ligne dépliée. `enabled` faux : grisé, sans survol ni main ; le geste reste celui de l'appelant,
/// qui ne branche rien dessus.
pub fn action_button(
    id: impl Into<ElementId>,
    label: &str,
    icon: Option<Icon>,
    kind: ButtonKind,
    enabled: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(theme.px(6.))
        .h(theme.px(BUTTON_HEIGHT))
        .rounded(theme.px(6.))
        .text_size(theme.text(12.5));
    let base = match kind {
        ButtonKind::Danger => base.pr(theme.px(10.)),
        _ => base.px(theme.px(10.)),
    };
    let (text, icon_color) = match (kind, enabled) {
        (_, false) => (white(TEXT_DISABLED), white(TEXT_DISABLED)),
        (ButtonKind::Primary, true) => (white(1.), white(1.)),
        (ButtonKind::Ghost, true) => (white(TEXT_SECONDARY), white(ICON_SECONDARY)),
        (ButtonKind::Danger, true) => (color(ERROR_TEXT), color(ERROR_TEXT)),
    };
    let base = match (kind, enabled) {
        (ButtonKind::Primary, true) => base.bg(color(FOCUS)),
        (ButtonKind::Primary, false) => base.bg(white(CONTROL)),
        _ => base,
    };
    let base = match (kind, enabled) {
        (ButtonKind::Primary, true) => base.cursor_pointer().hover(|style| style.opacity(0.85)),
        (_, true) => base.cursor_pointer().hover(|style| style.bg(white(HOVER))),
        (_, false) => base,
    };
    base.text_color(text)
        .children(icon.map(|icon| {
            svg()
                .path(icon.path())
                .flex_none()
                .size(theme.px(16.))
                .text_color(icon_color)
        }))
        .child(SharedString::from(label.to_string()))
}

/// Un bouton d'icône d'une ligne de liste (monter, descendre, supprimer, œil). `enabled` faux : grisé, sans survol ; le
/// geste reste celui de l'appelant, qui ne branche rien dessus.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon: Icon,
    enabled: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(theme.px(ICON_BUTTON))
        .rounded(theme.px(5.));
    let base = if enabled {
        base.cursor_pointer().hover(|style| style.bg(white(HOVER)))
    } else {
        base
    };
    // La couleur va sur le tracé lui-même : un `svg` n'hérite pas du texte de son parent.
    base.child(
        svg()
            .path(icon.path())
            .flex_none()
            .size(theme.px(16.))
            .text_color(white(if enabled { ICON_SECONDARY } else { ICON_MUTED })),
    )
}

/// La pastille radio de la colonne « Par défaut » : un anneau, un point `FOCUS` quand elle est choisie.
pub fn radio(on: bool, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(theme.px(16.))
        .rounded_full()
        .border_1()
        .border_color(if on {
            color(FOCUS)
        } else {
            white(CHECKBOX_OUTLINE)
        })
        .children(on.then(|| div().size(theme.px(8.)).rounded_full().bg(color(FOCUS))))
}

/// Le titre d'une colonne de liste (11/600, tertiaire).
pub fn column_title(label: &str, theme: &Theme) -> Div {
    div()
        .text_size(theme.text(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(white(TEXT_MIN))
        .child(SharedString::from(label.to_string()))
}
