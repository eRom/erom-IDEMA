//! L'en-tête de la galerie : le titre, la recherche, le filtre et la disposition. Sous-module de `view`, dont il lit l'état.

use gpui::{
    Context, Focusable as _, FontWeight, MouseButton, MouseDownEvent, canvas, div, prelude::*, svg,
};

use super::{
    ArtifactsLayout, ArtifactsView, COMPACT_SIDE_PADDING, SEARCH_KEY_CONTEXT, SIDE_PADDING,
    SearchField,
};
use crate::assets::Icon;
use crate::files::panel::{FIELD_HEIGHT, FIELD_PADDING, FIELD_RADIUS, field_input};
use crate::help_bubble::HelpBubble;
use crate::theme::{
    BUTTON_HOVER, FIELD_BACKGROUND, FIELD_RING, FOCUS_TEXT, HOVER_STRONG, TEXT_HOVER, TEXT_MIN,
    TEXT_PRIMARY, Theme, UI_FONT, black, color, white,
};

/// Les cotes de l'en-tête, en unités de la maquette : hauteur de l'en-tête de la colonne principale, boutons de 28, titre
/// en 22/700 (« Title » du handoff : le design system n'a pas de serif).
const HEADER_HEIGHT: f32 = 48.;
const BUTTON_SIZE: f32 = 28.;
const BUTTON_ICON: f32 = 18.;
const BUTTON_GAP: f32 = 2.;
const TITLE_SIZE: f32 = 22.;
/// La boîte de recherche : largeur de 240 ; elle prend la hauteur et le rayon des champs de Fichiers.
const SEARCH_WIDTH: f32 = 240.;
/// En dessous de cette largeur de colonne (unités de la maquette), l'en-tête passe en version serrée : à côté du volet
/// Browser la colonne tient ~170 unités, où le titre, le champ de recherche et trois boutons de 28 ne tiennent pas.
pub(super) const COMPACT_BELOW: f32 = 280.;
/// Les cotes de la version serrée : écart, titre, boutons (icône à 15). Le retrait est `COMPACT_SIDE_PADDING`.
const COMPACT_GAP: f32 = 4.;
const COMPACT_TITLE_SIZE: f32 = 15.;
const COMPACT_BUTTON_SIZE: f32 = 22.;
const COMPACT_BUTTON_ICON: f32 = 15.;

/// La version serrée de l'en-tête pour une colonne de `width` unités ; une largeur pas encore mesurée (zéro) n'en est pas.
pub(super) fn is_compact_width(width: f32) -> bool {
    width > 0. && width < COMPACT_BELOW
}

impl ArtifactsView {
    /// Un bouton de l'en-tête : 28 × 28, rayon 6, icône à 55 %, survol fond à 7 % et icône à 92 %. `lit` : l'icône est
    /// allumée (un filtre actif) ; `pressed` : le fond reste (la recherche ouverte).
    fn header_button(
        id: &'static str,
        icon: Icon,
        help: &'static str,
        lit: bool,
        pressed: bool,
        compact: bool,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        let (size, glyph) = if compact {
            (COMPACT_BUTTON_SIZE, COMPACT_BUTTON_ICON)
        } else {
            (BUTTON_SIZE, BUTTON_ICON)
        };
        let tone = if lit {
            color(FOCUS_TEXT)
        } else {
            white(TEXT_MIN)
        };
        div()
            .id(id)
            .group(id)
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(theme.px(size))
            .rounded(theme.px(6.))
            .cursor_pointer()
            .when(pressed, |button| button.bg(white(HOVER_STRONG)))
            .hover(|style| style.bg(white(HOVER_STRONG)))
            .tooltip(move |_, cx| cx.new(|_| HelpBubble::new(help)).into())
            .child(
                svg()
                    .path(icon.path())
                    .size(theme.px(glyph))
                    .text_color(tone)
                    .group_hover(id, move |style| {
                        style.text_color(if lit {
                            color(FOCUS_TEXT)
                        } else {
                            white(TEXT_HOVER)
                        })
                    }),
            )
    }

