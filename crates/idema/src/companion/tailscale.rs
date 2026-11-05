//! L'adresse tailnet du compagnon : `tailscale serve status --json` dit quelle entrée `Web` mandate notre port, et si
//! Funnel l'ouvre à Internet. Tout ici est bloquant : le service l'appelle depuis `spawn_blocking`.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use serde_json::Value;
use url::Url;

/// L'app Tailscale embarque le CLI : repli quand `tailscale` n'est pas dans le `PATH`.
const BUNDLE_BINARY: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

/// Le CLI peut pendre quand le démon est arrêté : au-delà, l'adresse est « inconnue ».
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Le temps que la sortie d'une commande finie a pour arriver au lecteur.
const READ_GRACE: Duration = Duration::from_millis(500);

/// Le premier port HTTPS proposé par la commande à lancer (l'exemple de la spec, §3.2).
const FIRST_HTTPS_PORT: u16 = 8455;

/// Une adresse tailnet qui mandate le port du compagnon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailnetEndpoint {
    /// `https://<hôte>[:<port>]`, sans chemin.
    pub url: String,
    /// Funnel ouvre cette adresse à Internet.
    pub funnel: bool,
}

/// Ce que la détection a trouvé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailnetState {
    /// Le port est mandaté (ou, faute de mieux, `public_url` l'affirme).
    Exposed(TailnetEndpoint),
    /// Tailscale répond, mais rien ne mandate le port : la commande exacte à lancer.
    NotExposed { command: String },
    /// La détection n'a pas pu se faire : un mot technique (`binary_not_found`, `timeout`…).
    Unknown(String),
}

/// La détection, telle que le service l'appelle : remplaçable par un test.
pub type Detector = Arc<dyn Fn(u16, Option<&str>) -> TailnetState + Send + Sync>;

pub fn system_detector() -> Detector {
    Arc::new(detect)
}

/// L'adresse qui mandate `port` dans la sortie de `tailscale serve status --json`, ou `None` (rien ne le mandate, ou
/// sortie illisible). Plusieurs adresses possibles : celle que Funnel ouvre à Internet passe devant, c'est elle que
/// l'alerte doit nommer.
pub fn parse_serve_status(json: &str, port: u16) -> Option<TailnetEndpoint> {
    endpoint_in(&serde_json::from_str(json).ok()?, port)
}

fn endpoint_in(status: &Value, port: u16) -> Option<TailnetEndpoint> {
    status
        .get("Web")?
        .as_object()?
        .iter()
        .filter_map(|(host_port, web)| {
            let (host, https_port) = host_port.rsplit_once(':')?;
            let https_port: u16 = https_port.parse().ok()?;
            // Un `--http=` ne donne pas une adresse de PWA : seul le TLS compte.
            let terminates_tls = status
                .pointer(&format!("/TCP/{https_port}/HTTPS"))
                .and_then(Value::as_bool)
                == Some(true);
            let proxy = web.pointer("/Handlers/~1/Proxy")?.as_str()?;
            if !terminates_tls || !proxies_to_loopback(proxy, port) {
                return None;
            }
            let url = if https_port == 443 {
                format!("https://{host}")
            } else {
                format!("https://{host}:{https_port}")
            };
            // L'hôte finit dans une adresse affichée et un QR code : il doit en être un.
            if Url::parse(&url).ok()?.host_str() != Some(host) {
                return None;
            }
            let funnel = status
                .pointer("/AllowFunnel")
                .and_then(|allowed| allowed.get(host_port))
                .and_then(Value::as_bool)
                == Some(true);
            Some(TailnetEndpoint { url, funnel })
        })
        .min_by_key(|endpoint| !endpoint.funnel)
}

/// `http://localhost:<port>` ou `http://127.0.0.1:<port>`, sans chemin : les deux orthographes que `tailscale serve`
/// écrit pour un service local.
fn proxies_to_loopback(proxy: &str, port: u16) -> bool {
    let Ok(target) = Url::parse(proxy) else {
        return false;
    };
    target.scheme() == "http"
        && matches!(target.host_str(), Some("localhost" | "127.0.0.1"))
        && target.port_or_known_default() == Some(port)
        && target.path() == "/"
}

