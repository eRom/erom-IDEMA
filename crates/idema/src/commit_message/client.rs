//! Le transport vers Gemini (spec B-11 §4.6 et §4.7) : `/usr/bin/curl` en processus, configuration sur son entrée
//! standard (la clé ne paraît ni dans les arguments, ni dans l'URL), lecture du flux SSE, classement des erreurs.
//! Pur : aucun GPUI, le pied du volet Changes l'appelle.

use std::io::{ErrorKind, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use serde_json::{Value, json};

// Le faux serveur Gemini de l'exemple, réutilisé tel quel par les tests du transport.
#[cfg(test)]
#[path = "../../examples/fake_gemini.rs"]
mod fake_gemini;

pub const DEFAULT_ENDPOINT: &str = "https://generativelanguage.googleapis.com";

/// Le seul curl que l'on lance : celui du système, jamais celui du `PATH`.
const CURL: &str = "/usr/bin/curl";

/// Les arguments de curl. `-q` en tout premier ignore le `~/.curlrc` de l'utilisateur (un `fail`, un `-v` ou un `trace`
/// y changerait le corps d'erreur ou écrirait l'en-tête de la clé sur stderr). La ligne `IDEMA_HTTP` sort sur stderr
/// après le corps ; la configuration vient de stdin.
const CURL_ARGS: [&str; 11] = [
    "-q",
    "-sS",
    "-N",
    "--connect-timeout",
    "10",
    "--max-time",
    "60",
    "-w",
    r"%{stderr}IDEMA_HTTP %{http_code}\n",
    "--config",
    "-",
];

/// Ce que l'on garde du corps de la réponse pour le détail d'une erreur : un corps d'erreur de l'API tient dans
/// quelques centaines d'octets, un flux réussi n'a pas à grossir en mémoire.
const MAX_RAW_BYTES: usize = 64 * 1024;

/// `{endpoint}/v1beta/models/{model}:streamGenerateContent?alt=sse`.
pub fn stream_url(endpoint: &str, model: &str) -> String {
    format!(
        "{}/v1beta/models/{model}:streamGenerateContent?alt=sse",
        endpoint.trim_end_matches('/')
    )
}

/// Le corps JSON compact : un seul message `user`, sans `generationConfig` (le défaut du modèle `gemini-3.5-flash-lite`
/// est déjà la réflexion `minimal` ; `thinkingLevel` est refusé par la série 2.5 et `minimal` par Gemini 3.8 Flash, ce
/// qui casserait le réglage `model`). Il commence toujours par `{` : jamais pris pour un `@fichier` par `data-binary`.
pub fn request_body(prompt: &str) -> String {
    json!({
        "contents": [{"role": "user", "parts": [{"text": prompt}]}],
    })
    .to_string()
}

/// Une valeur entre guillemets dans une configuration curl : `\` doublé, `"` échappé. Les retours à la ligne et
/// tabulations réels sont écrits en échappements : une clé collée avec un saut de ligne ne doit pas pouvoir ouvrir
/// une directive de plus.
fn escape_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// La configuration que curl lit sur son entrée standard : l'URL, deux en-têtes, le corps. La clé n'est que sur
/// la ligne `header = "x-goog-api-key: …"`.
pub fn curl_config(url: &str, api_key: &str, body: &str) -> String {
    format!(
        "url = \"{}\"\nheader = \"Content-Type: application/json\"\nheader = \"x-goog-api-key: {}\"\ndata-binary = \"{}\"\n",
        escape_value(url),
        escape_value(api_key),
        escape_value(body),
    )
}

/// Ce que le flux SSE a donné : un morceau de texte, ou un événement d'erreur de l'API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    Text(String),
    Error(String),
}

