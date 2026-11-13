//! La vue de la galerie Artifacts (spec §3.2, §3.3, §3.6) : l'en-tête (titre, recherche, filtre, disposition), la
//! grille ou la liste, et les états qui les remplacent (chargement, non connecté, erreur, vide). La vue ne sait ni où elle
//! est affichée ni comment ouvrir une adresse : elle dit ce qu'elle veut par `ArtifactsEvent`, son propriétaire
//! (`Workspace`) le fait. Le dessin des cartes et des lignes est dans `cards`.
//!
//! Le propriétaire appelle `reload` à chaque fois que la galerie est montrée, la première incluse : `new` ne lance aucune
//! requête et la vue montre ses squelettes tant que `reload` n'a pas été appelé.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use chrono::Utc;
use futures::channel::oneshot;
use gpui::KeyBinding;
use gpui::{
    Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Point, Subscription,
    Task, Window, actions, canvas, div, point, prelude::*, px,
};
use gpui_component::input::{InputEvent, InputState};
use serde::{Deserialize, Serialize};

use super::cards::{GRID_GAP, ItemView, MIN_CARD_WIDTH, grid_columns};
use super::listing::{Applied, Listing, Status};
use super::model::{Artifact, ArtifactKind, KindFilter, apply, classify, kind_flag, opened_label};
use super::source::ArtifactsSource;
use super::thumbnails::{ThumbState, Thumbnails};
use super::{ApiError, ListQuery};
use crate::hud;
use crate::popover::{MENU_WIDTH, MenuEntry, MenuItemSpec, MenuSpec};
use crate::theme::Theme;

/// Contexte de touches de la boîte de recherche : Échap y ferme et vide la recherche.
pub const SEARCH_KEY_CONTEXT: &str = "ArtifactsSearch";

actions!(artifacts, [CloseSearch]);

/// Les liaisons de la galerie : Échap, depuis la boîte de recherche seulement.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new(
        "escape",
        CloseSearch,
        Some(SEARCH_KEY_CONTEXT),
    )]
}

/// Le délai de frappe : la recherche part quand la frappe s'arrête depuis tant de temps (spec §3.3).
const SEARCH_DELAY: Duration = Duration::from_millis(300);
/// La variable qui fait lire un fichier JSON à la place de l'API (tests dans la VM, qui n'a pas de session claude.ai), et
/// celle, facultative, qui fait répondre ce fichier comme un code HTTP donné (404 : erreur, 401 : non connecté).
pub const FIXTURE_ENV: &str = "IDEMA_ARTIFACTS_FIXTURE";
pub const FIXTURE_STATUS_ENV: &str = "IDEMA_ARTIFACTS_FIXTURE_STATUS";

/// Les variables des tests dans la VM, qui n'a pas de session claude.ai : l'adresse rendue à la place d'une création,
/// le code HTTP forcé (ou `missing-type`), et le délai de la fausse création en millisecondes.
#[allow(dead_code)] // branchées par la vue dans la tâche suivante
pub const CREATE_FIXTURE_ENV: &str = "IDEMA_ARTIFACTS_CREATE_FIXTURE";
#[allow(dead_code)] // branchées par la vue dans la tâche suivante
pub const CREATE_FIXTURE_STATUS_ENV: &str = "IDEMA_ARTIFACTS_CREATE_FIXTURE_STATUS";
#[allow(dead_code)] // branchées par la vue dans la tâche suivante
pub const CREATE_FIXTURE_DELAY_ENV: &str = "IDEMA_ARTIFACTS_CREATE_FIXTURE_DELAY_MS";

/// Le retrait de chaque côté de l'en-tête et des cartes, en unités de la maquette.
const SIDE_PADDING: f32 = 22.;
/// Le retrait de la version serrée (colonne à côté du volet Browser), pour les cartes comme pour l'en-tête.
const COMPACT_SIDE_PADDING: f32 = 8.;
/// Sous la barre : respiration au-dessus des cartes et en bas de la liste.
const CONTENT_TOP: f32 = 16.;
const CONTENT_BOTTOM: f32 = 24.;

mod header;
mod states;

/// La disposition de la galerie. Persistée dans `state.json` (`ui.artifacts_layout`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactsLayout {
    #[default]
    Grid,
    List,
}

