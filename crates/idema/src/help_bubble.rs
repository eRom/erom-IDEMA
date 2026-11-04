//! La bulle d'aide d'un élément : une ligne de texte sur le fond d'un popover, suivie de son raccourci s'il en a un.
//! Partagée par le pied de Changes (boutons) et les onglets du navigateur (titre complet).

use gpui::{Context, IntoElement, Render, SharedString, Window, div, prelude::*};

use crate::theme::{CONTROL, POPOVER, TEXT_PRIMARY, TEXT_SECONDARY, Theme, color_alpha, white};

/// Plafond de largeur du texte : au-delà il est coupé par une ellipse, la bulle reste sur une ligne (plus haute, elle
/// descendrait jusque sur la page native, que GPUI ne recouvre pas).
const MAX_TEXT_WIDTH: f32 = 520.;

pub struct HelpBubble {
    text: SharedString,
    keys: Option<SharedString>,
}

impl HelpBubble {
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self {
            text: text.into(),
            keys: None,
        }
    }

    pub fn with_keys(text: impl Into<SharedString>, keys: impl Into<SharedString>) -> Self {
        Self {
            text: text.into(),
            keys: Some(keys.into()),
        }
    }
}

impl Render for HelpBubble {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        div()
            .flex()
            .items_center()
            .gap(theme.px(8.))
            .px(theme.px(8.))
            .py(theme.px(4.))
            .rounded(theme.px(6.))
            .border_1()
            .border_color(white(CONTROL))
            .bg(color_alpha(POPOVER))
            .text_size(theme.text(11.5))
            .text_color(white(TEXT_PRIMARY))
            .child(
                div()
                    .max_w(theme.px(MAX_TEXT_WIDTH))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(self.text.clone()),
            )
            .children(
                self.keys
                    .clone()
                    .map(|keys| div().text_color(white(TEXT_SECONDARY)).child(keys)),
            )
    }
}
