//! L'envoi d'un message Web Push : la configuration de curl, la lecture de sa réponse, et ce qu'un code de réponse
//! veut dire pour l'abonnement. `/usr/bin/curl` fait le TLS (aucune pile TLS dans le binaire) ; ni l'URL de
//! l'abonnement ni l'en-tête VAPID ne passent en argument de process : tout vient par l'entrée standard.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use super::devices::Subscription;
use super::webpush::{self, PushError, UaKeys, Vapid};
use crate::commit_message::client::escape_value;

/// Le seul curl que l'on lance : celui du système, jamais celui du `PATH`.
const CURL: &str = "/usr/bin/curl";

/// Le délai de curl pour l'échange entier (spec §3.7).
const MAX_TIME_SECONDS: u32 = 10;

/// Ce que l'on lit de la sortie de curl au plus : un corps d'erreur de service push tient en quelques centaines
/// d'octets, le reste n'est pas lu (la sortie est fermée, curl s'arrête).
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;

/// Le plus long point d'accès admis à l'abonnement (ceux d'Apple, de Google et de Mozilla font moins de 300 octets).
const ENDPOINT_MAX_LEN: usize = 2048;

/// Combien de noms de fichier de corps l'envoi essaie avant de renoncer (un nom pris est un reste d'un process mort).
const BODY_FILE_ATTEMPTS: usize = 8;

/// La ligne que curl écrit après le corps de la réponse : elle porte le code HTTP (`000` sans réponse).
const STATUS_MARKER: &str = "IDEMA_HTTP ";

/// Variable qui remplace l'adresse de chaque abonnement à l'envoi (instance d'acceptation seulement).
pub const OVERRIDE_VAR: &str = "IDEMA_COMPANION_PUSH_OVERRIDE";

/// Un message prêt à partir : où, avec quels en-têtes, quel corps (déjà chiffré).
pub struct PushMessage {
    pub endpoint: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

/// Ce que la réponse du service push dit de l'envoi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// 201 : le service push a pris le message.
    Sent,
    /// L'abonnement n'existe plus ou n'est plus le nôtre : à retirer.
    Gone,
    /// 413 : la charge est trop grosse pour ce service.
    TooLarge,
    /// 429, 5xx ou aucune réponse : à retenter plus tard.
    RetryLater,
    /// Toute autre réponse : journalisée, pas de nouvel essai.
    Failed,
}

impl Delivery {
    /// Le mot du journal (`reason=`) et de la réponse JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Gone => "gone",
            Self::TooLarge => "too_large",
            Self::RetryLater => "retry_later",
            Self::Failed => "failed",
        }
    }
}

/// La table de la spec §3.7. `status` est `None` quand curl n'a obtenu aucune réponse (délai dépassé, connexion
/// refusée) ; `body` est le corps de la réponse, lu pour distinguer les 403 de VAPID.
pub fn classify(status: Option<u16>, body: &str) -> Delivery {
    match status {
        Some(201) => Delivery::Sent,
        Some(404 | 410) => Delivery::Gone,
        // Un 403 ne retire l'abonnement que si le service dit que la clé VAPID n'est pas la bonne : un jeton expiré
        // ou mal formé n'a rien à voir avec l'abonnement.
        Some(403) if body.contains("VapidPkHashMismatch") || body.contains("BadVapidPublicKey") => {
            Delivery::Gone
        }
        Some(413) => Delivery::TooLarge,
        Some(429 | 500..=599) | None => Delivery::RetryLater,
        Some(_) => Delivery::Failed,
    }
}

/// Le point d'accès d'un abonnement est-il chez un service push connu ? `https` seulement, hôtes `*.push.apple.com`,
/// `fcm.googleapis.com`, `updates.push.services.mozilla.com`, `*.notify.windows.com` : l'URL est celle que le téléphone
/// annonce, et c'est vers elle que le Mac enverra, il ne doit pas pouvoir viser une adresse locale.
pub fn endpoint_allowed(endpoint: &str) -> bool {
    // Seulement de l'ASCII visible, sans guillemet ni antislash : un point d'accès légitime n'en porte jamais, et ce
    // qui s'en écarte finirait réécrit par l'analyse de l'URL ou changerait la ligne de configuration de curl.
    if endpoint.is_empty()
        || endpoint.len() > ENDPOINT_MAX_LEN
        || !endpoint
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\')
    {
        return false;
    }
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    let Some(url::Host::Domain(host)) = url.host() else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        // `port()` rend `None` pour le port par défaut (443) : tout autre port est refusé.
        && url.port().is_none()
        && host_allowed(host)
}