impl ArtifactsLayout {
    fn other(self) -> Self {
        match self {
            Self::Grid => Self::List,
            Self::List => Self::Grid,
        }
    }
}

/// Ce que la galerie demande à son propriétaire.
pub enum ArtifactsEvent {
    /// Ouvrir cette adresse (`open_url` de l'artifact) dans le volet Browser.
    Open(String),
    /// Ouvrir `https://claude.ai/login` dans le volet Browser.
    SignIn,
    /// Le choix grille ou liste a changé : `state.json` se réécrit.
    LayoutChanged(ArtifactsLayout),
    /// Une case du filtre a changé : `state.json` se réécrit.
    FilterChanged(KindFilter),
    /// Le menu du filtre à poser à `position` (coordonnées de la fenêtre), comme le menu du volet Browser.
    OpenMenu {
        position: Point<Pixels>,
        spec: MenuSpec,
    },
}

/// La boîte de recherche ouverte dans l'en-tête.
struct SearchField {
    input: Entity<InputState>,
    _subscription: Subscription,
}

pub struct ArtifactsView {
    source: Entity<ArtifactsSource>,
    thumbnails: Entity<Thumbnails>,
    layout: ArtifactsLayout,
    filter: KindFilter,
    /// Les éléments lus, où en est la demande, et la règle qui jette les réponses périmées.
    listing: Listing,
    /// La recherche partie au serveur (rognée) ; vide : pas de `q`.
    applied_query: String,
    search: Option<SearchField>,
    /// La frappe en attente de son délai : la remplacer annule la précédente.
    debounce: Option<Task<()>>,
    /// La largeur de la zone des cartes, en unités de la maquette, mesurée au dessin ; zéro tant qu'elle ne l'est pas.
    content_width: f32,
    /// Les bornes du bouton du filtre, mesurées au dessin : le menu s'y ancre.
    filter_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// La largeur de l'en-tête (de la colonne), en unités de la maquette, mesurée au dessin ; zéro tant qu'elle ne l'est pas.
    header_width: Rc<Cell<f32>>,
    /// Le retrait gauche de l'en-tête quand la barre latérale est repliée (les feux de la fenêtre) ; posé par `Workspace`.
    header_inset: Option<Pixels>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ArtifactsEvent> for ArtifactsView {}

impl Focusable for ArtifactsView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Lit le fichier de test comme le ferait l'API : le corps tel quel, ou l'erreur du code HTTP demandé. Un code illisible est
/// ignoré (le fichier répond 200).
pub(crate) fn fixture_response(path: &str, status: Option<&str>) -> Result<String, ApiError> {
    let code = status.and_then(|status| status.trim().parse::<u16>().ok());
    if let Some(error) = code.and_then(|code| classify(code, false)) {
        return Err(error);
    }
    std::fs::read_to_string(path)
        .map_err(|error| ApiError::Unavailable(format!("fixture unreadable: {error}")))
}

/// Le motif d'un échec, tel que la galerie le dit (spec §3.6).
pub(super) fn error_reason(error: &ApiError) -> String {
    match error {
        ApiError::Http(code) => format!("Erreur HTTP {code}"),
        ApiError::Unreadable => "Réponse illisible".to_owned(),
        ApiError::Unavailable(reason) if is_capture_timeout(reason) => {
            "claude.ai n'a pas répondu à temps".to_owned()
        }
        ApiError::SignedOut | ApiError::Unavailable(_) => "claude.ai injoignable".to_owned(),
    }
}

/// Le motif d'un délai de capture (voir `source::timeout_reason`) : la page a répondu trop lentement, elle n'est pas
/// injoignable.
fn is_capture_timeout(reason: &str) -> bool {
    reason.starts_with("capture timeout")
}

/// Le journal dit, une seule fois, que la galerie lit un fichier de test et non claude.ai. Le code HTTP forcé y figure,
/// jamais le contenu du fichier.
fn log_fixture_in_use(status: Option<&str>) {
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| {
        let code = status
            .and_then(|status| status.trim().parse::<u16>().ok())
            .map_or_else(|| "none".to_owned(), |code| code.to_string());
        hud::log_event(&format!("artifacts fixture in use status={code}"));
    });
}

/// Le titre et la case du filtre pour un genre.
pub(crate) fn kind_label(kind: ArtifactKind) -> &'static str {
    kind.type_title().unwrap_or("Pages")
}

