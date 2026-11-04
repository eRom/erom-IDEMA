//! Ce que la galerie montre à la place de la liste, ou en attendant elle : les squelettes, la liste elle-même, le vide, le
//! refus de connexion et l'échec. Sous-module de `view`, dont il lit l'état.

use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*};

use super::{ArtifactsEvent, ArtifactsLayout, ArtifactsView, SIDE_PADDING, error_reason};
use crate::artifacts::ApiError;
use crate::artifacts::cards::{self, GRID_GAP, ROW_GAP, skeleton_card, skeleton_row};
use crate::theme::{
    FOCUS, FOCUS_TEXT, HOVER_STRONG, TEXT_FULL, TEXT_MIN, TEXT_PRIMARY, Theme, color, white,
};

/// Le nombre de squelettes pendant un chargement : huit cartes (quatre rangées à deux colonnes, deux à quatre) et huit
/// lignes de liste.
const SKELETON_CARDS: usize = 8;
const SKELETON_ROWS: usize = 8;

impl ArtifactsView {
    /// Un message centré dans la zone des cartes : un titre, une ligne de détail et, au choix, un bouton ou un lien.
    fn message(
        title: &'static str,
        detail: Option<SharedString>,
        action: Option<AnyElement>,
        theme: &Theme,
    ) -> impl IntoElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(theme.px(6.))
            .px(theme.px(SIDE_PADDING))
            .pb(theme.px(60.))
            .child(
                div()
                    .text_size(theme.text(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(white(TEXT_PRIMARY))
                    .child(title),
            )
            .children(detail.map(|detail| {
                div()
                    .text_size(theme.text(12.5))
                    .text_color(white(TEXT_MIN))
                    .child(detail)
            }))
            .children(action.map(|action| div().pt(theme.px(10.)).child(action)))
    }

    /// Un bouton plein (l'accent du design system, comme Commit) ou discret (le fond de Push).
    fn button(
        id: &'static str,
        label: &'static str,
        primary: bool,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        let button = div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .h(theme.px(26.))
            .px(theme.px(12.))
            .rounded(theme.px(6.))
            .text_size(theme.text(12.5))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .child(label);
        if primary {
            button
                .bg(color(FOCUS))
                .text_color(white(TEXT_FULL))
                .hover(|style| style.opacity(0.88))
        } else {
            button
                .bg(white(HOVER_STRONG))
                .text_color(white(TEXT_PRIMARY))
                .hover(|style| style.bg(white(crate::theme::CONTROL)))
        }
    }

    /// Un lien discret : le bleu de texte de l'accent, comme « Ouvrir config.toml » de la page d'accueil du navigateur.
    fn link(id: &'static str, label: &'static str, theme: &Theme) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .text_size(theme.text(12.5))
            .text_color(color(FOCUS_TEXT))
            .cursor_pointer()
            .hover(|style| style.opacity(0.8))
            .child(label)
    }

    /// Les squelettes d'un chargement : des cartes ou des lignes, selon la disposition.
    pub(super) fn render_skeletons(&self, columns: u16, theme: &Theme) -> AnyElement {
        match self.layout {
            ArtifactsLayout::Grid => div()
                .w_full()
                .grid()
                .grid_cols(columns)
                .gap(theme.px(GRID_GAP))
                .children((0..SKELETON_CARDS).map(|index| skeleton_card(index, theme)))
                .into_any_element(),
            ArtifactsLayout::List => div()
                .w_full()
                .flex()
                .flex_col()
                .gap(theme.px(ROW_GAP))
                .children((0..SKELETON_ROWS).map(|index| skeleton_row(index, theme)))
                .into_any_element(),
        }
    }

