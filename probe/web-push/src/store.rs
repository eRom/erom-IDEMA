//! Etat persistant de la sonde : dossier prive, cle VAPID, abonnements, derniers envois, journal.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use web_push_native::jwt_simple::algorithms::{ECDSAP256PublicKeyLike, ES256KeyPair};

const VAPID_KEY_FILE: &str = "vapid-private.key";
const SUBSCRIPTIONS_FILE: &str = "subscriptions.json";
const LAST_PUSHES_FILE: &str = "last-pushes.json";
const LOG_FILE: &str = "probe.log";

/// Hotes autorises pour un endpoint de souscription (suffixe de domaine).
/// Evite que le serveur serve de relais POST vers un hote arbitraire.
const PUSH_HOST_SUFFIXES: &[&str] = &[
    "push.apple.com",
    "fcm.googleapis.com",
    "push.services.mozilla.com",
    "notify.windows.com",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

/// Forme de `PushSubscription.toJSON()`, plus deux champs ajoutes par la sonde.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
    #[serde(default)]
    pub added_at: String,
    #[serde(default)]
    pub user_agent: Option<String>,
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("RFC 3339 formatting of a valid timestamp cannot fail")
}

pub fn b64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode du base64url, avec ou sans padding (les navigateurs n'en mettent pas).
pub fn b64url_decode(text: &str) -> Result<Vec<u8>, String> {
    URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .map_err(|err| format!("invalid base64url: {err}"))
}

/// Dossier d'etat : `$IDEMA_PROBE_DIR`, sinon `~/.config/idema/companion-probe/`.
pub fn state_dir() -> Result<PathBuf, String> {
    let dir = match std::env::var_os("IDEMA_PROBE_DIR") {
        Some(custom) if !custom.is_empty() => PathBuf::from(custom),
        _ => {
            let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
            PathBuf::from(home).join(".config/idema/companion-probe")
        }
    };
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .map_err(|err| format!("cannot chmod {}: {err}", dir.display()))?;
    Ok(dir)
}

/// Ecriture atomique (fichier temporaire puis rename), en mode 0600 des la creation.
fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|err| format!("cannot open {}: {err}", tmp.display()))?;
    file.write_all(data)
        .map_err(|err| format!("cannot write {}: {err}", tmp.display()))?;
    drop(file);
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("cannot chmod {}: {err}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|err| format!("cannot rename to {}: {err}", path.display()))
}

/// Cle VAPID : lue si presente, sinon generee et ecrite en 0600. Jamais affichee.
pub fn load_or_create_vapid(dir: &Path) -> Result<ES256KeyPair, String> {
    let path = dir.join(VAPID_KEY_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let raw = b64url_decode(text.trim())?;
            ES256KeyPair::from_bytes(&raw).map_err(|err| format!("invalid VAPID key file: {err}"))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let pair = ES256KeyPair::generate();
            let encoded = b64url_encode(&pair.to_bytes());
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|err| format!("cannot create {}: {err}", path.display()))?;
            file.write_all(encoded.as_bytes())
                .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
            Ok(pair)
        }
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

/// Cle publique VAPID (point P-256 non compresse) en base64url sans padding.
pub fn vapid_public_key(pair: &ES256KeyPair) -> String {
    b64url_encode(&pair.public_key().public_key().to_bytes_uncompressed())
}

/// Controle d'une souscription recue d'un navigateur (frontiere non fiable).
pub fn validate_subscription(sub: &Subscription) -> Result<(), String> {
    let uri: reqwest::Url = sub
        .endpoint
        .parse()
        .map_err(|err| format!("invalid endpoint: {err}"))?;
    if uri.scheme() != "https" {
        return Err("endpoint must use https".into());
    }
    let host = uri.host_str().ok_or("endpoint has no host")?;
    let allowed = PUSH_HOST_SUFFIXES
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")));
    if !allowed {
        return Err(format!("endpoint host {host} is not a known push service"));
    }
    let p256dh = b64url_decode(&sub.keys.p256dh)?;
    web_push_native::p256::PublicKey::from_sec1_bytes(&p256dh)
        .map_err(|_| "keys.p256dh is not a valid P-256 point".to_string())?;
    let auth = b64url_decode(&sub.keys.auth)?;
    if auth.len() != 16 {
        return Err(format!("keys.auth must be 16 bytes, got {}", auth.len()));
    }
    Ok(())
}

pub fn load_subscriptions(dir: &Path) -> Result<Vec<Subscription>, String> {
    let path = dir.join(SUBSCRIPTIONS_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|err| format!("cannot parse {}: {err}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

/// Ajoute ou remplace une souscription (deduplication par endpoint).
/// L'appelant serialise les appels concurrents avec son propre verrou.
pub fn upsert_subscription(dir: &Path, sub: Subscription) -> Result<usize, String> {
    let mut all = load_subscriptions(dir)?;
    all.retain(|existing| existing.endpoint != sub.endpoint);
    all.push(sub);
    let text = serde_json::to_string_pretty(&all).map_err(|err| err.to_string())?;
    write_private(&dir.join(SUBSCRIPTIONS_FILE), text.as_bytes())?;
    Ok(all.len())
}

pub fn load_last_pushes(dir: &Path) -> Result<BTreeMap<String, Value>, String> {
    let path = dir.join(LAST_PUSHES_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|err| format!("cannot parse {}: {err}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

pub fn record_last_push(dir: &Path, session: &str, payload: &Value) -> Result<(), String> {
    let mut all = load_last_pushes(dir)?;
    all.insert(session.to_string(), payload.clone());
    let text = serde_json::to_string_pretty(&all).map_err(|err| err.to_string())?;
    write_private(&dir.join(LAST_PUSHES_FILE), text.as_bytes())
}

/// Ajoute une ligne JSON au journal (append-only, 0600).
pub fn append_log(dir: &Path, event: &str, detail: Value, user_agent: Option<&str>) {
    let line = json!({
        "ts": now_rfc3339(),
        "event": event,
        "detail": detail,
        "user_agent": user_agent,
    });
    let result = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join(LOG_FILE))
        .and_then(|mut file| writeln!(file, "{line}"));
    if let Err(err) = result {
        eprintln!("cannot append to {LOG_FILE}: {err}");
    }
}