/// Lecteur incrémental du flux SSE de Gemini : `\n` et `\r\n` finissent une ligne, une ligne vide ferme
/// l'événement. Les octets attendent la fin de ligne et ne sont décodés qu'entiers : un caractère coupé entre deux
/// lectures n'est jamais abîmé.
#[derive(Debug, Default)]
pub struct SseParser {
    /// La ligne en cours, sans son `\n`.
    line: Vec<u8>,
    /// Les lignes `data:` de l'événement en cours.
    data: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.read_line(&line, &mut events);
            } else {
                self.line.push(byte);
            }
        }
        events
    }

    /// La fin du flux : la dernière ligne sans saut de ligne et l'événement resté ouvert comptent encore.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.read_line(&line, &mut events);
        }
        self.dispatch(&mut events);
        events
    }

    fn read_line(&mut self, raw: &[u8], events: &mut Vec<StreamEvent>) {
        let decoded = String::from_utf8_lossy(raw);
        let line = decoded.strip_suffix('\r').unwrap_or(&decoded);
        if line.is_empty() {
            self.dispatch(events);
            return;
        }
        // Un commentaire SSE commence par `:` ; seul le champ `data` nous intéresse (Gemini n'envoie pas d'autre).
        let Some((field, value)) = line.split_once(':') else {
            return;
        };
        if field == "data" {
            self.data
                .push(value.strip_prefix(' ').unwrap_or(value).to_string());
        }
    }

    fn dispatch(&mut self, events: &mut Vec<StreamEvent>) {
        if self.data.is_empty() {
            return;
        }
        let payload = self.data.join("\n");
        self.data.clear();
        interpret(&payload, events);
    }
}

/// Un événement `data:` : une erreur de l'API, ou le texte des parts de `candidates[0]`. Les parts de réflexion
/// (`"thought": true`) sont écartées ; un texte vide (l'événement final porte `finishReason` et rien d'autre)
/// ne produit rien. Le texte est rendu tel quel : aucun nettoyage (comportement de Zed, B11-R4).
fn interpret(payload: &str, events: &mut Vec<StreamEvent>) {
    let payload = payload.trim();
    if payload == "[DONE]" {
        return;
    }
    let value: Value = match serde_json::from_str(payload) {
        Ok(value) => value,
        Err(error) => {
            events.push(StreamEvent::Error(format!(
                "unreadable stream event: {error}"
            )));
            return;
        }
    };
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| error.to_string(), str::to_string);
        events.push(StreamEvent::Error(message));
        return;
    }
    let Some(parts) = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
    else {
        return;
    };
    let text: String = parts
        .iter()
        .filter(|part| part.get("thought").and_then(Value::as_bool) != Some(true))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    if !text.is_empty() {
        events.push(StreamEvent::Text(text));
    }
}

/// Pourquoi une génération a échoué (spec §4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationError {
    KeyRejected,
    ModelUnavailable,
    QuotaExceeded,
    Unreachable,
    CurlMissing,
    Failed,
}

/// Le motif de la vraie API pour une clé fausse : 400 `API_KEY_INVALID` (la sonde du 19/10/2026), alors qu'une
/// requête mal formée est un 400 `INVALID_ARGUMENT`.
const KEY_INVALID_MARKER: &str = "API_KEY_INVALID";

/// Classe l'issue de curl. Le code HTTP prime ; sans code HTTP en erreur, la sortie de curl décide
/// (6 hôte, 7 connexion, 28 délai, 35 TLS : injoignable ; toute autre sortie non nulle : échec). `None` : succès.
pub fn classify(
    http_status: Option<u16>,
    curl_exit: Option<i32>,
    body: &str,
) -> Option<GenerationError> {
    if let Some(status) = http_status.filter(|status| *status >= 400) {
        return Some(match status {
            403 => GenerationError::KeyRejected,
            400 if body.contains(KEY_INVALID_MARKER) => GenerationError::KeyRejected,
            404 => GenerationError::ModelUnavailable,
            429 => GenerationError::QuotaExceeded,
            _ => GenerationError::Failed,
        });
    }
    match curl_exit {
        Some(0) | None => None,
        Some(6 | 7 | 28 | 35) => Some(GenerationError::Unreachable),
        Some(_) => Some(GenerationError::Failed),
    }
}

/// Ce que le pied du volet Changes passe pour une génération.
pub struct GenerationRequest {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub prompt: String,
}

/// Ce que le fil de lecture envoie : chaque morceau de texte, puis `Done` ou `Failed`, jamais les deux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationUpdate {
    Text(String),
    Done,
    Failed { error: GenerationError, raw: String },
}

/// L'état partagé entre la poignée d'annulation et le fil de lecture.
struct Process {
    cancelled: bool,
    child: Option<Child>,
}

fn lock(process: &Mutex<Process>) -> MutexGuard<'_, Process> {
    process.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Marque la génération annulée, tue curl et attend sa fin. Idempotent.
