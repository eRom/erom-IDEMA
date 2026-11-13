//! La création d'un artifact : la partie pure (choix du type, requête, clé d'idempotence, lecture de la réponse). Le
//! rejeu lui-même passe par `source`, dans la page cachée.

use rand::RngCore as _;
use rand::rngs::OsRng;
use serde::Deserialize;
use serde_json::Value;
use url::form_urlencoded;

use super::ApiError;
use super::model::{ArtifactKind, classify};

/// Pourquoi une création a échoué.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    /// Aucun type du catalogue ne porte ce titre.
    TypeNotFound,
    /// L'API n'a pas répondu comme attendu : rien n'a été créé.
    Api(ApiError),
    /// Le POST a réussi (l'artifact existe) mais son adresse n'a pas pu être lue, même après un second essai.
    Unopened(ApiError),
    /// Le POST lui-même s'est perdu (délai, page injoignable) : l'artifact existe peut-être.
    Uncertain,
}

impl From<ApiError> for CreateError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

#[derive(Deserialize)]
struct TypesBody {
    types: Vec<TypeEntry>,
}

#[derive(Deserialize)]
struct TypeEntry {
    slug: String,
    #[serde(default)]
    title: String,
}

/// Le `slug` du type dont le titre est `title` (sans casse, rogné) dans le corps de `GET /api/frame/types`. Il entre tel
/// quel dans le chemin du POST : s'il n'a pas la forme d'un UUID, le type est tenu pour introuvable.
pub fn type_slug(types_body: &str, title: &str) -> Result<String, CreateError> {
    let body: TypesBody =
        serde_json::from_str(types_body).map_err(|_| CreateError::Api(ApiError::Unreadable))?;
    let wanted = title.trim();
    body.types
        .into_iter()
        .find(|entry| entry.title.trim().eq_ignore_ascii_case(wanted))
        .map(|entry| entry.slug)
        .filter(|slug| is_uuid(slug))
        .ok_or(CreateError::TypeNotFound)
}

/// La chaîne de requête `org=<org>` de `GET /api/frame/types` et `GET /api/frame/<uuid>`.
pub fn org_query(org: &str) -> String {
    form_urlencoded::Serializer::new(String::new())
        .append_pair("org", org)
        .finish()
}

/// La chaîne de requête de `POST /api/frame/types/<slug>/create`.
pub fn create_query(org: &str, idempotency_key: &str) -> String {
    form_urlencoded::Serializer::new(String::new())
        .append_pair("org", org)
        .append_pair("idempotency_key", idempotency_key)
        .finish()
}

/// Le corps exact de la création : un titre provisoire, la version de type par défaut.
pub const CREATE_BODY: &str = r#"{"title":"Untitled","target_type_version":null}"#;

/// Une clé d'idempotence neuve : un UUID v4 en minuscules.
pub fn new_idempotency_key() -> String {
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    uuid_v4_from(bytes)
}

