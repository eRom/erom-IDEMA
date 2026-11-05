//! Serveur HTTP de la sonde : PWA statique + petite API JSON, lie a 127.0.0.1 uniquement.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::push::endpoint_host;
use crate::store::{self, Subscription};

struct AppState {
    dir: PathBuf,
    vapid_public: String,
    /// Serialise les lectures-modifications-ecritures de subscriptions.json.
    write_lock: Mutex<()>,
}

type ApiError = (StatusCode, Json<Value>);

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (
        status,
        Json(json!({ "ok": false, "error": message.into() })),
    )
}

fn user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

pub async fn run(dir: PathBuf, vapid_public: String, port: u16) -> Result<(), String> {
    let state = Arc::new(AppState {
        dir,
        vapid_public,
        write_lock: Mutex::new(()),
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/sw.js", get(service_worker))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon-180.png", get(icon_180))
        .route("/icon-512.png", get(icon_512))
        .route("/api/vapid-public-key", get(vapid_public_key))
        .route("/api/subscribe", post(subscribe))
        .route("/api/session", get(session))
        .route("/api/reply", post(reply))
        .route("/api/probe-log", post(probe_log))
        .layer(middleware::from_fn(log_request))
        .with_state(state);

    // Liaison en loopback seulement : jamais 0.0.0.0.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|err| format!("cannot bind 127.0.0.1:{port}: {err}"))?;
    println!(
        "{} listening on http://127.0.0.1:{port}",
        store::now_rfc3339()
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|err| format!("server error: {err}"))
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).expect("cannot install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    println!("{} shutting down", store::now_rfc3339());
}

/// Une ligne par requete sur stdout.
async fn log_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let response = next.run(request).await;
    println!(
        "{} {} {} -> {}",
        store::now_rfc3339(),
        method,
        path,
        response.status().as_u16()
    );
    response
}

async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../static/index.html"),
    )
}

async fn service_worker() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../static/sw.js"),
    )
}

async fn manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        include_str!("../static/manifest.webmanifest"),
    )
}

async fn icon_180() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/png")],
        include_bytes!("../static/icon-180.png").as_slice(),
    )
}

async fn icon_512() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/png")],
        include_bytes!("../static/icon-512.png").as_slice(),
    )
}

async fn vapid_public_key(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({ "key": state.vapid_public }))
}

async fn subscribe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(mut sub): Json<Subscription>,
) -> Result<Json<Value>, ApiError> {
    store::validate_subscription(&sub).map_err(|err| api_error(StatusCode::BAD_REQUEST, err))?;
    sub.added_at = store::now_rfc3339();
    sub.user_agent = user_agent(&headers);
    let host = endpoint_host(&sub.endpoint);
    let total = {
        let _guard = state.write_lock.lock().expect("write lock poisoned");
        store::upsert_subscription(&state.dir, sub.clone())
            .map_err(|err| api_error(StatusCode::INTERNAL_SERVER_ERROR, err))?
    };
    store::append_log(
        &state.dir,
        "subscribe",
        json!({ "endpoint_host": host, "total_subscriptions": total }),
        sub.user_agent.as_deref(),
    );
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SessionQuery {
    id: String,
}

async fn session(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<Value>, ApiError> {
    let pushes = store::load_last_pushes(&state.dir)
        .map_err(|err| api_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    pushes
        .get(&query.id)
        .cloned()
        .map(Json)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "no push recorded for this session"))
}

#[derive(Deserialize)]
struct ReplyRequest {
    session: String,
    keys: String,
    source: String,
}

async fn reply(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ReplyRequest>,
) -> Result<Json<Value>, ApiError> {
    if body.source != "page" && body.source != "notification-action" {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "source must be \"page\" or \"notification-action\"",
        ));
    }
    if body.keys.is_empty() || body.keys.len() > 1024 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "keys must be 1 to 1024 bytes",
        ));
    }
    let received_at = store::now_rfc3339();
    // La sonde ne transmet rien a tmux : elle journalise seulement la reponse.
    store::append_log(
        &state.dir,
        "reply",
        json!({ "session": body.session, "keys": body.keys, "source": body.source }),
        user_agent(&headers).as_deref(),
    );
    Ok(Json(json!({ "ok": true, "received_at": received_at })))
}

#[derive(Deserialize)]
struct ProbeLogRequest {
    event: String,
    #[serde(default)]
    detail: Value,
}

async fn probe_log(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ProbeLogRequest>,
) -> Result<Json<Value>, ApiError> {
    if body.event.is_empty() || body.event.len() > 128 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "event must be 1 to 128 bytes",
        ));
    }
    store::append_log(
        &state.dir,
        &body.event,
        body.detail,
        user_agent(&headers).as_deref(),
    );
    Ok(Json(json!({ "ok": true })))
}
