//! Sonde 2 du compagnon iOS : un petit service HTTP dans le process d'IdemA. Un fil dédié porte son propre runtime
//! tokio ; il expose la flotte de sessions (instantané lu sans jamais attendre le fil d'interface) et tape une touche
//! dans le pane actif d'une session par le client de contrôle tmux déjà là. Tout est derrière la variable
//! `IDEMA_COMPANION_PORT` : absente ou invalide, aucun fil, aucune prise, aucun changement de comportement.
//! Ni push, ni page, ni appairage : la sonde ne prouve que le service dans le process et son coût.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::channel::{mpsc, oneshot};
use idema_term::activity::ActivityState;
use serde::{Deserialize, Serialize};

use crate::hud;
use crate::sidebar_model::{SessionSummary, dock_badge_count};

/// La variable qui ouvre la porte de la sonde (la version finale lira une section `[companion]` de la config).
pub const PORT_VAR: &str = "IDEMA_COMPANION_PORT";

/// Le temps que le fil d'interface a pour répondre à une commande avant le 503.
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// Taille maximale du texte d'une commande, en octets UTF-8.
const MAX_TEXT_BYTES: usize = 1024;

/// Taille maximale du corps d'une requête : un texte de 1024 octets échappé en JSON tient largement dedans.
const MAX_BODY_BYTES: usize = 16 * 1024;

/// Ce que la variable d'environnement décide.
#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    /// Variable absente : la sonde n'existe pas.
    Off,
    /// Variable posée mais inutilisable : on le dit une fois au journal, rien ne démarre.
    Invalid(String),
    On(u16),
}

/// Lit la valeur brute de la variable. Le port 0 (choisi par le système) est refusé : la sonde doit être joignable
/// à une adresse connue d'avance.
pub fn gate_of(raw: Option<&str>) -> Gate {
    let Some(raw) = raw else {
        return Gate::Off;
    };
    match raw.trim().parse::<u16>() {
        Ok(0) => Gate::Invalid("port 0 is not allowed".into()),
        Ok(port) => Gate::On(port),
        Err(_) => Gate::Invalid(format!("not a port number: {raw:?}")),
    }
}

/// Une session telle que le compagnon la montre : ce que la barre sait, sous forme de texte stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FleetSession {
    pub name: String,
    pub display_name: String,
    pub project: String,
    pub state: &'static str,
    pub seen: bool,
    pub ideation: bool,
}

/// L'instantané de la flotte, tel que `GET /api/fleet` le rend.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Fleet {
    pub sessions: Vec<FleetSession>,
    /// Le même nombre que le badge du Dock.
    pub badge: usize,
}

/// L'instantané partagé entre l'espace de travail (qui l'écrit) et le service (qui le lit sans attendre personne).
pub type SharedFleet = Arc<Mutex<Fleet>>;

/// Le nom JSON d'un état d'activité.
pub fn state_label(state: ActivityState) -> &'static str {
    match state {
        ActivityState::Idle => "idle",
        ActivityState::Running => "running",
        ActivityState::Waiting => "waiting",
        ActivityState::Error => "error",
    }
}

/// Construit l'instantané depuis les sessions de la barre (dans l'ordre de la barre) et leur drapeau « vue ».
pub fn fleet_of<'a>(
    sessions: impl IntoIterator<Item = (&'a SessionSummary, bool)> + Clone,
    ideation_enabled: bool,
) -> Fleet {
    let badge = dock_badge_count(
        sessions.clone().into_iter().map(|(summary, _)| summary),
        ideation_enabled,
    );
    let sessions = sessions
        .into_iter()
        .map(|(summary, seen)| FleetSession {
            name: summary.name.clone(),
            display_name: summary.display_name.clone(),
            project: summary.project.to_string_lossy().into_owned(),
            state: state_label(summary.state),
            seen,
            ideation: summary.ideation,
        })
        .collect();
    Fleet { sessions, badge }
}

/// Ce qui s'est passé quand le fil d'interface a traité une commande de saisie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    Sent,
    RefusedAtPrompt,
    UnknownSession,
    NoPane,
    ReadOnly,
}

impl InputOutcome {
    /// Le nom du résultat, tel que le journal l'écrit.
    pub fn label(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::RefusedAtPrompt => "refused_at_prompt",
            Self::UnknownSession => "unknown_session",
            Self::NoPane => "no_pane",
            Self::ReadOnly => "read_only",
        }
    }
}