fn stop(process: &Mutex<Process>) {
    let mut state = lock(process);
    state.cancelled = true;
    if let Some(mut child) = state.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// L'annulation d'une génération. Annuler ferme le canal sans `Done` ni `Failed` ; lâcher la poignée annule aussi.
pub struct CancelHandle {
    process: Arc<Mutex<Process>>,
}

impl CancelHandle {
    /// Tue curl et attend sa fin ; sans effet si la génération est finie ou déjà annulée.
    pub fn cancel(&self) {
        stop(&self.process);
    }

    /// Le pid de curl tant qu'il tourne (pour prouver, dans les tests, que l'annulation le tue vraiment).
    #[cfg(test)]
    fn pid(&self) -> Option<u32> {
        lock(&self.process).child.as_ref().map(Child::id)
    }
}

impl Drop for CancelHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Lance la génération dans un fil et rend l'annulation et le canal des mises à jour.
pub fn spawn_generation(
    request: GenerationRequest,
) -> (CancelHandle, UnboundedReceiver<GenerationUpdate>) {
    spawn_with(CURL.to_string(), Vec::new(), request)
}

/// `envs` : variables d'environnement ajoutées au seul processus curl (les tests y posent un `HOME` jetable).
fn spawn_with(
    curl_path: String,
    envs: Vec<(String, String)>,
    request: GenerationRequest,
) -> (CancelHandle, UnboundedReceiver<GenerationUpdate>) {
    let (sender, receiver) = unbounded();
    let process = Arc::new(Mutex::new(Process {
        cancelled: false,
        child: None,
    }));
    let shared = Arc::clone(&process);
    thread::Builder::new()
        .name("idema-gemini".into())
        .spawn(move || run_generation(&curl_path, &envs, request, &shared, &sender))
        .expect("spawn the generation thread");
    (CancelHandle { process }, receiver)
}

/// Envoie une mise à jour tant que la génération n'est pas annulée. Le contrôle et l'envoi se font sous le même
/// verrou que `stop` : une fois `cancel()` revenu, plus rien ne part, ni texte, ni `Done`, ni `Failed`.
/// `false` : annulée, ou plus personne n'écoute.
fn send(
    process: &Mutex<Process>,
    sender: &UnboundedSender<GenerationUpdate>,
    update: GenerationUpdate,
) -> bool {
    let state = lock(process);
    !state.cancelled && sender.unbounded_send(update).is_ok()
}

/// Passe les événements du flux au canal. `false` : annulée, ou plus personne n'écoute.
fn forward(
    events: Vec<StreamEvent>,
    stream_error: &mut Option<String>,
    process: &Mutex<Process>,
    sender: &UnboundedSender<GenerationUpdate>,
) -> bool {
    for event in events {
        match event {
            StreamEvent::Text(text) => {
                if !send(process, sender, GenerationUpdate::Text(text)) {
                    return false;
                }
            }
            StreamEvent::Error(message) => {
                stream_error.get_or_insert(message);
            }
        }
    }
    true
}

fn run_generation(
    curl_path: &str,
    envs: &[(String, String)],
    request: GenerationRequest,
    process: &Arc<Mutex<Process>>,
    sender: &UnboundedSender<GenerationUpdate>,
) {
    let fail = |error: GenerationError, raw: String| {
        send(process, sender, GenerationUpdate::Failed { error, raw });
    };

    let config = curl_config(
        &stream_url(&request.endpoint, &request.model),
        &request.api_key,
        &request_body(&request.prompt),
    );
    let spawned = Command::new(curl_path)
        .args(CURL_ARGS)
        .envs(envs.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return fail(GenerationError::CurlMissing, String::new());
        }
        Err(error) => return fail(GenerationError::Failed, error.to_string()),
    };
    let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        let _ = child.wait();
        return fail(GenerationError::Failed, "curl pipes unavailable".into());
    };

    // Une annulation arrivée avant que le processus soit rangé gagne : on le tue ici.
    {
        let mut state = lock(process);
        if state.cancelled {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        state.child = Some(child);
    }

    // stderr dans son fil : curl n'y écrit que peu, mais un tube plein le bloquerait.
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr
            .by_ref()
            .take(MAX_RAW_BYTES as u64)
            .read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    });

    // curl lit toute sa configuration avant d'ouvrir la connexion ; l'entrée se ferme ensuite. Une écriture qui
    // échoue (curl mort ou tué) se lit dans sa sortie, pas ici.
    let _ = stdin.write_all(config.as_bytes());
    drop(stdin);

    let mut parser = SseParser::default();
    let mut body = Vec::new();
    let mut stream_error = None;
    let mut buffer = [0u8; 8192];
    loop {
        let read = match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let keep = read.min(MAX_RAW_BYTES.saturating_sub(body.len()));
        body.extend_from_slice(&buffer[..keep]);
        if !forward(
            parser.push(&buffer[..read]),
            &mut stream_error,
            process,
            sender,
        ) {
            stop(process);
            return;
        }
    }
    if !forward(parser.finish(), &mut stream_error, process, sender) {
        stop(process);
        return;
    }

    let stderr_text = stderr_reader.join().unwrap_or_default();
    let Some(mut child) = lock(process).child.take() else {
        return; // annulée : le canal se ferme sans rien de plus
    };
    let status = child.wait();

    let curl_exit = match status {
        Ok(status) => Some(status.code().unwrap_or(-1)),
        Err(_) => Some(-1),
    };
    let (http_status, curl_message) = split_stderr(&stderr_text);
    let body_text = String::from_utf8_lossy(&body);
    let update = match classify(http_status, curl_exit, &body_text) {
        Some(error) => GenerationUpdate::Failed {
            error,
            raw: failure_raw(http_status, &body_text, &curl_message, &request.api_key),
        },
        None => match stream_error {
            Some(message) => GenerationUpdate::Failed {
                error: GenerationError::Failed,
                raw: scrub(&message, &request.api_key),
            },
            None => GenerationUpdate::Done,
        },
    };
    send(process, sender, update);
}