/// Le binaire `tailscale` : le `PATH` d'abord, puis l'app (un IdemA lancé depuis le Finder n'a pas le `PATH` d'un shell).
pub fn find_binary(path_env: Option<&str>) -> Option<PathBuf> {
    find_binary_in(path_env, Path::new(BUNDLE_BINARY))
}

fn find_binary_in(path_env: Option<&str>, bundle: &Path) -> Option<PathBuf> {
    path_env
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("tailscale"))
        .find(|candidate| is_executable(candidate))
        .or_else(|| is_executable(bundle).then(|| bundle.to_path_buf()))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// La commande qui expose `port` sur le tailnet. Le port HTTPS est le premier libre à partir de 8455 : `tailscale serve`
/// remplace sans prévenir ce qui occupe déjà le sien.
fn serve_command(port: u16, status: Option<&Value>) -> String {
    let taken = |https_port: u16| {
        status
            .and_then(|status| status.get("TCP")?.get(https_port.to_string()))
            .is_some()
    };
    let https_port = (FIRST_HTTPS_PORT..=u16::MAX)
        .find(|candidate| !taken(*candidate))
        .unwrap_or(FIRST_HTTPS_PORT);
    format!("tailscale serve --bg --https={https_port} localhost:{port}")
}

/// Détecte l'adresse tailnet de `port` ; `configured` est le `public_url` de la config, qui sert de repli quand la
/// détection ne trouve rien. Bloquant, au plus `STATUS_TIMEOUT`.
pub fn detect(port: u16, configured: Option<&str>) -> TailnetState {
    let binary = find_binary(std::env::var("PATH").ok().as_deref());
    detect_with(binary.as_deref(), port, configured, STATUS_TIMEOUT)
}

fn detect_with(
    binary: Option<&Path>,
    port: u16,
    configured: Option<&str>,
    timeout: Duration,
) -> TailnetState {
    // Le `public_url` ne dit pas si Funnel est ouvert : il reste le dernier recours, jamais une preuve.
    let stand_in = || {
        configured.map(|url| {
            TailnetState::Exposed(TailnetEndpoint {
                url: url.to_string(),
                funnel: false,
            })
        })
    };
    let unknown = |reason: &str| stand_in().unwrap_or_else(|| TailnetState::Unknown(reason.into()));
    let Some(binary) = binary else {
        return unknown("binary_not_found");
    };
    let output = match run_status(binary, timeout) {
        Ok(output) => output,
        Err(reason) => return unknown(&reason),
    };
    if let Some(endpoint) = parse_serve_status(&output, port) {
        return TailnetState::Exposed(endpoint);
    }
    let Ok(status) = serde_json::from_str::<Value>(&output) else {
        return unknown("unreadable_output");
    };
    stand_in().unwrap_or_else(|| TailnetState::NotExposed {
        command: serve_command(port, Some(&status)),
    })
}

