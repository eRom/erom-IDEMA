//! La barre de recherche ⌘F (spec jalon 4, §4.9) : un champ « Rechercher », le compteur « 3/17 », précédent, suivant
//! et fermer, dans le style des popovers. Elle ne cherche rien elle-même : elle rend `SearchEvent` au pane, qui
//! interroge son émulateur et lui rend le compteur à afficher. La saisie passe par le champ de renommage
//! (`rename_field`), donc l'IME et les touches mortes fonctionnent ; ce qu'on y tape n'atteint jamais le programme.

use gpui::{
    App, BoxShadow, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    MouseButton, MouseDownEvent, Render, Subscription, Window, actions, black, div, point,
    prelude::*, px, svg,
};
use idema_term::emulator::SearchMatch;

use crate::assets::Icon;
use crate::rename_field::{self, FieldOptions, RenameEvent, RenameField};
use crate::theme::{
    CONTROL, HOVER_STRONG, POPOVER, SEARCH_BAR_SHADOW, TEXT_HOVER, TEXT_MIN, TEXT_SECONDARY, Theme,
    color_alpha, white,
};

actions!(
    search,
    [OpenSearch, SearchNext, SearchPrevious, CloseSearch]
);

/// Contexte de touches du champ de la barre : `enter`, `shift-enter`, `escape` et `cmd-f` s'y accrochent.
pub const KEY_CONTEXT: &str = "SearchField";

/// Hauteur de la barre dans la maquette, et marge qui la sépare du coin du pane.
pub const HEIGHT: f32 = 32.;
pub const MARGIN: f32 = 8.;
const FIELD_WIDTH: f32 = 150.;

/// Ce que la barre demande au pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchEvent {
    /// Le texte du champ a changé (frappe, collage, composition validée).
    Changed(String),
    Next,
    Previous,
    Close,
}

/// Le texte du compteur : « 3/17 » avec un résultat courant, « Aucun résultat » sans.
pub fn counter_label(current: Option<usize>, total: usize) -> String {
    match current {
        Some(index) => format!("{}/{total}", index + 1),
        None => "Aucun résultat".to_owned(),
    }
}

/// Le résultat suivant, en boucle. Sans résultat courant, le premier.
pub fn next_index(current: Option<usize>, total: usize) -> Option<usize> {
    (total > 0).then(|| current.map_or(0, |index| (index + 1) % total))
}

/// Le résultat précédent, en boucle. Sans résultat courant, le dernier.
pub fn previous_index(current: Option<usize>, total: usize) -> Option<usize> {
    (total > 0).then(|| current.map_or(total - 1, |index| (index + total - 1) % total))
}

/// Le résultat que prend une nouvelle requête. `matches` va du plus récent au plus ancien (le numéro 1 est le plus
/// récent) : le plus près du bas de la vue en remontant est donc le premier dont la ligne ne dépasse pas
/// `bottom_line` ; sinon (tous sous la vue) le plus récent.
pub fn closest_above_bottom(matches: &[SearchMatch], bottom_line: usize) -> Option<usize> {
    matches
        .iter()
        .position(|found| found.start.0 <= bottom_line)
        .or((!matches.is_empty()).then_some(0))
}

/// Où retrouver, dans une recherche refaite (du plus récent au plus ancien), l'occurrence qui était courante : la
/// première qui ne commence pas après elle (la même, ou la plus récente des plus anciennes si elle a disparu).
pub fn index_of(matches: &[SearchMatch], previous: &SearchMatch) -> Option<usize> {
    matches
        .iter()
        .position(|found| found.start <= previous.start)
}

/// Le résultat que prend une recherche refaite après un reflow (`matches` du plus récent au plus ancien). Un reflow
/// replie les lignes : toute occurrence sous une ligne repliée change de coordonnées, qui ne désignent plus rien de
/// sûr. L'ordre des occurrences, lui, ne bouge pas : le même rang reste la même occurrence tant que leur nombre n'a
/// pas changé ; sinon (de la sortie est arrivée entre-temps) la plus récente.
pub fn current_after_reflow(
    matches: &[SearchMatch],
    previous_count: usize,
    previous: Option<usize>,
) -> Option<usize> {
    previous
        .filter(|&index| matches.len() == previous_count && index < matches.len())
        .or((!matches.is_empty()).then_some(0))
}

pub struct SearchBar {
    field: Entity<RenameField>,
    counter: String,
    _field_events: Subscription,
}

impl EventEmitter<SearchEvent> for SearchBar {}

