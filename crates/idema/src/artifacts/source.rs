//! La source des artifacts : une `WebPage` cachée sur claude.ai, dans le magasin de cookies du volet Browser, qui rejoue
//! la requête de liste de l'app web elle-même. Le cookie seul ne suffit pas (404 sans les en-têtes de la page) : un
//! script injecté au début du document (`capture.js`) retient les en-têtes de la première requête de l'app, et IdemA
//! les rejoue avec son propre `fetch`, dans le monde de la page. Rien de claude.ai n'est écrit sur disque, et le journal
//! ne reçoit jamais une valeur d'en-tête, seulement des noms.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::{Either, select};
use gpui::{AnyWindowHandle, App, AppContext as _, AsyncApp, Context, Task, WeakEntity, Window};
use serde::Deserialize;
use serde_json::Value;

use super::model::{PAGE_SIZE, classify, query_string};
use super::{ApiError, ListQuery};
use crate::browser::webkit::{PageEvent, WebPage};
use crate::hud;

/// La page chargée : elle appelle l'API des artifacts au chargement, ce qui donne les en-têtes à retenir.
const ARTIFACTS_PAGE: &str = "https://claude.ai/artifacts?tab=mine";

/// Le script de capture, injecté au début de chaque document du cadre principal.
const CAPTURE_SCRIPT: &str = include_str!("capture.js");

/// Cadence du sondage de la capture, et durée au-delà de laquelle on renonce.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

/// L'état de la page, lu par sondage : la capture est-elle faite, sur quel chemin et sous quel titre (un défi
/// Cloudflare s'y reconnaît) la page est-elle ? `org` est le paramètre capturé, que Rust met dans la requête.
const STATE_BODY: &str = "const captured = window.__idemaArtifacts; return JSON.stringify({ ready: !!captured, path: location.pathname, title: document.title, org: (captured && captured.org) || '' });";

/// Rejoue la requête de liste avec les en-têtes retenus. La chaîne de requête arrive toute faite en argument (`query`,
/// construite par `query_string`) : le script ne connaît aucun paramètre. Rend l'état, le corps brut, les NOMS des
/// en-têtes rejoués et si la réponse est arrivée sur la page de connexion ; `status` 0 si la capture a disparu.
const REPLAY_BODY: &str = "\
const captured = window.__idemaArtifacts;
if (!captured) { return JSON.stringify({ status: 0, body: '', headers: [], org_present: false, landed_on_login: false }); }
const response = await window.fetch('/api/frame/artifacts?' + query, { method: 'GET', headers: captured.headers, credentials: 'include' });
const body = await response.text();
let landedOnLogin = false;
try { landedOnLogin = new URL(response.url, location.href).pathname.startsWith('/login'); } catch (error) {}
return JSON.stringify({ status: response.status, body: body, headers: Object.keys(captured.headers), org_present: !!captured.org, landed_on_login: landedOnLogin });";

#[derive(Deserialize)]
struct PageState {
    ready: bool,
    path: String,
    #[serde(default)]
    title: String,
    /// Le paramètre `org` capturé (jamais journalisé).
    #[serde(default)]
    org: String,
}

/// La réponse brute du rejeu : le code HTTP, le corps et les noms des en-têtes envoyés.
#[derive(Debug, Deserialize)]
pub struct RawResponse {
    pub status: u16,
    pub body: String,
    #[serde(default)]
    pub headers: Vec<String>,
    /// Le paramètre `org` a-t-il été capturé ? (sa valeur ne sort jamais)
    #[serde(default)]
    pub org_present: bool,
    /// La requête a-t-elle fini sur la page de connexion (redirection suivie par `fetch`) ?
    #[serde(default)]
    pub landed_on_login: bool,
}

/// La page cachée et ce qu'elle a signalé. Clonée par chaque demande en vol : elle ne tient pas l'entité.
#[derive(Clone)]
struct Session {
    page: Rc<WebPage>,
    /// Le dernier échec de chargement rapporté par la page (vide tant que tout va bien).
    failure: Rc<RefCell<Option<String>>>,
}

pub struct ArtifactsSource {
    window: AnyWindowHandle,
    /// Créée à la première demande, gardée entre deux ouvertures de la galerie, jamais montrée.
    session: Option<Session>,
    _reader: Option<Task<()>>,
}

impl ArtifactsSource {
    pub fn new(window: AnyWindowHandle) -> Self {
        Self {
            window,
            session: None,
            _reader: None,
        }
    }

