//! companion-probe : sonde jetable go/no-go pour le Web Push vers une PWA iPhone.
//!
//! Sous-commandes : `serve`, `send`, `selftest-apple`.

mod push;
mod server;
mod store;

use std::process::ExitCode;

use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde_json::json;
use web_push_native::p256::elliptic_curve::rand_core::{OsRng, RngCore};
use web_push_native::p256::elliptic_curve::sec1::ToEncodedPoint;

use push::{
    PushResponse, VAPID_CONTACT, build_request, endpoint_host, execute, header_text, is_success,
};
use store::{Subscription, SubscriptionKeys, b64url_encode};

const USAGE: &str = "usage:
  companion-probe serve
  companion-probe send --session <id> --title <text> --body <text> [--badge <n>]
  companion-probe selftest-apple";

/// Endpoint Apple synthetique : le jeton n'appartient a aucun appareil.
const SYNTHETIC_APPLE_ENDPOINT: &str = "https://web.push.apple.com/QGZha2UtcHJvYmUtdG9rZW4";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("serve") if args.len() == 1 => cmd_serve().await,
        Some("send") => cmd_send(&args[1..]).await,
        Some("selftest-apple") if args.len() == 1 => cmd_selftest_apple().await,
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_serve() -> Result<bool, String> {
    let port: u16 = match std::env::var("PORT") {
        Ok(text) => text.parse().map_err(|_| format!("invalid PORT: {text}"))?,
        Err(_) => 3910,
    };
    let dir = store::state_dir()?;
    let key = store::load_or_create_vapid(&dir)?;
    store::append_log(&dir, "serve-start", json!({ "port": port }), None);
    server::run(dir, store::vapid_public_key(&key), port).await?;
    Ok(true)
}

struct SendArgs {
    session: String,
    title: String,
    body: String,
    badge: Option<u32>,
}

fn parse_send_args(args: &[String]) -> Result<SendArgs, String> {
    let (mut session, mut title, mut body, mut badge) = (None, None, None, None);
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let mut value = || {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--session" => session = Some(value()?),
            "--title" => title = Some(value()?),
            "--body" => body = Some(value()?),
            "--badge" => {
                let text = value()?;
                badge = Some(
                    text.parse()
                        .map_err(|_| format!("invalid --badge: {text}"))?,
                );
            }
            other => return Err(format!("unknown argument: {other}\n{USAGE}")),
        }
    }
    Ok(SendArgs {
        session: session.ok_or("missing --session")?,
        title: title.ok_or("missing --title")?,
        body: body.ok_or("missing --body")?,
        badge,
    })
}

async fn cmd_send(args: &[String]) -> Result<bool, String> {
    let args = parse_send_args(args)?;
    let dir = store::state_dir()?;
    let key = store::load_or_create_vapid(&dir)?;
    let subscriptions = store::load_subscriptions(&dir)?;
    if subscriptions.is_empty() {
        eprintln!("no stored subscription: open the PWA and enable notifications first");
        return Ok(false);
    }

    let payload = json!({
        "title": args.title,
        "body": args.body,
        "session": args.session,
        "badge": args.badge,
        "sent_at": store::now_rfc3339(),
    });
    // Enregistre avant l'envoi : la page doit retrouver le texte meme si un push echoue.
    store::record_last_push(&dir, &args.session, &payload)?;
    let bytes = serde_json::to_vec(&payload).map_err(|err| err.to_string())?;

    let client = push::http_client()?;
    let mut all_ok = true;
    for sub in &subscriptions {
        let host = endpoint_host(&sub.endpoint);
        let outcome = async {
            let request = build_request(sub, &bytes, &key, VAPID_CONTACT)?;
            execute(&client, request).await
        }
        .await;
        match outcome {
            Ok(response) => {
                let ok = is_success(response.status);
                all_ok &= ok;
                println!(
                    "{host}: HTTP {} apns-id={} body={:?}",
                    response.status,
                    header_text(&response.headers, "apns-id").unwrap_or_else(|| "-".into()),
                    response.body
                );
                store::append_log(
                    &dir,
                    "push-sent",
                    json!({
                        "endpoint_host": host,
                        "status": response.status,
                        "apns_id": header_text(&response.headers, "apns-id"),
                        "body": response.body,
                        "session": args.session,
                    }),
                    None,
                );
            }
            Err(err) => {
                all_ok = false;
                println!("{host}: ERROR {err}");
                store::append_log(
                    &dir,
                    "push-error",
                    json!({ "endpoint_host": host, "error": err, "session": args.session }),
                    None,
                );
            }
        }
    }
    Ok(all_ok)
}

/// Abonnement synthetique : vraie cle P-256 fraiche, secret d'auth aleatoire, jeton Apple bidon.
fn synthetic_subscription() -> Subscription {
    let secret = web_push_native::p256::SecretKey::random(&mut OsRng);
    let point = secret.public_key().to_encoded_point(false);
    let mut auth = [0u8; 16];
    OsRng.fill_bytes(&mut auth);
    Subscription {
        endpoint: SYNTHETIC_APPLE_ENDPOINT.to_string(),
        keys: SubscriptionKeys {
            p256dh: b64url_encode(point.as_bytes()),
            auth: b64url_encode(&auth),
        },
        added_at: store::now_rfc3339(),
        user_agent: None,
    }
}

fn print_response(label: &str, outcome: Result<PushResponse, String>) {
    println!("--- {label}");
    match outcome {
        Ok(response) => {
            println!("status: {}", response.status);
            for (name, value) in &response.headers {
                println!("header {name}: {}", value.to_str().unwrap_or("<non-utf8>"));
            }
            println!("body: {:?}", response.body);
        }
        Err(err) => println!("error: {err}"),
    }
}

async fn cmd_selftest_apple() -> Result<bool, String> {
    let dir = store::state_dir()?;
    let key = store::load_or_create_vapid(&dir)?;
    let sub = synthetic_subscription();
    let payload = serde_json::to_vec(&json!({ "title": "selftest", "body": "synthetic" }))
        .map_err(|err| err.to_string())?;
    let client = push::http_client()?;

    // (a) configuration VAPID reelle.
    let request = build_request(&sub, &payload, &key, VAPID_CONTACT)?;
    let result_a = execute(&client, request).await;
    store::append_log(&dir, "selftest-apple-a", summarize(&result_a), None);
    print_response("(a) real VAPID setup", result_a);

    // (b) `sub` volontairement invalide (ni mailto: ni https:).
    let request = build_request(&sub, &payload, &key, "not-a-contact")?;
    let result_b = execute(&client, request).await;
    store::append_log(&dir, "selftest-apple-b", summarize(&result_b), None);
    print_response("(b) invalid VAPID sub \"not-a-contact\"", result_b);

    // (c) complement : jeton JWT illisible, avec la vraie cle publique dans `k`.
    let mut request = build_request(&sub, &payload, &key, VAPID_CONTACT)?;
    let garbage = format!(
        "vapid t=garbage.jwt.token, k={}",
        store::vapid_public_key(&key)
    );
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&garbage).map_err(|err| err.to_string())?,
    );
    let result_c = execute(&client, request).await;
    store::append_log(&dir, "selftest-apple-c", summarize(&result_c), None);
    print_response("(c) extra: unreadable JWT", result_c);

    Ok(true)
}

fn summarize(outcome: &Result<PushResponse, String>) -> serde_json::Value {
    match outcome {
        Ok(response) => json!({ "status": response.status, "body": response.body }),
        Err(err) => json!({ "error": err }),
    }
}