impl SearchBar {
    /// Crée la barre et donne le focus à son champ.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = rename_field::open_with(
            "",
            FieldOptions {
                key_context: KEY_CONTEXT,
                // Cliquer dans le terminal ne ferme pas la barre : seul Échap ou le bouton la ferme.
                cancel_on_blur: false,
                // Le champ s'ouvre vide : sélectionné, il n'afficherait pas son curseur.
                select_initial: false,
                emit_changes: true,
                placeholder: Some("Rechercher"),
                text_size: rename_field::DEFAULT_TEXT_SIZE,
            },
            window,
            cx,
        );
        let subscription = cx.subscribe(&field, |_, _, event: &RenameEvent, cx| {
            if let RenameEvent::Changed(text) = event {
                cx.emit(SearchEvent::Changed(text.clone()));
            }
        });
        Self {
            field,
            counter: String::new(),
            _field_events: subscription,
        }
    }

    /// Le compteur que le pane a calculé (vide tant que le champ l'est).
    pub fn set_counter(&mut self, label: String, cx: &mut Context<Self>) {
        if self.counter != label {
            self.counter = label;
            cx.notify();
        }
    }

    /// Rend le focus au champ et sélectionne son texte : la frappe suivante le remplace (second ⌘F).
    pub fn focus_and_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.field.focus_handle(cx), cx);
        self.field
            .update(cx, |field, cx| field.select_everything(cx));
    }

    fn open_again(&mut self, _: &OpenSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_and_select(window, cx);
    }

    fn next(&mut self, _: &SearchNext, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SearchEvent::Next);
    }

    fn previous(&mut self, _: &SearchPrevious, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SearchEvent::Previous);
    }

    fn close(&mut self, _: &CloseSearch, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SearchEvent::Close);
    }

    /// Un bouton de 24 × 24 : l'appui garde le clavier dans le champ et n'arrive ni au pane ni à la fenêtre.
    fn button(
        &self,
        theme: &Theme,
        id: &'static str,
        icon: Icon,
        event: SearchEvent,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let field = self.field.focus_handle(cx);
        div()
            .id(id)
            .group(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(theme.px(24.))
            .rounded(theme.px(6.))
            .cursor_pointer()
            .hover(|style| style.bg(white(HOVER_STRONG)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&field, cx);
                    cx.emit(event.clone());
                }),
            )
            .child(
                svg()
                    .path(icon.path())
                    .size(theme.px(18.))
                    .text_color(white(TEXT_MIN))
                    .group_hover(id, |style| style.text_color(white(TEXT_HOVER))),
            )
    }
}

impl Focusable for SearchBar {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.field.focus_handle(cx)
    }
}