    /// Lit une page de la liste et rend le corps JSON brut. `done` est rappelé sur le fil principal, une fois.
    pub fn fetch(
        &mut self,
        query: ListQuery,
        done: impl FnOnce(Result<String, ApiError>) + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let result = Self::request(&this, query.search, None, cx)
                .await
                .and_then(outcome);
            // Une page restée sur /login, un 4xx ou des en-têtes périmés ne se réparent pas à chaud : la prochaine
            // lecture repart d'un chargement neuf et d'une capture neuve.
            if should_reset(&result) {
                let _ = this.update(cx, |source, _| source.reset());
            }
            done(result);
        })
        .detach();
    }

    /// La page cachée, créée si besoin. `None` hors macOS ou sans fenêtre. Le chargement part dès la création.
    fn ensure_session(&mut self, window: &Window, cx: &mut Context<Self>) -> Option<Session> {
        if let Some(session) = &self.session {
            return Some(session.clone());
        }
        let (sender, mut receiver) = mpsc::unbounded();
        let page = WebPage::new_with_script(window, sender, CAPTURE_SCRIPT)?;
        page.load(ARTIFACTS_PAGE);
        let failure = Rc::new(RefCell::new(None));
        let reported = failure.clone();
        self._reader = Some(cx.spawn(async move |_, _| {
            while let Some(event) = receiver.next().await {
                match event {
                    PageEvent::Failed { description, .. } => {
                        *reported.borrow_mut() = Some(description);
                    }
                    PageEvent::Terminated => {
                        *reported.borrow_mut() = Some("web content process terminated".to_owned());
                    }
                    _ => {}
                }
            }
        }));
        let session = Session {
            page: Rc::new(page),
            failure,
        };
        self.session = Some(session.clone());
        Some(session)
    }

    /// Oublie la page : la prochaine demande en recrée une.
    fn reset(&mut self) {
        self.session = None;
        self._reader = None;
    }

    /// Attend la capture des en-têtes, puis rejoue la requête. Rend la réponse brute, sans l'interpréter. Toute erreur
    /// oublie la page cachée : la prochaine demande repart d'un chargement et d'une capture neufs (connexion faite
    /// entre-temps, défi Cloudflare résolu, changement de compte ou d'organisation, en-têtes périmés).
    async fn request(
        this: &WeakEntity<Self>,
        search: Option<String>,
        offset: Option<u32>,
        cx: &mut AsyncApp,
    ) -> Result<RawResponse, ApiError> {
        let result = Self::request_once(this, search, offset, cx).await;
        if should_reset(&result) {
            let _ = this.update(cx, |source, _| source.reset());
        }
        result
    }

    async fn request_once(
        this: &WeakEntity<Self>,
        search: Option<String>,
        offset: Option<u32>,
        cx: &mut AsyncApp,
    ) -> Result<RawResponse, ApiError> {
        let released = || ApiError::Unavailable("source released".to_owned());
        let window = this
            .read_with(cx, |source, _| source.window)
            .map_err(|_| released())?;
        let session = cx
            .update_window(window, |_, window, cx| {
                this.update(cx, |source, cx| source.ensure_session(window, cx))
            })
            .map_err(|_| released())?
            .map_err(|_| released())?
            .ok_or_else(|| ApiError::Unavailable("hidden page unavailable".to_owned()))?;

        let deadline = Instant::now() + CAPTURE_TIMEOUT;
        // Le dernier état lu et la dernière erreur de sondage : sans eux, un délai dépassé ne dit pas pourquoi.
        let mut last_state: Option<(String, String)> = None;
        let mut last_error: Option<String> = None;
        let org;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match evaluate_within(cx, &session.page, STATE_BODY, Vec::new(), remaining).await {
                Ok(json) => match serde_json::from_str::<PageState>(&json) {
                    Ok(state) => {
                        if state.ready {
                            org = state.org;
                            break;
                        }
                        if state.path.starts_with("/login") {
                            return Err(ApiError::SignedOut);
                        }
                        last_state = Some((state.path, state.title));
                    }
                    Err(error) => last_error = Some(format!("unreadable state: {error}")),
                },
                // Une erreur de script (page pas encore prête) vaut « pas prête » : on retente jusqu'au délai.
                Err(message) => last_error = Some(message),
            }
            let failure = session.failure.borrow_mut().take();
            if let Some(message) = failure {
                return Err(ApiError::Unavailable(message));
            }
            if Instant::now() >= deadline {
                return Err(ApiError::Unavailable(timeout_reason(
                    last_state.as_ref(),
                    last_error.as_deref(),
                )));
            }
            cx.background_executor().timer(POLL_INTERVAL).await;
        }

        // `offset` ne sert qu'à la sonde (l'API l'ignore) : il se greffe après la requête de la liste.
        let mut query = query_string(&ListQuery { search }, &org);
        if let Some(offset) = offset {
            query.push_str(&format!("&offset={offset}"));
        }
        let arguments = vec![("query".to_owned(), query)];
        let json = evaluate_within(cx, &session.page, REPLAY_BODY, arguments, CAPTURE_TIMEOUT)
            .await
            .map_err(ApiError::Unavailable)?;
        let raw: RawResponse = serde_json::from_str(&json).map_err(|_| ApiError::Unreadable)?;
        if raw.status == 0 {
            return Err(ApiError::Unavailable("capture lost".to_owned()));
        }
        Ok(raw)
    }
}

