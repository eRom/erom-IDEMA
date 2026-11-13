//! Le dessin des éléments de la galerie : la carte de la grille, la ligne de la liste et leurs squelettes. Les cotes sont
//! celles de la maquette `maquette/artifacts-maquette.png` (colonne centrale), à l'échelle de l'interface comme toutes
//! les autres. Ce module ne connaît ni l'état de la vue ni ses gestes : l'appelant pose le clic sur l'élément rendu.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, Div, FontWeight, ObjectFit, RenderImage, Rgba, Stateful,
    StyledImage as _, div, img, prelude::*, pulsating_between, svg,
};

use super::model::ArtifactKind;
use crate::assets::Icon;
use crate::theme::CONTROL_HOVER;
use crate::theme::{
    ACTION_HOVER, ARTIFACT_DATA_BG, ARTIFACT_DATA_GLYPH, ARTIFACT_DESIGN_BG, ARTIFACT_DESIGN_GLYPH,
    ARTIFACT_DOCS_BG, ARTIFACT_DOCS_GLYPH, ARTIFACT_SHEETS_BG, ARTIFACT_SHEETS_GLYPH,
    ARTIFACT_SLIDES_BG, ARTIFACT_SLIDES_GLYPH, ARTIFACT_WHITEBOARD_BG, ARTIFACT_WHITEBOARD_GLYPH,
    FIELD_RING, HOVER, HOVER_STRONG, ICON_MUTED, TEXT_MIN, TEXT_PRIMARY, TEXT_SECONDARY, Theme,
    color, white,
};

/// Largeur minimale d'une carte, en unités de la maquette : à la largeur de la colonne centrale de la maquette, la grille
/// tient deux colonnes (spec §3.2). Abaissée de 188 à 160 pour que deux colonnes tiennent encore à côté du volet Browser
/// quand la fenêtre est large.
pub const MIN_CARD_WIDTH: f32 = 160.;
/// L'écart entre deux cartes, dans les deux sens (grille de 4).
pub const GRID_GAP: f32 = 16.;
/// Le rapport largeur sur hauteur de la vignette : celui de la maquette (327 sur 163) et des aperçus de l'API (1200 sur 630).
const THUMB_ASPECT: f32 = 2.;
/// La carte : rayon des fenêtres du design system.
const CARD_RADIUS: f32 = 12.;
/// La bande sous la vignette : retrait de 12, pastille de 32 (rayon 8) et écart de 12 avec le texte.
const CARD_PADDING: f32 = 12.;
const CHIP_SIZE: f32 = 32.;
const CHIP_RADIUS: f32 = 8.;
const CHIP_ICON: f32 = 18.;
/// Le titre (13/500) et la date (12) de la carte.
const TITLE_SIZE: f32 = 13.;
const DATE_SIZE: f32 = 12.;
/// Le glyphe du repli d'une vignette absente.
const FALLBACK_ICON: f32 = 30.;
/// La ligne de la liste : 44 de haut, rayon 8, pastille de 28 (rayon 7, glyphe de 16), écart de 12.
pub const ROW_HEIGHT: f32 = 44.;
const ROW_RADIUS: f32 = 8.;
const ROW_PADDING: f32 = 12.;
const ROW_CHIP_SIZE: f32 = 28.;
const ROW_CHIP_RADIUS: f32 = 7.;
const ROW_CHIP_ICON: f32 = 16.;
/// L'écart entre deux lignes de la liste.
pub const ROW_GAP: f32 = 2.;
/// Le souffle des squelettes : un aller-retour en 1,6 s entre 45 et 100 % d'opacité.
const SKELETON_PERIOD: Duration = Duration::from_millis(1600);
const SKELETON_MIN_OPACITY: f32 = 0.45;

/// Ce qu'une carte ou une ligne montre d'un artifact.
#[derive(Clone)]
pub struct ItemView {
    pub index: usize,
    pub title: String,
    /// « Ouvert il y a 2 min » ; vide si la date est illisible ou absente.
    pub opened: String,
    pub kind: ArtifactKind,
    /// L'image décodée de la vignette, une fois prête. Sinon (absente, en cours, en échec) le repli.
    pub image: Option<Arc<RenderImage>>,
}

