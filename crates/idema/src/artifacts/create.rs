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
    /// L'API n'a pas répondu comme attendu.
    Api(ApiError),
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

/// Le `slug` du type dont le titre est `title` (sans casse, rogné) dans le corps de `GET /api/frame/types`.
pub fn type_slug(types_body: &str, title: &str) -> Result<String, CreateError> {
    let body: TypesBody =
        serde_json::from_str(types_body).map_err(|_| CreateError::Api(ApiError::Unreadable))?;
    let wanted = title.trim();
    body.types
        .into_iter()
        .find(|entry| entry.title.trim().eq_ignore_ascii_case(wanted))
        .map(|entry| entry.slug)
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
const BASE58: &str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

fn is_uuid(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(part, len)| part.len() == len && part.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Un identifiant d'adresse d'artifact : 22 caractères base58.
fn is_base58_22(text: &str) -> bool {
    text.len() == 22 && text.chars().all(|c| BASE58.contains(c))
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

/// La forme d'une valeur, pour le journal : jamais la valeur, sauf une adresse d'artifact (`https://claude.ai/artifact/...`).
pub fn value_shape(value: &Value) -> String {
    match value {
        Value::String(text) => {
            if text.starts_with(ADDRESS_PREFIX) {
                artifact_address(text).unwrap_or_else(|| "other".to_owned())
            } else if text.starts_with("/artifact/") {
                "/artifact/<id>".to_owned()
            } else if is_uuid(text) {
                "uuid".to_owned()
            } else if is_base58_22(text) {
                "base58-22".to_owned()
            } else {
                "other".to_owned()
            }
        }
        Value::Object(_) => "object".to_owned(),
        Value::Array(_) => "array".to_owned(),
        Value::Number(_) => "number".to_owned(),
        Value::Bool(_) => "bool".to_owned(),
        Value::Null => "null".to_owned(),
    }
}

/// Ce que la sonde retient d'une réponse de création.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CreateSummary {
    /// Les clés du premier niveau, dans l'ordre du corps.
    pub keys: Vec<String>,
    /// `chemin=forme` des chaînes qui pourraient porter l'identité de l'artifact (un niveau d'objet en dessous).
    pub candidates: Vec<String>,
    /// L'adresse `https://claude.ai/artifact/<id>` si une valeur la porte.
    pub address: Option<String>,
    /// Le premier UUID rencontré (chemin, valeur) : l'identité à interroger par `GET /api/frame/<uuid>`.
    pub uuid: Option<(String, String)>,
}

/// Lit une réponse de création : clés, candidats, adresse, UUID. Un corps illisible rend un résumé vide.
pub fn summarize_create(body: &str) -> CreateSummary {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(body) else {
        return CreateSummary::default();
    };
    let mut summary = CreateSummary {
        keys: root.keys().cloned().collect(),
        ..CreateSummary::default()
    };
    for (key, value) in &root {
        visit(key, value, 0, &mut summary);
    }
    summary
}

fn visit(path: &str, value: &Value, depth: usize, summary: &mut CreateSummary) {
    match value {
        Value::String(text) => {
            let shape = value_shape(value);
            if shape != "other" {
                summary.candidates.push(format!("{path}={shape}"));
            }
            if summary.address.is_none() {
                summary.address = artifact_address(text);
            }
            if summary.uuid.is_none() && is_uuid(text) {
                summary.uuid = Some((path.to_owned(), text.clone()));
            }
        }
        Value::Object(map) if depth < 1 => {
            for (key, inner) in map {
                visit(&format!("{path}.{key}"), inner, depth + 1, summary);
            }
        }
        _ => {}
    }
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
pub fn created_url(frame_body: &str) -> Result<String, CreateError> {
    let unreadable = || CreateError::Api(ApiError::Unreadable);
    let value: Value = serde_json::from_str(frame_body).map_err(|_| unreadable())?;
    value
        .get("viewerPath")
        .and_then(Value::as_str)
        .filter(|path| path.starts_with("/artifact/"))
        .and_then(artifact_address)
        .ok_or_else(unreadable)
}

/// Le motif d'un échec de création, tel que la galerie le dit (spec §3.6).
pub fn reason(error: &CreateError) -> String {
    let detail = match error {
        CreateError::TypeNotFound => "type introuvable".to_owned(),
        CreateError::Api(ApiError::SignedOut) => "non connecté".to_owned(),
        CreateError::Api(ApiError::Http(code)) => format!("HTTP {code}"),
        CreateError::Api(ApiError::Unreadable) => "réponse illisible".to_owned(),
        CreateError::Api(ApiError::Unavailable(_)) => "claude.ai injoignable".to_owned(),
    };
    format!("Création impossible : {detail}")
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

    const TYPES: &str =
        r#"{"types":[{"slug":"doc-1","title":"Docs"},{"slug":"sheet-2","title":"Sheets"}]}"#;

    #[test]
    fn type_slug_picks_the_slug_of_the_matching_title() {
        assert_eq!(type_slug(TYPES, "Sheets"), Ok("sheet-2".to_owned()));
        assert_eq!(type_slug(TYPES, "Docs"), Ok("doc-1".to_owned()));
    }

    #[test]
    fn type_slug_matches_without_case() {
        assert_eq!(type_slug(TYPES, "  sHeEtS "), Ok("sheet-2".to_owned()));
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
    fn value_shape_never_echoes_a_plain_value() {
        let text = |s: &str| value_shape(&Value::String(s.to_owned()));
        assert_eq!(text("0b9a1c1e-6f7a-4d0e-9a52-3c1f8e2d4b71"), "uuid");
        assert_eq!(text("7Yq3kPz9TmWx4Ab2NcDe5f"), "base58-22");
        assert_eq!(text("/artifact/abc123"), "/artifact/<id>");
        assert_eq!(text("secret-org-name"), "other");
        assert_eq!(text("sk-ant-sid01-xxx"), "other");
    }

    #[test]
    fn value_shape_keeps_only_an_artifact_address() {
        assert_eq!(
            value_shape(&Value::String(
                "https://claude.ai/artifact/AbC123".to_owned()
            )),
            "https://claude.ai/artifact/AbC123"
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

    #[test]
    fn summarize_create_lists_keys_candidates_and_the_uuid() {
        let body = r#"{"capabilities":[],"contract":"1.0.0","id":"0b9a1c1e-6f7a-4d0e-9a52-3c1f8e2d4b71","history":{"head":"7Yq3kPz9TmWx4Ab2NcDe5f","note":"hello"}}"#;
        let summary = summarize_create(body);
        assert_eq!(summary.keys, ["capabilities", "contract", "id", "history"]);
        assert_eq!(summary.candidates, ["id=uuid", "history.head=base58-22"]);
        assert_eq!(
            summary.uuid,
            Some((
                "id".to_owned(),
                "0b9a1c1e-6f7a-4d0e-9a52-3c1f8e2d4b71".to_owned()
            ))
        );
        assert_eq!(summary.address, None);
    }

    #[test]
    fn summarize_create_finds_an_address() {
        let summary = summarize_create(r#"{"url":"https://claude.ai/artifact/AbC123"}"#);
        assert_eq!(
            summary.address,
            Some("https://claude.ai/artifact/AbC123".to_owned())
        );
    }

    #[test]
    fn summarize_create_survives_an_unreadable_body() {
        assert_eq!(summarize_create("<html>"), CreateSummary::default());
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
        let unreadable = Err(CreateError::Api(ApiError::Unreadable));
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
}