impl Render for SearchBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        // La coque occupe la largeur du pane moins ses marges et pousse la barre à droite ; sans fond ni écoute, elle
        // laisse passer les appuis. Dans un pane étroit, c'est le champ qui rétrécit, jamais le compteur ni les boutons.
        div()
            .absolute()
            .top(theme.px(MARGIN))
            .left(theme.px(MARGIN))
            .right(theme.px(MARGIN))
            .flex()
            .justify_end()
            .child(
                div()
                    .h(theme.px(HEIGHT))
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(theme.px(4.))
                    .pl(theme.px(10.))
                    .pr(theme.px(4.))
                    .rounded(theme.px(9.))
                    .bg(color_alpha(POPOVER))
                    .border_1()
                    .border_color(white(CONTROL))
                    .shadow(vec![BoxShadow {
                        color: black().opacity(SEARCH_BAR_SHADOW),
                        offset: point(px(0.), theme.px(8.)),
                        blur_radius: theme.px(24.),
                        spread_radius: px(0.),
                        inset: false,
                    }])
                    .text_size(theme.text(13.))
                    // La barre arrête les appuis : ni le terminal dessous ni la racine (qui rendrait le focus à la
                    // session).
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                        cx.stop_propagation()
                    })
                    .on_action(cx.listener(Self::open_again))
                    .on_action(cx.listener(Self::next))
                    .on_action(cx.listener(Self::previous))
                    .on_action(cx.listener(Self::close))
                    .child(
                        div()
                            .w(theme.px(FIELD_WIDTH))
                            .min_w_0()
                            .flex()
                            .child(self.field.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .min_w(theme.px(56.))
                            .text_size(theme.text(12.))
                            .text_color(white(TEXT_SECONDARY))
                            .child(self.counter.clone()),
                    )
                    .child(self.button(
                        &theme,
                        "search-previous",
                        Icon::ExpandLess,
                        SearchEvent::Previous,
                        cx,
                    ))
                    .child(self.button(
                        &theme,
                        "search-next",
                        Icon::ExpandMore,
                        SearchEvent::Next,
                        cx,
                    ))
                    .child(self.button(
                        &theme,
                        "search-close",
                        Icon::Close,
                        SearchEvent::Close,
                        cx,
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idema_term::emulator::{GridSize, PaneEmulator, SearchMatch, Source};

    fn found(line: usize) -> SearchMatch {
        SearchMatch {
            start: (line, 0),
            end: (line, 2),
        }
    }

    #[test]
    fn the_counter_shows_the_current_result_over_the_total() {
        assert_eq!(counter_label(Some(0), 3), "1/3");
        assert_eq!(counter_label(Some(16), 17), "17/17");
        assert_eq!(counter_label(None, 0), "Aucun résultat");
    }

    #[test]
    fn next_and_previous_wrap_around() {
        assert_eq!(next_index(Some(2), 3), Some(0));
        assert_eq!(next_index(Some(0), 3), Some(1));
        assert_eq!(previous_index(Some(0), 3), Some(2));
        assert_eq!(previous_index(Some(2), 3), Some(1));
        // Sans résultat courant, Suivant prend le premier et Précédent le dernier.
        assert_eq!(next_index(None, 3), Some(0));
        assert_eq!(previous_index(None, 3), Some(2));
        assert_eq!(next_index(None, 0), None);
        assert_eq!(previous_index(Some(0), 0), None);
    }

    #[test]
    fn a_new_query_picks_the_match_closest_to_the_bottom_of_the_view() {
        // Du plus récent au plus ancien.
        let matches = [found(30), found(10), found(2)];
        // La vue finit à la ligne 20 : la plus proche en remontant est celle de la ligne 10.
        assert_eq!(closest_above_bottom(&matches, 20), Some(1));
        // Une occurrence sur la dernière ligne de la vue compte.
        assert_eq!(closest_above_bottom(&matches, 30), Some(0));
        // Toutes sous la vue : la plus récente.
        assert_eq!(closest_above_bottom(&matches, 1), Some(0));
        assert_eq!(closest_above_bottom(&[], 20), None);
    }

    #[test]
    fn the_first_result_reads_one_and_next_goes_to_an_older_one() {
        let matches = [found(30), found(10), found(2)];
        let current = closest_above_bottom(&matches, 100);
        assert_eq!(counter_label(current, matches.len()), "1/3");
        let current = next_index(current, matches.len());
        assert_eq!(counter_label(current, matches.len()), "2/3");
        assert_eq!(
            matches[current.unwrap()].start.0,
            10,
            "Entrée remonte vers l'ancien"
        );
        // Précédent revient vers le plus récent, puis boucle sur le plus ancien.
        let current = previous_index(current, matches.len());
        assert_eq!(counter_label(current, matches.len()), "1/3");
        let current = previous_index(current, matches.len());
        assert_eq!(counter_label(current, matches.len()), "3/3");
    }

    #[test]
    fn a_step_keeps_the_match_that_was_current_after_a_new_search() {
        let before = [found(10), found(2)];
        // De la sortie est arrivée : une occurrence plus récente est apparue, les anciennes gardent leur ligne.
        let after = [found(30), found(10), found(5), found(2)];
        assert_eq!(index_of(&after, &before[0]), Some(1));
        // L'occurrence a disparu : la plus récente des plus anciennes reprend sa place.
        assert_eq!(
            index_of(&[found(30), found(8), found(2)], &before[0]),
            Some(1)
        );
        assert_eq!(index_of(&[found(30)], &before[1]), None);
    }

    fn pane(columns: usize) -> PaneEmulator {
        PaneEmulator::new(GridSize { columns, lines: 6 }, 1000, Source::TmuxPane)
    }

    #[test]
    fn a_reflow_keeps_the_current_occurrence_even_below_a_folded_line() {
        // Trois occurrences : une courte ligne, une ligne longue qui se replie quand le pane rétrécit, puis une
        // dernière courte ligne que le repli décale vers le bas.
        let mut emulator = pane(40);
        emulator.feed(b"needle here\r\n");
        emulator.feed(b"0123456789 0123456789 0123456789 needle\r\n");
        emulator.feed(b"needle last\r\n");
        emulator.track_scroll(true);
        let mut before = emulator.search("needle");
        before.reverse();
        assert_eq!(before.len(), 3);

        emulator.resize(GridSize {
            columns: 12,
            lines: 6,
        });
        let mut after = emulator.search("needle");
        after.reverse();
        assert_eq!(after.len(), 3, "the text did not change, only its folding");
        assert!(
            !after.contains(&before[0]),
            "the occurrence below the fold moved: {:?} is stale in {after:?}",
            before[0]
        );
        // Chaque occurrence courante reste la même, y compris celles que le repli a déplacées.
        for index in 0..3 {
            assert_eq!(current_after_reflow(&after, 3, Some(index)), Some(index));
        }
        // De la sortie est arrivée entre-temps : la plus récente.
        assert_eq!(current_after_reflow(&after, 2, Some(1)), Some(0));
        assert_eq!(current_after_reflow(&after, 3, None), Some(0));
        assert_eq!(current_after_reflow(&[], 3, Some(1)), None);
    }
}