    /// La boîte de recherche : le creux des champs du volet Fichiers, la loupe, le champ du kit et la croix qui ferme.
    fn search_box(
        &self,
        field: &SearchField,
        compact: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let field_focus = field.input.focus_handle(cx);
        div()
            .id("artifacts-search-box")
            .key_context(SEARCH_KEY_CONTEXT)
            .flex()
            .min_w_0()
            .max_w(theme.px(SEARCH_WIDTH))
            // Serrée, la boîte prend la place du titre : tout ce qui reste, jusqu'à 240. Sinon le ressort de l'en-tête
            // la laisse à sa largeur (240, moins si la colonne manque) contre les boutons.
            .when(compact, |boxed| boxed.flex_1())
            .when(!compact, |boxed| boxed.w(theme.px(SEARCH_WIDTH)))
            .items_center()
            .gap(theme.px(6.))
            .h(theme.px(FIELD_HEIGHT))
            .px(theme.px(FIELD_PADDING))
            .rounded(theme.px(FIELD_RADIUS))
            .border_1()
            .border_color(white(FIELD_RING))
            .bg(black(FIELD_BACKGROUND))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _: &MouseDownEvent, window, cx| {
                    // Un appui dans la boîte ne déplace pas la fenêtre (l'en-tête le ferait).
                    cx.stop_propagation();
                    window.focus(&field_focus, cx);
                    window.prevent_default();
                }),
            )
            // Serrée, la loupe cède sa place au texte : le bouton de recherche de l'en-tête la remplace.
            .when(!compact, |boxed| {
                boxed.child(
                    svg()
                        .path(Icon::Search.path())
                        .flex_none()
                        .size(theme.px(15.))
                        .text_color(white(TEXT_MIN)),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(field_input(&field.input, theme)),
            )
            .child(
                div()
                    .id("artifacts-search-close")
                    .group("artifacts-search-close")
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(theme.px(18.))
                    .rounded(theme.px(4.))
                    .cursor_pointer()
                    .hover(|style| style.bg(white(BUTTON_HOVER)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.close_search(window, cx);
                        }),
                    )
                    .child(
                        svg()
                            .path(Icon::Close.path())
                            .size(theme.px(14.))
                            .text_color(white(TEXT_MIN))
                            .group_hover("artifacts-search-close", |style| {
                                style.text_color(white(TEXT_HOVER))
                            }),
                    ),
            )
    }

    /// L'en-tête serré ? Oui quand la colonne, mesurée au dernier dessin, est plus étroite que `COMPACT_BELOW`.
    pub(super) fn header_is_compact(&self) -> bool {
        is_compact_width(self.header_width.get())
    }

    /// L'en-tête, dans la bande de 48 de la fenêtre : le titre (remplacé par la boîte quand la recherche est ouverte),
    /// puis à droite la recherche, le filtre et la disposition. Un appui sur une zone vide déplace la fenêtre.
    pub(super) fn render_header(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let search_open = self.search.is_some();
        let compact = self.header_is_compact();
        let measured = self.filter_bounds.clone();
        let header_width = self.header_width.clone();
        // L'icône de la bascule montre la disposition où elle mène.
        let (layout_icon, layout_help) = match self.layout {
            ArtifactsLayout::Grid => (Icon::ViewList, "Afficher en liste"),
            ArtifactsLayout::List => (Icon::GridView, "Afficher en grille"),
        };
        let target_layout = self.layout.other();
        let search_button = Self::header_button(
            "artifacts-search",
            Icon::Search,
            "Rechercher",
            false,
            search_open,
            compact,
            theme,
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.toggle_search(window, cx);
            }),
        );
        let filter_button = Self::header_button(
            "artifacts-filter",
            Icon::FilterList,
            "Filtrer par type",
            !self.filter_is_default(),
            false,
            compact,
            theme,
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.open_filter_menu(cx);
            }),
        )
        // La mesure du bouton, derrière son icône : le menu s'y ancre.
        .child(
            canvas(move |bounds, _, _| measured.set(bounds), |_, (), _, _| {})
                .absolute()
                .size_full(),
        );
        let layout_button = Self::header_button(
            "artifacts-layout",
            layout_icon,
            layout_help,
            false,
            false,
            compact,
            theme,
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.set_layout(target_layout, cx);
            }),
        );
        let (padding, gap, title_size) = if compact {
            (COMPACT_SIDE_PADDING, COMPACT_GAP, COMPACT_TITLE_SIZE)
        } else {
            (SIDE_PADDING, 8., TITLE_SIZE)
        };
        // Sidebar repliée, la bande laisse la place aux feux de la fenêtre.
        let left = self.header_inset.unwrap_or_else(|| theme.px(padding));
        div()
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .gap(theme.px(gap))
            .h(theme.px(HEADER_HEIGHT))
            .pl(left)
            .pr(theme.px(padding))
            .on_mouse_down(MouseButton::Left, crate::workspace::drag_window)
            // La mesure de la colonne, derrière le contenu : elle décide de la version serrée au dessin suivant.
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let scale = cx.global::<Theme>().scale;
                        let width = f32::from(bounds.size.width) / scale;
                        if (header_width.get() - width).abs() > 0.5 {
                            header_width.set(width);
                            window.request_animation_frame();
                        }
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .size_full(),
            )
            // Le titre ne se coupe jamais : la boîte de recherche ouverte prend sa place.
            .when(!search_open, |header| {
                header.child(
                    div()
                        .flex_none()
                        .font_family(UI_FONT)
                        .font_weight(FontWeight::BOLD)
                        .text_size(theme.text(title_size))
                        .text_color(white(TEXT_PRIMARY))
                        .child("Artifacts"),
                )
            })
            // Le reste de la bande pousse les boutons à droite. Serrée avec la recherche ouverte, la boîte prend
            // toute la place du titre et du ressort ; en pleine largeur, le ressort reste avant la boîte.
            .when(!(compact && search_open), |header| {
                header.child(div().flex_1().min_w_0().h_full())
            })
            .children(
                self.search
                    .as_ref()
                    .map(|field| self.search_box(field, compact, theme, cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme.px(if compact { 1. } else { BUTTON_GAP }))
                    // Serrée, la croix de la boîte ferme la recherche : le bouton de recherche n'est plus utile.
                    .when(!(compact && search_open), |buttons| {
                        buttons.child(search_button)
                    })
                    .child(filter_button)
                    .child(layout_button),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_goes_compact_only_in_a_measured_narrow_column() {
        // Colonne à côté du volet Browser (~170 unités) : serré. Colonne pleine largeur : normal.
        assert!(is_compact_width(168.));
        assert!(is_compact_width(COMPACT_BELOW - 1.));
        assert!(!is_compact_width(COMPACT_BELOW));
        assert!(!is_compact_width(900.));
        // Pas encore mesurée : la version normale.
        assert!(!is_compact_width(0.));
    }

    #[test]
    fn the_compact_header_fits_a_column_of_170_units() {
        // Retraits, titre estimé (9 lettres en 15 gras, ~0,55 em), puis les trois boutons et leurs écarts.
        let title = 9. * 0.55 * COMPACT_TITLE_SIZE;
        let buttons = 3. * COMPACT_BUTTON_SIZE + 2.;
        let needed = 2. * COMPACT_SIDE_PADDING + title + COMPACT_GAP + buttons;
        assert!(needed <= 170., "{needed} unités pour 170");
    }
}
