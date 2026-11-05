//! Les routes HTTP du service. Toutes les routes de données sont sous `/idema-api/` et passent par la garde ; les
//! fichiers de la page sont servis à la racine, sans cookie et sans donnée.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Extension, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::watch;

use super::ServiceEvent;
use super::access::{Access, Caller};
use super::auth::{COOKIE_NAME, cookie_header, origin_allowed};
use super::fleet::Fleet;
use super::sse::{self, COALESCE};
use super::timeline::Timeline;
use super::web_assets;
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
    /// L'adresse tailnet connue (`public_url`), dont l'origine est admise en écriture.
    pub known_origin: Option<String>,
    pub port: u16,
    /// Les routes de test existent-elles ? Décidé une fois par le contrôleur (`auth::test_routes_enabled`).
    pub test_routes: bool,
    /// Le dernier instantané de la flotte : `fleet` le lit, chaque flux `events` s'y abonne.
    pub fleet: watch::Receiver<Arc<Fleet>>,
    pub timeline: Arc<Mutex<Timeline>>,
}

pub fn router(state: AppState) -> Router {
    let mut router = Router::new()
        .route(HEALTH_PATH, get(health))
        .route(PAIR_PATH, post(pair))
        .route("/idema-api/me", get(me))
        .route("/idema-api/fleet", get(fleet))
        .route("/idema-api/events", get(events))
        .route("/idema-api/timeline", get(timeline))
        .route("/idema-api/client-metrics", post(client_metrics));
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
        if origin.is_none() || !origin_allowed(origin, state.known_origin.as_deref(), state.port) {
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
        serde_json::json!({ "ok": true, "version": state.version }).to_string(),
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

/// Les fichiers de la page, avec `ETag` et 304 ; tout autre chemin est un 404 (JSON sous `/idema-api/`).
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
    let etag = HeaderValue::from_static(asset.etag);
    let cache = HeaderValue::from_static("no-cache");
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|sent| etag_matches(sent, asset.etag))
    {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().insert(header::ETAG, etag);
        response.headers_mut().insert(header::CACHE_CONTROL, cache);
        return response;
    }
    let mut response = (StatusCode::OK, asset.bytes).into_response();
    let out = response.headers_mut();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    out.insert(header::ETAG, etag);
    out.insert(header::CACHE_CONTROL, cache);
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
        for (from, to) in [(Idle, Running), (Running, Waiting)] {
            service
                .events
                .send(ServiceEvent::Transition {
                    session: "idema-aaaaaaaa".into(),
                    from,
                    to,
                    at,
                })
                .unwrap();
        }
        // Le service range les transitions de façon asynchrone : on attend qu'elles se voient.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let events = loop {
            let events = get("").json()["events"].clone();
            if events.as_array().is_some_and(|list| !list.is_empty()) {
                break events;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no transition recorded"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
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
}