/// Sépare la ligne `IDEMA_HTTP <code>` du reste du stderr de curl. Le code `000` (rien reçu) devient `None`.
fn split_stderr(stderr: &str) -> (Option<u16>, String) {
    let mut status = None;
    let mut rest = Vec::new();
    for line in stderr.lines() {
        match line.strip_prefix("IDEMA_HTTP ") {
            Some(code) => status = code.trim().parse::<u16>().ok().filter(|code| *code != 0),
            None => rest.push(line),
        }
    }
    (status, rest.join("\n"))
}

/// Le message de l'API dans un corps d'erreur JSON : `{"error":{"message":…}}` (ou le même dans un tableau).
fn api_error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body.trim()).ok()?;
    let message = value
        .pointer("/error/message")
        .or_else(|| value.pointer("/0/error/message"))?;
    message.as_str().map(str::to_string)
}

/// Le détail d'un échec : le message du corps d'erreur de l'API (ou le corps brut) pour un code HTTP en erreur,
/// sinon le stderr de curl. La clé en est toujours retirée.
fn failure_raw(http_status: Option<u16>, body: &str, curl_message: &str, api_key: &str) -> String {
    let from_body = match http_status {
        Some(status) if status >= 400 => {
            api_error_message(body).unwrap_or_else(|| body.trim().to_string())
        }
        _ => String::new(),
    };
    let detail = if from_body.is_empty() {
        curl_message.trim().to_string()
    } else {
        from_body
    };
    scrub(&detail, api_key)
}

