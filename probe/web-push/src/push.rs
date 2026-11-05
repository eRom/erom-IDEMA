//! Construction et envoi des requetes Web Push (aes128gcm + VAPID) via web-push-native.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use web_push_native::jwt_simple::algorithms::ES256KeyPair;
use web_push_native::{Auth, WebPushBuilder};

use crate::store::{Subscription, b64url_decode};

pub const VAPID_CONTACT: &str = "https://github.com/eRom/erom-IDEMA";
const TTL: Duration = Duration::from_secs(3600);

/// Reponse HTTP brute du service push, sans interpretation.
pub struct PushResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
}

/// Hote d'un endpoint, pour l'affichage (l'URL complete est un jeton secret).
pub fn endpoint_host(endpoint: &str) -> String {
    reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "<invalid endpoint>".to_string())
}

/// Chiffre `payload` pour `sub` et signe la requete avec `vapid_contact` comme `sub` du JWT.
pub fn build_request(
    sub: &Subscription,
    payload: &[u8],
    vapid_key: &ES256KeyPair,
    vapid_contact: &str,
) -> Result<reqwest::Request, String> {
    let endpoint = sub
        .endpoint
        .parse()
        .map_err(|err| format!("invalid endpoint: {err}"))?;
    let p256dh =
        web_push_native::p256::PublicKey::from_sec1_bytes(&b64url_decode(&sub.keys.p256dh)?)
            .map_err(|_| "keys.p256dh is not a valid P-256 point".to_string())?;
    let auth_bytes = b64url_decode(&sub.keys.auth)?;
    if auth_bytes.len() != 16 {
        return Err(format!(
            "keys.auth must be 16 bytes, got {}",
            auth_bytes.len()
        ));
    }
    let auth = Auth::clone_from_slice(&auth_bytes);

    // with_valid_duration fixe a la fois l'en-tete TTL et l'expiration du JWT VAPID.
    let builder = WebPushBuilder::new(endpoint, p256dh, auth)
        .with_valid_duration(TTL)
        .with_vapid(vapid_key, vapid_contact);
    let mut request = builder
        .build(payload.to_vec())
        .map_err(|err| format!("cannot build push request: {err}"))?;
    request
        .headers_mut()
        .insert("Urgency", HeaderValue::from_static("high"));
    reqwest::Request::try_from(request).map_err(|err| format!("cannot convert request: {err}"))
}

pub async fn execute(
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<PushResponse, String> {
    let response = client
        .execute(request)
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response
        .text()
        .await
        .map_err(|err| format!("cannot read response body: {err}"))?;
    Ok(PushResponse {
        status,
        headers,
        body,
    })
}

/// Succes = 2xx (Apple repond 201 Created).
pub fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

pub fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Client HTTP avec délai maximal : un service push muet ne doit pas bloquer la sonde.
pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|err| format!("cannot build HTTP client: {err}"))
}
