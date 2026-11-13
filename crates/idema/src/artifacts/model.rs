//! Le modèle pur de la galerie : lecture de la réponse de l'API, filtre par type, chaîne de requête, verdict d'état HTTP,
//! suivi des réponses périmées et libellé « Ouvert … ». Aucune entrée-sortie ici : tout se teste sans fenêtre.

use chrono::{DateTime, Datelike as _, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::form_urlencoded;

use super::{ApiError, ListQuery};

/// Taille de la page demandée : la même que celle de l'app web. L'API ignore `offset` (sonde du 25/10), la v1 lit donc
/// une seule page.
pub const PAGE_SIZE: u32 = 48;

/// Le type d'un artifact tel que la galerie le range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    Docs,
    Slides,
    Sheets,
    Design,
    Data,
    Whiteboard,
    /// Un artifact sans `type_title` : une page.
    Page,
}

impl ArtifactKind {
    /// Les genres que la galerie sait créer, dans l'ordre des tuiles.
    pub const CREATABLE: [ArtifactKind; 6] = [
        ArtifactKind::Docs,
        ArtifactKind::Slides,
        ArtifactKind::Sheets,
        ArtifactKind::Design,
        ArtifactKind::Data,
        ArtifactKind::Whiteboard,
    ];

    /// Tous les genres, dans l'ordre du menu du filtre (les pages en dernier).
    pub const FILTER_ORDER: [ArtifactKind; 7] = [
        ArtifactKind::Docs,
        ArtifactKind::Slides,
        ArtifactKind::Sheets,
        ArtifactKind::Design,
        ArtifactKind::Data,
        ArtifactKind::Whiteboard,
        ArtifactKind::Page,
    ];

    /// Le titre exact que l'API donne au type. `None` pour une page, qui n'en a pas.
    pub fn type_title(self) -> Option<&'static str> {
        match self {
            ArtifactKind::Docs => Some("Docs"),
            ArtifactKind::Slides => Some("Slides"),
            ArtifactKind::Sheets => Some("Sheets"),
            ArtifactKind::Design => Some("Design"),
            ArtifactKind::Data => Some("Data"),
            ArtifactKind::Whiteboard => Some("Whiteboard"),
            ArtifactKind::Page => None,
        }
    }
}

/// Le genre d'un `type_title`, comparé sans casse sur le titre rogné. `None` : type exclu de la galerie (`Design System`
/// et tout titre inconnu).
pub fn kind_of(type_title: Option<&str>) -> Option<ArtifactKind> {
    match type_title.map(str::trim) {
        None | Some("") => Some(ArtifactKind::Page),
        Some(title) => ArtifactKind::CREATABLE.into_iter().find(|kind| {
            kind.type_title()
                .is_some_and(|known| known.eq_ignore_ascii_case(title))
        }),
    }
}

/// Un artifact tel que la galerie l'affiche.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub id: String,
    pub title: String,
    pub open_url: String,
    pub kind: ArtifactKind,
    /// La vignette sombre si l'API en donne une, sinon la claire.
    pub thumbnail: Option<String>,
    /// RFC 3339, tel que l'API le rend ; `opened_label` le met en forme.
    pub last_viewed_at: Option<String>,
}

/// Un élément de la réponse. Les champs inconnus sont ignorés, ceux qui manquent sont tolérés sauf `id` et `open_url`.
#[derive(Deserialize)]
struct RawArtifact {
    id: String,
    open_url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    type_title: Option<String>,
    #[serde(default)]
    thumbnail_url: Option<String>,
    #[serde(default)]
    thumbnail_dark_url: Option<String>,
    #[serde(default)]
    last_viewed_at: Option<String>,
}

#[derive(Deserialize)]
struct RawPage {
    artifacts: Vec<Value>,
}