/// `call_async` sous forme de futur. Une page détruite avant la réponse rend une erreur.
async fn evaluate(
    page: &WebPage,
    body: &str,
    arguments: Vec<(String, String)>,
) -> Result<String, String> {
    let (sender, receiver) = oneshot::channel();
    page.call_async(body, arguments, move |result| {
        let _ = sender.send(result);
    });
    receiver
        .await
        .unwrap_or_else(|_| Err("page released before the answer".to_owned()))
}

/// `call_async` borné : si WebKit ne répond pas avant `limit`, le futur rend une erreur au lieu de suspendre la boucle.
async fn evaluate_within(
    cx: &AsyncApp,
    page: &WebPage,
    body: &str,
    arguments: Vec<(String, String)>,
    limit: Duration,
) -> Result<String, String> {
    let call = evaluate(page, body, arguments);
    futures::pin_mut!(call);
    let timer = cx.background_executor().timer(limit);
    match select(call, timer).await {
        Either::Left((result, _)) => result,
        Either::Right(_) => Err("script call timed out".to_owned()),
    }
}

/// La raison d'un délai de capture : chemin et titre de la dernière page lue, dernière erreur de sondage. Ni en-tête ni
/// cookie ni `org`.
fn timeout_reason(state: Option<&(String, String)>, last_error: Option<&str>) -> String {
    let (path, title) = state.map_or(("-", "-"), |(path, title)| (path.as_str(), title.as_str()));
    format!(
        "capture timeout path={path} title={title:?} last_error={}",
        last_error.unwrap_or("-")
    )
}

/// La page cachée est-elle à oublier après ce résultat ? Oui pour toute erreur (déconnexion, HTTP, illisible, capture
/// perdue ou en délai) : la page est alors périmée ou sur /login. Non pour un succès.
fn should_reset<T>(result: &Result<T, ApiError>) -> bool {
    result.is_err()
}