/// Lance `tailscale serve status --json` et rend sa sortie. Un démon éteint peut faire pendre la commande : passé
/// `timeout`, elle est tuée.
fn run_status(binary: &Path, timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(binary)
        .args(["serve", "status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("spawn_failed: {:?}", error.kind()))?;
    let mut stdout = child.stdout.take().ok_or("spawn_failed: no_stdout")?;
    // Lue à part : une sortie plus grande que le tube bloquerait sinon la commande avant sa fin.
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = sender.send(stdout.read_to_string(&mut text).map(|_| text));
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timeout".into());
            }
            Err(error) => return Err(format!("wait_failed: {:?}", error.kind())),
        }
    };
    if !status.success() {
        return Err(match status.code() {
            Some(code) => format!("exit_failed: {code}"),
            None => "exit_failed: signal".into(),
        });
    }
    match receiver.recv_timeout(READ_GRACE) {
        Ok(Ok(text)) => Ok(text),
        _ => Err("unreadable_output".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::test_support::TempDir;

    const SERVE: &str = include_str!("fixtures/serve-status.json");
    const SERVE_FUNNEL: &str = include_str!("fixtures/serve-status-funnel.json");

    /// Écrit un exécutable dans `dir` et rend son chemin.
    fn script(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        let path = dir.0.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Un faux `tailscale` qui imprime le contenu d'un fichier de fixture.
    fn printing(dir: &TempDir, json: &str) -> PathBuf {
        let data = dir.0.join("status.json");
        std::fs::write(&data, json).unwrap();
        script(dir, "tailscale", &format!("cat '{}'", data.display()))
    }

    #[test]
    fn parses_the_proxy_to_our_port() {
        assert_eq!(
            parse_serve_status(SERVE, 3910),
            Some(TailnetEndpoint {
                url: "https://forge.example.ts.net:8455".into(),
                funnel: false
            })
        );
    }

    #[test]
    fn port_443_has_no_port_in_the_url() {
        assert_eq!(
            parse_serve_status(SERVE, 7900).map(|endpoint| endpoint.url),
            Some("https://forge.example.ts.net".into())
        );
    }

    #[test]
    fn loopback_spellings_both_match() {
        // 3910 est mandaté par `localhost`, 7767 par `127.0.0.1` : les deux orthographes de `tailscale serve`.
        assert!(parse_serve_status(SERVE, 3910).is_some());
        assert_eq!(
            parse_serve_status(SERVE, 7767).map(|endpoint| endpoint.url),
            Some("https://forge.example.ts.net:8450".into())
        );
    }

    #[test]
    fn funnel_is_reported() {
        assert_eq!(
            parse_serve_status(SERVE_FUNNEL, 3910),
            Some(TailnetEndpoint {
                url: "https://forge.example.ts.net:8455".into(),
                funnel: true
            })
        );
        // Funnel ne vaut que pour l'adresse nommée : une autre entrée du même fichier reste fermée.
        assert_eq!(
            parse_serve_status(SERVE_FUNNEL, 7767).map(|endpoint| endpoint.funnel),
            Some(false)
        );
    }

    #[test]
    fn absent_port_gives_none() {
        assert_eq!(parse_serve_status(SERVE, 4000), None);
        assert_eq!(parse_serve_status("{}", 3910), None);
        assert_eq!(parse_serve_status("not json", 3910), None);
    }

    #[test]
    fn only_an_https_root_handler_to_loopback_counts() {
        let status = |tcp: &str, handlers: &str| {
            format!(
                r#"{{"TCP":{{"8455":{tcp}}},"Web":{{"forge.example.ts.net:8455":{{"Handlers":{handlers}}}}}}}"#
            )
        };
        let https = r#"{"HTTPS":true}"#;
        let root = |proxy: &str| format!(r#"{{"/":{{"Proxy":"{proxy}"}}}}"#);
        // Le cas sain, pour que les refus ci-dessous ne passent pas par une fixture mal formée.
        assert!(parse_serve_status(&status(https, &root("http://localhost:3910")), 3910).is_some());
        // En HTTP clair, l'adresse n'est pas celle d'une PWA.
        assert_eq!(
            parse_serve_status(
                &status(r#"{"HTTP":true}"#, &root("http://localhost:3910")),
                3910
            ),
            None
        );
        // Un autre hôte que la boucle locale, un autre chemin monté, un chemin mandaté : pas notre service.
        for handlers in [
            root("http://192.168.1.5:3910"),
            root("http://localhost:3910/app"),
            r#"{"/app":{"Proxy":"http://localhost:3910"}}"#.to_string(),
            r#"{"/":{"Path":"/tmp/site"}}"#.to_string(),
        ] {
            assert_eq!(
                parse_serve_status(&status(https, &handlers), 3910),
                None,
                "{handlers}"
            );
        }
    }

    #[test]
    fn the_not_exposed_command_is_exact() {
        assert_eq!(
            serve_command(3910, None),
            "tailscale serve --bg --https=8455 localhost:3910"
        );
    }

    #[test]
    fn the_command_never_takes_a_port_already_served() {
        // 8455 sert déjà un autre service dans la fixture : la commande ne le remplace pas.
        let status: serde_json::Value = serde_json::from_str(SERVE).unwrap();
        assert_eq!(
            serve_command(4000, Some(&status)),
            "tailscale serve --bg --https=8456 localhost:4000"
        );
    }

    #[test]
    fn bundle_path_is_the_fallback() {
        let dir = TempDir::new("ts-bundle");
        let bundle = script(&dir, "Tailscale", "exit 0");
        let empty = TempDir::new("ts-empty-path");
        assert_eq!(
            find_binary_in(Some(empty.0.to_str().unwrap()), &bundle),
            Some(bundle.clone())
        );
        assert_eq!(find_binary_in(None, &bundle), Some(bundle));
        assert_eq!(
            find_binary_in(None, &dir.0.join("missing")),
            None,
            "neither the PATH nor the bundle has it"
        );
    }

    #[test]
    fn the_path_wins_over_the_bundle() {
        let bin = TempDir::new("ts-path");
        let on_path = script(&bin, "tailscale", "exit 0");
        let other = TempDir::new("ts-other");
        let bundle = script(&other, "Tailscale", "exit 0");
        let path_env = format!("/nonexistent:{}", bin.0.display());
        assert_eq!(find_binary_in(Some(&path_env), &bundle), Some(on_path));
    }

    #[test]
    fn a_file_that_is_not_executable_is_not_a_binary() {
        let dir = TempDir::new("ts-plain");
        std::fs::write(dir.0.join("tailscale"), "x").unwrap();
        let none = dir.0.join("missing");
        assert_eq!(find_binary_in(Some(dir.0.to_str().unwrap()), &none), None);
    }

    const LONG: Duration = Duration::from_secs(10);

    #[test]
    fn detection_reads_what_the_cli_prints() {
        let dir = TempDir::new("ts-detect");
        let binary = printing(&dir, SERVE_FUNNEL);
        assert_eq!(
            detect_with(Some(&binary), 3910, None, LONG),
            TailnetState::Exposed(TailnetEndpoint {
                url: "https://forge.example.ts.net:8455".into(),
                funnel: true
            })
        );
        assert_eq!(
            detect_with(Some(&binary), 4000, None, LONG),
            TailnetState::NotExposed {
                command: "tailscale serve --bg --https=8456 localhost:4000".into()
            }
        );
    }

    #[test]
    fn a_configured_address_stands_in_when_nothing_is_detected() {
        let dir = TempDir::new("ts-configured");
        let binary = printing(&dir, "{}");
        let configured = "https://mac.example.ts.net:8443";
        let standing_in = TailnetState::Exposed(TailnetEndpoint {
            url: configured.into(),
            funnel: false,
        });
        assert_eq!(
            detect_with(Some(&binary), 3910, Some(configured), LONG),
            standing_in
        );
        // Sans binaire, ou binaire en panne : la même adresse reste la meilleure réponse.
        assert_eq!(detect_with(None, 3910, Some(configured), LONG), standing_in);
        // La détection prime quand elle trouve quelque chose.
        let found = printing(&dir, SERVE);
        assert_eq!(
            detect_with(Some(&found), 3910, Some(configured), LONG),
            TailnetState::Exposed(TailnetEndpoint {
                url: "https://forge.example.ts.net:8455".into(),
                funnel: false
            })
        );
    }

    #[test]
    fn an_unusable_cli_is_unknown_with_a_reason() {
        let dir = TempDir::new("ts-unknown");
        let reason = |state: TailnetState| match state {
            TailnetState::Unknown(reason) => reason,
            other => panic!("expected Unknown, got {other:?}"),
        };
        assert_eq!(
            reason(detect_with(None, 3910, None, LONG)),
            "binary_not_found"
        );
        let failing = script(&dir, "failing", "exit 1");
        assert_eq!(
            reason(detect_with(Some(&failing), 3910, None, LONG)),
            "exit_failed: 1"
        );
        let garbage = script(&dir, "garbage", "echo 'not json'");
        assert_eq!(
            reason(detect_with(Some(&garbage), 3910, None, LONG)),
            "unreadable_output"
        );
    }

    #[test]
    fn a_hung_cli_is_cut_at_the_timeout() {
        let dir = TempDir::new("ts-hung");
        // `exec` : tuer le process tue le `sleep`, sinon il garderait le tube ouvert.
        let hung = script(&dir, "hung", "exec sleep 30");
        let started = std::time::Instant::now();
        assert_eq!(
            detect_with(Some(&hung), 3910, None, Duration::from_millis(300)),
            TailnetState::Unknown("timeout".into())
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "cut after {:?}",
            started.elapsed()
        );
    }
}