/// Lit le corps d'une page de la liste. Un corps sans `artifacts` est illisible ; un élément de type exclu, ou sans `id`
/// ni `open_url`, est sauté sans faire échouer le reste. Si aucun élément d'une liste non vide n'est lisible (champ
/// renommé par l'API), la réponse est illisible : elle ne doit pas passer pour une galerie vide.
pub fn parse_page(body: &str) -> Result<Vec<Artifact>, ApiError> {
    let page: RawPage = serde_json::from_str(body).map_err(|_| ApiError::Unreadable)?;
    let total = page.artifacts.len();
    let readable: Vec<RawArtifact> = page
        .artifacts
        .into_iter()
        .filter_map(|value| serde_json::from_value::<RawArtifact>(value).ok())
        .collect();
    if total > 0 && readable.is_empty() {
        return Err(ApiError::Unreadable);
    }
    Ok(readable
        .into_iter()
        .filter_map(|raw| {
            let kind = kind_of(raw.type_title.as_deref())?;
            Some(Artifact {
                id: raw.id,
                title: raw.title,
                open_url: raw.open_url,
                kind,
                thumbnail: raw.thumbnail_dark_url.or(raw.thumbnail_url),
                last_viewed_at: raw.last_viewed_at,
            })
        })
        .collect())
}

/// Les genres affichés. Par défaut, tous. Les quatre genres ajoutés après la v1 sont affichés quand le champ manque :
/// un `state.json` d'avant ne les masque pas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindFilter {
    pub docs: bool,
    pub slides: bool,
    #[serde(default = "shown")]
    pub sheets: bool,
    #[serde(default = "shown")]
    pub design: bool,
    #[serde(default = "shown")]
    pub data: bool,
    #[serde(default = "shown")]
    pub whiteboard: bool,
    pub pages: bool,
}

fn shown() -> bool {
    true
}

impl Default for KindFilter {
    fn default() -> Self {
        Self {
            docs: true,
            slides: true,
            sheets: true,
            design: true,
            data: true,
            whiteboard: true,
            pages: true,
        }
    }
}

/// La case d'un genre dans le filtre.
pub fn kind_flag(filter: &mut KindFilter, kind: ArtifactKind) -> &mut bool {
    match kind {
        ArtifactKind::Docs => &mut filter.docs,
        ArtifactKind::Slides => &mut filter.slides,
        ArtifactKind::Sheets => &mut filter.sheets,
        ArtifactKind::Design => &mut filter.design,
        ArtifactKind::Data => &mut filter.data,
        ArtifactKind::Whiteboard => &mut filter.whiteboard,
        ArtifactKind::Page => &mut filter.pages,
    }
}

/// Les artifacts dont le genre est affiché, dans l'ordre reçu.
pub fn apply(items: &[Artifact], filter: &KindFilter) -> Vec<Artifact> {
    let mut flags = *filter;
    items
        .iter()
        .filter(|item| *kind_flag(&mut flags, item.kind))
        .cloned()
        .collect()
}

/// La chaîne de requête de la liste, sans `?` : celle de l'app web, plus `q` si la recherche n'est pas vide une fois
/// rognée.
pub fn query_string(query: &ListQuery, org: &str) -> String {
    let mut pairs = form_urlencoded::Serializer::new(String::new());
    pairs
        .append_pair("rel", "mine")
        .append_pair("empty", "exclude")
        .append_pair("limit", &PAGE_SIZE.to_string())
        .append_pair("org", org)
        .append_pair("sort", "last_viewed");
    if let Some(search) = query.search.as_deref().map(str::trim)
        && !search.is_empty()
    {
        pairs.append_pair("q", search);
    }
    pairs.finish()
}

/// Le verdict d'une réponse : `None` pour un 200 resté sur l'API ; `SignedOut` pour 401, 403 ou une redirection vers la
/// connexion ; `Http` pour le reste.
pub fn classify(status: u16, landed_on_login: bool) -> Option<ApiError> {
    match status {
        _ if landed_on_login => Some(ApiError::SignedOut),
        200 => None,
        401 | 403 => Some(ApiError::SignedOut),
        other => Some(ApiError::Http(other)),
    }
}

/// Numérote les demandes : seule la dernière a le droit d'afficher sa réponse (frappe dans la recherche, bascule du
/// filtre, réouverture).
#[derive(Debug, Default)]
pub struct RequestTracker {
    latest: u64,
}

impl RequestTracker {
    /// Ouvre une nouvelle demande et périme les précédentes.
    pub fn begin(&mut self) -> u64 {
        self.latest += 1;
        self.latest
    }