/// Le corps d'un 200 resté sur l'API, sinon la raison que donne `classify`.
fn outcome(raw: RawResponse) -> Result<String, ApiError> {
    match classify(raw.status, raw.landed_on_login) {
        None => Ok(raw.body),
        Some(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// La sonde de la phase 0 (`IDEMA_ARTIFACTS_PROBE=1`).

/// Lance la sonde : une lecture de la première page, et si `hosted_total` dépasse ce qu'elle rend, un essai
/// d'`offset=48`. Le résultat va au journal, lignes `artifacts probe ...`.
pub fn run_probe(window: AnyWindowHandle, cx: &mut App) {
    let source = cx.new(|_| ArtifactsSource::new(window));
    cx.spawn(async move |cx| {
        let weak = source.downgrade();
        probe(&weak, cx).await;
        // La page cachée vit autant que l'entité : on la garde jusqu'au bout de la sonde.
        drop(source);
    })
    .detach();
}

async fn probe(source: &WeakEntity<ArtifactsSource>, cx: &mut AsyncApp) {
    let first = match ArtifactsSource::request(source, None, None, cx).await {
        Ok(raw) => raw,
        Err(error) => {
            hud::log_event(&format!("artifacts probe error={error:?}"));
            return;
        }
    };
    hud::log_event(&probe_line(&first));
    let summary = summarize(&first.body);
    let (Some(count), Some(total)) = (summary.count, summary.hosted_total) else {
        return;
    };
    if total <= count as u64 {
        return;
    }
    // Plus d'éléments hébergés que la page n'en rend : `offset` est-il accepté ?
    match ArtifactsSource::request(source, None, Some(PAGE_SIZE), cx).await {
        Ok(second) => {
            let again = summarize(&second.body);
            hud::log_event(&format!(
                "artifacts probe offset={PAGE_SIZE} status={} count={} same_first_item={}",
                second.status,
                number_or_dash(again.count),
                summary.first_item.is_some() && summary.first_item == again.first_item,
            ));
        }
        Err(error) => hud::log_event(&format!(
            "artifacts probe offset={PAGE_SIZE} error={error:?}"
        )),
    }
}

/// Ce que la sonde retient d'un corps de liste.
#[derive(Debug, Default, PartialEq)]
struct Summary {
    count: Option<usize>,
    hosted_total: Option<u64>,
    first_item: Option<Value>,
}

fn summarize(body: &str) -> Summary {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Summary::default();
    };
    let items = value.get("artifacts").and_then(Value::as_array);
    Summary {
        count: items.map(Vec::len),
        hosted_total: value.get("hosted_total").and_then(Value::as_u64),
        first_item: items.and_then(|items| items.first().cloned()),
    }
}

fn number_or_dash<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

/// `artifacts probe status=<code> count=<n> hosted_total=<n> org=present|absent headers=<noms>` : des noms, jamais une valeur.
fn probe_line(raw: &RawResponse) -> String {
    let summary = summarize(&raw.body);
    format!(
        "artifacts probe status={} count={} hosted_total={} org={} headers={}",
        raw.status,
        number_or_dash(summary.count),
        number_or_dash(summary.hosted_total),
        if raw.org_present { "present" } else { "absent" },
        raw.headers.join(","),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(status: u16, body: &str) -> RawResponse {
        RawResponse {
            status,
            body: body.to_owned(),
            headers: vec![
                "x-frame-platform".to_owned(),
                "anthropic-device-id".to_owned(),
            ],
            org_present: true,
            landed_on_login: false,
        }
    }

    #[test]
    fn probe_line_reports_counts_and_header_names_only() {
        let line = probe_line(&raw(
            200,
            r#"{"artifacts":[{"id":"a"},{"id":"b"}],"hosted_total":9}"#,
        ));
        assert_eq!(
            line,
            "artifacts probe status=200 count=2 hosted_total=9 org=present headers=x-frame-platform,anthropic-device-id"
        );
    }

    #[test]
    fn probe_line_survives_an_unreadable_body() {
        let line = probe_line(&raw(404, "<html>nope</html>"));
        assert!(line.starts_with("artifacts probe status=404 count=- hosted_total=- org=present "));
    }

    #[test]
    fn only_a_200_yields_a_body() {
        assert_eq!(outcome(raw(200, "{}")), Ok("{}".to_owned()));
        assert_eq!(outcome(raw(401, "")), Err(ApiError::SignedOut));
        assert_eq!(outcome(raw(403, "")), Err(ApiError::SignedOut));
        assert_eq!(outcome(raw(404, "")), Err(ApiError::Http(404)));
        let mut redirected = raw(200, "<html>login</html>");
        redirected.landed_on_login = true;
        assert_eq!(outcome(redirected), Err(ApiError::SignedOut));
    }

    #[test]
    fn every_error_resets_the_hidden_page_and_a_success_keeps_it() {
        assert!(!should_reset(&outcome(raw(200, "{}"))));
        for status in [401, 403, 404, 500] {
            assert!(should_reset(&outcome(raw(status, ""))), "status {status}");
        }
        let mut redirected = raw(200, "<html>login</html>");
        redirected.landed_on_login = true;
        assert!(should_reset(&outcome(redirected)));
        for error in [
            ApiError::SignedOut,
            ApiError::Unreadable,
            ApiError::Unavailable("capture lost".to_owned()),
        ] {
            assert!(should_reset::<()>(&Err(error)));
        }
    }

    #[test]
    fn timeout_reason_names_the_last_page_and_error() {
        let state = ("/artifacts".to_owned(), "Just a moment...".to_owned());
        assert_eq!(
            timeout_reason(Some(&state), Some("script call timed out")),
            r#"capture timeout path=/artifacts title="Just a moment..." last_error=script call timed out"#
        );
        assert_eq!(
            timeout_reason(None, None),
            r#"capture timeout path=- title="-" last_error=-"#
        );
    }

    #[test]
    fn replay_result_parses_from_the_script_shape() {
        let parsed: RawResponse = serde_json::from_str(
            r#"{"status":200,"body":"{}","headers":["a","b"],"org_present":true,"landed_on_login":true}"#,
        )
        .unwrap();
        assert_eq!(
            (
                parsed.status,
                parsed.headers.len(),
                parsed.org_present,
                parsed.landed_on_login
            ),
            (200, 2, true, true)
        );
    }
}