/// L'icône du genre : un pictogramme Lucide par genre nommé, deux feuilles superposées pour Pages.
pub fn kind_icon(kind: ArtifactKind) -> Icon {
    match kind {
        ArtifactKind::Docs => Icon::ArtifactDocs,
        ArtifactKind::Slides => Icon::ArtifactSlides,
        ArtifactKind::Sheets => Icon::ArtifactSheets,
        ArtifactKind::Design => Icon::ArtifactDesign,
        ArtifactKind::Data => Icon::ArtifactData,
        ArtifactKind::Whiteboard => Icon::ArtifactWhiteboard,
        ArtifactKind::Page => Icon::FilterNone,
    }
}

/// Le fond et le glyphe de la pastille d'un genre : une paire sombre et claire de même teinte par genre nommé, le gris
/// neutre pour Pages.
pub(crate) fn chip_colors(kind: ArtifactKind) -> (Rgba, Rgba) {
    let pair = |background, glyph| (color(background), color(glyph));
    match kind {
        ArtifactKind::Docs => pair(ARTIFACT_DOCS_BG, ARTIFACT_DOCS_GLYPH),
        ArtifactKind::Slides => pair(ARTIFACT_SLIDES_BG, ARTIFACT_SLIDES_GLYPH),
        ArtifactKind::Sheets => pair(ARTIFACT_SHEETS_BG, ARTIFACT_SHEETS_GLYPH),
        ArtifactKind::Design => pair(ARTIFACT_DESIGN_BG, ARTIFACT_DESIGN_GLYPH),
        ArtifactKind::Data => pair(ARTIFACT_DATA_BG, ARTIFACT_DATA_GLYPH),
        ArtifactKind::Whiteboard => pair(ARTIFACT_WHITEBOARD_BG, ARTIFACT_WHITEBOARD_GLYPH),
        ArtifactKind::Page => (white(HOVER_STRONG), white(TEXT_SECONDARY)),
    }
}

/// Le nombre de colonnes de la grille pour `width` (unités de la maquette) : autant de cartes de `MIN_CARD_WIDTH` au
/// moins que la largeur en tient, écarts compris, et une colonne au minimum. Une largeur pas encore mesurée (zéro) donne
/// deux colonnes, celles de la maquette.
pub fn grid_columns(width: f32, min_card: f32, gap: f32) -> u16 {
    if width <= 0. {
        return 2;
    }
    (((width + gap) / (min_card + gap)).floor() as u16).max(1)
}

/// Le titre d'une carte ou d'une ligne : une ligne, coupée par une ellipse.
fn title_element(title: String, theme: &Theme) -> Div {
    div()
        .min_w_0()
        .truncate()
        .text_size(theme.text(TITLE_SIZE))
        .font_weight(FontWeight::MEDIUM)
        .text_color(white(TEXT_PRIMARY))
        .child(title)
}

/// La date d'ouverture, en retrait du titre.
fn date_element(opened: String, theme: &Theme) -> Div {
    div()
        .min_w_0()
        .truncate()
        .text_size(theme.text(DATE_SIZE))
        .text_color(white(TEXT_MIN))
        .child(opened)
}

/// La pastille du genre : son glyphe dans un carré arrondi teinté.
fn chip(kind: ArtifactKind, size: f32, radius: f32, glyph: f32, theme: &Theme) -> Div {
    let (background, foreground) = chip_colors(kind);
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(theme.px(size))
        .rounded(theme.px(radius))
        .bg(background)
        .child(
            svg()
                .path(kind_icon(kind).path())
                .size(theme.px(glyph))
                .text_color(foreground),
        )
}

/// La coque d'une carte : fond blanc à 5 %, contour à 8 %, rayon 12, tout ce qui dépasse est rogné (la vignette suit les
/// coins). Au survol : fond à 7 % et contour à 14 %.
fn card_shell(id: impl Into<gpui::ElementId>, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .min_w_0()
        .overflow_hidden()
        .rounded(theme.px(CARD_RADIUS))
        .border_1()
        .border_color(white(FIELD_RING))
        .bg(white(HOVER))
}

/// Le rayon des coins hauts de la vignette : celui de la carte moins son contour de 1. Le rognage de GPUI
/// (`overflow_hidden`) est un rectangle, il ne suit pas les coins arrondis : la vignette et son image portent donc
/// elles-mêmes ce rayon, sans quoi leurs coins carrés mordraient sur ceux de la carte.
fn thumb_radius(theme: &Theme) -> gpui::Pixels {
    theme.px(CARD_RADIUS - 1.)
}

