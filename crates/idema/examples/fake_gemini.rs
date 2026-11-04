//! Faux serveur Gemini pour les tests de B-11 : std seul, HTTP/1.1 en clair sur 127.0.0.1.
//! Usage : `cargo run -p idema --example fake_gemini -- --record <fichier> [--chunks <n>] [--delay-ms <n>]
//! [--status <code>] [--hang-after <n>] [--midstream-error] [--unterminated-then-hang]`. Il écrit `port=<n>` sur stdout, ajoute à `--record`
//! la ligne de requête, les en-têtes et le corps de chaque requête, puis répond :
//! - `n` fragments SSE à la forme de l'échantillon réel (CRLF), espacés de `delay-ms`, puis un événement final
//!   `finishReason` à texte vide (comme la vraie API, sans sentinelle) ;
//! - ou le code `--status` avec un corps d'erreur JSON (400 porte `API_KEY_INVALID`, comme la vraie API) ;
//! - ou, avec `--hang-after n`, il cesse d'écrire après `n` fragments et garde la connexion ouverte ;
//! - ou, avec `--midstream-error`, un événement `{"error":…}` après le premier fragment ;
//! - ou, avec `--unterminated-then-hang`, un premier fragment SANS sa ligne vide, puis le silence connexion ouverte.
//!
//! Les tests du crate l'incluent par `#[path]` (précédent : `editor_probe.rs`) et appellent `serve`.

// Inclus par les tests, `main` et l'analyse des arguments n'y ont pas de lecteur ; en exemple, tout est lu.
#![allow(dead_code)]

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Les réglages d'un faux serveur : le même jeu que la ligne de commande.
#[derive(Debug, Clone)]
pub struct FakeOptions {
    pub record: PathBuf,
    pub chunks: usize,
    pub delay_ms: u64,
    pub status: Option<u16>,
    pub hang_after: Option<usize>,
    pub midstream_error: bool,
    pub unterminated_then_hang: bool,
}

impl FakeOptions {
    pub fn new(record: PathBuf) -> Self {
        Self {
            record,
            chunks: 3,
            delay_ms: 0,
            status: None,
            hang_after: None,
            midstream_error: false,
            unterminated_then_hang: false,
        }
    }
}

/// Le temps maximal pendant lequel une connexion « suspendue » reste ouverte si le client ne la ferme pas.
const HANG_LIMIT: Duration = Duration::from_secs(30);

/// Écoute sur `127.0.0.1:0` et sert chaque connexion dans son fil. Rend le port et le fil d'écoute, qui ne s'arrête
/// pas de lui-même : les tests le laissent partir avec le processus.
pub fn serve(options: FakeOptions) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake server");
    let port = listener.local_addr().expect("local address").port();
    let handle = thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let options = options.clone();
            thread::spawn(move || {
                let _ = handle_connection(stream, &options);
            });
        }
    });
    (port, handle)
}

fn handle_connection(mut stream: TcpStream, options: &FakeOptions) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let request = read_request(&mut stream)?;
    record_request(&options.record, &request)?;

    if let Some(status) = options.status {
        return write_error(&mut stream, status);
    }

    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    )?;
    stream.flush()?;

    if options.unterminated_then_hang {
        // Un événement ouvert : la ligne `data:` finie par un seul CRLF, sans la ligne vide qui le ferme.
        stream.write_all(format!("data: {}\r\n", fragment_json(1)).as_bytes())?;
        stream.flush()?;
        return hang_until_closed(&mut stream);
    }

    for index in 1..=options.chunks {
        write_event(&mut stream, &fragment_json(index))?;
        if options.midstream_error && index == 1 {
            write_event(
                &mut stream,
                r#"{"error":{"code":500,"message":"boom","status":"INTERNAL"}}"#,
            )?;
            return Ok(());
        }
        if options.hang_after == Some(index) {
            return hang_until_closed(&mut stream);
        }
        if index < options.chunks && options.delay_ms > 0 {
            thread::sleep(Duration::from_millis(options.delay_ms));
        }
    }
    write_event(&mut stream, &final_event())
}

