//! Les routes HTTP du service. Toutes les routes de données sont sous `/idema-api/` et passent par la garde ; les
//! fichiers de la page sont servis à la racine, sans cookie et sans donnée.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::PathRejection;
use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use idema_tmux::server::TmuxServer;
use serde::Deserialize;
use tokio::sync::{oneshot, watch};

use super::access::{Access, Caller};
use super::auth::{COOKIE_NAME, cookie_header, origin_allowed};
use super::devices::Subscription;
use super::fleet::Fleet;
use super::input::{self, HTTP_WAIT};
use super::notify::Message;
use super::push_send::{Delivery, PushSender, endpoint_allowed};
use super::scratchpad::{self, AppendError};
use super::screen;
use super::sse::{self, COALESCE};
use super::timeline::Timeline;
use super::web_assets;
use super::webpush::UaKeys;
use super::{AckOrigin, InputOutcome, PaneCommand, PaneProbe, ServiceEvent};
use crate::hud;

/// L'intervalle du commentaire de maintien du flux : il garde la connexion ouverte à travers les relais.
const KEEP_ALIVE: Duration = Duration::from_secs(20);

/// Un corps de requête dépasse-t-il cette taille, il est refusé (413) : aucune route n'attend davantage.
const BODY_LIMIT: usize = 64 * 1024;

const API_PREFIX: &str = "/idema-api/";
const HEALTH_PATH: &str = "/idema-api/health";
const PAIR_PATH: &str = "/idema-api/pair";
const TEST_PAIRING_CODE_PATH: &str = "/idema-api/test/pairing-code";
const TEST_FORGET_PATH: &str = "/idema-api/test/forget";

/// La politique de contenu de toutes les réponses (spec §3.3) : rien d'extérieur, aucun script en ligne.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

/// Les mesures que la page peut déclarer, et la borne de leur durée (10 minutes).
const METRIC_NAMES: [&str; 3] = ["shell_ready", "fleet_ready", "resume_ready"];
const METRIC_MAX_MS: u64 = 600_000;

/// Ce que les routes partagent.
#[derive(Clone)]
pub struct AppState {
    pub version: &'static str,
    /// Passe à `true` quand le service doit s'arrêter : les flux se ferment alors d'eux-mêmes.
    pub stop: watch::Receiver<bool>,
    pub access: Arc<Access>,
    /// Les adresses tailnet connues (le `public_url` de la config et l'adresse détectée), dont les origines sont
    /// admises en écriture. Suit la config et la détection à chaud.
    pub known_origins: Arc<Mutex<Vec<String>>>,
    pub port: u16,
    /// Les routes de test existent-elles ? Décidé une fois par le contrôleur (`auth::test_routes_enabled`).
    pub test_routes: bool,
    /// Le dernier instantané de la flotte : `fleet` le lit, chaque flux `events` s'y abonne.
    pub fleet: watch::Receiver<Arc<Fleet>>,
    pub timeline: Arc<Mutex<Timeline>>,
    /// Le serveur tmux d'IdemA, pour capturer l'écran d'un pane.
    pub tmux: Arc<TmuxServer>,
    /// La file des commandes sur un pane, traitées par l'espace de travail sur le fil d'interface.
    pub commands: UnboundedSender<PaneCommand>,
    /// L'envoi des notifications. `None` : la clé VAPID est illisible, les routes push répondent 503.
    pub push: Option<Arc<PushSender>>,
    /// Un envoi de test est-il en cours ? Un seul à la fois : chacun tient un fil bloquant jusqu'à 10 s.
    pub push_test_busy: Arc<AtomicBool>,
    /// La file de la boucle des notifications : `PUT presence` y dépose le pane que la page montre.
    pub inbox: tokio::sync::mpsc::UnboundedSender<Message>,
    /// La note `scratchpad.md` du dossier d'IdemA.
    pub scratchpad: PathBuf,
}

pub fn router(state: AppState) -> Router {
    let mut router = Router::new()
        .route(HEALTH_PATH, get(health))
        .route(PAIR_PATH, post(pair))
        .route("/idema-api/me", get(me))
        .route("/idema-api/fleet", get(fleet))
        .route("/idema-api/events", get(events))
        .route("/idema-api/timeline", get(timeline))
        .route(
            "/idema-api/sessions/{name}/panes/{pane}/screen",
            get(screen),
        )
        .route("/idema-api/sessions/{name}/panes/{pane}/input", post(input))
        .route("/idema-api/sessions/{name}/panes/{pane}/ack", post(ack))
        .route("/idema-api/presence", put(presence))
        .route("/idema-api/scratchpad", get(scratchpad_read))
        .route("/idema-api/scratchpad/append", post(scratchpad_append))
        .route("/idema-api/client-metrics", post(client_metrics))
        .route("/idema-api/push/key", get(push_key))
        .route(
            "/idema-api/push/subscription",
            put(push_subscribe).delete(push_unsubscribe),
        )
        .route("/idema-api/push/test", post(push_test));
    if state.test_routes {
        router = router
            .route(TEST_PAIRING_CODE_PATH, post(test_pairing_code))
            .route(TEST_FORGET_PATH, post(test_forget));
    }
    router
        .fallback(app_files)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .layer(middleware::from_fn(security_headers))
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

fn error_reply(status: StatusCode, error: &str) -> Response {
    json_reply(status, serde_json::json!({ "error": error }).to_string())
}

/// La CSP et `nosniff` sur toute réponse, y compris les refus de la garde et les 404.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// La garde de `/idema-api/*`. Dans l'ordre : l'origine d'une écriture (403), le cookie (401, sauf `health`, `pair` et,
/// quand elles existent, les deux routes de test), le type du corps d'une écriture (415). Les fichiers de la page ne
/// passent pas par elle.
async fn guard(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if !path.starts_with(API_PREFIX) {
        return next.run(request).await;
    }
    // Les deux chemins exacts des routes de test, et seulement quand elles sont allumées : tout autre chemin sous
    // `/idema-api/test/` demande le cookie comme le reste de l'API.
    let exempt = path == HEALTH_PATH
        || path == PAIR_PATH
        || (state.test_routes && (path == TEST_PAIRING_CODE_PATH || path == TEST_FORGET_PATH));
    let write = !matches!(*request.method(), Method::GET | Method::HEAD);

    if write && let Some(origin) = request.headers().get(header::ORIGIN) {
        let origin = origin.to_str().ok();
        let known_origins = state
            .known_origins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let known: Vec<&str> = known_origins.iter().map(String::as_str).collect();
        if origin.is_none() || !origin_allowed(origin, &known, state.port) {
            return error_reply(StatusCode::FORBIDDEN, "forbidden_origin");
        }
    }
    if !exempt {
        let authenticated =
            cookie_token(request.headers()).and_then(|token| state.access.authenticate(&token));
        let Some((caller, touch_due)) = authenticated else {
            return error_reply(StatusCode::UNAUTHORIZED, "unpaired");
        };
        if touch_due {
            let access = Arc::clone(&state.access);
            let id = caller.id.clone();
            // La dernière activité s'écrit sur disque : hors du runtime. Un échec est journalisé par `touch`.
            let _ = tokio::task::spawn_blocking(move || access.touch(&id)).await;
        }
        request.extensions_mut().insert(caller);
    }
    if write && !is_json(request.headers()) {
        return error_reply(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");
    }
    next.run(request).await
}

/// Le jeton du cookie `idema_companion`, parmi les en-têtes `Cookie` de la requête.
fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|line| line.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == COOKIE_NAME).then(|| value.trim().to_owned())
        })
}

/// Le corps annonce-t-il `application/json` (avec ou sans paramètres) ?
fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

async fn health(State(state): State<AppState>) -> Response {
    json_reply(
        StatusCode::OK,
        serde_json::json!({ "ok": true, "version": state.version, "assets": web_assets::bundle_id() })
            .to_string(),
    )
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
}