/// Une saisie à taper dans le pane actif de `session`, avec le canal de la réponse.
pub struct InputCommand {
    pub session: String,
    pub bytes: Vec<u8>,
    pub reply: oneshot::Sender<InputOutcome>,
}

/// Ce que `start` rend à l'espace de travail : l'instantané à tenir à jour et la file des commandes à traiter sur
/// le fil d'interface.
pub struct Companion {
    pub fleet: SharedFleet,
    pub commands: mpsc::UnboundedReceiver<InputCommand>,
}

/// Lit la porte et, si elle est ouverte, lance le fil du service. `None` : rien ne tourne.
pub fn start() -> Option<Companion> {
    let raw = std::env::var(PORT_VAR).ok();
    let port = match gate_of(raw.as_deref()) {
        Gate::Off => return None,
        Gate::Invalid(reason) => {
            hud::log_event(&format!("companion disabled reason={reason}"));
            return None;
        }
        Gate::On(port) => port,
    };
    let fleet: SharedFleet = Arc::new(Mutex::new(Fleet::default()));
    let (sender, commands) = mpsc::unbounded();
    let state = AppState {
        fleet: Arc::clone(&fleet),
        commands: sender,
    };
    let spawned = std::thread::Builder::new()
        .name("companion".into())
        .spawn(move || serve(port, state));
    if let Err(error) = spawned {
        hud::log_event(&format!("companion disabled reason=thread failed: {error}"));
        return None;
    }
    Some(Companion { fleet, commands })
}

#[derive(Clone)]
struct AppState {
    fleet: SharedFleet,
    commands: mpsc::UnboundedSender<InputCommand>,
}

/// Le corps du fil : un runtime à lui, une prise sur 127.0.0.1 seulement.
fn serve(port: u16, state: AppState) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            hud::log_event(&format!(
                "companion disabled reason=runtime failed: {error}"
            ));
            return;
        }
    };
    runtime.block_on(async move {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = match tokio::net::TcpListener::bind(address).await {
            Ok(listener) => listener,
            Err(error) => {
                hud::log_event(&format!(
                    "companion disabled reason=bind {address}: {error}"
                ));
                return;
            }
        };
        hud::log_event(&format!("companion listening addr={address}"));
        if let Err(error) = axum::serve(listener, router(state)).await {
            hud::log_event(&format!("companion stopped: {error}"));
        }
    });
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/fleet", get(get_fleet))
        .route("/api/sessions/{name}/input", post(post_input))
        .with_state(state)
}

fn json_reply(status: StatusCode, body: String) -> Response {
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn error_reply(status: StatusCode, error: &str) -> Response {
    json_reply(
        status,
        serde_json::json!({ "ok": false, "error": error }).to_string(),
    )
}

/// L'instantané seul : le verrou ne protège qu'une copie, il n'attend jamais le fil d'interface.
async fn get_fleet(State(state): State<AppState>) -> Response {
    let body = {
        let fleet = state.fleet.lock().unwrap_or_else(PoisonError::into_inner);
        serde_json::to_string(&*fleet)
    };
    match body {
        Ok(body) => json_reply(StatusCode::OK, body),
        Err(_) => error_reply(StatusCode::INTERNAL_SERVER_ERROR, "serialization"),
    }
}

async fn post_input(
    State(state): State<AppState>,
    Path(session): Path<String>,
    body: Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
        return error_reply(StatusCode::BAD_REQUEST, "body_too_large");
    }
    let bytes = match input_bytes(&body) {
        Ok(bytes) => bytes,
        Err(reason) => return error_reply(StatusCode::BAD_REQUEST, reason.label()),
    };
    let (reply, answer) = oneshot::channel();
    let command = InputCommand {
        session,
        bytes,
        reply,
    };
    if state.commands.unbounded_send(command).is_err() {
        return error_reply(StatusCode::SERVICE_UNAVAILABLE, "ui_unavailable");
    }
    match tokio::time::timeout(REPLY_TIMEOUT, answer).await {
        Ok(Ok(outcome)) => outcome_reply(outcome),
        // Délai dépassé, ou la file a été lâchée sans réponse.
        Ok(Err(_)) | Err(_) => error_reply(StatusCode::SERVICE_UNAVAILABLE, "ui_timeout"),
    }
}