fn host_allowed(host: &str) -> bool {
    let under = |suffix: &str| host.len() > suffix.len() && host.ends_with(suffix);
    under(".push.apple.com")
        || host == "fcm.googleapis.com"
        || host == "updates.push.services.mozilla.com"
        || under(".notify.windows.com")
}

/// L'adresse de remplacement des envois, si l'instance est une instance d'acceptation : `IDEMA_HOME` ET
/// `IDEMA_COMPANION_TEST_ROUTES=1` posés, et une URL `http(s)` bien formée dans la variable. Sinon `None`.
pub fn override_endpoint(
    var: Option<&str>,
    idema_home: Option<&str>,
    test_routes: Option<&str>,
) -> Option<String> {
    if !idema_home.is_some_and(|home| !home.is_empty()) || test_routes != Some("1") {
        return None;
    }
    let var = var?.trim();
    let url = url::Url::parse(var).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some()).then(|| var.to_owned())
}

/// L'hôte d'une adresse de remplacement, pour la ligne de journal.
pub fn override_host(override_url: &str) -> String {
    url::Url::parse(override_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Où part le message : le point d'accès de l'abonnement, ou, sous une adresse de remplacement, cette adresse suivie
/// du chemin et de la requête du point d'accès (deux appareils se distinguent alors au faux service push).
fn target_url(override_url: Option<&str>, endpoint: &str) -> String {
    let Some(base) = override_url else {
        return endpoint.to_owned();
    };
    let Ok(parsed) = url::Url::parse(endpoint) else {
        return endpoint.to_owned();
    };
    let mut target = base.trim_end_matches('/').to_owned();
    target.push_str(parsed.path());
    if let Some(query) = parsed.query() {
        target.push('?');
        target.push_str(query);
    }
    target
}

/// La configuration que curl lit sur son entrée standard (`-K -`). Chaque valeur passe par l'échappement du message
/// de commit : un guillemet, un antislash ou un saut de ligne dans une URL ou un en-tête n'ouvre aucune directive.
pub(crate) fn curl_config(message: &PushMessage, body_file: &Path, max_time: u32) -> String {
    let mut config = String::from("silent\nproto = \"=http,https\"\n");
    config.push_str(&format!("url = \"{}\"\n", escape_value(&message.endpoint)));
    for (name, value) in &message.headers {
        config.push_str(&format!(
            "header = \"{}\"\n",
            escape_value(&format!("{name}: {value}"))
        ));
    }
    config.push_str(&format!("max-time = {max_time}\n"));
    config.push_str(&format!("connect-timeout = {max_time}\n"));
    config.push_str(&format!(
        "data-binary = \"@{}\"\n",
        escape_value(&body_file.to_string_lossy())
    ));
    // Le code HTTP après le corps, sur sa ligne : `\n` est lu par curl (valeur entre guillemets) puis par `write-out`.
    config.push_str(&format!(
        "write-out = \"\\n{STATUS_MARKER}%{{http_code}}\\n\"\n"
    ));
    config
}

/// Le corps chiffré, écrit en 0600 dans le dossier du compagnon le temps de l'envoi, retiré ensuite (même si l'envoi
/// échoue ou panique).
struct BodyFile {
    path: PathBuf,
}

impl BodyFile {
    fn create(folder: &Path, bytes: &[u8]) -> io::Result<Self> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        for _ in 0..BODY_FILE_ATTEMPTS {
            let path = folder.join(format!(
                "push-{}-{}.body",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let body = Self { path };
            file.write_all(bytes)?;
            return Ok(body);
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free name for the push body file",
        ))
    }
}

impl Drop for BodyFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Envoie le message par curl (bloquant) : `(code HTTP, corps de la réponse)`. Sans réponse, le code est `None` et le
/// texte dit pourquoi en un mot (`curl_exit=28` pour un délai dépassé).
pub fn send(message: &PushMessage, dir: &Path) -> (Option<u16>, String) {
    send_with(message, dir, MAX_TIME_SECONDS)
}

fn send_with(message: &PushMessage, dir: &Path, max_time: u32) -> (Option<u16>, String) {
    let folder = dir.join("companion");
    let prepared = fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&folder)
        .and_then(|()| BodyFile::create(&folder, &message.body));
    let body_file = match prepared {
        Ok(body_file) => body_file,
        Err(error) => return (None, format!("body_file_failed kind={:?}", error.kind())),
    };
    run_curl(CURL, &curl_config(message, &body_file.path, max_time), &[])
}

/// Lance curl (`-q` en tout premier : le `~/.curlrc` de l'utilisateur est ignoré, un `fail` ou un `write-out` y
/// changerait ce que l'on lit) avec cette configuration sur son entrée standard, et lit sa sortie.
fn run_curl(curl_path: &str, config: &str, envs: &[(&str, &OsStr)]) -> (Option<u16>, String) {
    let mut command = Command::new(curl_path);
    command
        .args(["-q", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (name, value) in envs {
        command.env(name, value);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return (None, format!("curl_spawn_failed kind={:?}", error.kind())),
    };
    // curl lit toute sa configuration avant d'ouvrir la connexion ; l'entrée se ferme ensuite. Une écriture qui
    // échoue (curl mort) se lit dans sa sortie.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(config.as_bytes());
    }
    let mut output = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        // `stdout` tombe en fin d'instruction : si le plafond est atteint, curl se voit couper sa sortie.
        let _ = stdout.take(MAX_OUTPUT_BYTES).read_to_end(&mut output);
    }
    let exit = child.wait().ok().and_then(|status| status.code());
    let text = String::from_utf8_lossy(&output);
    let marker = format!("\n{STATUS_MARKER}");
    if let Some((body, rest)) = text.rsplit_once(&marker)
        && let Some(code) = rest.trim().parse::<u16>().ok().filter(|code| *code != 0)
    {
        return (Some(code), body.to_owned());
    }
    (None, format!("curl_exit={}", exit.unwrap_or(-1)))
}

/// Ce qui sert à envoyer : la clé VAPID du Mac, le dossier d'IdemA, et l'adresse de remplacement d'une instance
/// d'acceptation.
pub struct PushSender {
    vapid: Mutex<Vapid>,
    dir: PathBuf,
    override_url: Option<String>,
}

impl PushSender {
    /// Charge (ou crée) la clé VAPID, et retire les corps laissés par un process mort : ils contiennent des messages
    /// chiffrés mais mieux vaut ne rien laisser traîner. Les corps du process courant (envois en cours d'un service
    /// précédent) sont laissés.
    pub fn open(dir: &Path, override_url: Option<String>) -> io::Result<Self> {
        let vapid = Vapid::load_or_create(dir)?;
        let own = format!("push-{}-", std::process::id());
        if let Ok(entries) = fs::read_dir(dir.join("companion")) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("push-") && name.ends_with(".body") && !name.starts_with(&own) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Ok(Self {
            vapid: Mutex::new(vapid),
            dir: dir.to_path_buf(),
            override_url,
        })
    }

    /// La clé publique VAPID en base64url : la valeur de `applicationServerKey` côté page.
    pub fn public_key(&self) -> String {
        self.vapid
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .public_key_b64url()
    }

    /// Chiffre, signe et envoie `payload` à cet abonnement (bloquant), avec le `Topic` de `session`. Rend le verdict
    /// et le code HTTP s'il y en a un. Un abonnement abîmé ou hors des hôtes admis n'envoie rien (`Failed`) ; une
    /// charge qui ne tient pas dans un enregistrement non plus (`TooLarge`).
    pub fn deliver(
        &self,
        subscription: &Subscription,
        session: &str,
        payload: &[u8],
    ) -> (Delivery, Option<u16>) {
        let message = match self.build(subscription, session, payload) {
            Ok(message) => message,
            Err(PushError::PayloadTooLarge) => return (Delivery::TooLarge, None),
            Err(_) => return (Delivery::Failed, None),
        };
        let (status, body) = send(&message, &self.dir);
        (classify(status, &body), status)
    }

    fn build(
        &self,
        subscription: &Subscription,
        session: &str,
        payload: &[u8],
    ) -> Result<PushMessage, PushError> {
        // Revérifié à l'envoi : `devices.json` est un fichier, qu'une main ou une version plus ancienne a pu écrire.
        if !endpoint_allowed(&subscription.endpoint) {
            return Err(PushError::BadEndpoint);
        }
        let keys = UaKeys::from_subscription(subscription)?;
        let body = webpush::encrypt(payload, &keys)?;
        let authorization = self
            .vapid
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .authorization(&subscription.endpoint, SystemTime::now())?;
        let mut headers: Vec<(&'static str, String)> = webpush::headers(session).into();
        headers.push(("Authorization", authorization));
        Ok(PushMessage {
            endpoint: target_url(self.override_url.as_deref(), &subscription.endpoint),
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use p256::elliptic_curve::sec1::ToEncodedPoint;

    use super::*;
    use crate::companion::test_support::{FakePush, TempDir};

    #[test]
    fn responses_are_classified() {
        // Chaque ligne de la table de la spec §3.7.
        assert_eq!(classify(Some(201), ""), Delivery::Sent);
        assert_eq!(classify(Some(404), ""), Delivery::Gone);
        assert_eq!(classify(Some(410), "gone"), Delivery::Gone);
        assert_eq!(
            classify(Some(403), r#"{"reason":"VapidPkHashMismatch"}"#),
            Delivery::Gone
        );
        assert_eq!(classify(Some(403), "BadVapidPublicKey"), Delivery::Gone);
        assert_eq!(classify(Some(413), ""), Delivery::TooLarge);
        assert_eq!(classify(Some(429), ""), Delivery::RetryLater);
        assert_eq!(classify(Some(500), ""), Delivery::RetryLater);
        assert_eq!(classify(Some(503), ""), Delivery::RetryLater);
        assert_eq!(classify(Some(599), ""), Delivery::RetryLater);
        assert_eq!(classify(None, ""), Delivery::RetryLater);
        // Tout le reste : journalisé, pas de nouvel essai. Un 403 d'une autre raison (jeton refusé) ne retire rien.
        assert_eq!(classify(Some(403), "ExpiredJwt"), Delivery::Failed);
        assert_eq!(classify(Some(403), ""), Delivery::Failed);
        for status in [200, 202, 204, 301, 400, 401, 405, 600] {
            assert_eq!(classify(Some(status), ""), Delivery::Failed, "{status}");
        }
    }

    #[test]
    fn only_known_push_hosts_are_accepted() {
        for endpoint in [
            "https://web.push.apple.com/QXX",
            "https://api.sandbox.push.apple.com/3/device/abc",
            "https://fcm.googleapis.com/fcm/send/abc:def",
            "https://updates.push.services.mozilla.com/wpush/v2/abc",
            "https://wns2-par02p.notify.windows.com/w/?token=abc",
        ] {
            assert!(endpoint_allowed(endpoint), "{endpoint}");
        }
        for endpoint in [
            "http://web.push.apple.com/x",
            "https://evil.example/x",
            "",
            "web.push.apple.com/x",
            // Des hôtes qui ressemblent sans l'être.
            "https://push.apple.com/x",
            "https://evilpush.apple.com/x",
            "https://web.push.apple.com.evil.example/x",
            "https://fcm.googleapis.com.evil.example/x",
            "https://notfcm.googleapis.com/x",
            "https://notify.windows.com/x",
            "https://x.notify.windows.com.evil.example/x",
            "https://web.push.apple.com./x",
            // Ce qui détournerait l'envoi ou la ligne de configuration.
            "https://user@web.push.apple.com/x",
            "https://web.push.apple.com@evil.example/x",
            "https://web.push.apple.com:8443/x",
            "https://127.0.0.1/x",
            "https://localhost/x",
            "https://web.push.apple.com/a b",
            "https://web.push.apple.com/a\nurl = \"http://evil.example/\"",
            "https://web.push.apple.com/a\"b",
            "https://web.push.apple.com/\u{e9}",
            "https://[::1]/x",
        ] {
            assert!(!endpoint_allowed(endpoint), "{endpoint:?}");
        }
        assert!(
            !endpoint_allowed(&format!("https://web.push.apple.com/{}", "a".repeat(3000))),
            "an endpoint of 3000 characters is refused"
        );
        assert!(endpoint_allowed("https://web.push.apple.com:443/QXX"));
    }

    #[test]
    fn override_needs_idema_home() {
        let url = Some("http://127.0.0.1:4000");
        let home = Some("/tmp/idema-home");
        let on = Some("1");
        assert_eq!(
            override_endpoint(url, home, on).as_deref(),
            Some("http://127.0.0.1:4000")
        );
        // AC-16 : sans l'une des deux conditions, la variable est ignorée.
        assert_eq!(override_endpoint(url, None, on), None);
        assert_eq!(override_endpoint(url, Some(""), on), None);
        assert_eq!(override_endpoint(url, home, None), None);
        assert_eq!(override_endpoint(url, home, Some("0")), None);
        assert_eq!(override_endpoint(url, home, Some("true")), None);
        assert_eq!(override_endpoint(url, None, None), None);
        // Variable absente, vide ou qui n'est pas une URL http(s) : rien à remplacer.
        assert_eq!(override_endpoint(None, home, on), None);
        assert_eq!(override_endpoint(Some(""), home, on), None);
        assert_eq!(override_endpoint(Some("not a url"), home, on), None);
        assert_eq!(
            override_endpoint(Some("file:///etc/passwd"), home, on),
            None
        );
        assert_eq!(override_endpoint(Some("ftp://127.0.0.1/"), home, on), None);
    }

    /// Défait les échappements d'une valeur entre guillemets comme le fait curl (`\\`, `\"`, `\n`, `\r`, `\t`).
    fn curl_unescape(value: &str) -> String {
        let mut text = String::new();
        let mut chars = value.chars();
        while let Some(character) = chars.next() {
            if character != '\\' {
                text.push(character);
                continue;
            }
            text.push(match chars.next().unwrap() {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
        }
        text
    }

    #[test]
    fn curl_config_escapes_hostile_values() {
        let hostile = "https://web.push.apple.com/a\"b\\c\nurl = \"http://evil.example/\"\nheader = \"X-Evil: 1";
        let message = PushMessage {
            endpoint: hostile.to_string(),
            headers: vec![
                ("TTL", "3600".to_string()),
                (
                    "Authorization",
                    "vapid t=a.b.c, k=d\nheader = \"X-Evil: 2".to_string(),
                ),
            ],
            body: vec![1, 2, 3],
        };
        let body_file = Path::new("/tmp/with \"quote\" and \\slash/push-1.body");
        let config = curl_config(&message, body_file, 10);

        let lines: Vec<&str> = config.lines().collect();
        // Une directive par ligne, et seulement celles qu'on a voulues : un saut de ligne ou un guillemet dans une
        // valeur n'en ouvre pas une de plus.
        let directives: Vec<&str> = lines
            .iter()
            .map(|line| line.split([' ', '=']).next().unwrap())
            .collect();
        assert_eq!(
            directives,
            [
                "silent",
                "proto",
                "url",
                "header",
                "header",
                "max-time",
                "connect-timeout",
                "data-binary",
                "write-out"
            ],
            "{config}"
        );
        // Les valeurs reviennent intactes après le décodage de curl.
        let value_of = |prefix: &str| -> String {
            let line = lines.iter().find(|line| line.starts_with(prefix)).unwrap();
            let quoted = line.split_once(" = ").unwrap().1;
            assert!(quoted.starts_with('"') && quoted.ends_with('"'), "{line}");
            curl_unescape(&quoted[1..quoted.len() - 1])
        };
        assert_eq!(value_of("url"), hostile);
        assert_eq!(value_of("data-binary"), format!("@{}", body_file.display()));
        let headers: Vec<String> = lines
            .iter()
            .filter(|line| line.starts_with("header"))
            .map(|line| {
                let quoted = line.split_once(" = ").unwrap().1;
                curl_unescape(&quoted[1..quoted.len() - 1])
            })
            .collect();
        assert_eq!(headers[0], "TTL: 3600");
        assert_eq!(
            headers[1],
            "Authorization: vapid t=a.b.c, k=d\nheader = \"X-Evil: 2"
        );
        assert!(config.contains("max-time = 10"), "{config}");
    }

    fn message_to(port: u16, path: &str) -> PushMessage {
        PushMessage {
            endpoint: format!("http://127.0.0.1:{port}{path}"),
            headers: vec![
                ("TTL", "3600".to_string()),
                ("Authorization", "vapid t=a.b.c, k=d".to_string()),
                ("Content-Type", "application/octet-stream".to_string()),
            ],
            body: vec![0, 1, 2, 255, 254, b'\n', b'"', b'\\'],
        }
    }

    #[test]
    fn a_message_reaches_the_service_with_its_headers_and_exact_body() {
        let home = TempDir::new("send-ok");
        std::fs::create_dir_all(home.0.join("companion")).unwrap();
        let fake =
            FakePush::start_watching(Some((201, String::new())), Some(home.0.join("companion")));
        let message = message_to(fake.port, "/wpush/v2/abc?x=1");

        let (status, _) = send(&message, &home.0);

        assert_eq!(status, Some(201));
        let requests = fake.wait_for(1);
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/wpush/v2/abc?x=1");
        assert_eq!(request.header("TTL"), Some("3600"));
        assert_eq!(request.header("Authorization"), Some("vapid t=a.b.c, k=d"));
        assert_eq!(
            request.header("Content-Type"),
            Some("application/octet-stream")
        );
        assert_eq!(request.body, message.body, "binary body, byte for byte");
        // Le corps part d'un fichier 0600 du dossier du compagnon, retiré après l'envoi.
        assert_eq!(request.watched.len(), 1, "{:?}", request.watched);
        assert_eq!(request.watched[0].1, 0o600);
        let left: Vec<_> = std::fs::read_dir(home.0.join("companion"))
            .unwrap()
            .collect();
        assert!(left.is_empty(), "the body file is removed: {left:?}");
    }

    #[test]
    fn the_error_body_of_the_service_is_handed_back() {
        let home = TempDir::new("send-403");
        let fake = FakePush::start(403, r#"{"reason":"VapidPkHashMismatch"}"#);
        let (status, body) = send(&message_to(fake.port, "/x"), &home.0);
        assert_eq!(status, Some(403));
        assert_eq!(classify(status, &body), Delivery::Gone, "{body}");
        let fake = FakePush::start(410, "");
        let (status, body) = send(&message_to(fake.port, "/x"), &home.0);
        assert_eq!(classify(status, &body), Delivery::Gone);
        let fake = FakePush::start(429, "slow down");
        let (status, body) = send(&message_to(fake.port, "/x"), &home.0);
        assert_eq!(classify(status, &body), Delivery::RetryLater);
    }

    #[test]
    fn a_silent_service_is_cut_by_curl() {
        let home = TempDir::new("send-silence");
        let fake = FakePush::start_watching(None, None);
        let started = Instant::now();
        let (status, text) = send_with(&message_to(fake.port, "/x"), &home.0, 1);
        let elapsed = started.elapsed();
        assert_eq!(status, None);
        assert!(text.starts_with("curl_exit="), "{text}");
        assert!(elapsed.as_millis() >= 900, "{elapsed:?}");
        assert!(elapsed.as_secs() < 4, "curl gave up on time: {elapsed:?}");
        assert_eq!(classify(status, &text), Delivery::RetryLater);
    }

    #[test]
    fn a_refused_connection_is_a_missing_answer_not_a_panic() {
        let home = TempDir::new("send-refused");
        // Un port fermé : on lie puis on libère.
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (status, text) = send(&message_to(port, "/x"), &home.0);
        assert_eq!(status, None);
        assert!(text.starts_with("curl_exit="), "{text}");
    }

    #[test]
    fn the_curlrc_of_the_user_is_not_read() {
        let home = TempDir::new("send-curlrc");
        // Un ~/.curlrc qui ajouterait une ligne à la sortie, ou un en-tête, changerait ce que l'on lit.
        std::fs::write(
            home.0.join(".curlrc"),
            "write-out = \"CURLRC_WAS_READ\"\nheader = \"X-Curlrc: yes\"\n",
        )
        .unwrap();
        let fake = FakePush::start(201, "ok");
        let (status, body) = run_curl_with_home(&message_to(fake.port, "/x"), &home.0);
        assert_eq!(status, Some(201));
        assert!(!body.contains("CURLRC_WAS_READ"), "{body}");
        assert_eq!(fake.wait_for(1)[0].header("X-Curlrc"), None);
    }

    /// Comme `send`, avec `HOME` vers un dossier qui porte un `.curlrc`.
    fn run_curl_with_home(message: &PushMessage, home: &Path) -> (Option<u16>, String) {
        let folder = home.join("companion");
        std::fs::create_dir_all(&folder).unwrap();
        let body_file = folder.join("manual.body");
        std::fs::write(&body_file, &message.body).unwrap();
        let config = curl_config(message, &body_file, 5);
        run_curl(CURL, &config, &[("HOME", home.as_os_str())])
    }

    fn phone() -> (Subscription, p256::SecretKey) {
        let private = p256::SecretKey::random(&mut rand::rngs::OsRng);
        let public = private.public_key().to_encoded_point(false);
        let subscription = Subscription {
            endpoint: "https://web.push.apple.com/QXXdevice-one?token=abc".into(),
            p256dh: B64.encode(public.as_bytes()),
            auth: B64.encode([7u8; 16]),
        };
        (subscription, private)
    }

    #[test]
    fn a_delivery_is_encrypted_signed_and_sent_to_the_override_with_the_endpoint_path() {
        let home = TempDir::new("deliver");
        let fake = FakePush::start(201, "");
        let sender = PushSender::open(&home.0, Some(fake.url())).unwrap();
        let (subscription, _) = phone();

        let (delivery, status) = sender.deliver(&subscription, "claude-1", b"{\"title\":\"x\"}");

        assert_eq!((delivery, status), (Delivery::Sent, Some(201)));
        let request = &fake.wait_for(1)[0];
        // L'adresse de l'abonnement est remplacée, son chemin reste : deux appareils se distinguent.
        assert_eq!(request.path, "/QXXdevice-one?token=abc");
        assert_eq!(request.header("Content-Encoding"), Some("aes128gcm"));
        assert_eq!(request.header("TTL"), Some("3600"));
        assert_eq!(request.header("Urgency"), Some("high"));
        let topic = request.header("Topic").unwrap();
        assert_eq!(topic.len(), 32);
        assert_eq!(topic, webpush::topic("claude-1"));
        assert!(!topic.contains("claude"), "never the raw session name");
        let authorization = request.header("Authorization").unwrap();
        assert!(authorization.starts_with("vapid t="), "{authorization}");
        assert!(authorization.contains(&format!(", k={}", sender.public_key())));
        // aes128gcm : 86 octets d'en-tête, le texte, le délimiteur et la balise.
        assert_eq!(request.body.len(), 86 + 13 + 1 + 16);
    }

    #[test]
    fn a_payload_that_cannot_fit_is_too_large_and_never_sent() {
        let home = TempDir::new("deliver-big");
        let fake = FakePush::start(201, "");
        let sender = PushSender::open(&home.0, Some(fake.url())).unwrap();
        let (subscription, _) = phone();
        let (delivery, status) = sender.deliver(&subscription, "s", &vec![b'a'; 4000]);
        assert_eq!((delivery, status), (Delivery::TooLarge, None));
        assert!(fake.requests().is_empty());
    }

    #[test]
    fn a_broken_subscription_fails_without_a_request() {
        let home = TempDir::new("deliver-broken");
        let fake = FakePush::start(201, "");
        let sender = PushSender::open(&home.0, Some(fake.url())).unwrap();
        let broken = Subscription {
            endpoint: "https://web.push.apple.com/x".into(),
            p256dh: "AAAA".into(),
            auth: "AAAA".into(),
        };
        assert_eq!(sender.deliver(&broken, "s", b"x"), (Delivery::Failed, None));
        let (mut phone, _) = phone();
        phone.endpoint = "ftp://web.push.apple.com/x".into();
        assert_eq!(sender.deliver(&phone, "s", b"x"), (Delivery::Failed, None));
        assert!(fake.requests().is_empty());
    }

    #[test]
    fn opening_removes_the_body_files_of_a_dead_process() {
        let home = TempDir::new("open-stale");
        let folder = home.0.join("companion");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("push-1-0.body"), b"secret payload").unwrap();
        std::fs::write(folder.join("devices.json"), b"{}").unwrap();
        PushSender::open(&home.0, None).unwrap();
        assert!(!folder.join("push-1-0.body").exists());
        assert!(folder.join("devices.json").exists(), "other files stay");
        assert_eq!(
            std::fs::metadata(folder.join("vapid.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