/// La vignette : l'aperçu de l'API rogné pour remplir la zone, sinon un aplat avec l'icône du genre.
fn thumbnail(kind: ArtifactKind, image: Option<Arc<RenderImage>>, theme: &Theme) -> Div {
    let area = div()
        .relative()
        .w_full()
        .flex_none()
        .aspect_ratio(THUMB_ASPECT)
        .rounded_t(thumb_radius(theme))
        .bg(white(HOVER_STRONG));
    match image {
        Some(image) => area.child(
            img(image)
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .rounded_t(thumb_radius(theme))
                .object_fit(ObjectFit::Cover),
        ),
        None => area.flex().items_center().justify_center().child(
            svg()
                .path(kind_icon(kind).path())
                .size(theme.px(FALLBACK_ICON))
                .text_color(white(ICON_MUTED)),
        ),
    }
}

/// Une carte de la grille : la vignette, puis la bande avec la pastille du genre, le titre et la date. `narrow` : la
/// colonne est plus étroite qu'une carte normale ; la pastille (44 de large avec son écart) cède sa place au titre.
pub fn card(item: ItemView, narrow: bool, theme: &Theme) -> Stateful<Div> {
    let footer = div()
        .flex()
        .items_center()
        .gap(theme.px(CARD_PADDING))
        .p(theme.px(CARD_PADDING))
        .when(!narrow, |footer| {
            footer.child(chip(item.kind, CHIP_SIZE, CHIP_RADIUS, CHIP_ICON, theme))
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .justify_center()
                .gap(theme.px(1.))
                .child(title_element(item.title, theme))
                .when(!item.opened.is_empty(), |text| {
                    text.child(date_element(item.opened.clone(), theme))
                }),
        );
    card_shell(("artifact-card", item.index), theme)
        .cursor_pointer()
        .hover(|style| {
            style
                .bg(white(HOVER_STRONG))
                .border_color(white(CONTROL_HOVER))
        })
        .child(thumbnail(item.kind, item.image, theme))
        .child(footer)
}

/// Une ligne de la liste : la pastille du genre, le titre et la date à droite. Au survol, le fond des lignes d'action.
pub fn row(item: ItemView, theme: &Theme) -> Stateful<Div> {
    div()
        .id(("artifact-row", item.index))
        .flex()
        .flex_none()
        .items_center()
        .gap(theme.px(ROW_PADDING))
        .h(theme.px(ROW_HEIGHT))
        .px(theme.px(ROW_PADDING))
        .rounded(theme.px(ROW_RADIUS))
        .cursor_pointer()
        .hover(|style| style.bg(white(ACTION_HOVER)))
        .child(chip(
            item.kind,
            ROW_CHIP_SIZE,
            ROW_CHIP_RADIUS,
            ROW_CHIP_ICON,
            theme,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(title_element(item.title, theme)),
        )
        .when(!item.opened.is_empty(), |row| {
            row.child(div().flex_none().child(date_element(item.opened, theme)))
        })
}

/// Un bloc de squelette : un aplat arrondi à 7 %.
fn bone(width: gpui::Length, height: f32, radius: f32, theme: &Theme) -> Div {
    div()
        .flex_none()
        .w(width)
        .h(theme.px(height))
        .rounded(theme.px(radius))
        .bg(white(HOVER_STRONG))
}

/// Le souffle d'un squelette : l'opacité va et vient. Sans lui, le chargement ressemblerait à une page figée.
fn breathing(skeleton: Stateful<Div>, index: usize) -> impl IntoElement {
    skeleton.with_animation(
        ("artifact-skeleton", index),
        Animation::new(SKELETON_PERIOD)
            .repeat()
            .with_easing(pulsating_between(SKELETON_MIN_OPACITY, 1.)),
        |element, delta| element.opacity(delta),
    )
}

/// Une carte de squelette, de la même taille qu'une carte réelle.
pub fn skeleton_card(index: usize, theme: &Theme) -> impl IntoElement {
    let footer = div()
        .flex()
        .items_center()
        .gap(theme.px(CARD_PADDING))
        .p(theme.px(CARD_PADDING))
        .child(bone(
            theme.px(CHIP_SIZE).into(),
            CHIP_SIZE,
            CHIP_RADIUS,
            theme,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(theme.px(6.))
                .child(bone(gpui::relative(0.7).into(), 11., 4., theme))
                .child(bone(gpui::relative(0.4).into(), 9., 4., theme)),
        );
    let shell = card_shell(("artifact-skeleton-card", index), theme)
        .child(
            div()
                .w_full()
                .flex_none()
                .aspect_ratio(THUMB_ASPECT)
                .rounded_t(thumb_radius(theme))
                .bg(white(HOVER_STRONG)),
        )
        .child(footer);
    breathing(shell, index)
}

/// Une ligne de squelette, de la même hauteur qu'une ligne réelle.
pub fn skeleton_row(index: usize, theme: &Theme) -> impl IntoElement {
    let shell = div()
        .id(("artifact-skeleton-row", index))
        .flex()
        .flex_none()
        .items_center()
        .gap(theme.px(ROW_PADDING))
        .h(theme.px(ROW_HEIGHT))
        .px(theme.px(ROW_PADDING))
        .child(bone(
            theme.px(ROW_CHIP_SIZE).into(),
            ROW_CHIP_SIZE,
            ROW_CHIP_RADIUS,
            theme,
        ))
        .child(bone(gpui::relative(0.4).into(), 11., 4., theme));
    breathing(shell, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_follow_the_width_and_never_drop_below_one() {
        // Largeur de la colonne centrale de la maquette, moins ses retraits : deux colonnes.
        assert_eq!(grid_columns(420., MIN_CARD_WIDTH, GRID_GAP), 2);
        assert_eq!(grid_columns(471., MIN_CARD_WIDTH, GRID_GAP), 2);
        // Trop étroit pour deux cartes : une colonne, même sous la largeur minimale d'une carte.
        assert_eq!(grid_columns(300., MIN_CARD_WIDTH, GRID_GAP), 1);
        assert_eq!(grid_columns(120., MIN_CARD_WIDTH, GRID_GAP), 1);
        // Large : trois, puis quatre colonnes.
        assert_eq!(grid_columns(620., MIN_CARD_WIDTH, GRID_GAP), 3);
        assert_eq!(grid_columns(830., MIN_CARD_WIDTH, GRID_GAP), 4);
    }

    #[test]
    fn a_card_is_never_narrower_than_the_minimum() {
        // Pour toute largeur mesurée, la carte (largeur utile moins les écarts, partagée) garde au moins la largeur minimale,
        // sauf quand une seule colonne tient dans moins que cela.
        let mut width = MIN_CARD_WIDTH;
        while width < 1400. {
            let columns = f32::from(grid_columns(width, MIN_CARD_WIDTH, GRID_GAP));
            let card = (width - GRID_GAP * (columns - 1.)) / columns;
            assert!(
                card >= MIN_CARD_WIDTH - 0.001,
                "{width} -> {columns} colonnes de {card}"
            );
            width += 3.;
        }
    }

    #[test]
    fn an_unmeasured_width_gives_the_mockup_two_columns() {
        assert_eq!(grid_columns(0., MIN_CARD_WIDTH, GRID_GAP), 2);
    }

    #[test]
    fn every_kind_has_its_own_icon() {
        let icons = ArtifactKind::FILTER_ORDER.map(kind_icon);
        for (i, first) in icons.iter().enumerate() {
            for second in &icons[i + 1..] {
                assert_ne!(first, second);
            }
        }
    }

    #[test]
    fn named_kinds_use_the_spec_colors() {
        use crate::theme::*;
        let expected = [
            (ArtifactKind::Docs, ARTIFACT_DOCS_BG, ARTIFACT_DOCS_GLYPH),
            (ArtifactKind::Slides, ARTIFACT_SLIDES_BG, ARTIFACT_SLIDES_GLYPH),
            (ArtifactKind::Sheets, ARTIFACT_SHEETS_BG, ARTIFACT_SHEETS_GLYPH),
            (ArtifactKind::Design, ARTIFACT_DESIGN_BG, ARTIFACT_DESIGN_GLYPH),
            (ArtifactKind::Data, ARTIFACT_DATA_BG, ARTIFACT_DATA_GLYPH),
            (
                ArtifactKind::Whiteboard,
                ARTIFACT_WHITEBOARD_BG,
                ARTIFACT_WHITEBOARD_GLYPH,
            ),
        ];
        assert_eq!(expected.len(), ArtifactKind::CREATABLE.len());
        for (kind, background, glyph) in expected {
            assert!(ArtifactKind::CREATABLE.contains(&kind));
            assert_eq!(chip_colors(kind), (color(background), color(glyph)));
        }
    }

    #[test]
    fn pages_keep_the_neutral_chip() {
        assert_eq!(
            chip_colors(ArtifactKind::Page),
            (white(HOVER_STRONG), white(TEXT_SECONDARY))
        );
    }
}
