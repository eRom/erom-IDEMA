//! Les widgets d'une ligne de serveur MCP, partagés entre « Personnaliser > Serveurs MCP » et le volet Projet :
//! l'éditeur JSON, son cadre, le badge de type et l'indicateur d'appel en cours. Rendu identique à la maquette.

use gpui::{Context, Div, Entity, FontWeight, SharedString, Window, div, prelude::*, relative};
use gpui_component::input::{Editor, EditorState};

use crate::theme::{
    AGENT_PURPLE, AGENT_PURPLE_TEXT, BADGE_HTTP, BADGE_STDIO, FIELD_BACKGROUND, FIELD_RING, FOCUS,
    FOCUS_TEXT, TEXT_MIN, Theme, black, color, white,
};

/// La hauteur d'une ligne de liste, et celles de la colonne du badge et du chevron (maquette).
pub const ROW_HEIGHT: f32 = 40.;
pub const BADGE_COLUMN: f32 = 90.;
pub const CHEVRON_COLUMN: f32 = 28.;
/// Les bornes de la hauteur de l'éditeur JSON, en lignes de texte.
const MIN_LINES: usize = 3;
const MAX_LINES: usize = 14;
/// Le texte de l'éditeur JSON : corps 12, interligne 1,65 (maquette `.code`), et la marge verticale de la boîte.
const CODE_SIZE: f32 = 12.;
const CODE_LEADING: f32 = 1.65;
const CODE_MARGIN: f32 = 28.;

/// Un éditeur JSON rempli de `text`.
pub fn json_editor<V: 'static>(
    text: &str,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Entity<EditorState> {
    let text = text.to_string();
    cx.new(|cx| {
        let mut state = EditorState::new(window, cx)
            .language("json")
            .soft_wrap(true)
            .line_number(false)
            .folding(false)
            .context_menu(false);
        state.set_value(text, window, cx);
        state
    })
}

/// Le badge de type d'un serveur (maquette `.badge`) ; `mono` est la police à chasse fixe du thème de composants.
pub fn type_badge(remote: bool, mono: SharedString, theme: &Theme) -> Div {
    let (label, background, text) = if remote {
        (
            "HTTP",
            gpui::Rgba {
                a: BADGE_HTTP,
                ..color(FOCUS)
            },
            color(FOCUS_TEXT),
        )
    } else {
        (
            "STDIO",
            gpui::Rgba {
                a: BADGE_STDIO,
                ..color(AGENT_PURPLE)
            },
            color(AGENT_PURPLE_TEXT),
        )
    };
    div().w(theme.px(BADGE_COLUMN)).flex_none().flex().child(
        div()
            .px(theme.px(6.))
            .py(theme.px(2.))
            .rounded(theme.px(4.))
            .bg(background)
            .text_color(text)
            .font_family(mono)
            .text_size(theme.text(10.))
            .font_weight(FontWeight::BOLD)
            .child(label),
    )
}

/// La boîte de l'éditeur JSON (maquette `.code`) : fond noir à 28 %, contour blanc à 8 %, rayon 7, haute de ses lignes.
pub fn code_box(editor: &Entity<EditorState>, text: &str, disabled: bool, theme: &Theme) -> Div {
    let lines = text.lines().count().clamp(MIN_LINES, MAX_LINES);
    // Le corps et l'interligne de la maquette (`.code` : 12 sur 1,65) ; la marge couvre le remplissage de l'éditeur.
    let font = theme.text(CODE_SIZE);
    let height = f32::from(font) * CODE_LEADING * lines as f32 + CODE_MARGIN;
    div()
        .h(gpui::px(height))
        .px(theme.px(12.))
        .py(theme.px(10.))
        .rounded(theme.px(7.))
        .border_1()
        .border_color(white(FIELD_RING))
        .bg(black(FIELD_BACKGROUND))
        .child(
            Editor::new(editor)
                .appearance(false)
                .bordered(false)
                .disabled(disabled)
                .text_size(font)
                .line_height(relative(CODE_LEADING))
                .size_full(),
        )
}

/// L'indicateur d'un appel en cours (« En cours... », « Ajout... », « Retrait... »), à gauche des boutons de sa ligne.
pub fn busy_text(text: &'static str, theme: &Theme) -> Div {
    div()
        .text_size(theme.text(12.))
        .text_color(white(TEXT_MIN))
        .child(text)
}