    pub fn is_current(&self, generation: u64) -> bool {
        generation == self.latest
    }
}

const MONTHS: [&str; 12] = [
    "janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.",
    "déc.",
];

/// « Ouvert à l'instant », « Ouvert il y a 2 min », « Ouvert il y a 3 h » (au-delà de 24 h : « Ouvert hier » si la date
/// locale est celle de la veille), puis « Ouvert le 22 oct. ».
/// Chaîne vide si la date est illisible : la carte n'affiche alors rien. Une date dans le futur (horloges décalées)
/// compte comme maintenant.
pub fn opened_label(now: DateTime<Utc>, last_viewed_at: &str) -> String {
    let Ok(viewed) = DateTime::parse_from_rfc3339(last_viewed_at) else {
        return String::new();
    };
    let elapsed = now.signed_duration_since(viewed);
    if elapsed.num_minutes() < 1 {
        "Ouvert à l'instant".to_owned()
    } else if elapsed.num_minutes() < 60 {
        format!("Ouvert il y a {} min", elapsed.num_minutes())
    } else if elapsed.num_hours() < 24 {
        format!("Ouvert il y a {} h", elapsed.num_hours())
    } else {
        // Au-delà de 24 h, « hier » se mesure en jours de calendrier locaux, pas en heures écoulées.
        let local = viewed.with_timezone(&Local);
        if local.date_naive().succ_opt() == Some(now.with_timezone(&Local).date_naive()) {
            "Ouvert hier".to_owned()
        } else {
            format!(
                "Ouvert le {} {}",
                local.day(),
                MONTHS[local.month0() as usize]
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, TimeZone as _, Utc};

    const FIXTURE: &str = include_str!("fixtures/artifacts-mine.json");

    fn titles(items: &[Artifact]) -> Vec<&str> {
        items.iter().map(|item| item.title.as_str()).collect()
    }

    const ALL_KINDS: &str = include_str!("fixtures/artifacts-all-kinds.json");

    #[test]
    fn parse_reads_the_original_fixture_with_design_included() {
        let items = parse_page(FIXTURE).unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!(
            items.iter().map(|item| item.kind).collect::<Vec<_>>(),
            vec![
                ArtifactKind::Docs,
                ArtifactKind::Slides,
                ArtifactKind::Page,
                ArtifactKind::Page,
                ArtifactKind::Design
            ]
        );
        assert!(titles(&items).contains(&"Maquette de la page d'accueil"));
    }

    #[test]
    fn parse_keeps_the_six_kinds_and_pages_but_never_design_system() {
        let items = parse_page(ALL_KINDS).unwrap();
        assert_eq!(items.len(), 7);
        assert_eq!(
            items.iter().map(|item| item.kind).collect::<Vec<_>>(),
            vec![
                ArtifactKind::Docs,
                ArtifactKind::Slides,
                ArtifactKind::Sheets,
                ArtifactKind::Design,
                ArtifactKind::Data,
                ArtifactKind::Whiteboard,
                ArtifactKind::Page
            ]
        );
        assert!(!titles(&items).contains(&"Charte graphique"));
    }

    #[test]
    fn type_titles_round_trip_through_kind_of() {
        for kind in ArtifactKind::CREATABLE {
            assert_eq!(kind_of(kind.type_title()), Some(kind));
        }
        assert_eq!(ArtifactKind::Page.type_title(), None);
    }

    #[test]
    fn filter_without_sheets_drops_sheets_only() {
        let items = parse_page(ALL_KINDS).unwrap();
        let filter = KindFilter {
            sheets: false,
            ..Default::default()
        };
        let kept = apply(&items, &filter);
        assert_eq!(kept.len(), 6);
        assert!(kept.iter().all(|item| item.kind != ArtifactKind::Sheets));
        assert_eq!(apply(&items, &KindFilter::default()).len(), 7);
    }

    #[test]
    fn kind_flag_reaches_every_kind_of_the_filter_order() {
        for kind in ArtifactKind::FILTER_ORDER {
            let mut filter = KindFilter::default();
            *kind_flag(&mut filter, kind) = false;
            let items = parse_page(ALL_KINDS).unwrap();
            let kept = apply(&items, &filter);
            assert_eq!(kept.len(), 6, "{kind:?}");
            assert!(kept.iter().all(|item| item.kind != kind), "{kind:?}");
        }
    }

    #[test]
    fn tolerates_missing_and_unknown_fields() {
        let items = parse_page(FIXTURE).unwrap();
        // Avec une vignette sombre, elle est préférée.
        assert!(
            items[0]
                .thumbnail
                .as_deref()
                .unwrap()
                .contains("0001_dark_thumb")
        );
        // Sans vignette sombre, la vignette claire prend le relais.
        assert!(
            items[3]
                .thumbnail
                .as_deref()
                .unwrap()
                .contains("0004_thumb")
        );
        assert_eq!(items[3].last_viewed_at, None);
        assert_eq!(parse_page(r#"{"foo":1}"#), Err(ApiError::Unreadable));
        assert_eq!(parse_page("<html>nope</html>"), Err(ApiError::Unreadable));
    }

    #[test]
    fn a_list_where_nothing_is_readable_is_an_error_not_an_empty_gallery() {
        // Champs renommés par l'API : tous les éléments échouent, ce n'est pas une galerie vide.
        let renamed = r#"{"artifacts":[{"uuid":"a","url":"u"},{"uuid":"b","url":"v"}]}"#;
        assert_eq!(parse_page(renamed), Err(ApiError::Unreadable));
        // Une vraie liste vide, ou une liste de seuls types exclus, reste une réponse lisible.
        assert_eq!(parse_page(r#"{"artifacts":[]}"#), Ok(vec![]));
        let only_excluded =
            r#"{"artifacts":[{"id":"a","open_url":"u","type_title":"Design System"}]}"#;
        assert_eq!(parse_page(only_excluded), Ok(vec![]));
    }

    #[test]
    fn an_element_without_id_is_skipped_not_fatal() {
        let body = r#"{"artifacts":[{"title":"sans id"},{"id":"a","title":"ok","open_url":"u"}]}"#;
        assert_eq!(titles(&parse_page(body).unwrap()), vec!["ok"]);
    }

    #[test]
    fn kind_of_matches_titles_without_case_or_padding() {
        assert_eq!(kind_of(Some("Docs")), Some(ArtifactKind::Docs));
        assert_eq!(kind_of(Some("Slides")), Some(ArtifactKind::Slides));
        assert_eq!(kind_of(Some(" sheets ")), Some(ArtifactKind::Sheets));
        assert_eq!(kind_of(Some("DATA")), Some(ArtifactKind::Data));
        assert_eq!(kind_of(Some("Design")), Some(ArtifactKind::Design));
        assert_eq!(kind_of(Some("whiteboard")), Some(ArtifactKind::Whiteboard));
        assert_eq!(kind_of(None), Some(ArtifactKind::Page));
        assert_eq!(kind_of(Some("Design System")), None);
        assert_eq!(kind_of(Some("Dashboard")), None);
        assert_eq!(kind_of(Some("Quelque chose")), None);
    }

    #[test]
    fn filter_without_pages_drops_untyped() {
        let items = parse_page(FIXTURE).unwrap();
        let filter = KindFilter {
            pages: false,
            ..Default::default()
        };
        let kept = apply(&items, &filter);
        assert_eq!(kept.len(), 3);
        assert!(kept.iter().all(|item| item.kind != ArtifactKind::Page));
        assert_eq!(apply(&items, &KindFilter::default()).len(), 5);
    }

    #[test]
    fn filter_roundtrips_through_json() {
        let filter = KindFilter {
            slides: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        assert_eq!(serde_json::from_str::<KindFilter>(&json).unwrap(), filter);
    }

    #[test]
    fn query_string_without_search() {
        assert_eq!(
            query_string(&ListQuery::default(), "org-1"),
            "rel=mine&empty=exclude&limit=48&org=org-1&sort=last_viewed"
        );
    }

    #[test]
    fn query_string_adds_trimmed_search() {
        let query = ListQuery {
            search: Some("  claude ".to_owned()),
        };
        assert!(query_string(&query, "o").ends_with("&q=claude"));
        let blank = ListQuery {
            search: Some("   ".to_owned()),
        };
        assert!(!query_string(&blank, "o").contains("q="));
    }

    #[test]
    fn query_string_encodes_reserved_characters() {
        let query = ListQuery {
            search: Some("a&b=c é".to_owned()),
        };
        assert!(query_string(&query, "o").ends_with("&q=a%26b%3Dc+%C3%A9"));
    }

    #[test]
    fn login_redirect_means_signed_out() {
        assert_eq!(classify(200, true), Some(ApiError::SignedOut));
        assert_eq!(classify(200, false), None);
    }

    #[test]
    fn status_401_and_403_mean_signed_out() {
        assert_eq!(classify(401, false), Some(ApiError::SignedOut));
        assert_eq!(classify(403, false), Some(ApiError::SignedOut));
    }

    #[test]
    fn status_404_is_http_error() {
        assert_eq!(classify(404, false), Some(ApiError::Http(404)));
        assert_eq!(classify(500, false), Some(ApiError::Http(500)));
    }

    #[test]
    fn stale_response_is_ignored() {
        let mut tracker = RequestTracker::default();
        let a = tracker.begin();
        let b = tracker.begin();
        assert!(!tracker.is_current(a));
        assert!(tracker.is_current(b));
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Un instant de l'horloge locale : les tests de calendrier ne dépendent pas du fuseau de la machine.
    fn local(month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(2026, month, day, hour, minute, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn opened_label_buckets() {
        // Mi-octobre : loin du changement d'heure, 26 h écoulées sont bien 26 h d'horloge murale.
        let now = local(10, 12, 12, 0);
        let ago = |delta: Duration| (now - delta).to_rfc3339();
        assert_eq!(
            opened_label(now, &ago(Duration::seconds(30))),
            "Ouvert à l'instant"
        );
        assert_eq!(
            opened_label(now, &ago(Duration::minutes(2))),
            "Ouvert il y a 2 min"
        );
        assert_eq!(
            opened_label(now, &ago(Duration::hours(3))),
            "Ouvert il y a 3 h"
        );
        // La veille, 26 h plus tôt : le 11 à 10 h pour un « maintenant » du 12 à midi.
        assert_eq!(opened_label(now, &ago(Duration::hours(26))), "Ouvert hier");
        assert_eq!(
            opened_label(now, &local(10, 9, 12, 0).to_rfc3339()),
            "Ouvert le 9 oct."
        );
    }

    #[test]
    fn yesterday_follows_the_calendar_day_not_the_hours() {
        // Passé depuis 24,5 h mais encore la veille : le 10 à 23 h, maintenant le 11 à 23 h 30.
        assert_eq!(
            opened_label(local(10, 11, 23, 30), &local(10, 10, 23, 0).to_rfc3339()),
            "Ouvert hier"
        );
        // 47 h écoulées mais deux jours de calendrier : le 9 à 23 h, maintenant le 11 à 22 h.
        assert_eq!(
            opened_label(local(10, 11, 22, 0), &local(10, 9, 23, 0).to_rfc3339()),
            "Ouvert le 9 oct."
        );
    }

    #[test]
    fn opened_label_reads_fractional_seconds_and_rejects_garbage() {
        let now = at("2026-10-25T10:25:00Z");
        assert_eq!(
            opened_label(now, "2026-10-25T10:24:31.394313Z"),
            "Ouvert à l'instant"
        );
        assert_eq!(opened_label(now, "pas une date"), "");
        assert_eq!(opened_label(now, ""), "");
    }

    #[test]
    fn opened_label_treats_a_future_date_as_now() {
        let now = at("2026-10-25T10:00:00Z");
        assert_eq!(
            opened_label(now, "2026-10-25T10:00:30Z"),
            "Ouvert à l'instant"
        );
    }

    #[test]
    fn opened_label_names_every_french_month() {
        let now = local(12, 31, 12, 0);
        let months = [
            "janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.",
            "nov.", "déc.",
        ];
        for (index, name) in months.iter().enumerate() {
            let date = local(index as u32 + 1, 15, 12, 0).to_rfc3339();
            assert_eq!(opened_label(now, &date), format!("Ouvert le 15 {name}"));
        }
    }
}