    /// Le corps de la liste lue : la grille, la liste, ou le message de vide.
    pub(super) fn render_items(
        &mut self,
        columns: u16,
        narrow_cards: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let items = self.item_views(cx);
        if items.is_empty() {
            return self.render_empty(theme, cx);
        }
        match self.layout {
            ArtifactsLayout::Grid => div()
                .w_full()
                .grid()
                .grid_cols(columns)
                .gap(theme.px(GRID_GAP))
                .children(items.into_iter().map(|(url, item)| {
                    cards::card(item, narrow_cards, theme).on_click(cx.listener(
                        move |_, _, _, cx| {
                            cx.emit(ArtifactsEvent::Open(url.clone()));
                        },
                    ))
                }))
                .into_any_element(),
            ArtifactsLayout::List => div()
                .w_full()
                .flex()
                .flex_col()
                .gap(theme.px(ROW_GAP))
                .children(items.into_iter().map(|(url, item)| {
                    cards::row(item, theme).on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(ArtifactsEvent::Open(url.clone()));
                    }))
                }))
                .into_any_element(),
        }
    }

    /// Rien à montrer : « Aucun artifact » quand l'API n'en a rendu aucun, sinon « Aucun résultat » (une recherche ou le
    /// filtre masque ce qui existe) et le lien qui lève ce qui masque.
    fn render_empty(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let searching = !self.applied_query.is_empty();
        if self.listing.items.is_empty() && !searching {
            return Self::message("Aucun artifact", None, None, theme).into_any_element();
        }
        let link = if searching {
            Self::link("artifacts-clear-search", "Effacer la recherche", theme)
                .on_click(cx.listener(|this, _, window, cx| this.close_search(window, cx)))
        } else {
            Self::link("artifacts-reset-filter", "Afficher tous les types", theme)
                .on_click(cx.listener(|this, _, _, cx| this.reset_filter(cx)))
        };
        Self::message("Aucun résultat", None, Some(link.into_any_element()), theme)
            .into_any_element()
    }

    /// Le message de non-connexion : le bouton ouvre la page de connexion dans le volet Browser, « Réessayer » relit
    /// la liste une fois la connexion faite.
    pub(super) fn render_signed_out(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let button = Self::button("artifacts-sign-in", "Se connecter", true, theme)
            .on_click(cx.listener(|_, _, _, cx| cx.emit(ArtifactsEvent::SignIn)));
        // Une fois connecté dans le volet Browser, « Réessayer » relit la liste sur une page cachée neuve.
        let retry = Self::button("artifacts-signed-out-retry", "Réessayer", false, theme)
            .on_click(cx.listener(|this, _, _, cx| this.reload(cx)));
        let actions = div()
            .flex()
            .items_center()
            .gap(theme.px(8.))
            .child(button)
            .child(retry);
        Self::message(
            "Connecte-toi à claude.ai dans le navigateur",
            None,
            Some(actions.into_any_element()),
            theme,
        )
        .into_any_element()
    }

    /// Le message d'échec : ce qui a échoué, et « Réessayer » qui relance l'appel. Quand claude.ai est injoignable ou
    /// reste sur un défi (spec §3.7), « Se connecter » s'y ajoute : le volet Browser sait résoudre le défi à la main.
    pub(super) fn render_failed(
        &self,
        error: &ApiError,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let retry = Self::button("artifacts-retry", "Réessayer", false, theme)
            .on_click(cx.listener(|this, _, _, cx| this.reload(cx)));
        let sign_in = failure_offers_sign_in(error).then(|| {
            Self::button("artifacts-failed-sign-in", "Se connecter", true, theme)
                .on_click(cx.listener(|_, _, _, cx| cx.emit(ArtifactsEvent::SignIn)))
        });
        let actions = div()
            .flex()
            .items_center()
            .gap(theme.px(8.))
            .child(retry)
            .children(sign_in);
        Self::message(
            "Les artifacts n'ont pas pu être chargés",
            Some(error_reason(error).into()),
            Some(actions.into_any_element()),
            theme,
        )
        .into_any_element()
    }
}

/// Un échec qui propose « Se connecter » : claude.ai injoignable, page sans capture ou défi qui ne passe pas (spec §3.7).
/// Une erreur HTTP ou une réponse illisible n'a rien à voir avec la session.
pub(super) fn failure_offers_sign_in(error: &ApiError) -> bool {
    matches!(error, ApiError::Unavailable(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_unreachable_source_offers_sign_in_next_to_retry() {
        assert!(failure_offers_sign_in(&ApiError::Unavailable(
            "capture timeout".to_owned()
        )));
        assert!(!failure_offers_sign_in(&ApiError::Http(404)));
        assert!(!failure_offers_sign_in(&ApiError::Unreadable));
    }
}
