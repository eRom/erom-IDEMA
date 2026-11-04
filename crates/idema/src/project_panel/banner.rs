//! Les bandeaux du volet Projet (maquette `volet-projet-connecteurs.html`, `.drift` et `.notice`) : l'avertissement
//! orange, l'information discrète et l'erreur. Le contenu (texte, puces des harnais, boutons) vient de l'appelant ; ce
//! module ne dessine que la boîte. Les puces sont celles des cinq harnais de `mcp_files` : celles qui portent le serveur
//! en vert, les autres cerclées de blanc.

use gpui::{AnyElement, Div, Rgba, SharedString, Stateful, div, prelude::*, svg};

use super::mcp_files::Harness;
use crate::assets::Icon;
use crate::theme::{
    CHIP_ABSENT_RING, CHIP_PRESENT, CHIP_PRESENT_TEXT, CONTROL, ERROR, HOVER_STRONG, NOTICE,
    RUNNING, STATUS_MODIFIED, TEXT_DISABLED, TEXT_MIN, TEXT_PRIMARY, TREE_TEXT, Theme,
    WARNING_BACKGROUND, WARNING_RING, color, white,
};

/// La police des puces (maquette : `SF Mono`, `Menlo`). `Menlo` est de tout Mac ; le thème de composants n'est pas
/// joignable d'ici (les bandeaux se construisent sans contexte) et `SF Mono` n'est pas installé partout.
const CHIP_FONT: &str = "Menlo";

#[derive(Clone, Copy)]
enum Kind {
    Warning,
    Info,
    Error,
}

/// Le bandeau orange (`.drift`) : l'icône d'avertissement, le texte, les puces des harnais s'il y en a, et les `actions`
/// à droite.
pub fn warning_banner(
    text: SharedString,
    chips: Option<Vec<(Harness, bool)>>,
    actions: Vec<AnyElement>,
    theme: &Theme,
) -> Div {
    banner(Kind::Warning, text, None, chips, actions, theme)
}

/// `warning_banner`, avec une ligne de précision sous le texte : pourquoi le bouton est éteint.
pub fn warning_banner_with_reason(
    text: SharedString,
    reason: Option<SharedString>,
    actions: Vec<AnyElement>,
    theme: &Theme,
) -> Div {
    banner(Kind::Warning, text, reason, None, actions, theme)
}

/// Le bandeau d'information (`.notice`) : blanc à 4 %, texte à 55 %, sans liseré.
pub fn info_banner(
    text: SharedString,
    chips: Option<Vec<(Harness, bool)>>,
    actions: Vec<AnyElement>,
    theme: &Theme,
) -> Div {
    banner(Kind::Info, text, None, chips, actions, theme)
}

/// Le bandeau d'erreur : la forme du bandeau orange sur le rouge d'erreur.
pub fn error_banner(
    text: SharedString,
    chips: Option<Vec<(Harness, bool)>>,
    actions: Vec<AnyElement>,
    theme: &Theme,
) -> Div {
    banner(Kind::Error, text, None, chips, actions, theme)
}

fn banner(
    kind: Kind,
    text: SharedString,
    reason: Option<SharedString>,
    chips: Option<Vec<(Harness, bool)>>,
    actions: Vec<AnyElement>,
    theme: &Theme,
) -> Div {
    let (accent, icon) = match kind {
        Kind::Warning => (STATUS_MODIFIED, Icon::Warning),
        Kind::Error => (ERROR, Icon::Warning),
        Kind::Info => (STATUS_MODIFIED, Icon::Info),
    };
    let veil = |alpha: f32| Rgba {
        a: alpha,
        ..color(accent)
    };
    let (background, ring, text_color) = match kind {
        Kind::Info => (white(NOTICE), None, white(TEXT_MIN)),
        _ => (
            veil(WARNING_BACKGROUND),
            Some(veil(WARNING_RING)),
            white(TREE_TEXT),
        ),
    };
    // Le remplissage de la maquette : `.drift` 7 sur 8 (bandeau orange), `.notice` 8 sur 10 (`settings-window.html`).
    let padding = match kind {
        Kind::Info => (10., 8.),
        _ => (8., 7.),
    };
    let icon_color = match kind {
        Kind::Info => white(TEXT_MIN),
        _ => veil(1.),
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.px(8.))
        .px(theme.px(padding.0))
        .py(theme.px(padding.1))
        .rounded(theme.px(7.))
        .bg(background)
        .when_some(ring, |banner, ring| banner.border_1().border_color(ring))
        .child(
            svg()
                .path(icon.path())
                .flex_none()
                .size(theme.px(15.))
                .text_color(icon_color),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(theme.px(3.))
                .text_size(theme.text(11.5))
                .text_color(text_color)
                .child(text)
                .children(chips.map(|chips| chips_row(&chips, theme)))
                .children(reason.map(|reason| {
                    div()
                        .text_size(theme.text(11.))
                        .text_color(white(TEXT_MIN))
                        .child(reason)
                })),
        )
        .when(!actions.is_empty(), |banner| {
            banner.child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(theme.px(6.))
                    .children(actions),
            )
        })
}

/// La rangée des puces (`.chips`) : une par harnais, verte quand il porte le serveur, cerclée de blanc sinon.
fn chips_row(chips: &[(Harness, bool)], theme: &Theme) -> Div {
    div()
        .flex()
        .flex_wrap()
        .gap(theme.px(3.))
        .children(chips.iter().map(|(harness, present)| {
            let chip = div()
                .px(theme.px(5.))
                .py(theme.px(1.))
                .rounded(theme.px(4.))
                .font_family(CHIP_FONT)
                .text_size(theme.text(9.5))
                .font_weight(gpui::FontWeight::BOLD)
                .child(harness.chip());
            if *present {
                chip.bg(Rgba {
                    a: CHIP_PRESENT,
                    ..color(RUNNING)
                })
                .text_color(color(CHIP_PRESENT_TEXT))
            } else {
                chip.border_1()
                    .border_color(white(CHIP_ABSENT_RING))
                    .text_color(white(TEXT_MIN))
            }
        }))
}

/// Le bouton d'un bandeau (maquette `.btn`) : 26 de haut, rayon 6, texte à 88 %, fond blanc à 7 % puis 10 % au survol.
/// Éteint, son texte passe à 25 % et il ne répond ni au survol ni au pointeur ; l'appelant n'y branche aucun geste. Le
/// bouton rendu est un `Stateful` pour recevoir `on_mouse_down`.
pub fn banner_button(
    id: &'static str,
    label: &'static str,
    enabled: bool,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(theme.px(26.))
        .flex_none()
        .flex()
        .items_center()
        .px(theme.px(9.))
        .rounded(theme.px(6.))
        .text_size(theme.text(12.))
        .whitespace_nowrap()
        .bg(white(HOVER_STRONG))
        .when(enabled, |button| {
            button
                .text_color(white(TEXT_PRIMARY))
                .cursor_pointer()
                .hover(|style| style.bg(white(CONTROL)))
        })
        .when(!enabled, |button| button.text_color(white(TEXT_DISABLED)))
        .child(label)
}