/// Un fragment à la forme de l'échantillon réel : un seul `candidates[0].content.parts[0].text`.
fn fragment_json(index: usize) -> String {
    format!(
        r#"{{"candidates": [{{"content": {{"parts": [{{"text": "fragment {index}"}}],"role": "model"}},"index": 0}}],"modelVersion": "fake-gemini"}}"#
    )
}

/// Le dernier événement réel : `finishReason`, texte vide.
fn final_event() -> String {
    r#"{"candidates": [{"content": {"parts": [{"text": ""}],"role": "model"},"finishReason": "STOP","index": 0}],"modelVersion": "fake-gemini"}"#.to_string()
}

fn write_event(stream: &mut TcpStream, json: &str) -> std::io::Result<()> {
    stream.write_all(format!("data: {json}\r\n\r\n").as_bytes())?;
    stream.flush()
}

/// Garde la connexion ouverte sans rien écrire, jusqu'à la fermeture par le client (lecture à 0) ou `HANG_LIMIT`.
fn hang_until_closed(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(HANG_LIMIT))?;
    let mut sink = [0u8; 64];
    loop {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => return Ok(()),
            Ok(_) => {}
        }
    }
}

/// La réponse d'erreur : JSON comme la vraie API, avec `API_KEY_INVALID` pour le 400 de la clé fausse.
fn write_error(stream: &mut TcpStream, status: u16) -> std::io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let body = if status == 400 {
        r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID","domain":"googleapis.com"}]}}"#.to_string()
    } else {
        format!(
            r#"{{"error":{{"code":{status},"message":"fake error {status}","status":"FAKE"}}}}"#
        )
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

/// Lit la requête : ligne, en-têtes, puis le corps selon `Content-Length`. Rend le texte tel que reçu.
fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut received = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(position) = find(&received, b"\r\n\r\n") {
            break position + 4;
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(String::from_utf8_lossy(&received).into_owned());
        }
        received.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&received[..header_end]).into_owned();
    let content_length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while received.len() < header_end + content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        received.extend_from_slice(&chunk[..read]);
    }
    Ok(String::from_utf8_lossy(&received).into_owned())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Ajoute la requête au fichier d'enregistrement, séparée de la précédente par une ligne de tirets.
fn record_request(path: &PathBuf, request: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(request.as_bytes())?;
    file.write_all(b"\n-----\n")
}

fn parse_options(args: &[String]) -> Result<FakeOptions, String> {
    let mut record = None;
    let mut options = FakeOptions::new(PathBuf::new());
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let mut value = |name: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{name} expects a value"))
        };
        match flag.as_str() {
            "--record" => record = Some(PathBuf::from(value("--record")?)),
            "--chunks" => options.chunks = parse_number(&value("--chunks")?)?,
            "--delay-ms" => options.delay_ms = parse_number(&value("--delay-ms")?)?,
            "--status" => options.status = Some(parse_number(&value("--status")?)?),
            "--hang-after" => options.hang_after = Some(parse_number(&value("--hang-after")?)?),
            "--midstream-error" => options.midstream_error = true,
            "--unterminated-then-hang" => options.unterminated_then_hang = true,
            other => return Err(format!("unknown option: {other}")),
        }
    }
    options.record = record.ok_or("--record is required")?;
    Ok(options)
}

fn parse_number<T: std::str::FromStr>(text: &str) -> Result<T, String> {
    text.parse().map_err(|_| format!("number expected: {text}"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_options(&args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("fake_gemini: {message}");
            std::process::exit(2);
        }
    };
    let (port, handle) = serve(options);
    println!("port={port}");
    let _ = std::io::stdout().flush();
    let _ = handle.join();
}