fn outcome_reply(outcome: InputOutcome) -> Response {
    match outcome {
        InputOutcome::Sent => json_reply(
            StatusCode::OK,
            serde_json::json!({ "ok": true, "outcome": "sent" }).to_string(),
        ),
        InputOutcome::RefusedAtPrompt => error_reply(StatusCode::CONFLICT, "at_prompt"),
        InputOutcome::UnknownSession => error_reply(StatusCode::NOT_FOUND, "unknown_session"),
        InputOutcome::NoPane => error_reply(StatusCode::CONFLICT, "no_pane"),
        InputOutcome::ReadOnly => error_reply(StatusCode::CONFLICT, "read_only"),
    }
}

/// Pourquoi un corps de saisie est refusé (400).
#[derive(Debug, PartialEq, Eq)]
pub enum BadInput {
    NotJson,
    NeitherKeyNorText,
    BothKeyAndText,
    UnknownKey,
    EnterWithKey,
    EmptyText,
    TextTooLong,
}

impl BadInput {
    fn label(&self) -> &'static str {
        match self {
            Self::NotJson => "bad_json",
            Self::NeitherKeyNorText => "key_or_text_required",
            Self::BothKeyAndText => "key_and_text_exclusive",
            Self::UnknownKey => "unknown_key",
            Self::EnterWithKey => "enter_needs_text",
            Self::EmptyText => "empty_text",
            Self::TextTooLong => "text_too_long",
        }
    }
}

/// Le corps d'une saisie : une touche nommée, ou un texte suivi ou non d'Entrée.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputBody {
    key: Option<String>,
    text: Option<String>,
    enter: Option<bool>,
}

/// Les octets d'une touche nommée ; `None` pour un nom hors de la liste.
pub fn key_bytes(key: &str) -> Option<Vec<u8>> {
    let bytes: &[u8] = match key {
        "1" => b"1",
        "2" => b"2",
        "3" => b"3",
        "y" => b"y",
        "n" => b"n",
        "Enter" => b"\r",
        "Escape" => b"\x1b",
        "C-c" => b"\x03",
        _ => return None,
    };
    Some(bytes.to_vec())
}

/// Les octets d'un texte (1 à 1024 octets UTF-8), suivis de `\r` quand `enter`.
pub fn text_bytes(text: &str, enter: bool) -> Result<Vec<u8>, BadInput> {
    if text.is_empty() {
        return Err(BadInput::EmptyText);
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(BadInput::TextTooLong);
    }
    let mut bytes = text.as_bytes().to_vec();
    if enter {
        bytes.push(b'\r');
    }
    Ok(bytes)
}