impl ArtifactsView {
    /// La galerie dans la disposition et le filtre que `state.json` a gardés. Aucune requête ne part ici : voir `reload`.
    pub fn new(
        source: Entity<ArtifactsSource>,
        layout: ArtifactsLayout,
        filter: KindFilter,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let thumbnails = cx.new(|_| Thumbnails::default());
        // Une vignette qui finit de charger redessine la galerie.
        let subscriptions = vec![cx.observe(&thumbnails, |_, _, cx| cx.notify())];
        Self {
            source,
            thumbnails,
            layout,
            filter,
            listing: Listing::default(),
            applied_query: String::new(),
            search: None,
            debounce: None,
            content_width: 0.,
            filter_bounds: Rc::new(Cell::new(Bounds::default())),
            header_width: Rc::new(Cell::new(0.)),
            header_inset: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    /// Relit la liste, avec la recherche en cours. À appeler à chaque fois que la galerie est montrée. Les éléments déjà
    /// là restent à l'écran jusqu'à la réponse ; après un échec, les squelettes reviennent.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        // Une frappe qui attend encore son délai n'est pas perdue : sa recherche part dans cette même requête.
        self.debounce = None;
        if let Some(text) = self.typed_text(cx) {
            self.applied_query = text;
        }
        self.request(cx);
    }

    /// Le retrait gauche de l'en-tête : `Some` quand la barre latérale est repliée et que la bande doit laisser la place aux
    /// feux de la fenêtre, `None` sinon (le retrait habituel).
    pub fn set_header_inset(&mut self, inset: Option<Pixels>) {
        self.header_inset = inset;
    }

    /// Le texte de la boîte de recherche, rogné ; `None` quand la boîte est fermée.
    fn typed_text(&self, cx: &Context<Self>) -> Option<String> {
        self.search
            .as_ref()
            .map(|search| search.input.read(cx).value().trim().to_owned())
    }

    /// Lance une requête, la dernière seule aura le droit d'afficher sa réponse.
    fn request(&mut self, cx: &mut Context<Self>) {
        let generation = self.listing.begin();
        let query = ListQuery {
            search: (!self.applied_query.is_empty()).then(|| self.applied_query.clone()),
        };
        hud::log_event(&format!(
            "artifacts list requested generation={generation} search={}",
            query.search.is_some()
        ));
        let (sender, receiver) = oneshot::channel();
        match std::env::var(FIXTURE_ENV) {
            // Les tests de la VM : le fichier tient lieu de réponse, la source n'est pas sollicitée.
            Ok(path) if !path.is_empty() => {
                let status = std::env::var(FIXTURE_STATUS_ENV).ok();
                log_fixture_in_use(status.as_deref());
                let _ = sender.send(fixture_response(&path, status.as_deref()));
            }
            _ => self.source.update(cx, |source, cx| {
                source.fetch(
                    query,
                    move |result| {
                        let _ = sender.send(result);
                    },
                    cx,
                );
            }),
        }
        cx.spawn(async move |this, cx| {
            let result = receiver
                .await
                .unwrap_or_else(|_| Err(ApiError::Unavailable("request dropped".to_owned())));
            let _ = this.update(cx, |view, cx| view.finish(generation, result, cx));
        })
        .detach();
        cx.notify();
    }

    /// La réponse de la demande `generation` : `Listing` décide si elle a encore le droit de s'afficher.
    fn finish(
        &mut self,
        generation: u64,
        result: Result<String, ApiError>,
        cx: &mut Context<Self>,
    ) {
        match self.listing.apply(generation, result) {
            Applied::Ignored => hud::log_event(&format!(
                "artifacts list response ignored generation={generation}"
            )),
            Applied::Loaded(count) => {
                hud::log_event(&format!("artifacts list loaded count={count}"));
            }
            Applied::SignedOut => hud::log_event("artifacts list refused, signed out"),
            Applied::Failed(error) => hud::log_event(&format!("artifacts list failed: {error:?}")),
        }
        cx.notify();
    }

    /// Les artifacts affichés : la dernière liste, filtrée par genre.
    fn visible(&self) -> Vec<Artifact> {
        apply(&self.listing.items, &self.filter)
    }

    fn filter_is_default(&self) -> bool {
        self.filter == KindFilter::default()
    }

    /// Bascule la case d'un genre, puis rouvre le menu à la même place : il se ferme à chaque entrée, mais les trois cases
    /// se cochent à la suite.
    fn toggle_kind(&mut self, kind: ArtifactKind, cx: &mut Context<Self>) {
        let flag = kind_flag(&mut self.filter, kind);
        *flag = !*flag;
        cx.emit(ArtifactsEvent::FilterChanged(self.filter));
        cx.notify();
        self.open_filter_menu(cx);
    }

    /// Coche tous les genres.
    fn reset_filter(&mut self, cx: &mut Context<Self>) {
        self.filter = KindFilter::default();
        cx.emit(ArtifactsEvent::FilterChanged(self.filter));
        cx.notify();
    }

    fn set_layout(&mut self, layout: ArtifactsLayout, cx: &mut Context<Self>) {
        if self.layout != layout {
            self.layout = layout;
            cx.emit(ArtifactsEvent::LayoutChanged(layout));
            cx.notify();
        }
    }

    /// Demande le menu du filtre, sous le bouton et aligné sur son bord droit.
    fn open_filter_menu(&mut self, cx: &mut Context<Self>) {
        let theme = *cx.global::<Theme>();
        let bounds = self.filter_bounds.get();
        let position = point(
            (bounds.right() - theme.px(MENU_WIDTH)).max(px(0.)),
            bounds.bottom() + theme.px(4.),
        );
        let view = cx.entity();
        let entries = ArtifactKind::FILTER_ORDER
            .into_iter()
            .map(|kind| {
                let view = view.clone();
                let mut flags = self.filter;
                let checked = *kind_flag(&mut flags, kind);
                MenuEntry::Item(
                    MenuItemSpec::new(kind_label(kind), move |_, _, cx| {
                        view.update(cx, |view, cx| view.toggle_kind(kind, cx));
                    })
                    .checked(checked),
                )
            })
            .collect();
        cx.emit(ArtifactsEvent::OpenMenu {
            position,
            spec: MenuSpec {
                title: Some("Afficher".into()),
                entries,
            },
        });
    }

    /// Ouvre la boîte de recherche et lui donne le focus (ou, déjà ouverte, la ferme : le bouton est une bascule).
    fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() {
            self.close_search(window, cx);
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Rechercher"));
        let subscription = cx.subscribe_in(&input, window, Self::on_search_event);
        input.update(cx, |input, cx| input.focus(window, cx));
        self.search = Some(SearchField {
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Ferme la boîte et vide la recherche : si une recherche était partie au serveur, la liste complète revient.
    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search = None;
        self.debounce = None;
        if !self.applied_query.is_empty() {
            self.applied_query.clear();
            self.request(cx);
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn on_search_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // Chaque frappe repousse le départ : seule la dernière, après `SEARCH_DELAY` de calme, interroge le serveur.
            InputEvent::Change => {
                self.debounce = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(SEARCH_DELAY).await;
                    let _ = this.update(cx, |view, cx| view.apply_search(cx));
                }));
            }
            // Entrée n'attend pas le délai.
            InputEvent::PressEnter { .. } => {
                self.debounce = None;
                self.apply_search(cx);
            }
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    /// Envoie au serveur le texte de la boîte, s'il diffère de la recherche déjà partie.
    fn apply_search(&mut self, cx: &mut Context<Self>) {
        let text = self.typed_text(cx).unwrap_or_default();
        if text == self.applied_query {
            return;
        }
        self.applied_query = text;
        self.request(cx);
    }

    /// Les éléments à dessiner, avec l'image de leur vignette quand elle est prête. Chaque rendu demande chaque vignette :
    /// la première demande lance le téléchargement, les suivantes lisent le cache. Une page ne porte jamais plus de 48
    /// éléments, bien en dessous de la capacité du cache.
    fn item_views(&mut self, cx: &mut Context<Self>) -> Vec<(String, ItemView)> {
        let now = Utc::now();
        self.visible()
            .into_iter()
            .enumerate()
            .map(|(index, artifact)| {
                let image = artifact.thumbnail.as_deref().and_then(|url| {
                    match self
                        .thumbnails
                        .update(cx, |thumbnails, cx| thumbnails.get(url, cx))
                    {
                        ThumbState::Ready(image) => Some(image),
                        ThumbState::Loading | ThumbState::Failed => None,
                    }
                });
                let opened = artifact
                    .last_viewed_at
                    .as_deref()
                    .map(|viewed| opened_label(now, viewed))
                    .unwrap_or_default();
                (
                    artifact.open_url,
                    ItemView {
                        index,
                        title: artifact.title,
                        opened,
                        kind: artifact.kind,
                        image,
                    },
                )
            })
            .collect()
    }

    /// La mesure de la zone des cartes, derrière elles : elle dit de quelle largeur la grille dispose. Rend vrai si la
    /// largeur a changé.
    fn note_width(&mut self, width: f32) -> bool {
        let changed = (self.content_width - width).abs() > 0.5;
        self.content_width = width;
        changed
    }
}

impl Render for ArtifactsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        let compact = self.header_is_compact();
        let side = if compact {
            COMPACT_SIDE_PADDING
        } else {
            SIDE_PADDING
        };
        let columns = grid_columns(self.content_width, MIN_CARD_WIDTH, GRID_GAP);
        // Une seule colonne plus étroite qu'une carte normale : la carte lâche sa pastille pour garder un titre lisible.
        let narrow_cards = self.content_width > 0. && self.content_width < MIN_CARD_WIDTH;
        let status = self.listing.status.clone();
        let body = match &status {
            Status::Loading => self.render_skeletons(columns, &theme),
            Status::Loaded => self.render_items(columns, narrow_cards, &theme, cx),
            Status::SignedOut => self.render_signed_out(&theme, cx),
            Status::Failed(error) => self.render_failed(error, &theme, cx),
        };
        // Les messages remplissent la zone ; les cartes et les lignes défilent.
        let scrolling = matches!(status, Status::Loading | Status::Loaded)
            && !(status == Status::Loaded && self.visible().is_empty());
        let view = cx.entity();
        let content = if scrolling {
            div()
                .id("artifacts-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(
                    div()
                        .relative()
                        .w_full()
                        .px(theme.px(side))
                        .pt(theme.px(CONTENT_TOP))
                        .pb(theme.px(CONTENT_BOTTOM))
                        // La mesure : la largeur de la zone, retraits ôtés, décide du nombre de colonnes.
                        .child(
                            canvas(
                                move |bounds, window, cx| {
                                    let scale = cx.global::<Theme>().scale;
                                    let inner = f32::from(bounds.size.width) / scale - 2. * side;
                                    let changed = view.update(cx, |this, _| this.note_width(inner));
                                    // Pendant le dessin, `notify` est ignoré : sans cette demande, les colonnes ne
                                    // suivraient qu'à la prochaine image venue d'ailleurs.
                                    if changed {
                                        window.request_animation_frame();
                                    }
                                },
                                |_, (), _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                        .child(body),
                )
                .into_any_element()
        } else {
            body
        };
        div()
            .id("artifacts")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &CloseSearch, window, cx| {
                this.close_search(window, cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_header(&theme, cx))
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/artifacts/fixtures/artifacts-mine.json"
    );

    #[test]
    fn a_capture_timeout_says_claude_ai_was_too_slow() {
        let timeout = ApiError::Unavailable(
            r#"capture timeout path=/artifacts title="Just a moment..." last_error=-"#.to_owned(),
        );
        assert_eq!(error_reason(&timeout), "claude.ai n'a pas répondu à temps");
        assert_eq!(
            error_reason(&ApiError::Unavailable("capture lost".to_owned())),
            "claude.ai injoignable"
        );
        assert_eq!(
            error_reason(&ApiError::Unavailable(
                "web content process terminated".to_owned()
            )),
            "claude.ai injoignable"
        );
    }

    #[test]
    fn the_default_layout_is_the_grid() {
        assert_eq!(ArtifactsLayout::default(), ArtifactsLayout::Grid);
        assert_eq!(ArtifactsLayout::Grid.other(), ArtifactsLayout::List);
        assert_eq!(ArtifactsLayout::List.other(), ArtifactsLayout::Grid);
    }

    #[test]
    fn the_layout_is_written_as_a_plain_word() {
        assert_eq!(
            serde_json::to_string(&ArtifactsLayout::List).unwrap(),
            "\"list\""
        );
        assert_eq!(
            serde_json::from_str::<ArtifactsLayout>("\"grid\"").unwrap(),
            ArtifactsLayout::Grid
        );
    }

    #[test]
    fn every_failure_has_its_reason() {
        assert_eq!(error_reason(&ApiError::Http(404)), "Erreur HTTP 404");
        assert_eq!(error_reason(&ApiError::Http(500)), "Erreur HTTP 500");
        assert_eq!(error_reason(&ApiError::Unreadable), "Réponse illisible");
        assert_eq!(
            error_reason(&ApiError::Unavailable("no capture".to_owned())),
            "claude.ai injoignable"
        );
    }

    #[test]
    fn the_fixture_answers_200_by_default_and_the_status_it_is_told() {
        let body = fixture_response(FIXTURE_PATH, None).unwrap();
        assert!(crate::artifacts::model::parse_page(&body).is_ok());
        assert_eq!(
            fixture_response(FIXTURE_PATH, Some("404")),
            Err(ApiError::Http(404))
        );
        assert_eq!(
            fixture_response(FIXTURE_PATH, Some(" 401 ")),
            Err(ApiError::SignedOut)
        );
        // Un code illisible est ignoré : le fichier répond.
        assert!(fixture_response(FIXTURE_PATH, Some("oups")).is_ok());
        assert!(fixture_response(FIXTURE_PATH, Some("200")).is_ok());
    }

    #[test]
    fn a_missing_fixture_is_an_unavailable_source_not_a_panic() {
        assert!(matches!(
            fixture_response("/nonexistent/artifacts.json", None),
            Err(ApiError::Unavailable(_))
        ));
    }

    #[test]
    fn the_kind_flags_map_to_their_checkbox() {
        let mut filter = KindFilter::default();
        *kind_flag(&mut filter, ArtifactKind::Page) = false;
        assert_eq!(
            filter,
            KindFilter {
                pages: false,
                ..KindFilter::default()
            }
        );
        assert_eq!(kind_label(ArtifactKind::Docs), "Docs");
        assert_eq!(kind_label(ArtifactKind::Slides), "Slides");
        assert_eq!(kind_label(ArtifactKind::Page), "Pages");
    }

    #[test]
    fn filter_menu_lists_seven_kinds_in_spec_order() {
        assert_eq!(
            ArtifactKind::FILTER_ORDER.map(kind_label),
            [
                "Docs",
                "Slides",
                "Sheets",
                "Design",
                "Data",
                "Whiteboard",
                "Pages"
            ]
        );
    }

    #[test]
    fn escape_in_the_search_field_reaches_close_search_and_nowhere_else() {
        use crate::files::panel::tests::kit_input_bindings;
        use crate::workspace::KEY_CONTEXT as WORKSPACE_CONTEXT;
        use gpui::{Action, KeyContext, Keymap, Keystroke};
        // Le kit lie aussi Échap, au contexte `Input` (plus profond que la boîte) : son action passe d'abord, puis sa
        // dernière ligne (`cx.propagate()`, quand il n'a rien à fermer) laisse la touche à la liaison suivante, la nôtre.
        let mut bindings = kit_input_bindings();
        bindings.extend(key_bindings());
        let keymap = Keymap::new(bindings);
        let escape_actions = |within: &[&str]| -> Vec<&'static str> {
            let stack: Vec<KeyContext> = within
                .iter()
                .map(|name| KeyContext::parse(name).unwrap())
                .collect();
            let (found, _) =
                keymap.bindings_for_input(&[Keystroke::parse("escape").unwrap()], &stack);
            found
                .iter()
                .map(|binding| binding.action().name())
                .collect()
        };
        let close = CloseSearch.name();
        let kit = gpui_component::input::Escape.name();
        let in_field = escape_actions(&[WORKSPACE_CONTEXT, SEARCH_KEY_CONTEXT, "Input"]);
        let close_at = in_field.iter().position(|name| *name == close);
        let kit_at = in_field.iter().position(|name| *name == kit);
        assert!(close_at.is_some(), "CloseSearch absente : {in_field:?}");
        assert!(kit_at < close_at, "le kit doit passer avant : {in_field:?}");
        // Hors de la boîte de recherche, Échap n'est pas pris par la galerie.
        assert!(!escape_actions(&[WORKSPACE_CONTEXT]).contains(&close));
        assert!(!escape_actions(&[WORKSPACE_CONTEXT, "Input"]).contains(&close));
    }
}