/// Retire la clé d'un texte destiné à l'écran ou au journal.
fn scrub(text: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        text.to_string()
    } else {
        text.replace(api_key, "***")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use futures::channel::mpsc::TryRecvError;

    use super::fake_gemini::{FakeOptions, serve};
    use super::*;

    const SAMPLE: &str = include_str!("testdata/sse-sample.txt");
    const KEY: &str = "fake-key-for-tests";

    fn texts(events: &[StreamEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::Text(text) => Some(text.as_str()),
                StreamEvent::Error(_) => None,
            })
            .collect()
    }

    fn parse_whole(input: &str) -> Vec<StreamEvent> {
        let mut parser = SseParser::default();
        let mut events = parser.push(input.as_bytes());
        events.extend(parser.finish());
        events
    }

    #[test]
    fn sse_parser_reads_the_probe_sample() {
        let events = parse_whole(SAMPLE);
        assert!(
            events
                .iter()
                .all(|event| matches!(event, StreamEvent::Text(_)))
        );
        // Le texte réel est en deux événements dont le dernier est vide : un seul `Text`.
        assert_eq!(texts(&events).concat(), "`docs: add README`");
    }

    #[test]
    fn sse_parser_gives_the_same_events_whether_pushed_whole_or_byte_by_byte() {
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        for byte in SAMPLE.as_bytes() {
            events.extend(parser.push(&[*byte]));
        }
        events.extend(parser.finish());
        assert_eq!(events, parse_whole(SAMPLE));
    }

    #[test]
    fn sse_parser_keeps_a_char_split_across_reads() {
        let input = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"é\"}]}}]}\n\n";
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        for byte in input.as_bytes() {
            events.extend(parser.push(&[*byte]));
        }
        events.extend(parser.finish());
        assert_eq!(events, vec![StreamEvent::Text("é".to_string())]);
    }

    #[test]
    fn sse_parser_accepts_lf_and_crlf_line_ends() {
        let lf = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}\n\n";
        let crlf = lf.replace('\n', "\r\n");
        assert_eq!(parse_whole(lf), vec![StreamEvent::Text("a".to_string())]);
        assert_eq!(parse_whole(&crlf), parse_whole(lf));
    }

    #[test]
    fn sse_parser_keeps_the_text_as_received() {
        // Aucun nettoyage : les accents graves et les sauts de ligne du modèle restent (B11-R4).
        let input =
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"`fix: x`\\n\"}]}}]}\n\n";
        assert_eq!(
            parse_whole(input),
            vec![StreamEvent::Text("`fix: x`\n".to_string())]
        );
    }

    #[test]
    fn sse_parser_reports_an_error_event() {
        let events = parse_whole("data: {\"error\":{\"code\":500,\"message\":\"boom\"}}\n\n");
        match events.as_slice() {
            [StreamEvent::Error(message)] => assert!(message.contains("boom"), "{message}"),
            other => panic!("expected one error event, got {other:?}"),
        }
    }

    #[test]
    fn sse_parser_reports_an_unreadable_event_instead_of_swallowing_it() {
        let events = parse_whole("data: {\"candidates\": [\n\n");
        assert!(
            matches!(events.as_slice(), [StreamEvent::Error(_)]),
            "{events:?}"
        );
    }

    #[test]
    fn sse_parser_flushes_an_event_left_open_at_the_end() {
        let input = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"z\"}]}}]}";
        let mut parser = SseParser::default();
        assert!(parser.push(input.as_bytes()).is_empty());
        assert_eq!(parser.finish(), vec![StreamEvent::Text("z".to_string())]);
    }

    #[test]
    fn sse_parser_ignores_a_done_sentinel_and_comments() {
        assert!(parse_whole(": keep-alive\n\ndata: [DONE]\n\n").is_empty());
    }

    #[test]
    fn thought_parts_are_skipped() {
        let input = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"pondering\",\"thought\":true},{\"text\":\"feat: x\"}]}}]}\n\n";
        assert_eq!(
            parse_whole(input),
            vec![StreamEvent::Text("feat: x".to_string())]
        );
    }

    #[test]
    fn a_final_event_with_only_a_finish_reason_gives_no_text() {
        let input = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"\",\"thoughtSignature\":\"abc\"}]},\"finishReason\":\"STOP\"}]}\r\n\r\n";
        assert!(parse_whole(input).is_empty());
    }

    #[test]
    fn stream_url_has_the_streaming_shape() {
        assert_eq!(
            stream_url(DEFAULT_ENDPOINT, "gemini-3.5-flash-lite"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.5-flash-lite:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            stream_url("http://127.0.0.1:9/", "m"),
            "http://127.0.0.1:9/v1beta/models/m:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn request_body_is_one_user_message_and_nothing_else() {
        let body = request_body("say \"hi\"\nnow");
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["contents"][0]["role"], "user");
        assert_eq!(value["contents"][0]["parts"][0]["text"], "say \"hi\"\nnow");
        // Aucun réglage de réflexion : le défaut du modèle vaut pour tous les modèles que `model` peut nommer.
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["contents"]);
        assert!(!body.contains('\n'), "compact JSON, one line");
    }

    #[test]
    fn curl_config_escapes_quotes_and_backslashes() {
        // Le corps contient un `"` et un `\n` de deux caractères (backslash, n), comme du JSON compact.
        let body = r#"{"a":"x\ny"}"#;
        let config = curl_config("http://h/p", KEY, body);
        let line = config
            .lines()
            .find(|line| line.starts_with("data-binary = "))
            .expect("a data-binary line");
        assert_eq!(line, r#"data-binary = "{\"a\":\"x\\ny\"}""#);
    }

    #[test]
    fn curl_config_cannot_be_split_by_a_control_character_in_a_value() {
        let config = curl_config("http://h/p", "k\"\nurl = \"http://evil\"", "{}");
        assert_eq!(config.lines().count(), 4, "{config:?}");
    }

    #[test]
    fn the_key_is_only_in_the_header_line() {
        let url = stream_url(DEFAULT_ENDPOINT, "gemini-test");
        let body = request_body("a prompt");
        let config = curl_config(&url, KEY, &body);
        let lines_with_key: Vec<&str> = config.lines().filter(|line| line.contains(KEY)).collect();
        assert_eq!(
            lines_with_key,
            vec![format!("header = \"x-goog-api-key: {KEY}\"")]
        );
        assert!(!url.contains(KEY));
        assert!(!body.contains(KEY));
    }

    #[test]
    fn classify_maps_status_and_curl_codes() {
        let none = "";
        assert_eq!(
            classify(Some(403), Some(0), none),
            Some(GenerationError::KeyRejected)
        );
        assert_eq!(
            classify(
                Some(400),
                Some(0),
                "... \"reason\": \"API_KEY_INVALID\" ..."
            ),
            Some(GenerationError::KeyRejected)
        );
        assert_eq!(
            classify(
                Some(400),
                Some(0),
                "{\"error\":{\"status\":\"INVALID_ARGUMENT\"}}"
            ),
            Some(GenerationError::Failed)
        );
        assert_eq!(
            classify(Some(404), Some(0), none),
            Some(GenerationError::ModelUnavailable)
        );
        assert_eq!(
            classify(Some(429), Some(0), none),
            Some(GenerationError::QuotaExceeded)
        );
        for code in [6, 7, 28, 35] {
            assert_eq!(
                classify(None, Some(code), none),
                Some(GenerationError::Unreachable),
                "{code}"
            );
        }
        assert_eq!(
            classify(Some(500), Some(0), none),
            Some(GenerationError::Failed)
        );
        assert_eq!(
            classify(None, Some(18), none),
            Some(GenerationError::Failed)
        );
        assert_eq!(classify(Some(200), Some(0), none), None);
        // Un délai dépassé en plein flux, après un 200, reste un réseau qui lâche.
        assert_eq!(
            classify(Some(200), Some(28), none),
            Some(GenerationError::Unreachable)
        );
    }

    #[test]
    fn failure_raw_prefers_the_api_message_and_never_keeps_the_key() {
        let body = format!("{{\"error\":{{\"message\":\"bad key {KEY} sent\"}}}}");
        let raw = failure_raw(Some(403), &body, "", KEY);
        assert_eq!(raw, "bad key *** sent");
        // Sans code HTTP, c'est le stderr de curl.
        let raw = failure_raw(None, "", "curl: (7) Failed to connect", KEY);
        assert_eq!(raw, "curl: (7) Failed to connect");
    }

    // --- Avec le faux serveur ---

    fn record_path(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("idema-b11-fake-{}-{name}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn request_for(port: u16, prompt: &str) -> GenerationRequest {
        GenerationRequest {
            endpoint: format!("http://127.0.0.1:{port}"),
            model: "gemini-test".into(),
            api_key: KEY.into(),
            prompt: prompt.into(),
        }
    }

    /// La prochaine mise à jour, ou `None` quand le canal est fermé ; échoue si rien n'arrive en 20 s.
    fn next_update(receiver: &mut UnboundedReceiver<GenerationUpdate>) -> Option<GenerationUpdate> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match receiver.try_recv() {
                Ok(update) => return Some(update),
                Err(TryRecvError::Closed) => return None,
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "no update and the channel stays open"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    /// Attend la fermeture du canal et rend les mises à jour avec leur heure d'arrivée.
    fn collect(
        receiver: &mut UnboundedReceiver<GenerationUpdate>,
    ) -> Vec<(Instant, GenerationUpdate)> {
        let mut updates = Vec::new();
        while let Some(update) = next_update(receiver) {
            updates.push((Instant::now(), update));
        }
        updates
    }

    fn failure_of(status: u16, name: &str) -> (GenerationError, String) {
        let mut options = FakeOptions::new(record_path(name));
        options.status = Some(status);
        let (port, _server) = serve(options);
        let (_cancel, mut receiver) = spawn_generation(request_for(port, "p"));
        match collect(&mut receiver).pop().map(|(_, update)| update) {
            Some(GenerationUpdate::Failed { error, raw }) => (error, raw),
            other => panic!("expected a failure for {status}, got {other:?}"),
        }
    }

    #[test]
    fn a_generation_streams_from_the_fake_server() {
        let record = record_path("stream");
        let mut options = FakeOptions::new(record.clone());
        options.chunks = 3;
        options.delay_ms = 50;
        let (port, _server) = serve(options);

        let (_cancel, mut receiver) = spawn_generation(request_for(port, "write a message"));
        let updates = collect(&mut receiver);

        let kinds: Vec<&GenerationUpdate> = updates.iter().map(|(_, update)| update).collect();
        assert_eq!(
            kinds,
            vec![
                &GenerationUpdate::Text("fragment 1".into()),
                &GenerationUpdate::Text("fragment 2".into()),
                &GenerationUpdate::Text("fragment 3".into()),
                &GenerationUpdate::Done,
            ]
        );
        // Les fragments arrivent au fil de l'eau (`-N`), pas d'un bloc à la fin : deux écarts de 50 ms au moins.
        let spread = updates[2].0.duration_since(updates[0].0);
        assert!(spread >= Duration::from_millis(80), "{spread:?}");

        let recorded = std::fs::read_to_string(&record).unwrap();
        let request_line = recorded.lines().next().unwrap();
        assert!(
            request_line
                .starts_with("POST /v1beta/models/gemini-test:streamGenerateContent?alt=sse")
        );
        assert!(
            !request_line.contains(KEY),
            "the key must not be in the URL"
        );
        assert!(
            recorded
                .lines()
                .any(|line| line.eq_ignore_ascii_case(&format!("x-goog-api-key: {KEY}")))
        );
        assert!(
            recorded.contains("write a message"),
            "the body reaches the server"
        );
    }

    #[test]
    fn the_key_is_not_in_curls_arguments() {
        // Le faux serveur suspendu garde curl en vie : on lit ses arguments dans la table des processus.
        let mut options = FakeOptions::new(record_path("args"));
        options.hang_after = Some(1);
        let (port, _server) = serve(options);
        let (cancel, mut receiver) = spawn_generation(request_for(port, "p"));
        assert!(
            next_update(&mut receiver).is_some(),
            "the first fragment arrives"
        );
        let pid = cancel.pid().expect("curl is running");
        let listing = Command::new("/bin/ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let command = String::from_utf8_lossy(&listing.stdout).into_owned();
        assert!(command.contains("--config"), "{command}");
        assert!(!command.contains(KEY), "{command}");
        cancel.cancel();
    }

    #[test]
    fn cancel_stops_curl() {
        let mut options = FakeOptions::new(record_path("hang"));
        options.hang_after = Some(1);
        let (port, _server) = serve(options);
        let (cancel, mut receiver) = spawn_generation(request_for(port, "p"));

        // Le premier fragment arrive, puis le serveur se tait connexion ouverte.
        let first = next_update(&mut receiver);
        assert_eq!(first, Some(GenerationUpdate::Text("fragment 1".into())));
        let pid = cancel.pid().expect("curl is running") as i32;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "curl is alive before cancel"
        );

        let started = Instant::now();
        cancel.cancel();
        cancel.cancel(); // idempotent
        let rest = collect(&mut receiver);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert!(rest.is_empty(), "nothing after a cancel, got {rest:?}");
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "curl is gone after cancel"
        );
    }

    #[test]
    fn nothing_is_sent_after_cancel_even_for_an_event_left_open() {
        // Le serveur écrit un `data:` sans sa ligne vide puis se tait : à l'annulation, la lecture rend 0 et
        // `finish()` aurait vidé l'événement ouvert vers le canal.
        let mut options = FakeOptions::new(record_path("unterminated"));
        options.unterminated_then_hang = true;
        let (port, _server) = serve(options);
        let (cancel, mut receiver) = spawn_generation(request_for(port, "p"));
        // Laisse curl recevoir la ligne ouverte avant d'annuler.
        let deadline = Instant::now() + Duration::from_secs(10);
        while cancel.pid().is_none() {
            assert!(Instant::now() < deadline, "curl never started");
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(300));
        cancel.cancel();
        let after = collect(&mut receiver);
        assert!(after.is_empty(), "nothing after a cancel, got {after:?}");
    }

    #[test]
    fn a_curlrc_in_home_cannot_change_the_outcome() {
        // Un `~/.curlrc` avec `fail` ferait taire le corps d'erreur (donc `API_KEY_INVALID`) : `-q` l'ignore.
        let home = std::env::temp_dir().join(format!("idema-b11-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".curlrc"), "fail\n").unwrap();

        let mut options = FakeOptions::new(record_path("curlrc"));
        options.status = Some(400);
        let (port, _server) = serve(options);
        let (_cancel, mut receiver) = spawn_with(
            CURL.to_string(),
            vec![("HOME".to_string(), home.to_string_lossy().into_owned())],
            request_for(port, "p"),
        );
        match collect(&mut receiver).pop().map(|(_, update)| update) {
            Some(GenerationUpdate::Failed { error, .. }) => {
                assert_eq!(error, GenerationError::KeyRejected);
            }
            other => panic!("expected KeyRejected, got {other:?}"),
        }
    }

    #[test]
    fn dropping_the_handle_stops_curl() {
        let mut options = FakeOptions::new(record_path("drop"));
        options.hang_after = Some(1);
        let (port, _server) = serve(options);
        let (cancel, mut receiver) = spawn_generation(request_for(port, "p"));
        assert!(
            next_update(&mut receiver).is_some(),
            "the first fragment arrives"
        );
        let pid = cancel.pid().expect("curl is running") as i32;
        drop(cancel);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert!(collect(&mut receiver).is_empty());
    }

    #[test]
    fn errors_come_back_classified() {
        for (status, expected) in [
            (400, GenerationError::KeyRejected),
            (403, GenerationError::KeyRejected),
            (404, GenerationError::ModelUnavailable),
            (429, GenerationError::QuotaExceeded),
            (500, GenerationError::Failed),
        ] {
            let (error, raw) = failure_of(status, &format!("status-{status}"));
            assert_eq!(error, expected, "status {status}");
            assert!(!raw.is_empty(), "status {status} keeps its message");
        }

        // Un port fermé : curl sort en 7, sans réponse HTTP.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let (_cancel, mut receiver) = spawn_generation(request_for(port, "p"));
        match collect(&mut receiver).pop().map(|(_, update)| update) {
            Some(GenerationUpdate::Failed { error, raw }) => {
                assert_eq!(error, GenerationError::Unreachable);
                assert!(raw.contains("curl"), "curl's own message: {raw:?}");
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    #[test]
    fn a_failure_never_carries_the_key() {
        let (error, raw) = failure_of(403, "no-key-in-raw");
        assert_eq!(error, GenerationError::KeyRejected);
        assert!(!raw.is_empty());
        assert!(!raw.contains(KEY), "{raw:?}");
    }

    #[test]
    fn an_error_in_the_middle_of_the_stream_keeps_the_text_already_received() {
        let mut options = FakeOptions::new(record_path("midstream"));
        options.midstream_error = true;
        let (port, _server) = serve(options);
        let (_cancel, mut receiver) = spawn_generation(request_for(port, "p"));
        let updates: Vec<GenerationUpdate> = collect(&mut receiver)
            .into_iter()
            .map(|(_, update)| update)
            .collect();
        assert_eq!(updates[0], GenerationUpdate::Text("fragment 1".into()));
        match &updates[1..] {
            [GenerationUpdate::Failed { error, raw }] => {
                assert_eq!(*error, GenerationError::Failed);
                assert!(raw.contains("boom"), "{raw:?}");
            }
            other => panic!("expected one failure after the text, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_curl_is_reported() {
        let (_cancel, mut receiver) = spawn_with(
            "/nonexistent/idema-curl".to_string(),
            Vec::new(),
            request_for(1, "p"),
        );
        let last = collect(&mut receiver).pop().map(|(_, update)| update);
        assert_eq!(
            last,
            Some(GenerationUpdate::Failed {
                error: GenerationError::CurlMissing,
                raw: String::new()
            })
        );
    }
}