/// Un UUID v4 tiré de 16 octets (bits de version et de variante forcés).
fn uuid_v4_from(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

const ADDRESS_PREFIX: &str = "https://claude.ai/artifact/";

fn is_uuid(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(part, len)| part.len() == len && part.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// L'adresse d'un artifact contenue dans une chaîne : `https://claude.ai/artifact/<id>` telle quelle, ou un chemin
/// `/artifact/<id>` complété.
pub fn artifact_address(text: &str) -> Option<String> {
    let rest = if let Some(rest) = text.strip_prefix(ADDRESS_PREFIX) {
        rest
    } else {
        text.strip_prefix("/artifact/")?
    };
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!id.is_empty()).then(|| format!("{ADDRESS_PREFIX}{id}"))
}

/// Le `slug` de premier niveau de la réponse de création : l'UUID du nouvel artifact (le `slug` de `type` est celui du
/// type, on ne le lit pas). Absent ou pas un UUID : réponse illisible.
pub fn created_slug(create_body: &str) -> Result<String, CreateError> {
    let unreadable = || CreateError::Api(ApiError::Unreadable);
    let value: Value = serde_json::from_str(create_body).map_err(|_| unreadable())?;
    value
        .get("slug")
        .and_then(Value::as_str)
        .filter(|slug| is_uuid(slug))
        .map(str::to_owned)
        .ok_or_else(unreadable)
}

/// L'adresse du nouvel artifact, lue dans la réponse de `GET /api/frame/<slug>` : `viewerPath` (`/artifact/<id>`)
/// complété par l'hôte, comme l'app web.
pub fn created_url(frame_body: &str) -> Result<String, ApiError> {
    let value: Value = serde_json::from_str(frame_body).map_err(|_| ApiError::Unreadable)?;
    value
        .get("viewerPath")
        .and_then(Value::as_str)
        .filter(|path| path.starts_with("/artifact/"))
        .and_then(artifact_address)
        .ok_or(ApiError::Unreadable)
}

/// Le motif court d'une erreur d'API, sans détail interne (le détail va au journal de la source, jamais à l'écran).
pub fn api_detail(error: &ApiError) -> String {
    match error {
        ApiError::SignedOut => "non connecté".to_owned(),
        ApiError::Http(code) => format!("HTTP {code}"),
        ApiError::Unreadable => "réponse illisible".to_owned(),
        ApiError::Unavailable(_) => "claude.ai injoignable".to_owned(),
    }
}

/// Le texte d'un échec de création, tel que la galerie le dit (spec §3.4). Trois formes : « Création impossible » (rien
/// n'a été créé), « Artifact créé, mais impossible de l'ouvrir » (l'artifact existe) et « Création incertaine » (le POST
/// a pu aboutir).
pub fn reason(error: &CreateError) -> String {
    match error {
        CreateError::TypeNotFound => "Création impossible : type introuvable".to_owned(),
        CreateError::Api(error) => format!("Création impossible : {}", api_detail(error)),
        CreateError::Unopened(error) => {
            format!(
                "Artifact créé, mais impossible de l'ouvrir : {}",
                api_detail(error)
            )
        }
        CreateError::Uncertain => {
            "Création incertaine : vérifie claude.ai avant de réessayer".to_owned()
        }
    }
}

/// L'erreur d'un POST de création. Une réponse reçue (HTTP, déconnexion, corps illisible) dit que rien n'a été créé ;
/// un délai ou une page injoignable laisse la création incertaine : la requête a pu partir et aboutir.
pub fn post_error(error: ApiError) -> CreateError {
    match error {
        ApiError::Unavailable(_) => CreateError::Uncertain,
        other => CreateError::Api(other),
    }
}

/// Où en est la création côté galerie : une seule à la fois, l'échec reste affiché jusqu'à ce qu'on l'efface.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CreateState {
    #[default]
    Idle,
    Running(ArtifactKind),
    /// Le motif complet, prêt à afficher (« Création impossible : ... »).
    Failed(String),
}

impl CreateState {
    /// Démarre une création. Faux (rien ne change) si une autre tourne ; un échec affiché ne bloque pas.
    pub fn begin(&mut self, kind: ArtifactKind) -> bool {
        if matches!(self, Self::Running(_)) {
            return false;
        }
        *self = Self::Running(kind);
        true
    }

    /// Clôt la création en cours : retour au repos, ou l'échec à afficher.
    pub fn finish(&mut self, result: &Result<String, CreateError>) {
        *self = match result {
            Ok(_) => Self::Idle,
            Err(error) => Self::Failed(reason(error)),
        };
    }

    /// Efface l'échec affiché ; sans effet sur une création en cours.
    pub fn clear_failure(&mut self) {
        if matches!(self, Self::Failed(_)) {
            *self = Self::Idle;
        }
    }
}

/// Ce que répond la fausse création : `missing-type` pour un type introuvable, un code HTTP pour l'erreur que `classify`
/// en tire, sinon (rien, 200 ou illisible) l'adresse du fichier de test.
pub fn fixture_outcome(url: &str, status: Option<&str>) -> Result<String, CreateError> {
    let status = status.map(str::trim);
    if status == Some("missing-type") {
        return Err(CreateError::TypeNotFound);
    }
    let code = status.and_then(|status| status.parse::<u16>().ok());
    match code.and_then(|code| classify(code, false)) {
        Some(error) => Err(CreateError::Api(error)),
        None => Ok(url.to_owned()),
    }
}

/// Les lignes du journal d'une création : le titre du type et l'issue, jamais un en-tête, un cookie, l'`org` ni la clé
/// d'idempotence. L'adresse d'un artifact créé peut y figurer.
pub fn requested_line(title: &str) -> String {
    format!("artifacts create requested type={title}")
}

pub fn post_line(title: &str) -> String {
    format!("artifacts create post type={title}")
}

/// L'artifact existe mais son adresse n'a pas pu être lue : son UUID peut figurer au journal, avec le motif court.
pub fn unopened_line(title: &str, artifact: &str, error: &ApiError) -> String {
    format!(
        "artifacts create unopened type={title} artifact={artifact} reason={}",
        api_detail(error)
    )
}

