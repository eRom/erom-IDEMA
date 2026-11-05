//! Les routes HTTP du service, toutes sous `/idema-api/`.

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::StreamExt;
use tokio::sync::watch;

/// L'intervalle du commentaire de maintien du flux : il garde la connexion ouverte à travers les relais.
const KEEP_ALIVE: Duration = Duration::from_secs(20);

/// Ce que les routes partagent.
#[derive(Clone)]
pub struct AppState {
    pub version: &'static str,
    /// Passe à `true` quand le service doit s'arrêter : les flux se ferment alors d'eux-mêmes.
    pub stop: watch::Receiver<bool>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/idema-api/health", get(health))
        .route("/idema-api/events", get(events))
        .with_state(state)
}

/// Résout quand le signal d'arrêt est posé, ou que son émetteur a disparu.
pub async fn stopped(mut stop: watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

fn json_reply(status: StatusCode, body: String) -> Response {
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

async fn health(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    json_reply(
        StatusCode::OK,
        serde_json::json!({ "ok": true, "version": state.version }).to_string(),
    )
}

/// Le flux d'événements, provisoire : il n'émet que le commentaire de maintien et se ferme à l'arrêt du service.
async fn events(axum::extract::State(state): axum::extract::State<AppState>) -> impl IntoResponse {
    let stream = futures::stream::pending::<Result<Event, Infallible>>()
        .take_until(stopped(state.stop.clone()));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(KEEP_ALIVE).text(""))
}