/// `POST pair` : le code contre un cookie d'appareil. Tout refus rend 403, avec sa raison dans `error`.
async fn pair(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Ok(PairBody { code }) = serde_json::from_slice(&body) else {
        return error_reply(StatusCode::BAD_REQUEST, "bad_request");
    };
    let agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let access = Arc::clone(&state.access);
    let outcome = tokio::task::spawn_blocking(move || access.pair(&code, agent.as_deref())).await;
    let paired = match outcome {
        Ok(Ok(paired)) => paired,
        Ok(Err(refusal)) => {
            let status = match refusal {
                super::access::PairRefusal::Store => StatusCode::INTERNAL_SERVER_ERROR,
                _ => StatusCode::FORBIDDEN,
            };
            return error_reply(status, refusal.error_code());
        }
        Err(_) => return error_reply(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    };
    let Ok(cookie) = HeaderValue::from_str(&cookie_header(&paired.token)) else {
        return error_reply(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    };
    let mut response = json_reply(
        StatusCode::OK,
        serde_json::json!({ "device": { "id": paired.caller.id, "name": paired.caller.name } })
            .to_string(),
    );
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    response
}

/// `GET me` : l'appareil du cookie, et s'il a un abonnement.
async fn me(Extension(caller): Extension<Caller>) -> Response {
    json_reply(
        StatusCode::OK,
        serde_json::json!({
            "device": { "id": caller.id, "name": caller.name },
            "subscribed": caller.subscribed,
        })
        .to_string(),
    )
}

/// `GET fleet` : le dernier instantané publié.
async fn fleet(State(state): State<AppState>) -> Response {
    let latest = Arc::clone(&state.fleet.borrow());
    match serde_json::to_string(&*latest) {
        Ok(json) => json_reply(StatusCode::OK, json),
        Err(_) => error_reply(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// `GET events` : le flux SSE de la flotte. Un événement `fleet` à chaque nouvel instantané (regroupé sur 250 ms), un
/// commentaire de maintien toutes les 20 s, et la fin du flux au signal d'arrêt du service.
async fn events(State(state): State<AppState>) -> impl IntoResponse {
    let stream =
        sse::fleet_events(state.fleet.clone(), COALESCE).take_until(stopped(state.stop.clone()));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(KEEP_ALIVE).text("keepalive"))
}

#[derive(Deserialize)]
struct TimelineQuery {
    /// Les transitions dont l'instant est au moins celui-ci, en millisecondes ; absent : toutes.
    since: Option<u64>,
}

/// `GET timeline?since=<ms>` : les transitions qui comptent, et l'instant de démarrage d'IdemA.
async fn timeline(
    State(state): State<AppState>,
    query: Result<Query<TimelineQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(TimelineQuery { since })) = query else {
        return error_reply(StatusCode::BAD_REQUEST, "bad_request");
    };
    let body = {
        let timeline = state
            .timeline
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        serde_json::json!({
            "started_at": timeline.started_at(),
            "events": timeline.since(since.unwrap_or(0)),
        })
    };
    json_reply(StatusCode::OK, body.to_string())
}

/// Ce qui peut empêcher d'obtenir la réponse du fil d'interface.
enum Unanswered {
    /// L'espace de travail n'écoute plus (compagnon retiré, IdemA qui quitte).
    Gone,
    /// Pas de réponse en `HTTP_WAIT`.
    TimedOut,
}

/// Envoie une commande au fil d'interface et attend sa réponse au plus `HTTP_WAIT`. L'attente est asynchrone : le
/// runtime du service ne bloque jamais sur le fil d'interface.
async fn ask<T>(
    commands: &UnboundedSender<PaneCommand>,
    build: impl FnOnce(oneshot::Sender<T>) -> PaneCommand,
) -> Result<T, Unanswered> {
    let (reply, answer) = oneshot::channel();
    commands
        .unbounded_send(build(reply))
        .map_err(|_| Unanswered::Gone)?;
    match tokio::time::timeout(HTTP_WAIT, answer).await {
        Ok(Ok(answer)) => Ok(answer),
        Ok(Err(_)) => Err(Unanswered::Gone),
        Err(_) => Err(Unanswered::TimedOut),
    }
}

fn unanswered_reply(why: Unanswered) -> Response {
    match why {
        Unanswered::Gone => error_reply(StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        // Garanti : le fil d'interface jette toute commande prise plus de 1,5 s après sa réception, et ne tape plus une
        // Entrée différée au-delà de 2,2 s (`input::enter_still_allowed`), donc aucune touche ne part après cette
        // réponse. Pas garanti : un texte ou une touche pris avant le délai ont pu être tapés (sans leur Entrée) quand
        // le fil d'interface a calé ensuite.
        Unanswered::TimedOut => error_reply(StatusCode::SERVICE_UNAVAILABLE, "expired"),
    }
}

/// `{name}` et `{pane}` d'une route de pane. Un identifiant de pane illisible est un pane inconnu (404 chez l'appelant).
fn pane_target(path: Result<Path<(String, String)>, PathRejection>) -> Option<(String, u32)> {
    let Ok(Path((session, pane))) = path else {
        return None;
    };
    Some((session, pane.parse().ok()?))
}

fn pane_not_found() -> Response {
    error_reply(StatusCode::NOT_FOUND, "not_found")
}

#[derive(Deserialize)]
struct ScreenQuery {
    /// Les lignes d'historique au-dessus de l'écran, 1 à 2 000 (200 par défaut).
    history: Option<u16>,
}

const DEFAULT_HISTORY: u16 = 200;
const MAX_HISTORY: u16 = 2000;

/// Le client accepte-t-il gzip ? Un jeton `gzip` explicite décide seul (donc `gzip;q=0` l'emporte sur `*`, RFC 9110) ;
/// à défaut, `*` ; dans les deux cas, un poids différent de zéro.
fn accepts_gzip(headers: &HeaderMap) -> bool {
    let mut explicit: Option<bool> = None;
    let mut star: Option<bool> = None;
    for token in headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|line| line.split(','))
    {
        let mut parts = token.split(';');
        let coding = parts.next().unwrap_or("").trim();
        let weight_zero = parts.any(|param| {
            param
                .trim()
                .strip_prefix("q=")
                .is_some_and(|q| q.trim().parse::<f32>().is_ok_and(|q| q <= 0.0))
        });
        if coding.eq_ignore_ascii_case("gzip") {
            explicit = Some(explicit.unwrap_or(false) || !weight_zero);
        } else if coding == "*" {
            star = Some(star.unwrap_or(false) || !weight_zero);
        }
    }
    explicit.or(star).unwrap_or(false)
}

/// L'écran capturé, prêt à servir : 304 ou un corps JSON (compressé ou non).
enum Rendered {
    NotModified,
    Body { json: Vec<u8>, gzip: bool },
}

/// Capture le pane et prépare la réponse. Bloquant : capture tmux, JSON et compression sont ici, sur `spawn_blocking`.
fn render_screen(
    tmux: &TmuxServer,
    pane: u32,
    history: u16,
    probe: PaneProbe,
    if_none_match: Option<&str>,
    gzip: bool,
) -> Result<(Rendered, String, u64), String> {
    let screen = screen::capture(tmux, pane, history)?;
    // L'état et la prise en compte de la saisie figurent dans la réponse : ils changent l'`ETag` comme le texte.
    let etag = format!(
        "\"{}-{}-{}\"",
        screen.etag,
        u8::from(probe.accepts_input),
        probe.state
    );
    if if_none_match.is_some_and(|sent| etag_matches(sent, &etag)) {
        return Ok((Rendered::NotModified, etag, screen.took_ms));
    }
    let json = serde_json::to_vec(&serde_json::json!({
        "text": screen.text,
        "cols": screen.cols,
        "rows": screen.rows,
        "accepts_input": probe.accepts_input,
        "state": probe.state,
        "took_ms": screen.took_ms,
    }))
    .map_err(|error| error.to_string())?;
    let rendered = if gzip {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder
            .write_all(&json)
            .and_then(|()| encoder.finish())
            .map(|json| Rendered::Body { json, gzip: true })
            .map_err(|error| error.to_string())?
    } else {
        Rendered::Body { json, gzip: false }
    };
    Ok((rendered, etag, screen.took_ms))
}

/// `GET sessions/{name}/panes/{pane}/screen?history=<1..2000>` : l'écran du pane. La session et le pane sont vérifiés
/// par l'espace de travail (`Probe`) avant toute capture : un identifiant de pane tmux qui n'est pas celui de cette
/// session n'est jamais lu.
async fn screen(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
    query: Result<Query<ScreenQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let Some((session, pane)) = pane_target(path) else {
        return pane_not_found();
    };
    let Ok(Query(ScreenQuery { history })) = query else {
        return error_reply(StatusCode::BAD_REQUEST, "bad_request");
    };
    let history = history.unwrap_or(DEFAULT_HISTORY);
    if !(1..=MAX_HISTORY).contains(&history) {
        return error_reply(StatusCode::BAD_REQUEST, "bad_request");
    }
    let probe = ask(&state.commands, |reply| PaneCommand::Probe {
        session: session.clone(),
        pane,
        reply,
    })
    .await;
    let probe = match probe {
        Ok(Some(probe)) => probe,
        Ok(None) => return error_reply(StatusCode::NOT_FOUND, "not_found"),
        Err(why) => return unanswered_reply(why),
    };
    let tmux = Arc::clone(&state.tmux);
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let gzip = accepts_gzip(&headers);
    let work = tokio::task::spawn_blocking(move || {
        render_screen(&tmux, pane, history, probe, if_none_match.as_deref(), gzip)
    });
    let rendered = match tokio::time::timeout(HTTP_WAIT, work).await {
        Ok(Ok(Ok(rendered))) => rendered,
        // tmux n'a pas pu capturer un pane que l'espace de travail vient de nommer : il a disparu entre-temps.
        Ok(Ok(Err(_))) => return error_reply(StatusCode::NOT_FOUND, "not_found"),
        Ok(Err(_)) => return error_reply(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        Err(_) => return error_reply(StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    };
    let (rendered, etag, took_ms) = rendered;
    hud::log_event(&format!(
        "companion screen session={} pane={pane} took_ms={took_ms}",
        super::log_safe(&session)
    ));
    let Ok(etag) = HeaderValue::from_str(&etag) else {
        return error_reply(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    };
    let mut response = match rendered {
        Rendered::NotModified => StatusCode::NOT_MODIFIED.into_response(),
        Rendered::Body { json, gzip } => {
            let mut response = (StatusCode::OK, json).into_response();
            let out = response.headers_mut();
            out.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            if gzip {
                out.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            }
            response
        }
    };
    let out = response.headers_mut();
    out.insert(header::ETAG, etag);
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    out.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    response
}

/// `POST sessions/{name}/panes/{pane}/input` : taper une touche ou un texte. L'instant de réception est pris avant
/// tout : le fil d'interface jette la commande si elle lui arrive plus de 1,5 s plus tard.
async fn input(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
    body: Bytes,
) -> Response {
    let received_at = Instant::now();
    let Some((session, pane)) = pane_target(path) else {
        return pane_not_found();
    };
    let (typed, command_id) = match input::parse_input(&body) {
        Ok(parsed) => parsed,
        Err(bad) => return error_reply(StatusCode::BAD_REQUEST, bad.error_code()),
    };
    let outcome = ask(&state.commands, |reply| PaneCommand::Input {
        session,
        pane,
        input: typed,
        command_id,
        received_at,
        reply,
    })
    .await;
    match outcome {
        Ok(outcome) => input_reply(outcome),
        Err(why) => unanswered_reply(why),
    }
}

fn input_reply(outcome: InputOutcome) -> Response {
    let status = match outcome {
        InputOutcome::Sent | InputOutcome::SentWithoutEnter | InputOutcome::Duplicate => {
            return json_reply(
                StatusCode::OK,
                serde_json::json!({ "ok": true, "outcome": outcome.as_str() }).to_string(),
            );
        }
        InputOutcome::NoProgram | InputOutcome::ReadOnly => StatusCode::CONFLICT,
        InputOutcome::Disconnected | InputOutcome::Expired => StatusCode::SERVICE_UNAVAILABLE,
        InputOutcome::NoPane | InputOutcome::UnknownSession => StatusCode::NOT_FOUND,
    };
    error_reply(status, outcome.as_str())
}

/// `POST sessions/{name}/panes/{pane}/ack` : acquitter ce pane seul. Idempotent.
async fn ack(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let Some((session, pane)) = pane_target(path) else {
        return pane_not_found();
    };
    let known = ask(&state.commands, |reply| PaneCommand::Ack {
        session,
        pane,
        origin: AckOrigin::Phone,
        reply,
    })
    .await;
    match known {
        Ok(true) => json_reply(
            StatusCode::OK,
            serde_json::json!({ "ok": true }).to_string(),
        ),
        Ok(false) => error_reply(StatusCode::NOT_FOUND, "not_found"),
        Err(why) => unanswered_reply(why),
    }
}

/// Le plus long nom de session admis dans une déclaration : un nom tmux d'IdemA en fait 15.
const PRESENCE_SESSION_MAX_LEN: usize = 200;

#[derive(Deserialize)]
struct PresenceBody {
    session: String,
    pane: u32,
}

/// `PUT presence` : le pane que la page montre au premier plan (`{"session": ..., "pane": ...}`), ou `null` quand elle
/// n'en montre plus. Tant que la déclaration tient (30 s), une attente ou une erreur de ce pane est acquittée aussitôt
/// et n'est pas notifiée (spec §3.6). Elle n'a pas à nommer une session qui existe : elle ne fait que se comparer aux
/// épisodes.
async fn presence(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    body: Bytes,
) -> Response {
    let declared = match serde_json::from_slice::<Option<PresenceBody>>(&body) {
        Ok(Some(PresenceBody { session, pane }))
            if !session.is_empty() && session.len() <= PRESENCE_SESSION_MAX_LEN =>
        {
            Some((session, pane))
        }
        Ok(None) => None,
        _ => return error_reply(StatusCode::BAD_REQUEST, "bad_request"),
    };
    // La boucle ne se ferme qu'avec le service : un envoi qui échoue n'a plus personne à servir.
    let _ = state.inbox.send(Message::Watch {
        device: caller.id,
        declared,
    });
    json_reply(
        StatusCode::OK,
        serde_json::json!({ "ok": true }).to_string(),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MetricBody {
    name: String,
    ms: u64,
}

/// Une mesure valide : un nom connu et une durée entière de 0 à 10 minutes. Tout le reste est refusé.
fn parse_metric(body: &[u8]) -> Option<(&'static str, u64)> {
    let MetricBody { name, ms } = serde_json::from_slice(body).ok()?;
    let name = METRIC_NAMES.into_iter().find(|known| *known == name)?;
    (ms <= METRIC_MAX_MS).then_some((name, ms))
}

/// `POST client-metrics` : une mesure de la page, écrite au journal local (aucune télémétrie).
async fn client_metrics(body: Bytes) -> Response {
    let Some((name, ms)) = parse_metric(&body) else {
        return error_reply(StatusCode::BAD_REQUEST, "bad_request");
    };
    hud::log_event(&format!("companion client metric name={name} ms={ms}"));
    StatusCode::NO_CONTENT.into_response()
}

/// `GET scratchpad` : la note en texte, avec sa date de modification (millisecondes Unix, `null` tant qu'elle n'existe
/// pas). Lecture sur un fil bloquant : la note peut être un lien vers un disque lent.
async fn scratchpad_read(State(state): State<AppState>) -> Response {
    let path = state.scratchpad.clone();
    let read = tokio::task::spawn_blocking(move || {
        let modified_at = scratchpad::modified_millis(&path);
        scratchpad::read(&path).map(|text| (text, modified_at))
    })
    .await;
    match read {
        Ok(Ok((text, modified_at))) => json_reply(
            StatusCode::OK,
            serde_json::json!({ "text": text, "modified_at": modified_at }).to_string(),
        ),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::FileTooLarge => {
            error_reply(StatusCode::PAYLOAD_TOO_LARGE, "scratchpad_too_large")
        }
        _ => error_reply(StatusCode::INTERNAL_SERVER_ERROR, "scratchpad_unreadable"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppendBody {
    line: String,
}

/// `POST scratchpad/append` : ajoute `{"line": ...}` à la fin de la note (1 à 2 000 octets, sans caractère de
/// contrôle). Le texte de la ligne n'est jamais journalisé.
async fn scratchpad_append(State(state): State<AppState>, body: Bytes) -> Response {
    let Ok(AppendBody { line }) = serde_json::from_slice(&body) else {
        return error_reply(StatusCode::BAD_REQUEST, "bad_request");
    };
    let path = state.scratchpad.clone();
    let appended = tokio::task::spawn_blocking(move || scratchpad::append_line(&path, &line)).await;
    match appended {
        Ok(Ok(())) => json_reply(
            StatusCode::OK,
            serde_json::json!({ "ok": true }).to_string(),
        ),
        // Le journal du §3.12 n'a pas de ligne pour la note : un échec disque se dit par son code, sans la ligne.
        Ok(Err(failure @ AppendError::Io(_))) => {
            error_reply(StatusCode::INTERNAL_SERVER_ERROR, failure.error_code())
        }
        Ok(Err(refused)) => error_reply(StatusCode::BAD_REQUEST, refused.error_code()),
        Err(_) => error_reply(StatusCode::INTERNAL_SERVER_ERROR, "scratchpad_unwritable"),
    }
}

/// La notification de test (spec §3.7) : un titre et un corps fixes, pour l'appareil qui la demande.
const TEST_TITLE: &str = "Notification de test";
const TEST_BODY: &str = "IdemA peut te joindre.";

fn push_unavailable() -> Response {
    error_reply(StatusCode::SERVICE_UNAVAILABLE, "push_unavailable")
}

/// `GET push/key` : la clé publique VAPID, que la page compare à celle de son abonnement.
async fn push_key(State(state): State<AppState>) -> Response {
    let Some(push) = state.push.as_ref() else {
        return push_unavailable();
    };
    json_reply(
        StatusCode::OK,
        serde_json::json!({ "key": push.public_key() }).to_string(),
    )
}

/// Le corps de `PUT push/subscription` : ce que `PushSubscription.toJSON()` donne (le reste est ignoré).
#[derive(Deserialize)]
struct SubscriptionBody {
    endpoint: String,
    keys: SubscriptionKeys,
}

#[derive(Deserialize)]
struct SubscriptionKeys {
    p256dh: String,
    auth: String,
}

/// `PUT push/subscription` : l'abonnement de l'appareil qui appelle. Un même `endpoint` n'est gardé qu'une fois.
async fn push_subscribe(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    body: Bytes,
) -> Response {
    if state.push.is_none() {
        return push_unavailable();
    }
    let Ok(SubscriptionBody { endpoint, keys }) = serde_json::from_slice(&body) else {
        return error_reply(StatusCode::BAD_REQUEST, "bad_request");
    };
    if !endpoint_allowed(&endpoint) {
        return error_reply(StatusCode::BAD_REQUEST, "endpoint_not_allowed");
    }
    let subscription = Subscription {
        endpoint,
        p256dh: keys.p256dh,
        auth: keys.auth,
    };
    // Les clés du navigateur sont contrôlées ici : un abonnement qu'on ne saurait pas chiffrer ne servirait à rien.
    if UaKeys::from_subscription(&subscription).is_err() {
        return error_reply(StatusCode::BAD_REQUEST, "bad_subscription");
    }
    let access = Arc::clone(&state.access);
    let written =
        tokio::task::spawn_blocking(move || access.set_subscription(&caller.id, subscription))
            .await;
    match written {
        Ok(Ok(())) => {
            // Un épisode retenu faute d'appareil part sans attendre le prochain événement.
            let _ = state.inbox.send(Message::Wake);
            json_reply(
                StatusCode::OK,
                serde_json::json!({ "ok": true }).to_string(),
            )
        }
        // L'appareil a été oublié entre la garde et l'écriture.
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            error_reply(StatusCode::UNAUTHORIZED, "unpaired")
        }
        _ => error_reply(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// `DELETE push/subscription` : retire l'abonnement de l'appareil qui appelle, sans toucher aux autres.
async fn push_unsubscribe(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> Response {
    let access = Arc::clone(&state.access);
    let removed = tokio::task::spawn_blocking(move || access.clear_subscription(&caller.id)).await;
    match removed {
        Ok(Ok(removed)) => {
            if removed {
                hud::log_event("companion subscription removed reason=unsubscribed");
            }
            json_reply(
                StatusCode::OK,
                serde_json::json!({ "ok": true }).to_string(),
            )
        }
        _ => error_reply(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// La place de l'unique envoi de test en cours : prise avant l'envoi, rendue à sa chute (même si l'appelant a raccroché
/// et que le fil bloquant finit seul).
struct TestSendSlot(Arc<AtomicBool>);

impl TestSendSlot {
    fn take(busy: &Arc<AtomicBool>) -> Option<Self> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then(|| Self(Arc::clone(busy)))
    }
}

impl Drop for TestSendSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// `POST push/test` : envoie « Notification de test » à l'appareil qui demande, et à lui seul. La réponse dit ce que
/// le service push a répondu ; un abonnement qu'il déclare mort est retiré, comme à un vrai envoi.
async fn push_test(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> Response {
    let Some(push) = state.push.clone() else {
        return push_unavailable();
    };
    let Some(subscription) = state.access.subscription_of(&caller.id) else {
        return error_reply(StatusCode::CONFLICT, "not_subscribed");
    };
    let Some(slot) = TestSendSlot::take(&state.push_test_busy) else {
        return error_reply(StatusCode::TOO_MANY_REQUESTS, "test_in_progress");
    };
    let access = Arc::clone(&state.access);
    let sent = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        let payload = serde_json::json!({
            "title": TEST_TITLE,
            "body": TEST_BODY,
            "type": "test",
            "url": "/",
        })
        .to_string();
        let (delivery, status) = push.deliver(&subscription, "test", payload.as_bytes());
        let status_text = status.map_or_else(|| "none".to_owned(), |code| code.to_string());
        if delivery == Delivery::Sent {
            hud::log_event("companion push sent kind=test");
        } else {
            hud::log_event(&format!(
                "companion push failed kind=test reason={} status={status_text}",
                delivery.as_str()
            ));
        }
        if delivery == Delivery::Gone {
            // L'abonnement est mort, n'est plus le nôtre, ou n'a jamais été envoyable (sans code HTTP : hôte hors
            // liste ou clés abîmées dans `devices.json`) : le retirer, la page se réabonnera.
            let reason = status.map_or_else(|| "invalid".to_owned(), |code| format!("http_{code}"));
            match access.remove_endpoint(&subscription.endpoint) {
                Ok(true) => {
                    hud::log_event(&format!("companion subscription removed reason={reason}"))
                }
                Ok(false) => {}
                Err(_) => {} // `remove_endpoint` a déjà écrit l'échec au journal
            }
        }
        delivery
    })
    .await;
    match sent {
        Ok(Delivery::Sent) => json_reply(
            StatusCode::OK,
            serde_json::json!({ "result": "sent" }).to_string(),
        ),
        Ok(other) => json_reply(
            StatusCode::BAD_GATEWAY,
            serde_json::json!({ "error": format!("push_{}", other.as_str()), "result": other.as_str() })
                .to_string(),
        ),
        Err(_) => error_reply(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// Les routes de test ne servent que des requêtes locales : tout ce qui porte un en-tête de relais du tailnet est refusé.
fn refuse_relayed(headers: &HeaderMap) -> Option<Response> {
    (headers.contains_key("tailscale-user-login") || headers.contains_key("x-forwarded-for"))
        .then(|| error_reply(StatusCode::FORBIDDEN, "forbidden"))
}

/// `POST test/pairing-code` : émet un code d'appairage sans passer par Personnaliser.
async fn test_pairing_code(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(refusal) = refuse_relayed(&headers) {
        return refusal;
    }
    let access = Arc::clone(&state.access);
    let Ok(display) = tokio::task::spawn_blocking(move || access.start_pairing()).await else {
        return error_reply(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    };
    let expires_at = display
        .expires_at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    json_reply(
        StatusCode::OK,
        serde_json::json!({ "code": display.code, "expires_at": expires_at }).to_string(),
    )
}

#[derive(Deserialize)]
struct ForgetBody {
    id: String,
}

/// `POST test/forget` : oublie un appareil par le même événement que Personnaliser.
async fn test_forget(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(refusal) = refuse_relayed(&headers) {
        return refusal;
    }
    let Ok(ForgetBody { id }) = serde_json::from_slice(&body) else {
        return error_reply(StatusCode::BAD_REQUEST, "bad_request");
    };
    let access = Arc::clone(&state.access);
    let done = tokio::task::spawn_blocking(move || {
        access.handle(ServiceEvent::ForgetDevice { id });
    })
    .await;
    match done {
        Ok(()) => json_reply(
            StatusCode::OK,
            serde_json::json!({ "ok": true }).to_string(),
        ),
        Err(_) => error_reply(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// Les fichiers de la page, avec `ETag` et 304 ; tout autre chemin est un 404 (JSON sous `/idema-api/`). Un client qui
/// accepte gzip reçoit la version compressée à l'avance (son propre `ETag`, `Vary: Accept-Encoding`) ; le 304 se juge
/// sur la représentation qui serait envoyée.
async fn app_files(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path();
    // Une route de l'API qui n'existe pas (ou qui n'existe pas sans les routes de test) : 404, quelle que soit la méthode.
    if path.starts_with(API_PREFIX) {
        return error_reply(StatusCode::NOT_FOUND, "not_found");
    }
    if !matches!(method, Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(asset) = web_assets::asset(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let zipped = asset.gzip.filter(|_| accepts_gzip(&headers));
    let (bytes, etag_text) = match zipped {
        Some(zipped) => (zipped.bytes, zipped.etag),
        None => (asset.bytes, asset.etag),
    };
    let etag = HeaderValue::from_static(etag_text);
    let cache = HeaderValue::from_static("no-cache");
    let vary = HeaderValue::from_static("Accept-Encoding");
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|sent| etag_matches(sent, etag_text))
    {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        let out = response.headers_mut();
        out.insert(header::ETAG, etag);
        out.insert(header::CACHE_CONTROL, cache);
        if asset.gzip.is_some() {
            out.insert(header::VARY, vary);
        }
        return response;
    }
    let mut response = (StatusCode::OK, bytes).into_response();
    let out = response.headers_mut();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    out.insert(header::ETAG, etag);
    out.insert(header::CACHE_CONTROL, cache);
    if asset.gzip.is_some() {
        out.insert(header::VARY, vary);
    }
    if zipped.is_some() {
        out.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    response
}

/// `If-None-Match` désigne-t-il cet `ETag` ? Liste séparée par des virgules, `*`, préfixe faible `W/` admis.
fn etag_matches(sent: &str, etag: &str) -> bool {
    sent.split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate.trim_start_matches("W/") == etag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::fleet::unix_millis;
    use crate::companion::test_support::{Harness, Reply, open_stream};

    const JSON: (&str, &str) = ("Content-Type", "application/json");
    const IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 19_0 like Mac OS X)";

    fn assert_hardened(reply: &Reply, what: &str) {
        assert_eq!(
            reply.header("content-security-policy"),
            Some(CONTENT_SECURITY_POLICY),
            "CSP on {what}"
        );
        assert_eq!(
            reply.header("x-content-type-options"),
            Some("nosniff"),
            "{what}"
        );
    }

    #[test]
    fn the_api_wants_a_cookie_except_health_and_pair() {
        let service = Harness::start(false);
        let health = service.request("GET", "/idema-api/health", &[], None);
        assert_eq!(health.status, 200);
        assert_eq!(health.json()["ok"], true);
        for path in [
            "/idema-api/me",
            "/idema-api/events",
            "/idema-api/anything-else",
        ] {
            let reply = service.request("GET", path, &[], None);
            assert_eq!(reply.status, 401, "{path}");
            assert_eq!(reply.json()["error"], "unpaired", "{path}");
        }
        // Un cookie qui n'est la forme d'aucun jeton ne passe pas non plus.
        for cookie in [
            "idema_companion=",
            "idema_companion=not-a-token",
            "idema_companion=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "other=1",
        ] {
            let reply = service.request("GET", "/idema-api/me", &[("Cookie", cookie)], None);
            assert_eq!(reply.status, 401, "{cookie}");
        }
    }

    #[test]
    fn a_pairing_code_hands_a_cookie_bound_to_the_api() {
        let service = Harness::start(true);
        let code = service
            .request("POST", "/idema-api/test/pairing-code", &[JSON], Some("{}"))
            .json()["code"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(code.len(), 6);
        let body = serde_json::json!({ "code": code }).to_string();
        let pair = || {
            service.request(
                "POST",
                "/idema-api/pair",
                &[JSON, ("User-Agent", IPHONE)],
                Some(&body),
            )
        };

        let reply = pair();
        assert_eq!(reply.status, 200, "{}", reply.text());
        let cookie = reply.header("set-cookie").expect("a cookie");
        assert!(cookie.starts_with("idema_companion="), "{cookie}");
        assert!(
            cookie.ends_with(
                "; HttpOnly; Secure; SameSite=Strict; Path=/idema-api; Max-Age=34560000"
            ),
            "{cookie}"
        );
        let device = &reply.json()["device"];
        assert_eq!(device["name"], "iPhone");
        // Ni le jeton ni son empreinte ne sont dans le corps.
        let token = cookie["idema_companion=".len()..]
            .split(';')
            .next()
            .unwrap();
        assert!(!reply.text().contains(token));

        let me = service.request(
            "GET",
            "/idema-api/me",
            &[("Cookie", cookie.split(';').next().unwrap())],
            None,
        );
        assert_eq!(me.status, 200);
        assert_eq!(me.json()["device"]["id"], device["id"]);
        assert_eq!(me.json()["device"]["name"], "iPhone");
        assert_eq!(me.json()["subscribed"], false);

        // Le même code une seconde fois : refusé, il était à usage unique.
        let again = pair();
        assert_eq!(again.status, 403);
        assert_eq!(again.json()["error"], "no_code");
        assert!(again.header("set-cookie").is_none());
        assert_hardened(&again, "a refusal");
    }

    #[test]
    fn pair_refusals_say_why_and_five_wrong_codes_lock() {
        let service = Harness::start(true);
        let post = |code: &str| {
            let body = serde_json::json!({ "code": code }).to_string();
            service.request("POST", "/idema-api/pair", &[JSON], Some(&body))
        };
        let none = post("123456");
        assert_eq!(
            (none.status, none.json()["error"].clone()),
            (403, "no_code".into())
        );

        let code = service
            .request("POST", "/idema-api/test/pairing-code", &[JSON], Some("{}"))
            .json()["code"]
            .as_str()
            .unwrap()
            .to_owned();
        let wrong = if code == "000000" { "111111" } else { "000000" };
        for _ in 0..5 {
            let reply = post(wrong);
            assert_eq!(
                (reply.status, reply.json()["error"].clone()),
                (403, "wrong_code".into())
            );
        }
        let locked = post(&code);
        assert_eq!(
            (locked.status, locked.json()["error"].clone()),
            (403, "locked".into())
        );
        assert!(locked.header("set-cookie").is_none());

        // Un corps illisible est une mauvaise requête (400) ; un corps lisible qui présente un code tombe sur le code
        // verrouillé (403), même avec un champ en trop.
        for (bad, status, error) in [
            ("", 400, "bad_request"),
            ("not json", 400, "bad_request"),
            ("{}", 400, "bad_request"),
            (r#"{"code":5}"#, 400, "bad_request"),
            (r#"{"code":"1","x":1}"#, 403, "locked"),
        ] {
            let reply = service.request("POST", "/idema-api/pair", &[JSON], Some(bad));
            assert_eq!(reply.status, status, "{bad}");
            assert_eq!(reply.json()["error"], error, "{bad}");
            assert!(reply.header("set-cookie").is_none(), "{bad}");
        }
    }

    #[test]
    fn writes_need_json_and_a_known_origin() {
        let service = Harness::start(true);
        let cookie = service.pair(IPHONE);
        let with_cookie = |extra: &[(&str, &str)]| {
            let mut headers = vec![("Cookie", cookie.as_str())];
            headers.extend_from_slice(extra);
            service.request(
                "POST",
                "/idema-api/client-metrics",
                &headers,
                Some(r#"{"name":"shell_ready","ms":120}"#),
            )
        };
        assert_eq!(with_cookie(&[JSON]).status, 204);
        assert_eq!(
            with_cookie(&[JSON, ("Content-Type", "application/json; charset=utf-8")]).status,
            204
        );

        let text = with_cookie(&[("Content-Type", "text/plain")]);
        assert_eq!(text.status, 415);
        let none = with_cookie(&[]);
        assert_eq!(none.status, 415);

        let foreign = with_cookie(&[JSON, ("Origin", "https://evil.example")]);
        assert_eq!(foreign.status, 403);
        assert_eq!(foreign.json()["error"], "forbidden_origin");
        // L'origine étrangère passe avant le type : elle est refusée même avec un mauvais type.
        assert_eq!(
            with_cookie(&[
                ("Content-Type", "text/plain"),
                ("Origin", "https://evil.example")
            ])
            .status,
            403
        );
        // Les origines admises passent.
        let local = format!("http://127.0.0.1:{}", service.port);
        let local_origin = [JSON, ("Origin", local.as_str())];
        assert_eq!(with_cookie(&local_origin).status, 204);
        assert_eq!(
            with_cookie(&[JSON, ("Origin", "https://x.example.ts.net:8455")]).status,
            204
        );

        // Sans cookie, l'écriture prend 401 avant 415.
        let anonymous = service.request(
            "POST",
            "/idema-api/client-metrics",
            &[("Content-Type", "text/plain")],
            Some("{}"),
        );
        assert_eq!(anonymous.status, 401);
        // `pair` est une écriture aussi : origine et type y sont exigés.
        let wrong_type =
            service.request("POST", "/idema-api/pair", &[], Some(r#"{"code":"123456"}"#));
        assert_eq!(wrong_type.status, 415);
        let foreign_pair = service.request(
            "POST",
            "/idema-api/pair",
            &[JSON, ("Origin", "https://evil.example")],
            Some(r#"{"code":"123456"}"#),
        );
        assert_eq!(foreign_pair.status, 403);
    }

    #[test]
    fn a_forgotten_device_takes_a_401_and_the_other_keeps_its_access() {
        let service = Harness::start(true);
        let phone = service.pair(IPHONE);
        let tablet = service.pair("Mozilla/5.0 (iPad; CPU OS 19_0 like Mac OS X)");
        let id = service
            .request("GET", "/idema-api/me", &[("Cookie", phone.as_str())], None)
            .json()["device"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let body = serde_json::json!({ "id": id }).to_string();
        let forgotten = service.request("POST", "/idema-api/test/forget", &[JSON], Some(&body));
        assert_eq!(forgotten.status, 200);

        let me =
            |cookie: &str| service.request("GET", "/idema-api/me", &[("Cookie", cookie)], None);
        assert_eq!(me(&phone).status, 401);
        let kept = me(&tablet);
        assert_eq!(kept.status, 200);
        assert_eq!(kept.json()["device"]["name"], "iPad");
        assert_eq!(service.status.borrow().devices.len(), 1);
    }

    #[test]
    fn a_cookie_survives_a_restart_of_the_service() {
        let first = Harness::start(true);
        let cookie = first.pair(IPHONE);
        let home = first.stop();
        let second = Harness::start_in(home, false);
        let me = second.request("GET", "/idema-api/me", &[("Cookie", cookie.as_str())], None);
        assert_eq!(me.status, 200, "{}", me.text());
        assert_eq!(me.json()["device"]["name"], "iPhone");
    }

    #[test]
    fn test_routes_need_both_variables_and_a_local_request() {
        // Sans l'autorisation (une des deux variables manque, décision de `test_routes_enabled`), la route n'existe pas :
        // un client sans cookie prend 401 de la garde (il ne saurait pas si elle existe), un appareil appairé 404.
        let closed = Harness::start(false);
        let paired = closed.pair_with_events(IPHONE);
        for path in ["/idema-api/test/pairing-code", "/idema-api/test/forget"] {
            let anonymous = closed.request("POST", path, &[JSON], Some("{}"));
            assert_eq!(anonymous.status, 401, "{path}");
            let known = closed.request(
                "POST",
                path,
                &[JSON, ("Cookie", paired.as_str())],
                Some("{}"),
            );
            assert_eq!(known.status, 404, "{path}");
        }
        assert!(closed.status.borrow().pairing.is_none());

        // Avec elle, une requête relayée par le tailnet est refusée, locale elle est servie.
        let open = Harness::start(true);
        for relayed in [
            ("Tailscale-User-Login", "someone@example.com"),
            ("X-Forwarded-For", "100.64.0.1"),
        ] {
            for path in ["/idema-api/test/pairing-code", "/idema-api/test/forget"] {
                let reply = open.request("POST", path, &[JSON, relayed], Some(r#"{"id":"x"}"#));
                assert_eq!(reply.status, 403, "{path} with {}", relayed.0);
            }
        }
        assert!(
            open.status.borrow().pairing.is_none(),
            "a refused request issued no code"
        );
        let served = open.request("POST", "/idema-api/test/pairing-code", &[JSON], Some("{}"));
        assert_eq!(served.status, 200);
        assert!(open.status.borrow().pairing.is_some());
    }

    #[test]
    fn only_the_two_test_routes_skip_the_cookie() {
        let service = Harness::start(true);
        let cookie = service.pair(IPHONE);
        for path in [
            "/idema-api/test/other",
            "/idema-api/test/pairing-code/",
            "/idema-api/test/",
            "/idema-api/test/forget/x",
        ] {
            let anonymous = service.request("POST", path, &[JSON], Some("{}"));
            assert_eq!(anonymous.status, 401, "{path} without a cookie");
            let known = service.request(
                "POST",
                path,
                &[JSON, ("Cookie", cookie.as_str())],
                Some("{}"),
            );
            assert_eq!(known.status, 404, "{path} with a cookie");
        }
    }

    fn numbered_fleet(badge: usize) -> Arc<Fleet> {
        Arc::new(Fleet {
            mac: "Test Mac".into(),
            badge,
            ..Fleet::default()
        })
    }

    #[test]
    fn the_fleet_route_serves_the_last_snapshot_to_a_paired_device() {
        let service = Harness::start(true);
        let anonymous = service.request("GET", "/idema-api/fleet", &[], None);
        assert_eq!(anonymous.status, 401);
        let cookie = service.pair(IPHONE);
        let get = || {
            service.request(
                "GET",
                "/idema-api/fleet",
                &[("Cookie", cookie.as_str())],
                None,
            )
        };

        service.fleet.send(numbered_fleet(2)).unwrap();
        let reply = get();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.header("content-type"), Some("application/json"));
        assert_eq!(reply.json()["badge"], 2);
        assert_eq!(reply.json()["mac"], "Test Mac");

        service.fleet.send(numbered_fleet(5)).unwrap();
        assert_eq!(
            get().json()["badge"],
            5,
            "the latest snapshot, not the first"
        );
    }

    #[test]
    fn the_events_stream_pushes_one_fleet_event_per_burst() {
        let service = Harness::start(true);
        let cookie = service.pair(IPHONE);
        service.fleet.send(numbered_fleet(1)).unwrap();
        let mut stream = open_stream(
            service.port,
            "/idema-api/events",
            &[("Cookie", cookie.as_str())],
        );
        assert!(stream.head.starts_with("HTTP/1.1 200"), "{}", stream.head);
        assert!(
            stream
                .head
                .to_lowercase()
                .contains("content-type: text/event-stream"),
            "{}",
            stream.head
        );

        // Le premier événement est l'instantané de la connexion.
        let first = stream
            .next_frame(Duration::from_secs(2))
            .expect("a first event");
        let data = first
            .strip_prefix("event: fleet\ndata: ")
            .unwrap_or_else(|| panic!("{first}"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(data).unwrap()["badge"],
            1
        );

        // Dix publications en 50 ms : un événement, le dernier.
        for badge in 2..=11 {
            service.fleet.send(numbered_fleet(badge)).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        let burst = stream
            .next_frame(Duration::from_secs(2))
            .expect("the burst event");
        let data = burst
            .strip_prefix("event: fleet\ndata: ")
            .unwrap_or_else(|| panic!("{burst}"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(data).unwrap()["badge"],
            11
        );
        assert_eq!(
            stream.next_frame(Duration::from_millis(600)),
            None,
            "nothing else after the burst"
        );
    }

    #[test]
    fn the_timeline_route_lists_the_transitions_that_count() {
        use idema_term::activity::ActivityState::{Idle, Running, Waiting};
        use std::time::SystemTime;

        let before = unix_millis(SystemTime::now());
        let service = Harness::start(true);
        let cookie = service.pair(IPHONE);
        let get = |query: &str| {
            service.request(
                "GET",
                &format!("/idema-api/timeline{query}"),
                &[("Cookie", cookie.as_str())],
                None,
            )
        };
        assert_eq!(
            service
                .request("GET", "/idema-api/timeline", &[], None)
                .status,
            401
        );

        let empty = get("");
        assert_eq!(empty.status, 200);
        let started = empty.json()["started_at"].as_u64().unwrap();
        assert!(
            (before..=unix_millis(SystemTime::now())).contains(&started),
            "{started}"
        );
        assert_eq!(empty.json()["events"], serde_json::json!([]));

        let at = SystemTime::now();
        // L'espace de travail range les transitions dans la chronologie du contrôleur, que le service lit.
        for (from, to) in [(Idle, Running), (Running, Waiting)] {
            service
                .timeline
                .lock()
                .unwrap()
                .record("idema-aaaaaaaa", from, to, at);
        }
        let events = get("").json()["events"].clone();
        let shown = unix_millis(at);
        assert_eq!(
            events,
            serde_json::json!([{
                "at": shown, "session": "idema-aaaaaaaa", "from": "running", "to": "waiting", "finished": false
            }]),
            "the idle to running step does not count"
        );
        assert_eq!(get(&format!("?since={shown}")).json()["events"], events);
        assert_eq!(
            get(&format!("?since={}", shown + 1)).json()["events"],
            serde_json::json!([])
        );
        for bad in ["?since=abc", "?since=-1", "?since=1.5", "?since="] {
            let reply = get(bad);
            assert_eq!(reply.status, 400, "{bad}");
            assert_eq!(reply.json()["error"], "bad_request", "{bad}");
        }
    }

    #[test]
    fn app_files_are_served_without_a_cookie_and_revalidate() {
        let service = Harness::start(false);
        let index = service.request("GET", "/", &[], None);
        assert_eq!(index.status, 200);
        assert_eq!(
            index.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        assert!(index.text().contains("IdemA"));
        assert_hardened(&index, "the shell page");
        let etag = index.header("etag").expect("an etag").to_owned();

        let cached = service.request("GET", "/", &[("If-None-Match", etag.as_str())], None);
        assert_eq!(cached.status, 304);
        assert!(cached.body.is_empty());
        assert_hardened(&cached, "a 304");
        let weak = format!("W/{etag}");
        assert_eq!(
            service
                .request("GET", "/", &[("If-None-Match", weak.as_str())], None)
                .status,
            304
        );
        let stale = service.request("GET", "/", &[("If-None-Match", "\"other\"")], None);
        assert_eq!(stale.status, 200);

        assert_eq!(service.request("GET", "/sw.js", &[], None).status, 200);
        assert_eq!(
            service
                .request("GET", "/manifest.webmanifest", &[], None)
                .header("content-type"),
            Some("application/manifest+json")
        );
        let logo = service.request("GET", "/logos/claude.png", &[], None);
        assert_eq!(logo.status, 200);
        assert!(logo.body.starts_with(b"\x89PNG"));

        // Les fichiers de la page inconnus rendent 404 ; sous `/idema-api/` la garde répond avant : 401 sans cookie.
        for (missing, status) in [
            ("/logos/nothing.png", 404),
            ("/logos/../index.html", 404),
            ("/nothing", 404),
            ("/idema-api/", 401),
        ] {
            let reply = service.request("GET", missing, &[], None);
            assert_eq!(reply.status, status, "{missing}");
            assert_hardened(&reply, missing);
        }
        assert_eq!(
            service.request("POST", "/", &[JSON], Some("{}")).status,
            405
        );
    }

    fn inflate(body: &[u8]) -> Vec<u8> {
        use std::io::Read as _;
        let mut inflated = Vec::new();
        flate2::read::GzDecoder::new(body)
            .read_to_end(&mut inflated)
            .expect("a valid gzip body");
        inflated
    }

    #[test]
    fn app_files_are_gzipped_only_when_asked_with_their_own_etag() {
        let service = Harness::start(false);
        let zip = [("Accept-Encoding", "br, gzip")];

        // Les fichiers de texte : le brut sans demande, le gzip avec, deux `ETag` distincts, `Vary` dans les deux cas.
        for path in [
            "/",
            "/app.css",
            "/sw.js",
            "/js/main.js",
            "/manifest.webmanifest",
        ] {
            let plain = service.request("GET", path, &[], None);
            let zipped = service.request("GET", path, &zip, None);
            assert_eq!(plain.status, 200, "{path}");
            assert_eq!(plain.header("content-encoding"), None, "{path}");
            assert_eq!(plain.header("vary"), Some("Accept-Encoding"), "{path}");
            assert_eq!(zipped.status, 200, "{path}");
            assert_eq!(zipped.header("content-encoding"), Some("gzip"), "{path}");
            assert_eq!(zipped.header("vary"), Some("Accept-Encoding"), "{path}");
            assert_eq!(
                zipped.header("content-type"),
                plain.header("content-type"),
                "{path}"
            );
            assert_eq!(zipped.header("cache-control"), Some("no-cache"), "{path}");
            assert_hardened(&plain, path);
            assert_hardened(&zipped, path);
            // Les mêmes octets une fois décompressés ; le gzip pèse moins que le brut.
            assert_eq!(inflate(&zipped.body), plain.body, "{path}");
            assert!(zipped.body.len() < plain.body.len(), "{path}");
            let plain_etag = plain.header("etag").unwrap();
            let zipped_etag = zipped.header("etag").unwrap();
            assert_ne!(plain_etag, zipped_etag, "{path}");
            assert_eq!(
                zipped_etag,
                format!("{}-gz\"", plain_etag.trim_end_matches('"')),
                "{path}"
            );
        }

        // `gzip;q=0` et `identity` refusent la compression ; `*` l'accepte.
        for refused in ["gzip;q=0, identity", "identity", "br"] {
            let reply = service.request("GET", "/app.css", &[("Accept-Encoding", refused)], None);
            assert_eq!(reply.header("content-encoding"), None, "{refused}");
        }
        let any = service.request("GET", "/app.css", &[("Accept-Encoding", "*")], None);
        assert_eq!(any.header("content-encoding"), Some("gzip"));
        // Un `gzip;q=0` explicite l'emporte sur `*` (RFC 9110), dans les deux ordres.
        for vetoed in ["gzip;q=0, *", "*, gzip;q=0"] {
            let reply = service.request("GET", "/app.css", &[("Accept-Encoding", vetoed)], None);
            assert_eq!(reply.header("content-encoding"), None, "{vetoed}");
        }
        // `gzip` explicite accepté malgré `*;q=0`.
        let only_gzip = service.request(
            "GET",
            "/app.css",
            &[("Accept-Encoding", "gzip, *;q=0")],
            None,
        );
        assert_eq!(only_gzip.header("content-encoding"), Some("gzip"));

        // HEAD avec gzip : mêmes en-têtes que GET, corps vide.
        let head = service.request("HEAD", "/app.css", &zip, None);
        assert_eq!(head.status, 200);
        assert_eq!(head.header("content-encoding"), Some("gzip"));
        assert_eq!(head.header("vary"), Some("Accept-Encoding"));
        assert!(head.header("etag").is_some());
        assert!(head.body.is_empty());

        // Les images ne sont jamais recompressées, même demandées.
        for png in ["/icons/icon-180.png", "/logos/claude.png"] {
            let reply = service.request("GET", png, &zip, None);
            assert_eq!(reply.status, 200, "{png}");
            assert_eq!(reply.header("content-encoding"), None, "{png}");
            assert_eq!(reply.header("vary"), None, "{png}");
            assert!(reply.body.starts_with(b"\x89PNG"), "{png}");
            assert_hardened(&reply, png);
        }
    }

    #[test]
    fn app_files_revalidate_per_representation() {
        let service = Harness::start(false);
        let zip = ("Accept-Encoding", "gzip");
        let plain_etag = service
            .request("GET", "/js/main.js", &[], None)
            .header("etag")
            .unwrap()
            .to_owned();
        let zipped_etag = service
            .request("GET", "/js/main.js", &[zip], None)
            .header("etag")
            .unwrap()
            .to_owned();

        // 304 pour chaque représentation avec son `ETag`, sans corps, avec la CSP et `Vary`.
        for (headers, etag) in [
            (vec![("If-None-Match", plain_etag.as_str())], &plain_etag),
            (
                vec![zip, ("If-None-Match", zipped_etag.as_str())],
                &zipped_etag,
            ),
        ] {
            let reply = service.request("GET", "/js/main.js", &headers, None);
            assert_eq!(reply.status, 304, "{etag}");
            assert!(reply.body.is_empty());
            assert_eq!(reply.header("etag"), Some(etag.as_str()));
            assert_eq!(reply.header("vary"), Some("Accept-Encoding"));
            assert_eq!(reply.header("content-encoding"), None);
            assert_hardened(&reply, "a 304");
        }

        // Le `ETag` d'une représentation ne valide pas l'autre : le corps est renvoyé.
        let identity_for_gzip = service.request(
            "GET",
            "/js/main.js",
            &[zip, ("If-None-Match", plain_etag.as_str())],
            None,
        );
        assert_eq!(identity_for_gzip.status, 200);
        assert_eq!(identity_for_gzip.header("content-encoding"), Some("gzip"));
        let gzip_for_identity = service.request(
            "GET",
            "/js/main.js",
            &[("If-None-Match", zipped_etag.as_str())],
            None,
        );
        assert_eq!(gzip_for_identity.status, 200);
        assert_eq!(gzip_for_identity.header("content-encoding"), None);
    }

    #[test]
    fn every_api_answer_carries_the_csp() {
        let service = Harness::start(true);
        let cookie = service.pair(IPHONE);
        for (path, headers) in [
            ("/idema-api/health", vec![]),
            ("/idema-api/me", vec![]),
            ("/idema-api/me", vec![("Cookie", cookie.as_str())]),
            ("/idema-api/unknown", vec![("Cookie", cookie.as_str())]),
        ] {
            let reply = service.request("GET", path, &headers, None);
            assert_hardened(&reply, path);
        }
        let unknown = service.request(
            "GET",
            "/idema-api/unknown",
            &[("Cookie", cookie.as_str())],
            None,
        );
        assert_eq!(unknown.status, 404);
    }

    #[test]
    fn client_metrics_accept_only_known_names_and_bounded_integers() {
        assert_eq!(
            parse_metric(br#"{"name":"shell_ready","ms":0}"#),
            Some(("shell_ready", 0))
        );
        assert_eq!(
            parse_metric(br#"{"name":"resume_ready","ms":600000}"#),
            Some(("resume_ready", 600_000))
        );
        for bad in [
            &br#"{"name":"shell_ready","ms":600001}"#[..],
            br#"{"name":"shell_ready","ms":-1}"#,
            br#"{"name":"shell_ready","ms":12.5}"#,
            br#"{"name":"shell_ready","ms":"12"}"#,
            br#"{"name":"other","ms":1}"#,
            br#"{"name":"shell_ready\nfake","ms":1}"#,
            br#"{"name":"shell_ready"}"#,
            br#"{"ms":1}"#,
            br#"{"name":"shell_ready","ms":1,"extra":1}"#,
            b"not json",
            b"",
        ] {
            assert_eq!(parse_metric(bad), None, "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn client_metrics_answer_400_for_a_bad_body() {
        let service = Harness::start(true);
        let cookie = service.pair(IPHONE);
        let post = |body: &str| {
            service.request(
                "POST",
                "/idema-api/client-metrics",
                &[JSON, ("Cookie", cookie.as_str())],
                Some(body),
            )
        };
        assert_eq!(post(r#"{"name":"fleet_ready","ms":250}"#).status, 204);
        assert_eq!(post(r#"{"name":"fleet_ready","ms":600001}"#).status, 400);
        assert_eq!(post(r#"{"name":"unknown","ms":1}"#).status, 400);
        assert_eq!(post("nope").status, 400);
    }

    #[test]
    fn the_events_of_the_workspace_drive_the_pairing_state_and_the_device_list() {
        let service = Harness::start(true);
        let cookie = service.pair(IPHONE);
        let id = service
            .request("GET", "/idema-api/me", &[("Cookie", cookie.as_str())], None)
            .json()["device"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(service.status.borrow().devices.len(), 1);

        service.events.send(ServiceEvent::StartPairing).unwrap();
        let shown = service.wait(|status| status.pairing.is_some());
        assert_eq!(shown.pairing.unwrap().code.len(), 6);
        service.events.send(ServiceEvent::CancelPairing).unwrap();
        service.wait(|status| status.pairing.is_none());

        service
            .events
            .send(ServiceEvent::ForgetDevice { id })
            .unwrap();
        service.wait(|status| status.devices.is_empty());
        let me = service.request("GET", "/idema-api/me", &[("Cookie", cookie.as_str())], None);
        assert_eq!(me.status, 401);
    }

    // ---- Écran, saisie et acquittement d'un pane ----

    use crate::companion::input::{RemoteInput, RemoteKey};
    use crate::companion::test_support::{TempDir, fake_tmux, serve_commands};
    use std::sync::mpsc;

    const SCREEN: &str = "/idema-api/sessions/idema-ab12cd34/panes/5/screen";
    const INPUT: &str = "/idema-api/sessions/idema-ab12cd34/panes/5/input";
    const ACK: &str = "/idema-api/sessions/idema-ab12cd34/panes/5/ack";
    /// Ce que le faux tmux rend à `capture-pane` : des couleurs, une ligne de 300 colonnes, une fin `\n` suivie d'une
    /// ligne d'espaces, et un octet qui n'est pas de l'UTF-8.
    fn capture_bytes() -> Vec<u8> {
        let mut bytes = b"\x1b[31mred\x1b[0m\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 300));
        bytes.extend_from_slice(b"\nbad \xff byte\n   \n");
        bytes
    }

    /// Un service dont le fil d'interface répond `probe` à toute lecture, avec un faux tmux.
    fn screen_service(probe: Option<PaneProbe>) -> (Harness, String, std::path::PathBuf) {
        let dir = TempDir::new("fake-tmux");
        let (tmux, log) = fake_tmux(&dir.0, &capture_bytes());
        let mut service = Harness::start_with(dir, true, Some(tmux));
        serve_commands(&mut service, move |command| {
            if let PaneCommand::Probe { reply, .. } = command {
                let _ = reply.send(probe);
            }
        });
        let cookie = service.pair(IPHONE);
        (service, cookie, log)
    }

    fn calls(log: &std::path::Path) -> String {
        std::fs::read_to_string(log).unwrap_or_default()
    }

    #[test]
    fn the_screen_route_serves_the_exact_capture_with_an_etag_and_a_304() {
        let probe = PaneProbe {
            accepts_input: true,
            state: "waiting",
        };
        let (service, cookie, log) = screen_service(Some(probe));
        let get = |path: &str, extra: &[(&str, &str)]| {
            let mut headers = vec![("Cookie", cookie.as_str())];
            headers.extend_from_slice(extra);
            service.request("GET", path, &headers, None)
        };
        let reply = get(SCREEN, &[]);
        assert_eq!(reply.status, 200, "{}", reply.text());
        assert_hardened(&reply, "screen");
        let json = reply.json();
        // Le texte est celui de la capture, octet pour octet (le seul octet invalide devient U+FFFD).
        assert_eq!(
            json["text"].as_str().unwrap().as_bytes(),
            String::from_utf8_lossy(&capture_bytes()).as_bytes()
        );
        assert!(json["text"].as_str().unwrap().ends_with("byte\n   \n"));
        assert_eq!(
            (json["cols"].as_u64(), json["rows"].as_u64()),
            (Some(80), Some(24))
        );
        assert_eq!(json["accepts_input"], true);
        assert_eq!(json["state"], "waiting");
        assert!(json["took_ms"].is_u64());
        let etag = reply.header("etag").expect("an ETag").to_owned();

        // Une seconde lecture sans changement : 304, même ETag, pas de corps.
        let again = get(SCREEN, &[("If-None-Match", &etag)]);
        assert_eq!(again.status, 304);
        assert_eq!(again.header("etag"), Some(etag.as_str()));
        assert!(again.body.is_empty());
        // Un autre ETag n'est pas pris pour celui-là.
        assert_eq!(get(SCREEN, &[("If-None-Match", "\"autre\"")]).status, 200);

        // Par défaut 200 lignes d'historique ; la capture se fait avec les options de la spec.
        let log = calls(&log);
        assert!(log.contains("capture-pane -p -e -J -t %5 -S -200"), "{log}");
        assert!(log.contains("display-message -p -t %5"), "{log}");
    }

    #[test]
    fn the_etag_follows_what_the_pane_accepts_not_only_its_text() {
        let dir = TempDir::new("etag-follows");
        let (tmux, _log) = fake_tmux(&dir.0, b"same\n");
        let mut service = Harness::start_with(dir, true, Some(tmux));
        let accepts = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let seen = Arc::clone(&accepts);
        serve_commands(&mut service, move |command| {
            if let PaneCommand::Probe { reply, .. } = command {
                let _ = reply.send(Some(PaneProbe {
                    accepts_input: seen.load(std::sync::atomic::Ordering::SeqCst),
                    state: "running",
                }));
            }
        });
        let cookie = service.pair(IPHONE);
        let first = service.request("GET", SCREEN, &[("Cookie", &cookie)], None);
        let etag = first.header("etag").unwrap().to_owned();
        accepts.store(false, std::sync::atomic::Ordering::SeqCst);
        // Même texte, mais le pane n'accepte plus de saisie : un 304 ferait garder la barre de réponse à la page.
        let after = service.request(
            "GET",
            SCREEN,
            &[("Cookie", &cookie), ("If-None-Match", &etag)],
            None,
        );
        assert_eq!(after.status, 200);
        assert_eq!(after.json()["accepts_input"], false);
        assert_ne!(after.header("etag"), Some(etag.as_str()));
    }

    #[test]
    fn the_screen_is_compressed_only_when_the_client_asks() {
        use std::io::Read as _;
        let (service, cookie, _log) = screen_service(Some(PaneProbe {
            accepts_input: false,
            state: "idle",
        }));
        let plain = service.request("GET", SCREEN, &[("Cookie", &cookie)], None);
        assert_eq!(plain.header("content-encoding"), None);
        let zipped = service.request(
            "GET",
            SCREEN,
            &[("Cookie", &cookie), ("Accept-Encoding", "br, gzip;q=0.8")],
            None,
        );
        assert_eq!(zipped.header("content-encoding"), Some("gzip"));
        let mut inflated = Vec::new();
        flate2::read::GzDecoder::new(&zipped.body[..])
            .read_to_end(&mut inflated)
            .unwrap();
        // Le même écran (la durée de capture change d'une lecture à l'autre).
        let inflated: serde_json::Value = serde_json::from_slice(&inflated).unwrap();
        assert_eq!(inflated["text"], plain.json()["text"]);
        assert_eq!(inflated["cols"], plain.json()["cols"]);
        // `gzip;q=0` refuse la compression.
        let refused = service.request(
            "GET",
            SCREEN,
            &[
                ("Cookie", &cookie),
                ("Accept-Encoding", "gzip;q=0, identity"),
            ],
            None,
        );
        assert_eq!(refused.header("content-encoding"), None);
        assert_eq!(refused.json()["text"], plain.json()["text"]);
    }

    #[test]
    fn history_is_bounded_and_passed_to_tmux() {
        let (service, cookie, log) = screen_service(Some(PaneProbe {
            accepts_input: false,
            state: "idle",
        }));
        let get = |query: &str| {
            service.request(
                "GET",
                &format!("{SCREEN}{query}"),
                &[("Cookie", &cookie)],
                None,
            )
        };
        for bad in ["?history=0", "?history=2001", "?history=abc", "?history=-5"] {
            assert_eq!(get(bad).status, 400, "{bad}");
        }
        assert_eq!(get("?history=2000").status, 200);
        assert_eq!(get("?history=1").status, 200);
        let log = calls(&log);
        assert!(log.contains("-S -2000"), "{log}");
        assert!(log.contains("-S -1"), "{log}");
        // Les refus n'ont rien capturé : trois captures seulement.
        assert_eq!(log.matches("capture-pane").count(), 2, "{log}");
    }

    #[test]
    fn an_unknown_session_or_pane_takes_a_404_and_nothing_is_captured() {
        let (service, cookie, log) = screen_service(None);
        let get = |path: &str| service.request("GET", path, &[("Cookie", &cookie)], None);
        assert_eq!(get(SCREEN).status, 404);
        assert_eq!(get(SCREEN).json()["error"], "not_found");
        assert_eq!(
            get("/idema-api/sessions/idema-ab12cd34/panes/%255/screen").status,
            404
        );
        assert_eq!(
            get("/idema-api/sessions/idema-ab12cd34/panes/abc/screen").status,
            404
        );
        assert_eq!(
            calls(&log).matches("capture-pane").count(),
            0,
            "{}",
            calls(&log)
        );
    }

    /// Ce que le faux fil d'interface a reçu d'une saisie : session, pane, saisie, `command_id`.
    type SeenInput = (String, u32, RemoteInput, String);

    /// Un service dont le fil d'interface note chaque commande reçue et répond `outcome` aux saisies.
    fn input_service(outcome: InputOutcome) -> (Harness, String, mpsc::Receiver<SeenInput>) {
        let mut service = Harness::start(true);
        let (sender, received) = mpsc::channel();
        serve_commands(&mut service, move |command| {
            if let PaneCommand::Input {
                session,
                pane,
                input,
                command_id,
                reply,
                ..
            } = command
            {
                let _ = sender.send((session, pane, input, command_id));
                let _ = reply.send(outcome);
            }
        });
        let cookie = service.pair(IPHONE);
        (service, cookie, received)
    }

    #[test]
    fn an_input_reaches_the_workspace_with_its_session_pane_and_id() {
        let (service, cookie, received) = input_service(InputOutcome::Sent);
        let reply = service.request(
            "POST",
            INPUT,
            &[JSON, ("Cookie", &cookie)],
            Some(r#"{"key":"y","enter":true,"command_id":"abc-1"}"#),
        );
        assert_eq!(reply.status, 200, "{}", reply.text());
        assert_eq!(
            reply.json(),
            serde_json::json!({ "ok": true, "outcome": "sent" })
        );
        assert_eq!(
            received.try_recv().unwrap(),
            (
                "idema-ab12cd34".to_owned(),
                5,
                RemoteInput::Key {
                    key: RemoteKey::Y,
                    enter: true
                },
                "abc-1".to_owned()
            )
        );
        let text = service.request(
            "POST",
            INPUT,
            &[JSON, ("Cookie", &cookie)],
            Some(r#"{"text":"voir le diff","enter":false,"command_id":"abc-2"}"#),
        );
        assert_eq!(text.status, 200);
        assert_eq!(
            received.try_recv().unwrap().2,
            RemoteInput::Text {
                text: "voir le diff".into(),
                enter: false
            }
        );
    }

    #[test]
    fn every_outcome_has_its_status_and_none_but_sent_and_duplicate_succeed() {
        let cases = [
            (InputOutcome::Sent, 200, "sent"),
            (InputOutcome::SentWithoutEnter, 200, "sent_without_enter"),
            (InputOutcome::Duplicate, 200, "duplicate"),
            (InputOutcome::NoProgram, 409, "no_program"),
            (InputOutcome::ReadOnly, 409, "read_only"),
            (InputOutcome::Disconnected, 503, "disconnected"),
            (InputOutcome::NoPane, 404, "no_pane"),
            (InputOutcome::UnknownSession, 404, "unknown_session"),
            (InputOutcome::Expired, 503, "expired"),
        ];
        for (outcome, status, word) in cases {
            let (service, cookie, _received) = input_service(outcome);
            let reply = service.request(
                "POST",
                INPUT,
                &[JSON, ("Cookie", &cookie)],
                Some(r#"{"key":"1","command_id":"c"}"#),
            );
            assert_eq!(reply.status, status, "{word}");
            let json = reply.json();
            if status == 200 {
                assert_eq!(
                    (json["ok"].clone(), json["outcome"].clone()),
                    (true.into(), word.into())
                );
            } else {
                assert_eq!(json["error"], word, "{word}");
            }
        }
    }

    #[test]
    fn a_bad_input_body_takes_a_400_and_never_reaches_the_workspace() {
        let (service, cookie, received) = input_service(InputOutcome::Sent);
        let cases = [
            ("nope", "not_json"),
            (r#"{"command_id":"c"}"#, "neither_key_nor_text"),
            (
                r#"{"key":"y","text":"a","command_id":"c"}"#,
                "both_key_and_text",
            ),
            (r#"{"key":"F5","command_id":"c"}"#, "unknown_key"),
            (r#"{"text":"","command_id":"c"}"#, "empty_text"),
            (r#"{"text":"a\nb","command_id":"c"}"#, "control_char"),
            (r#"{"text":"a\u0085b","command_id":"c"}"#, "control_char"),
            (r#"{"key":"y"}"#, "missing_command_id"),
        ];
        for (body, error) in cases {
            let reply = service.request("POST", INPUT, &[JSON, ("Cookie", &cookie)], Some(body));
            assert_eq!(reply.status, 400, "{body}");
            assert_eq!(reply.json()["error"], error, "{body}");
        }
        let too_long = format!(r#"{{"text":"{}","command_id":"c"}}"#, "a".repeat(1025));
        let reply = service.request("POST", INPUT, &[JSON, ("Cookie", &cookie)], Some(&too_long));
        assert_eq!(
            (reply.status, reply.json()["error"].clone()),
            (400, "text_too_long".into())
        );
        assert!(received.try_recv().is_err(), "no command was queued");
    }

    #[test]
    fn the_input_route_is_closed_to_a_stranger_and_types_nothing() {
        let (service, cookie, received) = input_service(InputOutcome::Sent);
        let body = Some(r#"{"key":"y","command_id":"c"}"#);
        let anonymous = service.request("POST", INPUT, &[JSON], body);
        assert_eq!(anonymous.status, 401);
        let wrong_type = service.request(
            "POST",
            INPUT,
            &[("Cookie", &cookie), ("Content-Type", "text/plain")],
            body,
        );
        assert_eq!(wrong_type.status, 415);
        let foreign = service.request(
            "POST",
            INPUT,
            &[
                JSON,
                ("Cookie", &cookie),
                ("Origin", "https://evil.example"),
            ],
            body,
        );
        assert_eq!(foreign.status, 403);
        let get = service.request("GET", INPUT, &[("Cookie", &cookie)], None);
        assert_ne!(get.status, 200);
        assert!(
            received.try_recv().is_err(),
            "nothing reached the workspace"
        );
        // Le cookie d'un appareil oublié n'ouvre rien non plus.
        let forgotten = service.pair("Mozilla/5.0 (iPad)");
        let id = service
            .request("GET", "/idema-api/me", &[("Cookie", &forgotten)], None)
            .json()["device"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        service.request(
            "POST",
            "/idema-api/test/forget",
            &[JSON],
            Some(&serde_json::json!({ "id": id }).to_string()),
        );
        let after = service.request("POST", INPUT, &[JSON, ("Cookie", &forgotten)], body);
        assert_eq!(after.status, 401);
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn an_unanswered_input_takes_a_503_expired_after_the_http_wait() {
        let mut service = Harness::start(true);
        let (sender, received) = mpsc::channel();
        // Le fil d'interface reçoit la commande et ne répond pas.
        serve_commands(&mut service, move |command| {
            if let PaneCommand::Input {
                received_at, reply, ..
            } = command
            {
                let _ = sender.send(received_at);
                std::mem::forget(reply);
            }
        });
        let cookie = service.pair(IPHONE);
        let started = Instant::now();
        let reply = service.request(
            "POST",
            INPUT,
            &[JSON, ("Cookie", &cookie)],
            Some(r#"{"key":"y","command_id":"c"}"#),
        );
        assert_eq!(
            (reply.status, reply.json()["error"].clone()),
            (503, "expired".into())
        );
        let waited = started.elapsed();
        assert!(
            waited >= input::HTTP_WAIT && waited < input::HTTP_WAIT + Duration::from_millis(1000),
            "waited {waited:?}"
        );
        // La commande portait l'instant de réception HTTP, pris avant l'envoi.
        let received_at = received.try_recv().expect("the command was queued");
        assert!(received_at >= started);
        assert!(received_at.duration_since(started) < Duration::from_millis(500));
    }

    #[test]
    fn a_workspace_that_is_gone_takes_a_503_not_a_hang() {
        let mut service = Harness::start(true);
        drop(service.commands.take());
        let cookie = service.pair(IPHONE);
        let started = Instant::now();
        let reply = service.request(
            "POST",
            INPUT,
            &[JSON, ("Cookie", &cookie)],
            Some(r#"{"key":"y","command_id":"c"}"#),
        );
        assert_eq!(reply.status, 503);
        assert_eq!(reply.json()["error"], "unavailable");
        assert!(started.elapsed() < Duration::from_secs(1));
        let screen = service.request("GET", SCREEN, &[("Cookie", &cookie)], None);
        assert_eq!(screen.status, 503);
    }

    #[test]
    fn an_acknowledgement_asks_for_the_phone_origin_and_maps_the_answer() {
        for (known, status) in [(true, 200), (false, 404)] {
            let mut service = Harness::start(true);
            let (sender, received) = mpsc::channel();
            serve_commands(&mut service, move |command| {
                if let PaneCommand::Ack {
                    session,
                    pane,
                    origin,
                    reply,
                } = command
                {
                    let _ = sender.send((session, pane, origin));
                    let _ = reply.send(known);
                }
            });
            let cookie = service.pair(IPHONE);
            let reply = service.request("POST", ACK, &[JSON, ("Cookie", &cookie)], Some("{}"));
            assert_eq!(reply.status, status);
            assert_eq!(
                received.try_recv().unwrap(),
                ("idema-ab12cd34".to_owned(), 5, AckOrigin::Phone)
            );
            let wrong_type = service.request("POST", ACK, &[("Cookie", &cookie)], Some("{}"));
            assert_eq!(wrong_type.status, 415);
            assert!(received.try_recv().is_err());
        }
    }

    // ---- push : clé, abonnements, notification de test ----

    use crate::companion::test_support::FakePush;

    const KEY: &str = "/idema-api/push/key";
    const SUBSCRIPTION: &str = "/idema-api/push/subscription";
    const TEST_PUSH: &str = "/idema-api/push/test";
    const IPAD: &str = "Mozilla/5.0 (iPad; CPU OS 19_0 like Mac OS X)";

    /// Le corps de `PushSubscription.toJSON()` d'un téléphone, avec de vraies clés (point valide de la courbe).
    fn subscription_body(endpoint: &str) -> String {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let private = p256::SecretKey::random(&mut rand::rngs::OsRng);
        let public = private.public_key().to_encoded_point(false);
        serde_json::json!({
            "endpoint": endpoint,
            "expirationTime": null,
            "keys": { "p256dh": B64.encode(public.as_bytes()), "auth": B64.encode([5u8; 16]) },
        })
        .to_string()
    }

    fn put_subscription(service: &Harness, cookie: &str, endpoint: &str) -> Reply {
        service.request(
            "PUT",
            SUBSCRIPTION,
            &[JSON, ("Cookie", cookie)],
            Some(&subscription_body(endpoint)),
        )
    }

    fn me(service: &Harness, cookie: &str) -> serde_json::Value {
        service
            .request("GET", "/idema-api/me", &[("Cookie", cookie)], None)
            .json()
    }

    /// Les `endpoint` que `devices.json` garde, appareil par appareil (`None` : sans abonnement).
    fn stored_endpoints(service: &Harness) -> Vec<Option<String>> {
        let file = std::fs::read(service.home.0.join("companion/devices.json")).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&file).unwrap();
        json["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|device| {
                device["subscription"]["endpoint"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect()
    }

    #[test]
    fn the_push_key_is_the_vapid_public_key_and_asks_for_a_cookie() {
        let service = Harness::start(true);
        assert_eq!(service.request("GET", KEY, &[], None).status, 401);
        let cookie = service.pair(IPHONE);
        let reply = service.request("GET", KEY, &[("Cookie", &cookie)], None);
        assert_eq!(reply.status, 200, "{}", reply.text());
        let key = reply.json()["key"].as_str().unwrap().to_owned();
        assert_eq!(key.len(), 87, "an uncompressed P-256 point in base64url");
        let stored = crate::companion::webpush::Vapid::load_or_create(&service.home.0).unwrap();
        assert_eq!(key, stored.public_key_b64url());
        assert_hardened(&reply, "push key");
    }

    #[test]
    fn a_subscription_is_checked_then_kept_once_whatever_the_device() {
        let service = Harness::start(true);
        let phone = service.pair(IPHONE);
        let tablet = service.pair(IPAD);
        let put = |cookie: &str, body: &str| {
            service.request("PUT", SUBSCRIPTION, &[JSON, ("Cookie", cookie)], Some(body))
        };

        // Refus : rien n'est gardé.
        assert_eq!(put(&phone, "not json").status, 400);
        assert_eq!(put(&phone, "{}").status, 400);
        let refused = put_subscription(&service, &phone, "https://evil.example/x");
        assert_eq!(refused.status, 400);
        assert_eq!(refused.json()["error"], "endpoint_not_allowed");
        let http = put_subscription(&service, &phone, "http://web.push.apple.com/x");
        assert_eq!(http.json()["error"], "endpoint_not_allowed");
        let bad_keys = serde_json::json!({
            "endpoint": "https://web.push.apple.com/QXX",
            "keys": { "p256dh": "AAAA", "auth": "AAAA" },
        })
        .to_string();
        let bad = put(&phone, &bad_keys);
        assert_eq!(bad.status, 400);
        assert_eq!(bad.json()["error"], "bad_subscription");
        assert_eq!(stored_endpoints(&service), [None, None]);
        let no_type = service.request(
            "PUT",
            SUBSCRIPTION,
            &[("Cookie", &phone)],
            Some(&subscription_body("https://web.push.apple.com/QXX")),
        );
        assert_eq!(no_type.status, 415);
        let no_cookie = service.request(
            "PUT",
            SUBSCRIPTION,
            &[JSON],
            Some(&subscription_body("https://web.push.apple.com/QXX")),
        );
        assert_eq!(no_cookie.status, 401);

        // Le même endpoint donné deux fois par le même appareil : un seul abonnement.
        let endpoint = "https://web.push.apple.com/QXX-shared";
        assert_eq!(put_subscription(&service, &phone, endpoint).status, 200);
        assert_eq!(put_subscription(&service, &phone, endpoint).status, 200);
        assert_eq!(
            stored_endpoints(&service)
                .iter()
                .flatten()
                .collect::<Vec<_>>(),
            [endpoint]
        );
        assert_eq!(me(&service, &phone)["subscribed"], true);
        service.wait(|status| status.devices.iter().any(|device| device.subscribed));

        // Le même endpoint donné par un autre appareil : il n'appartient plus qu'à lui.
        assert_eq!(put_subscription(&service, &tablet, endpoint).status, 200);
        assert_eq!(
            stored_endpoints(&service),
            [None, Some(endpoint.to_owned())]
        );
        assert_eq!(me(&service, &phone)["subscribed"], false);
        assert_eq!(me(&service, &tablet)["subscribed"], true);
    }

    #[test]
    fn unsubscribing_removes_the_callers_subscription_only() {
        let service = Harness::start(true);
        let phone = service.pair(IPHONE);
        let tablet = service.pair(IPAD);
        let tablet_endpoint = "https://web.push.apple.com/QXX-tablet";
        put_subscription(&service, &phone, "https://web.push.apple.com/QXX-phone");
        put_subscription(&service, &tablet, tablet_endpoint);

        let removed = service.request("DELETE", SUBSCRIPTION, &[JSON, ("Cookie", &phone)], None);
        assert_eq!(removed.status, 200, "{}", removed.text());
        assert_eq!(
            stored_endpoints(&service),
            [None, Some(tablet_endpoint.to_owned())],
            "the tablet keeps its own"
        );
        assert_eq!(me(&service, &phone)["subscribed"], false);
        assert_eq!(me(&service, &tablet)["subscribed"], true);
        // Retirer deux fois ne casse rien.
        let again = service.request("DELETE", SUBSCRIPTION, &[JSON, ("Cookie", &phone)], None);
        assert_eq!(again.status, 200);
        let stranger = service.request("DELETE", SUBSCRIPTION, &[JSON], None);
        assert_eq!(stranger.status, 401);
    }

    #[test]
    fn the_test_push_goes_to_the_asking_device_only() {
        let fake = FakePush::start(201, "");
        let service = Harness::start_with_push(TempDir::new("push-test"), fake.url());
        let phone = service.pair(IPHONE);
        let tablet = service.pair(IPAD);
        put_subscription(&service, &phone, "https://web.push.apple.com/QXX-phone");
        put_subscription(&service, &tablet, "https://web.push.apple.com/QXX-tablet");

        let sent = service.request("POST", TEST_PUSH, &[JSON, ("Cookie", &phone)], Some("{}"));
        assert_eq!(sent.status, 200, "{}", sent.text());
        assert_eq!(sent.json()["result"], "sent");
        let requests = fake.wait_for(1);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(fake.requests().len(), 1, "one request for one device");
        assert_eq!(requests[0].path, "/QXX-phone");
        assert_eq!(requests[0].header("Topic").unwrap().len(), 32);
        assert!(
            requests[0]
                .header("Authorization")
                .unwrap()
                .starts_with("vapid t=")
        );
        assert_eq!(requests[0].header("Content-Encoding"), Some("aes128gcm"));

        // L'autre appareil a sa propre notification, chiffrée pour lui.
        let sent = service.request("POST", TEST_PUSH, &[JSON, ("Cookie", &tablet)], Some("{}"));
        assert_eq!(sent.status, 200);
        let requests = fake.wait_for(2);
        assert_eq!(requests[1].path, "/QXX-tablet");
        assert_ne!(
            requests[0].body, requests[1].body,
            "each device gets its own ciphertext"
        );
        // Le message de test ne retire rien.
        assert_eq!(stored_endpoints(&service).iter().flatten().count(), 2);
    }

    #[test]
    fn the_test_push_needs_a_subscription_and_a_cookie() {
        let fake = FakePush::start(201, "");
        let service = Harness::start_with_push(TempDir::new("push-none"), fake.url());
        let stranger = service.request("POST", TEST_PUSH, &[JSON], Some("{}"));
        assert_eq!(stranger.status, 401);
        let phone = service.pair(IPHONE);
        let reply = service.request("POST", TEST_PUSH, &[JSON, ("Cookie", &phone)], Some("{}"));
        assert_eq!(reply.status, 409);
        assert_eq!(reply.json()["error"], "not_subscribed");
        assert!(
            fake.requests().is_empty(),
            "nothing is sent without a subscription"
        );
    }

    #[test]
    fn the_answers_of_the_push_service_decide_what_happens_to_the_subscription() {
        // (code, corps, résultat, l'abonnement reste-t-il ?)
        let cases = [
            (410, "", "gone", false),
            (404, "", "gone", false),
            (403, "VapidPkHashMismatch", "gone", false),
            (403, "BadVapidPublicKey", "gone", false),
            (403, "ExpiredJwt", "failed", true),
            (413, "", "too_large", true),
            (429, "", "retry_later", true),
            (503, "", "retry_later", true),
            (400, "", "failed", true),
        ];
        for (status, body, result, kept) in cases {
            let fake = FakePush::start(status, body);
            let service = Harness::start_with_push(TempDir::new("push-answers"), fake.url());
            let phone = service.pair(IPHONE);
            let endpoint = "https://web.push.apple.com/QXX-answers";
            put_subscription(&service, &phone, endpoint);

            let reply = service.request("POST", TEST_PUSH, &[JSON, ("Cookie", &phone)], Some("{}"));

            let label = format!("{status} {body}");
            assert_eq!(reply.status, 502, "{label}");
            assert_eq!(reply.json()["result"], result, "{label}");
            assert_eq!(
                stored_endpoints(&service),
                [kept.then(|| endpoint.to_owned())],
                "{label}"
            );
            assert_eq!(me(&service, &phone)["subscribed"], kept, "{label}");
            // Le message de test n'est jamais renvoyé de lui-même, même sur un 429.
            std::thread::sleep(Duration::from_millis(200));
            assert_eq!(fake.requests().len(), 1, "{label}");
        }
    }

    #[test]
    fn a_push_service_that_hangs_up_ends_the_test_push_as_retry_later() {
        // La coupure à 10 s de curl se joue en VM (ac103) ; ici, un service qui ferme la connexion sans répondre.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(1) {
                drop(stream);
            }
        });
        let service = Harness::start_with_push(
            TempDir::new("push-closed"),
            format!("http://127.0.0.1:{port}"),
        );
        let phone = service.pair(IPHONE);
        put_subscription(&service, &phone, "https://web.push.apple.com/QXX-closed");
        let reply = service.request("POST", TEST_PUSH, &[JSON, ("Cookie", &phone)], Some("{}"));
        assert_eq!(reply.status, 502);
        assert_eq!(reply.json()["result"], "retry_later");
        assert_eq!(me(&service, &phone)["subscribed"], true, "kept for a retry");
    }

    #[test]
    fn an_unreadable_vapid_key_closes_the_push_routes_but_not_the_service() {
        let home = TempDir::new("push-badkey");
        std::fs::create_dir_all(home.0.join("companion")).unwrap();
        std::fs::write(home.0.join("companion/vapid.key"), b"short").unwrap();
        let service = Harness::start_in(home, true);
        let phone = service.pair(IPHONE);
        let cookie = ("Cookie", phone.as_str());
        assert_eq!(service.request("GET", KEY, &[cookie], None).status, 503);
        let subscribed = put_subscription(&service, &phone, "https://web.push.apple.com/QXX");
        assert_eq!(subscribed.status, 503);
        let test = service.request("POST", TEST_PUSH, &[JSON, cookie], Some("{}"));
        assert_eq!(test.status, 503);
        assert_eq!(test.json()["error"], "push_unavailable");
        let fleet = service.request("GET", "/idema-api/fleet", &[cookie], None);
        assert_eq!(fleet.status, 200);
        assert_eq!(
            std::fs::read(service.home.0.join("companion/vapid.key")).unwrap(),
            b"short",
            "a damaged key is never replaced"
        );
    }

    #[test]
    fn only_one_test_push_runs_at_a_time() {
        let fake = FakePush::start_slow(Duration::from_millis(1500));
        let service = Arc::new(Harness::start_with_push(
            TempDir::new("push-busy"),
            fake.url(),
        ));
        let phone = service.pair(IPHONE);
        put_subscription(&service, &phone, "https://web.push.apple.com/QXX-busy");

        let first = {
            let (service, phone) = (Arc::clone(&service), phone.clone());
            std::thread::spawn(move || {
                service.request("POST", TEST_PUSH, &[JSON, ("Cookie", &phone)], Some("{}"))
            })
        };
        // La requête est arrivée au service push : l'envoi est en cours.
        fake.wait_for(1);
        let second = service.request("POST", TEST_PUSH, &[JSON, ("Cookie", &phone)], Some("{}"));
        assert_eq!(second.status, 429, "{}", second.text());
        assert_eq!(second.json()["error"], "test_in_progress");

        assert_eq!(first.join().unwrap().status, 200);
        assert_eq!(fake.requests().len(), 1, "the refused call sent nothing");
        // L'envoi fini, la place est rendue.
        let third = service.request("POST", TEST_PUSH, &[JSON, ("Cookie", &phone)], Some("{}"));
        assert_eq!(third.status, 200, "{}", third.text());
    }

    /// `devices.json` est un fichier : un abonnement que `build` refuse (hôte hors liste, motif de globbing) ne doit
    /// pas rester, retenté à chaque épisode pour rien.
    #[test]
    fn a_stored_subscription_that_cannot_be_sent_is_removed() {
        for bad in [
            "https://evil.example/x",
            "https://fcm.googleapis.com/fcm/send/x[1-99999]",
        ] {
            let first = Harness::start(true);
            let phone = first.pair(IPHONE);
            put_subscription(&first, &phone, "https://web.push.apple.com/QXX-stored");
            let home = first.stop();
            let path = home.0.join("companion/devices.json");
            let edited = std::fs::read_to_string(&path)
                .unwrap()
                .replace("https://web.push.apple.com/QXX-stored", bad);
            std::fs::write(&path, edited).unwrap();

            let fake = FakePush::start(201, "");
            let service = Harness::start_with_push(home, fake.url());
            assert_eq!(me(&service, &phone)["subscribed"], true, "{bad}");
            let reply = service.request("POST", TEST_PUSH, &[JSON, ("Cookie", &phone)], Some("{}"));
            assert_eq!(reply.status, 502, "{bad}");
            assert_eq!(reply.json()["result"], "gone", "{bad}");
            assert_eq!(stored_endpoints(&service), [None], "{bad}");
            assert_eq!(me(&service, &phone)["subscribed"], false, "{bad}");
            std::thread::sleep(Duration::from_millis(200));
            assert!(fake.requests().is_empty(), "nothing is sent to {bad}");
        }
    }

    // ---- Scratchpad ----

    const SCRATCHPAD: &str = "/idema-api/scratchpad";
    const APPEND: &str = "/idema-api/scratchpad/append";

    fn append(service: &Harness, cookie: &str, body: &str) -> Reply {
        service.request("POST", APPEND, &[JSON, ("Cookie", cookie)], Some(body))
    }

    #[test]
    fn the_scratchpad_reads_the_note_and_appends_a_line_at_its_end() {
        let service = Harness::start(true);
        let phone = service.pair(IPHONE);
        let note = service.home.0.join("scratchpad.md");

        // Pas encore de note : texte vide et date inconnue.
        let missing = service.request("GET", SCRATCHPAD, &[("Cookie", &phone)], None);
        assert_eq!(missing.status, 200, "{}", missing.text());
        assert_eq!(missing.json()["text"], "");
        assert!(missing.json()["modified_at"].is_null());

        std::fs::write(&note, "from the Mac\r\n").unwrap();
        let reply = append(&service, &phone, r#"{"line":"from the phone é"}"#);
        assert_eq!(reply.status, 200, "{}", reply.text());
        assert_eq!(
            std::fs::read_to_string(&note).unwrap(),
            "from the Mac\r\nfrom the phone é\r\n"
        );

        let read = service.request("GET", SCRATCHPAD, &[("Cookie", &phone)], None);
        assert_eq!(read.status, 200);
        assert_eq!(read.header("content-type"), Some("application/json"));
        assert_hardened(&read, "scratchpad");
        assert_eq!(read.json()["text"], "from the Mac\r\nfrom the phone é\r\n");
        assert!(read.json()["modified_at"].as_u64().unwrap() > 0);
    }

    #[test]
    fn the_scratchpad_refuses_a_bad_line_and_leaves_the_note_alone() {
        let service = Harness::start(true);
        let phone = service.pair(IPHONE);
        let note = service.home.0.join("scratchpad.md");
        std::fs::write(&note, "kept\n").unwrap();
        let long = format!(r#"{{"line":"{}"}}"#, "x".repeat(2_001));
        let cases = [
            (r#"{"line":""}"#.to_owned(), "empty"),
            (long, "too_long"),
            (r#"{"line":"two\nlines"}"#.to_owned(), "control_char"),
            (r#"{"line":"tab\there"}"#.to_owned(), "control_char"),
            (r#"{"line":"ok","extra":1}"#.to_owned(), "bad_request"),
            (r#"{"line":42}"#.to_owned(), "bad_request"),
            ("not json".to_owned(), "bad_request"),
            (String::new(), "bad_request"),
        ];
        for (body, code) in cases {
            let reply = append(&service, &phone, &body);
            assert_eq!(reply.status, 400, "{body:.40}: {}", reply.text());
            assert_eq!(reply.json()["error"], code, "{body:.40}");
        }
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "kept\n");
    }

    #[test]
    fn the_scratchpad_routes_sit_behind_the_guard() {
        let service = Harness::start(true);
        let phone = service.pair(IPHONE);
        let note = service.home.0.join("scratchpad.md");
        std::fs::write(&note, "kept\n").unwrap();
        let body = r#"{"line":"intruder"}"#;

        let anonymous = service.request("GET", SCRATCHPAD, &[], None);
        assert_eq!(anonymous.status, 401);
        assert!(!anonymous.text().contains("kept"));
        let unpaired = service.request("POST", APPEND, &[JSON], Some(body));
        assert_eq!(unpaired.status, 401);
        let text_plain = service.request(
            "POST",
            APPEND,
            &[("Content-Type", "text/plain"), ("Cookie", &phone)],
            Some(body),
        );
        assert_eq!(text_plain.status, 415);
        let foreign = service.request(
            "POST",
            APPEND,
            &[JSON, ("Cookie", &phone), ("Origin", "https://evil.example")],
            Some(body),
        );
        assert_eq!(foreign.status, 403);
        for reply in [&unpaired, &text_plain, &foreign] {
            assert_hardened(reply, "refused scratchpad write");
        }
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "kept\n");
    }

    #[test]
    fn the_scratchpad_follows_a_symlinked_note() {
        let service = Harness::start(true);
        let phone = service.pair(IPHONE);
        let target = service.home.0.join("dotfiles-notes.md");
        std::fs::write(&target, "linked\n").unwrap();
        let link = service.home.0.join("scratchpad.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(append(&service, &phone, r#"{"line":"more"}"#).status, 200);

        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "linked\nmore\n");
        let read = service.request("GET", SCRATCHPAD, &[("Cookie", &phone)], None);
        assert_eq!(read.json()["text"], "linked\nmore\n");
    }

    #[test]
    fn the_scratchpad_refuses_what_the_mac_editor_would_refuse() {
        let service = Harness::start(true);
        let phone = service.pair(IPHONE);
        let note = service.home.0.join("scratchpad.md");

        // Une note de plus de 5 Mo : 413 en lecture, 500 à l'ajout, rien n'est chargé ni écrit.
        let big = vec![b'a'; 5 * 1024 * 1024 + 1];
        std::fs::write(&note, &big).unwrap();
        let read = service.request("GET", SCRATCHPAD, &[("Cookie", &phone)], None);
        assert_eq!(read.status, 413, "{}", read.text());
        assert_eq!(read.json()["error"], "scratchpad_too_large");
        let refused = append(&service, &phone, r#"{"line":"more"}"#);
        assert_eq!(refused.status, 500);
        assert_eq!(refused.json()["error"], "scratchpad_unwritable");
        assert_eq!(std::fs::read(&note).unwrap(), big);
        std::fs::remove_file(&note).unwrap();

        // Un lien dont la cible manque : le lien reste, rien n'est créé.
        let missing = service.home.0.join("dotfiles-missing.md");
        std::os::unix::fs::symlink(&missing, &note).unwrap();
        let refused = append(&service, &phone, r#"{"line":"more"}"#);
        assert_eq!(refused.status, 500);
        assert_eq!(refused.json()["error"], "scratchpad_unwritable");
        assert_eq!(std::fs::read_link(&note).unwrap(), missing);
        assert!(!missing.exists());
        std::fs::remove_file(&note).unwrap();

        // Une note en lecture seule.
        std::fs::write(&note, "locked\n").unwrap();
        std::fs::set_permissions(&note, std::os::unix::fs::PermissionsExt::from_mode(0o444))
            .unwrap();
        let refused = append(&service, &phone, r#"{"line":"more"}"#);
        assert_eq!(refused.status, 500);
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "locked\n");
    }
}
