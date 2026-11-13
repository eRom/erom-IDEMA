//! La rangée « Créer » : six tuiles, une par genre qu'on sait créer, au-dessus des cartes (spec §3.4). Sous-module de
//! `view`, dont elle lit l'état (`creation`) ; le geste lui-même est `ArtifactsView::start_create`.

use gpui::{Context, Div, SharedString, Stateful, div, prelude::*, svg};

use super::ArtifactsView;
use crate::artifacts::cards::{CARD_RADIUS, GRID_GAP, breathing, chip_colors, kind_icon};
use crate::artifacts::create::CreateState;
use crate::artifacts::listing::Status;
use crate::artifacts::model::ArtifactKind;
use crate::theme::{
    CONTROL_HOVER, ERROR_TEXT, FIELD_RING, TEXT_MIN, TEXT_SECONDARY, Theme, color, white,
};

/// Le libellé de la rangée (12, `TEXT_MIN`) et l'écart qui le sépare des tuiles.
const LABEL_SIZE: f32 = 12.;
const LABEL_GAP: f32 = 10.;
/// Une tuile : largeur minimale (la tuile s'élargit pour remplir la ligne), bloc de 56 de haut, glyphe de 24, nom dessous
/// (12) à 6 d'écart.
const TILE_MIN_WIDTH: f32 = 88.;
const TILE_BLOCK_HEIGHT: f32 = 56.;
const TILE_GLYPH: f32 = 24.;
const TILE_NAME_SIZE: f32 = 12.;
const TILE_NAME_GAP: f32 = 6.;
/// Les tuiles en attente : la moitié de leur opacité.
const PENDING_OPACITY: f32 = 0.5;
/// La ligne d'échec sous la rangée (12), à cet écart des tuiles.
const FAILURE_SIZE: f32 = 12.;
const FAILURE_GAP: f32 = 10.;
/// L'écart entre la rangée (ou sa ligne d'échec) et ce qui la suit, cartes ou message.
pub(super) const ROW_BOTTOM_GAP: f32 = 20.;

/// Vrai quand la rangée se montre : sur une liste lue (pleine, vide, ou vidée par le filtre), tant que la boîte de
/// recherche est vide. `search_text` est le texte de la boîte, rogné (« » quand elle est fermée). Chargement, non connecté
/// et échec la masquent : il n'y a alors rien à créer qu'on puisse atteindre.
pub(super) fn create_row_visible(status: &Status, search_text: &str) -> bool {
    matches!(status, Status::Loaded) && search_text.trim().is_empty()
}

impl ArtifactsView {
    /// La rangée : le libellé, les six tuiles qui passent à la ligne quand la largeur manque, et sous elles, le cas échéant,
    /// la ligne qui dit pourquoi la dernière création a échoué.
    pub(super) fn render_create_row(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let running = match &self.creation {
            CreateState::Running(kind) => Some(*kind),
            _ => None,
        };
        let tiles = ArtifactKind::CREATABLE
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                let on_press = cx.listener(move |this, _, _, cx| this.start_create(kind, cx));
                let tile = tile(index, kind, running, theme);
                match running {
                    // En attente : plus de main, plus de clic.
                    Some(_) => tile.opacity(PENDING_OPACITY),
                    None => tile.cursor_pointer().on_click(on_press),
                }
            });
        let failure = match &self.creation {
            CreateState::Failed(reason) => Some(reason.clone()),
            _ => None,
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .pb(theme.px(ROW_BOTTOM_GAP))
            .child(
                div()
                    .mb(theme.px(LABEL_GAP))
                    .text_size(theme.text(LABEL_SIZE))
                    .text_color(white(TEXT_MIN))
                    .child("Créer"),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .gap(theme.px(GRID_GAP))
                    .children(tiles),
            )
            .children(failure.map(|reason| {
                div()
                    .mt(theme.px(FAILURE_GAP))
                    .text_size(theme.text(FAILURE_SIZE))
                    .text_color(color(ERROR_TEXT))
                    .child(SharedString::from(reason))
            }))
    }
}

/// Une tuile : le bloc teinté avec son glyphe, le nom du genre dessous. Le survol (contour des cartes) joue sur le bloc
/// quand la tuile est active ; la tuile cliquée fait respirer son bloc comme un squelette.
fn tile(
    index: usize,
    kind: ArtifactKind,
    running: Option<ArtifactKind>,
    theme: &Theme,
) -> Stateful<Div> {
    let (background, glyph) = chip_colors(kind);
    let group = SharedString::from(format!("artifact-create-{index}"));
    let block = div()
        .id(("artifact-create-block", index))
        .flex()
        .items_center()
        .justify_center()
        .w_full()
        .h(theme.px(TILE_BLOCK_HEIGHT))
        .rounded(theme.px(CARD_RADIUS))
        .border_1()
        .border_color(white(FIELD_RING))
        .bg(background)
        .when(running.is_none(), |block| {
            block.group_hover(group.clone(), |style| {
                style.border_color(white(CONTROL_HOVER))
            })
        })
        .child(
            svg()
                .path(kind_icon(kind).path())
                .size(theme.px(TILE_GLYPH))
                .text_color(glyph),
        );
    let block = if running == Some(kind) {
        div().w_full().child(breathing(block, index))
    } else {
        div().w_full().child(block)
    };
    div()
        .id(("artifact-create", index))
        .group(group)
        .flex()
        .flex_col()
        .flex_grow(1.)
        .items_center()
        .gap(theme.px(TILE_NAME_GAP))
        .w(theme.px(TILE_MIN_WIDTH))
        .min_w(theme.px(TILE_MIN_WIDTH))
        .child(block)
        .child(
            div()
                .text_size(theme.text(TILE_NAME_SIZE))
                .text_color(white(TEXT_SECONDARY))
                .child(super::kind_label(kind)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::ApiError;

    #[test]
    fn create_row_shows_only_without_search_and_outside_loading_signed_out_and_failed() {
        // Une liste lue, pleine ou vide (la rangée ne regarde pas les éléments), sans recherche.
        assert!(create_row_visible(&Status::Loaded, ""));
        // Les autres états la masquent.
        assert!(!create_row_visible(&Status::Loading, ""));
        assert!(!create_row_visible(&Status::SignedOut, ""));
        assert!(!create_row_visible(
            &Status::Failed(ApiError::Http(500)),
            ""
        ));
        // Une recherche dans la boîte la masque, même sur une liste lue.
        assert!(!create_row_visible(&Status::Loaded, " a "));
        assert!(!create_row_visible(&Status::Loaded, "a"));
    }
}