/// Du corps JSON aux octets à taper : exactement une des deux formes, rien d'autre.
pub fn input_bytes(body: &[u8]) -> Result<Vec<u8>, BadInput> {
    let body: InputBody = serde_json::from_slice(body).map_err(|_| BadInput::NotJson)?;
    match (body.key, body.text) {
        (Some(_), Some(_)) => Err(BadInput::BothKeyAndText),
        (None, None) => Err(BadInput::NeitherKeyNorText),
        (Some(key), None) => {
            if body.enter.is_some() {
                return Err(BadInput::EnterWithKey);
            }
            key_bytes(&key).ok_or(BadInput::UnknownKey)
        }
        (None, Some(text)) => text_bytes(&text, body.enter.unwrap_or(false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn summary(name: &str, state: ActivityState, ideation: bool) -> SessionSummary {
        SessionSummary {
            name: name.into(),
            project: PathBuf::from("/work/p"),
            display_name: format!("Name of {name}"),
            created: 1,
            state,
            ideation,
        }
    }

    #[test]
    fn the_gate_is_closed_without_the_variable_and_reports_an_unusable_value() {
        assert_eq!(gate_of(None), Gate::Off);
        assert_eq!(gate_of(Some("3911")), Gate::On(3911));
        assert_eq!(gate_of(Some(" 3911 ")), Gate::On(3911));
        for raw in ["", "abc", "70000", "-1", "0", "3911x"] {
            assert!(matches!(gate_of(Some(raw)), Gate::Invalid(_)), "{raw:?}");
        }
    }

    #[test]
    fn every_named_key_maps_to_its_bytes() {
        assert_eq!(key_bytes("1"), Some(b"1".to_vec()));
        assert_eq!(key_bytes("2"), Some(b"2".to_vec()));
        assert_eq!(key_bytes("3"), Some(b"3".to_vec()));
        assert_eq!(key_bytes("y"), Some(b"y".to_vec()));
        assert_eq!(key_bytes("n"), Some(b"n".to_vec()));
        assert_eq!(key_bytes("Enter"), Some(vec![0x0d]));
        assert_eq!(key_bytes("Escape"), Some(vec![0x1b]));
        assert_eq!(key_bytes("C-c"), Some(vec![0x03]));
    }

    #[test]
    fn a_key_outside_the_list_is_refused() {
        for key in ["", "Y", "4", "enter", "C-d", "yy", "Tab"] {
            assert_eq!(key_bytes(key), None, "{key:?}");
        }
        assert_eq!(input_bytes(br#"{"key":"Tab"}"#), Err(BadInput::UnknownKey));
    }

    #[test]
    fn a_text_is_typed_as_utf8_and_enter_appends_a_carriage_return() {
        assert_eq!(text_bytes("ok", false), Ok(b"ok".to_vec()));
        assert_eq!(text_bytes("ok", true), Ok(b"ok\r".to_vec()));
        assert_eq!(
            text_bytes("é€", true),
            Ok(vec![0xc3, 0xa9, 0xe2, 0x82, 0xac, 0x0d])
        );
    }

    #[test]
    fn the_text_length_is_counted_in_bytes_between_1_and_1024() {
        assert_eq!(text_bytes("", true), Err(BadInput::EmptyText));
        assert!(text_bytes(&"a".repeat(1024), false).is_ok());
        assert_eq!(
            text_bytes(&"a".repeat(1025), false),
            Err(BadInput::TextTooLong)
        );
        // 513 fois « é » : 513 caractères mais 1026 octets.
        assert_eq!(
            text_bytes(&"é".repeat(513), false),
            Err(BadInput::TextTooLong)
        );
    }

    #[test]
    fn a_body_carries_exactly_one_of_key_and_text() {
        assert_eq!(input_bytes(br#"{"key":"y"}"#), Ok(b"y".to_vec()));
        assert_eq!(
            input_bytes(br#"{"text":"go","enter":true}"#),
            Ok(b"go\r".to_vec())
        );
        assert_eq!(input_bytes(br#"{"text":"go"}"#), Ok(b"go".to_vec()));
        assert_eq!(
            input_bytes(br#"{"key":"y","text":"go"}"#),
            Err(BadInput::BothKeyAndText)
        );
        assert_eq!(input_bytes(b"{}"), Err(BadInput::NeitherKeyNorText));
        assert_eq!(
            input_bytes(br#"{"enter":true}"#),
            Err(BadInput::NeitherKeyNorText)
        );
        assert_eq!(
            input_bytes(br#"{"key":"y","enter":true}"#),
            Err(BadInput::EnterWithKey)
        );
    }

    #[test]
    fn a_malformed_body_is_refused_whatever_its_shape() {
        for body in [
            &b""[..],
            b"not json",
            b"[]",
            b"\"y\"",
            br#"{"key":1}"#,
            br#"{"text":"a","enter":"yes"}"#,
            br#"{"key":"y","extra":1}"#,
        ] {
            assert_eq!(input_bytes(body), Err(BadInput::NotJson), "{body:?}");
        }
    }

    #[test]
    fn the_snapshot_counts_waiting_and_error_sessions_like_the_dock_badge() {
        let sessions = [
            summary("a", ActivityState::Running, false),
            summary("b", ActivityState::Waiting, false),
            summary("c", ActivityState::Error, false),
            summary("d", ActivityState::Waiting, true),
        ];
        let flags = [true, false, false, false];
        let rows = || sessions.iter().zip(flags);
        let with_ideation = fleet_of(rows(), true);
        assert_eq!(with_ideation.badge, 3);
        // Un bouillon masqué ne compte pas dans le badge, mais figure dans la liste.
        let without_ideation = fleet_of(rows(), false);
        assert_eq!(without_ideation.badge, 2);
        assert_eq!(without_ideation.sessions.len(), 4);
        let first = &without_ideation.sessions[0];
        assert_eq!(first.state, "running");
        assert!(first.seen);
        assert_eq!(first.project, "/work/p");
        assert_eq!(first.display_name, "Name of a");
        assert!(without_ideation.sessions[3].ideation);
    }

    #[test]
    fn the_snapshot_serializes_with_the_documented_keys() {
        let sessions = [summary("s", ActivityState::Idle, false)];
        let fleet = fleet_of(sessions.iter().map(|s| (s, false)), true);
        let value = serde_json::to_value(&fleet).unwrap();
        assert_eq!(value["badge"], 0);
        let row = &value["sessions"][0];
        for key in [
            "name",
            "display_name",
            "project",
            "state",
            "seen",
            "ideation",
        ] {
            assert!(row.get(key).is_some(), "{key}");
        }
        assert_eq!(row["state"], "idle");
    }
}
