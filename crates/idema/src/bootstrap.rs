//! Amorçage de tmux, hors du fil d'interface (AC-9 : la barre s'affiche avant que tmux ait répondu) :
//! binaire, version, config, intégration zsh, puis l'inventaire des sessions et des panes.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::oneshot;

use idema_tmux::env::server_env;
use idema_tmux::server::{ListedPane, ListedSession, ServerError, TmuxServer, locate_tmux};
use idema_tmux::shell::write_zsh_integration;

use crate::config::load_config;
use crate::hud;

/// Ce que l'amorçage rend au fil d'interface.
pub struct Bootstrap {
    pub server: Arc<TmuxServer>,
    pub sessions: Vec<ListedSession>,
    pub panes: Vec<ListedPane>,
}

/// Le résultat de l'amorçage et sa durée, tels que `spawn` les livre au fil d'interface.
pub type Outcome = (Result<Bootstrap, ServerError>, Duration);

/// Lance l'amorçage sur son propre fil, depuis `main` et avant GPUI : tmux répond pendant que la fenêtre se
/// crée (environ 200 ms), au lieu de ne démarrer qu'à la première image (P-1, mesure du 15/10). Le résultat
/// arrive par le canal ; `bootstrap_ms` compte depuis ce départ. La config est relue ici pour `tmux_path` :
/// absente ou invalide, elle ne change rien (l'espace de travail la signale de son côté).
pub fn spawn(dir: PathBuf, socket: String) -> oneshot::Receiver<Outcome> {
    let (sender, receiver) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("idema-bootstrap".into())
        .spawn(move || {
            let started = Instant::now();
            let tmux_path = load_config(&dir.join("config.toml"))
                .ok()
                .and_then(|config| config.tmux_path);
            let step = Instant::now();
            let inherited: Vec<(String, String)> = std::env::vars().collect();
            let locale = apple_locale();
            hud::log_step("environment_and_locale", step);
            let result = bootstrap(
                &dir,
                &socket,
                tmux_path.as_deref(),
                inherited,
                locale.as_deref(),
            );
            // Le récepteur est fermé quand la fenêtre l'est : plus personne à prévenir.
            let _ = sender.send((result, started.elapsed()));
        });
    if let Err(error) = spawned {
        hud::log_event(&format!("bootstrap thread failed: {error}"));
    }
    receiver
}

/// `LANG` UTF-8 de repli : la locale d'Apple, que `defaults` donne (spec §3.5).
pub(crate) fn apple_locale() -> Option<String> {
    let output = Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Prépare le serveur `socket` et liste ses sessions. `inherited` est l'environnement d'Idema, d'où la
/// liste blanche tire ses variables (spec §3.5) ; `apple_locale` donne un `LANG` UTF-8 s'il manque.
pub fn bootstrap(
    dir: &Path,
    socket: &str,
    tmux_path: Option<&Path>,
    inherited: Vec<(String, String)>,
    apple_locale: Option<&str>,
) -> Result<Bootstrap, ServerError> {
    let step = Instant::now();
    let path_var = inherited
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.clone());
    let binary = locate_tmux(tmux_path, path_var.as_deref())?;
    hud::log_step("locate_tmux", step);
    let step = Instant::now();
    let shell_dir = dir.join("shell").join("zsh");
    write_zsh_integration(&shell_dir)?;
    let shell_dir = shell_dir.to_string_lossy().into_owned();
    let mut env = server_env(inherited, apple_locale);
    env.push(("ZDOTDIR".into(), shell_dir.clone()));
    let server = TmuxServer::new(binary, socket, dir.join("tmux.conf"), env);
    hud::log_step("zsh_integration", step);
    let step = Instant::now();
    server.check_version()?;
    hud::log_step("check_version", step);
    let step = Instant::now();
    server.write_config()?;
    hud::log_step("write_config", step);
    // Un serveur lancé avant cette version d'Idema n'a pas l'intégration dans son environnement global.
    let step = Instant::now();
    server.set_global_environment("ZDOTDIR", &shell_dir)?;
    hud::log_step("set_global_environment", step);
    let step = Instant::now();
    let sessions = server.sessions()?;
    hud::log_step("list_sessions", step);
    let step = Instant::now();
    let panes = server.panes()?;
    hud::log_step("list_panes", step);
    Ok(Bootstrap {
        server: Arc::new(server),
        sessions,
        panes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use idema_tmux::server::NewSession;

    #[test]
    fn the_early_bootstrap_delivers_its_outcome_through_the_channel() {
        let dir = std::env::temp_dir().join(format!("idema-test-{}-early", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Le socket vit dans le dossier du test (`-S`) : le nettoyage du dossier le retire.
        let socket = dir.join("sock").to_string_lossy().into_owned();
        let (result, elapsed) = futures::executor::block_on(spawn(dir.clone(), socket))
            .expect("the bootstrap thread answers");
        let bootstrap = result.expect("tmux bootstraps on a fresh socket");
        assert!(bootstrap.sessions.is_empty() && bootstrap.panes.is_empty());
        assert!(elapsed > Duration::ZERO);
        let _ = bootstrap.server.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bootstrap_lists_sessions_and_sets_the_global_zdotdir() {
        let dir = std::env::temp_dir().join(format!("idema-test-{}-bootstrap", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("sock").to_string_lossy().into_owned();
        let mut inherited: Vec<(String, String)> = std::env::vars()
            .filter(|(key, _)| key != "SHELL" && key != "ZDOTDIR")
            .collect();
        inherited.push(("SHELL".into(), "/bin/sh".into()));
        // Un ZDOTDIR hérité (Idema lancé depuis un shell) ne doit pas passer : l'intégration le remplace.
        inherited.push(("ZDOTDIR".into(), "/nonexistent/zdot".into()));

        let first = bootstrap(&dir, &socket, None, inherited.clone(), None).unwrap();
        assert!(first.sessions.is_empty() && first.panes.is_empty());
        let created = first
            .server
            .create_session(&NewSession {
                project: &dir,
                display_name: "Shell",
                agent: "Shell",
                columns: 80,
                lines: 24,
                remote_name: None,
            })
            .unwrap();

        let second = bootstrap(&dir, &socket, None, inherited, None).unwrap();
        assert_eq!(second.sessions.len(), 1);
        assert_eq!(second.panes[0].pane, created.pane);
        let zdotdir = second
            .server
            .run(&["show-environment", "-g", "ZDOTDIR"])
            .unwrap();
        let expected = dir.join("shell").join("zsh");
        assert_eq!(zdotdir, format!("ZDOTDIR={}", expected.display()));
        assert!(expected.join(".zshenv").is_file());
        let _ = second.server.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
