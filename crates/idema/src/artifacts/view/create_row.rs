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
/// Le nom de la tuile à 6 sous son bloc, dans les deux gabarits.
const TILE_NAME_GAP: f32 = 6.;
/// Les tuiles d'une ligne normale : largeur minimale 88 (en deçà, gabarit serré) et plafond 160 (celui d'une carte, pour
/// que la rangée ne s'étire pas sur une grande fenêtre), écart 16, bloc de 56, glyphe 24, nom 12.
const TILE_MIN_WIDTH: f32 = 88.;
const TILE_MAX_WIDTH: f32 = 160.;
const TILE_GAP: f32 = GRID_GAP;
const TILE_BLOCK_HEIGHT: f32 = 56.;
const TILE_GLYPH: f32 = 24.;
const TILE_NAME_SIZE: f32 = 12.;
/// Le gabarit serré (colonne à côté du volet Browser) : les six tuiles tiennent encore sur une ligne, bloc de 40, glyphe 20,
/// nom 11, écart 8.
const COMPACT_GAP: f32 = 8.;
const COMPACT_BLOCK_HEIGHT: f32 = 40.;
const COMPACT_GLYPH: f32 = 20.;
const COMPACT_NAME_SIZE: f32 = 11.;
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

/// Les cotes des six tuiles pour une largeur donnée (unités de la maquette).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct TileMetrics {
    pub width: f32,
    pub gap: f32,
    pub block_height: f32,
    pub glyph: f32,
    pub label_size: f32,
}

/// Les cotes des tuiles pour `content_width`, la largeur de la zone des cartes. Les six tuiles tiennent toujours sur une
/// seule ligne : gabarit normal (88 à 160 de large) tant que six tuiles de 88 et leurs écarts tiennent, gabarit serré
/// dessous, où les tuiles se partagent la ligne. Une largeur pas encore mesurée (zéro) donne le gabarit normal à 88.
pub(super) fn create_tile_metrics(content_width: f32) -> TileMetrics {
    let count = ArtifactKind::CREATABLE.len() as f32;
    let fits_normal = count * TILE_MIN_WIDTH + (count - 1.) * TILE_GAP;
    if content_width > 0. && content_width < fits_normal {
        let share = (content_width - (count - 1.) * COMPACT_GAP) / count;
        return TileMetrics {
            width: share.clamp(0., TILE_MIN_WIDTH),
            gap: COMPACT_GAP,
            block_height: COMPACT_BLOCK_HEIGHT,
            glyph: COMPACT_GLYPH,
            label_size: COMPACT_NAME_SIZE,
        };
    }
    let width = if content_width > 0. {
        ((content_width - (count - 1.) * TILE_GAP) / count).clamp(TILE_MIN_WIDTH, TILE_MAX_WIDTH)
    } else {
        TILE_MIN_WIDTH
    };
    TileMetrics {
        width,
        gap: TILE_GAP,
        block_height: TILE_BLOCK_HEIGHT,
        glyph: TILE_GLYPH,
        label_size: TILE_NAME_SIZE,
    }
}

impl ArtifactsView {
    /// La rangée : le libellé, les six tuiles sur une seule ligne, alignées à gauche, et sous elles, le cas échéant,
    /// la ligne qui dit pourquoi la dernière création a échoué.
    pub(super) fn render_create_row(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let running = match &self.creation {
            CreateState::Running(kind) => Some(*kind),
            _ => None,
        };
        let metrics = create_tile_metrics(self.content_width);
        let tiles = ArtifactKind::CREATABLE
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                let on_press = cx.listener(move |this, _, _, cx| this.start_create(kind, cx));
                let tile = tile(index, kind, running, metrics, theme);
                match running {
                    // En attente : plus de main, plus de clic ; la tuile cliquée reste pleine et respire, les autres
                    // s'estompent.
                    Some(active) if active == kind => tile,
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
                    .gap(theme.px(metrics.gap))
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
    metrics: TileMetrics,
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
        .h(theme.px(metrics.block_height))
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
                .size(theme.px(metrics.glyph))
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
        .flex_none()
        .items_center()
        .gap(theme.px(TILE_NAME_GAP))
        .w(theme.px(metrics.width))
        .child(block)
        .child(
            div()
                .max_w_full()
                .truncate()
                .text_size(theme.text(metrics.label_size))
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

    fn line_width(metrics: TileMetrics) -> f32 {
        6. * metrics.width + 5. * metrics.gap
    }

    #[test]
    fn a_narrow_column_gets_compact_tiles_that_still_fit_on_one_line() {
        let metrics = create_tile_metrics(390.);
        assert_eq!(
            (
                metrics.block_height,
                metrics.glyph,
                metrics.label_size,
                metrics.gap
            ),
            (40., 20., 11., 8.)
        );
        assert!(line_width(metrics) <= 390., "{metrics:?}");
    }

    #[test]
    fn a_default_width_gets_normal_tiles() {
        let metrics = create_tile_metrics(990.);
        assert_eq!(
            (
                metrics.block_height,
                metrics.glyph,
                metrics.label_size,
                metrics.gap
            ),
            (56., 24., 12., 16.)
        );
        assert!((88. ..=160.).contains(&metrics.width), "{metrics:?}");
        assert!(line_width(metrics) <= 990., "{metrics:?}");
    }

    #[test]
    fn a_wide_window_caps_the_tile_width_at_a_card_minimum() {
        assert_eq!(create_tile_metrics(2560.).width, 160.);
    }

    #[test]
    fn six_tiles_always_fit_on_one_line_whatever_the_width() {
        // Du plus étroit des volets à une très grande fenêtre, y compris de part et d'autre du seuil des gabarits.
        let mut width = 120.;
        while width <= 3000. {
            let metrics = create_tile_metrics(width);
            assert!(line_width(metrics) <= width + 0.001, "{width}: {metrics:?}");
            width += 7.;
        }
        for width in [607., 608., 609.] {
            let metrics = create_tile_metrics(width);
            assert!(line_width(metrics) <= width + 0.001, "{width}: {metrics:?}");
        }
    }

    #[test]
    fn an_unmeasured_width_gives_normal_tiles_at_their_minimum() {
        let metrics = create_tile_metrics(0.);
        assert_eq!((metrics.width, metrics.block_height), (88., 56.));
    }
}