pub fn outcome_line(title: &str, result: &Result<String, CreateError>) -> String {
    match result {
        Ok(url) => format!("artifacts create ok type={title} url={url}"),
        Err(error) => format!(
            "artifacts create failed type={title} reason={}",
            reason(error)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC_SLUG: &str = "9a1c1e0b-7a6f-4e0d-a952-713c1f8e2d4b";
    const SHEET_SLUG: &str = "1c1e0b9a-6f7a-4d0e-952a-3c1f8e2d4b71";
    const TYPES: &str = r#"{"types":[{"slug":"9a1c1e0b-7a6f-4e0d-a952-713c1f8e2d4b","title":"Docs"},{"slug":"1c1e0b9a-6f7a-4d0e-952a-3c1f8e2d4b71","title":"Sheets"}]}"#;

    #[test]
    fn type_slug_picks_the_slug_of_the_matching_title() {
        assert_eq!(type_slug(TYPES, "Sheets"), Ok(SHEET_SLUG.to_owned()));
        assert_eq!(type_slug(TYPES, "Docs"), Ok(DOC_SLUG.to_owned()));
    }

    #[test]
    fn type_slug_matches_without_case() {
        assert_eq!(type_slug(TYPES, "  sHeEtS "), Ok(SHEET_SLUG.to_owned()));
    }

    #[test]
    fn a_type_slug_that_is_not_a_uuid_is_type_not_found() {
        // Le slug entre dans le chemin du POST : une valeur qui n'a pas la forme d'un UUID n'y va jamais.
        for slug in [
            "doc-1",
            "../x",
            "a/b",
            "",
            "9a1c1e0b-7a6f-4e0d-a952-713c1f8e2d4b/../x",
        ] {
            let body = format!(r#"{{"types":[{{"slug":"{slug}","title":"Docs"}}]}}"#);
            assert_eq!(
                type_slug(&body, "Docs"),
                Err(CreateError::TypeNotFound),
                "slug {slug:?}"
            );
        }
    }

    #[test]
    fn a_missing_type_is_type_not_found() {
        assert_eq!(
            type_slug(TYPES, "Whiteboard"),
            Err(CreateError::TypeNotFound)
        );
    }

    #[test]
    fn an_unreadable_types_body_is_unreadable() {
        assert_eq!(
            type_slug("<html>nope</html>", "Docs"),
            Err(CreateError::Api(ApiError::Unreadable))
        );
    }

    #[test]
    fn create_query_carries_org_and_key() {
        assert_eq!(create_query("o-1", "k-1"), "org=o-1&idempotency_key=k-1");
        assert_eq!(create_query("a&b", "k"), "org=a%26b&idempotency_key=k");
    }

    #[test]
    fn org_query_encodes_the_org() {
        assert_eq!(org_query("o-1"), "org=o-1");
        assert_eq!(org_query("a b&c"), "org=a+b%26c");
    }

    #[test]
    fn idempotency_key_is_a_fresh_uuid_v4() {
        let first = new_idempotency_key();
        assert!(is_uuid(&first));
        assert_eq!(&first[14..15], "4");
        assert!(matches!(&first[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(first, new_idempotency_key());
    }

    #[test]
    fn uuid_v4_forces_version_and_variant_bits() {
        assert_eq!(
            uuid_v4_from([0xff; 16]),
            "ffffffff-ffff-4fff-bfff-ffffffffffff"
        );
        assert_eq!(
            uuid_v4_from([0; 16]),
            "00000000-0000-4000-8000-000000000000"
        );
    }

    #[test]
    fn artifact_address_reads_a_full_url_or_a_path() {
        assert_eq!(
            artifact_address("https://claude.ai/artifact/AbC123?x=1"),
            Some("https://claude.ai/artifact/AbC123".to_owned())
        );
        assert_eq!(
            artifact_address("/artifact/AbC123"),
            Some("https://claude.ai/artifact/AbC123".to_owned())
        );
        assert_eq!(artifact_address("/artifacts/AbC123"), None);
        assert_eq!(artifact_address("/artifact/"), None);
    }

    const CREATED: &str = r#"{"capabilities":[],"owner_account_id":"11111111-2222-4333-8444-555555555555","slug":"0b9a1c1e-6f7a-4d0e-9a52-3c1f8e2d4b71","title":"Untitled","type":{"slug":"9a1c1e0b-7a6f-4e0d-a952-713c1f8e2d4b","title":"Docs"}}"#;
    const FRAME: &str = r#"{"viewerPath":"/artifact/7BGDZcfPeT4C9VougAt49z","title":"Untitled"}"#;

    #[test]
    fn created_slug_reads_the_top_level_slug_not_the_type_slug() {
        assert_eq!(
            created_slug(CREATED),
            Ok("0b9a1c1e-6f7a-4d0e-9a52-3c1f8e2d4b71".to_owned())
        );
    }

    #[test]
    fn created_slug_without_a_uuid_slug_is_unreadable() {
        let unreadable = Err(CreateError::Api(ApiError::Unreadable));
        assert_eq!(
            created_slug(r#"{"type":{"slug":"0b9a1c1e-6f7a-4d0e-9a52-3c1f8e2d4b71"}}"#),
            unreadable
        );
        assert_eq!(created_slug(r#"{"slug":"not-a-uuid"}"#), unreadable);
        assert_eq!(created_slug(r#"{"slug":12}"#), unreadable);
        assert_eq!(created_slug("<html>"), unreadable);
    }

    #[test]
    fn created_url_reads_the_phase0_field() {
        assert_eq!(
            created_url(FRAME),
            Ok("https://claude.ai/artifact/7BGDZcfPeT4C9VougAt49z".to_owned())
        );
    }

    #[test]
    fn created_url_without_the_field_is_unreadable() {
        let unreadable = Err(ApiError::Unreadable);
        assert_eq!(created_url(r#"{"title":"Untitled"}"#), unreadable);
        assert_eq!(
            created_url(r#"{"viewerPath":"/elsewhere/abc"}"#),
            unreadable
        );
        assert_eq!(created_url(r#"{"viewerPath":"/artifact/"}"#), unreadable);
        assert_eq!(created_url(r#"{"viewerPath":3}"#), unreadable);
        assert_eq!(created_url("<html>"), unreadable);
    }

    #[test]
    fn a_second_create_while_running_is_refused() {
        let mut state = CreateState::Idle;
        assert!(state.begin(ArtifactKind::Docs));
        assert_eq!(state, CreateState::Running(ArtifactKind::Docs));
        assert!(!state.begin(ArtifactKind::Sheets));
        assert_eq!(state, CreateState::Running(ArtifactKind::Docs));
        state.finish(&Ok("https://claude.ai/artifact/abc".to_owned()));
        assert_eq!(state, CreateState::Idle);
        assert!(state.begin(ArtifactKind::Sheets));
    }

    #[test]
    fn a_failure_is_shown_then_cleared() {
        let mut state = CreateState::Idle;
        assert!(state.begin(ArtifactKind::Docs));
        state.finish(&Err(CreateError::Api(ApiError::Http(500))));
        assert_eq!(
            state,
            CreateState::Failed("Création impossible : HTTP 500".to_owned())
        );
        assert!(state.begin(ArtifactKind::Slides));
        state.finish(&Err(CreateError::TypeNotFound));
        state.clear_failure();
        assert_eq!(state, CreateState::Idle);
    }

    #[test]
    fn clear_failure_leaves_a_running_create_alone() {
        let mut state = CreateState::Running(ArtifactKind::Data);
        state.clear_failure();
        assert_eq!(state, CreateState::Running(ArtifactKind::Data));
    }

    #[test]
    fn create_errors_have_their_reason() {
        assert_eq!(
            reason(&CreateError::TypeNotFound),
            "Création impossible : type introuvable"
        );
        assert_eq!(
            reason(&CreateError::Api(ApiError::SignedOut)),
            "Création impossible : non connecté"
        );
        assert_eq!(
            reason(&CreateError::Api(ApiError::Unreadable)),
            "Création impossible : réponse illisible"
        );
        assert_eq!(
            reason(&CreateError::Api(ApiError::Unavailable("x".to_owned()))),
            "Création impossible : claude.ai injoignable"
        );
        assert_eq!(
            reason(&CreateError::Api(ApiError::Http(404))),
            "Création impossible : HTTP 404"
        );
    }

    #[test]
    fn an_unopened_artifact_says_it_was_created() {
        assert_eq!(
            reason(&CreateError::Unopened(ApiError::Http(500))),
            "Artifact créé, mais impossible de l'ouvrir : HTTP 500"
        );
        assert_eq!(
            reason(&CreateError::Unopened(ApiError::Unreadable)),
            "Artifact créé, mais impossible de l'ouvrir : réponse illisible"
        );
        assert_eq!(
            reason(&CreateError::Unopened(ApiError::Unavailable(
                "x".to_owned()
            ))),
            "Artifact créé, mais impossible de l'ouvrir : claude.ai injoignable"
        );
        assert_eq!(
            reason(&CreateError::Unopened(ApiError::SignedOut)),
            "Artifact créé, mais impossible de l'ouvrir : non connecté"
        );
    }

    #[test]
    fn an_uncertain_creation_asks_to_check_claude_ai_first() {
        assert_eq!(
            reason(&CreateError::Uncertain),
            "Création incertaine : vérifie claude.ai avant de réessayer"
        );
    }

    #[test]
    fn only_an_unreachable_post_leaves_the_creation_uncertain() {
        // Le POST a pu partir : réponse perdue ou délai, l'artifact existe peut-être.
        assert_eq!(
            post_error(ApiError::Unavailable("script call timed out".to_owned())),
            CreateError::Uncertain
        );
        // Une réponse reçue dit clairement que rien n'a été créé.
        assert_eq!(
            post_error(ApiError::Http(500)),
            CreateError::Api(ApiError::Http(500))
        );
        assert_eq!(
            post_error(ApiError::SignedOut),
            CreateError::Api(ApiError::SignedOut)
        );
        assert_eq!(
            post_error(ApiError::Unreadable),
            CreateError::Api(ApiError::Unreadable)
        );
    }

    #[test]
    fn unopened_and_uncertain_failures_are_shown_then_cleared() {
        let mut state = CreateState::Idle;
        assert!(state.begin(ArtifactKind::Docs));
        state.finish(&Err(CreateError::Unopened(ApiError::Http(404))));
        assert_eq!(
            state,
            CreateState::Failed("Artifact créé, mais impossible de l'ouvrir : HTTP 404".to_owned())
        );
        // L'échec affiché ne bloque pas le clic suivant.
        assert!(state.begin(ArtifactKind::Docs));
        state.finish(&Err(CreateError::Uncertain));
        assert_eq!(
            state,
            CreateState::Failed(
                "Création incertaine : vérifie claude.ai avant de réessayer".to_owned()
            )
        );
        state.clear_failure();
        assert_eq!(state, CreateState::Idle);
    }

    #[test]
    fn the_fixture_answers_the_address_or_the_forced_failure() {
        let url = "https://claude.ai/artifact/AbC123";
        assert_eq!(fixture_outcome(url, None), Ok(url.to_owned()));
        assert_eq!(fixture_outcome(url, Some("200")), Ok(url.to_owned()));
        assert_eq!(fixture_outcome(url, Some("garbage")), Ok(url.to_owned()));
        assert_eq!(
            fixture_outcome(url, Some(" 500 ")),
            Err(CreateError::Api(ApiError::Http(500)))
        );
        assert_eq!(
            fixture_outcome(url, Some("401")),
            Err(CreateError::Api(ApiError::SignedOut))
        );
        assert_eq!(
            fixture_outcome(url, Some("missing-type")),
            Err(CreateError::TypeNotFound)
        );
    }

    #[test]
    fn journal_lines_carry_the_title_and_the_outcome_only() {
        assert_eq!(
            requested_line("Docs"),
            "artifacts create requested type=Docs"
        );
        assert_eq!(post_line("Docs"), "artifacts create post type=Docs");
        assert_eq!(
            outcome_line("Docs", &Ok("https://claude.ai/artifact/AbC123".to_owned())),
            "artifacts create ok type=Docs url=https://claude.ai/artifact/AbC123"
        );
        assert_eq!(
            outcome_line("Sheets", &Err(CreateError::Api(ApiError::Http(500)))),
            "artifacts create failed type=Sheets reason=Création impossible : HTTP 500"
        );
    }

    #[test]
    fn the_unopened_line_names_the_artifact_and_a_short_reason() {
        assert_eq!(
            unopened_line(
                "Docs",
                "0b9a1c1e-6f7a-4d0e-9a52-3c1f8e2d4b71",
                &ApiError::Http(502)
            ),
            "artifacts create unopened type=Docs artifact=0b9a1c1e-6f7a-4d0e-9a52-3c1f8e2d4b71 reason=HTTP 502"
        );
        assert_eq!(
            unopened_line(
                "Docs",
                "x",
                &ApiError::Unavailable("secret detail".to_owned())
            ),
            "artifacts create unopened type=Docs artifact=x reason=claude.ai injoignable"
        );
    }
}
