//! IdnmA, le réseau d'agents qu'IdemA lance (`idnma run <agent>`) : la version installée, la dernière release publiée
//! et l'installation par le script du site. L'état vit dans un global lu par la barre de menus (menu « Aide » et ligne
//! « Installer le réseau IDNMA » du menu « IdemA »).

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use gpui::{App, AppContext as _, Global, Window};
use regex::Regex;

use crate::errors::alert;
use crate::hud::log_event;
use crate::menus;
use crate::project_panel::skills_cli::login_shell;

/// Le script d'installation publié : il clone la dernière release, ou met à jour le checkout existant.
/// `pipefail` : sans lui, un `curl` raté donne un script vide à `bash`, qui rend 0, et l'échec passerait pour un succès.
const INSTALL_COMMAND: &str =
    "set -o pipefail; curl -fsSL https://erom.cloud/install_idnma.sh | bash";

/// Le dépôt dont les tags `v*` sont les releases (celui que clone le script).
const RELEASES_REPO: &str = "https://github.com/eRom/erom-IDNMA.git";

/// Le plus de lignes qu'une alerte d'échec montre : la fin du journal de l'installeur, où l'erreur se trouve.
const MAX_FAILURE_LINES: usize = 4;

/// Une version `x.y.z` ; l'ordre des champs donne l'ordre des versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// La version installée, telle que la sonde la connaît.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// La sonde n'a pas encore répondu.
    Unknown,
    /// `idnma --version` ne répond pas : la commande n'existe pas pour le shell de connexion.
    Missing,
    Present(Version),
}

/// Ce que la barre de menus sait d'IdnmA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdnmaStatus {
    pub installed: Installed,
    /// La dernière release ; `None` tant qu'on ne la connaît pas (sonde en cours, réseau absent).
    pub latest: Option<Version>,
    pub installing: bool,
}

impl Global for IdnmaStatus {}

impl Default for IdnmaStatus {
    fn default() -> Self {
        Self {
            installed: Installed::Unknown,
            latest: None,
            installing: false,
        }
    }
}

impl IdnmaStatus {
    /// Vrai sauf pendant une installation, ou quand la version installée est connue et au moins égale à la dernière
    /// release connue : une release inconnue (hors ligne) ne prouve pas qu'IdnmA est à jour.
    pub fn can_install(&self) -> bool {
        if self.installing {
            return false;
        }
        match (self.installed, self.latest) {
            (Installed::Present(installed), Some(latest)) => installed < latest,
            _ => true,
        }
    }

    /// La ligne du menu « Aide ».
    pub fn label(&self) -> String {
        if self.installing {
            return "IdnmA : installation en cours…".to_string();
        }
        match (self.installed, self.latest) {
            (Installed::Unknown, _) => "IdnmA …".to_string(),
            (Installed::Missing, _) => "IdnmA non installé".to_string(),
            (Installed::Present(installed), Some(latest)) if installed < latest => {
                format!("IdnmA {installed} ({latest} disponible)")
            }
            (Installed::Present(installed), _) => format!("IdnmA {installed}"),
        }
    }
}

fn version_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^v?(\d+)\.(\d+)\.(\d+)$").expect("valid version pattern"))
}

/// `x.y.z` ou `vx.y.z`, rien d'autre : une préversion (`v0.5.0-rc1`) n'est pas une release.
pub fn parse_version(text: &str) -> Option<Version> {
    let captures = version_pattern().captures(text.trim())?;
    let field = |index: usize| captures[index].parse::<u64>().ok();
    Some(Version(field(1)?, field(2)?, field(3)?))
}

/// La version qu'imprime `idnma --version` (« idnma 0.4.0 »), cherchée ligne à ligne : le `.zshrc` d'un shell
/// interactif peut écrire avant elle.
pub fn parse_version_output(stdout: &str) -> Option<Version> {
    stdout.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("idnma")?;
        parse_version(rest)
    })
}

/// La plus haute release parmi les lignes de `git ls-remote --tags --refs` (`<sha>\trefs/tags/v0.4.0`).
pub fn latest_from_ls_remote(stdout: &str) -> Option<Version> {
    stdout
        .lines()
        .filter_map(|line| line.split_once("refs/tags/"))
        .filter_map(|(_, tag)| tag.strip_prefix('v').and_then(parse_version))
        .max()
}

/// Les `max` dernières lignes non vides de `text`.
fn last_lines(text: &str, max: usize) -> String {
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    kept[kept.len().saturating_sub(max)..].join("\n")
}

/// `script` par le shell de connexion interactif de l'utilisateur, stdin fermé : une app ouverte depuis le Finder n'a
/// pas le `PATH` de son terminal, où vivent `bun` et `idnma`.
fn run_in_login_shell(script: &str) -> std::io::Result<Output> {
    let shell = login_shell(std::env::var_os("SHELL").as_deref());
    let mut command = Command::new(shell);
    command.args(["-l", "-i", "-c", script]);
    detached(&mut command).output()
}

/// Stdin fermé, sans invite Git, depuis le dossier personnel : le dossier courant de l'app peut être un dépôt cassé
/// (un worktree dont le `.git` pointe ailleurs), et `git ls-remote` y échoue (« not a git repository »).
fn detached(command: &mut Command) -> &mut Command {
    command
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        command.current_dir(home);
    }
    command
}

fn probe_installed() -> Installed {
    match run_in_login_shell("idnma --version") {
        Ok(output) if output.status.success() => {
            match parse_version_output(&String::from_utf8_lossy(&output.stdout)) {
                Some(version) => Installed::Present(version),
                None => {
                    log_event("idnma version unreadable");
                    Installed::Missing
                }
            }
        }
        Ok(_) => Installed::Missing,
        Err(error) => {
            log_event(&format!("idnma version probe failed: {error}"));
            Installed::Missing
        }
    }
}

fn probe_latest() -> Option<Version> {
    let output =
        detached(Command::new("git").args(["ls-remote", "--tags", "--refs", RELEASES_REPO, "v*"]))
            .output();
    match output {
        Ok(output) if output.status.success() => {
            latest_from_ls_remote(&String::from_utf8_lossy(&output.stdout))
        }
        Ok(output) => {
            log_event(&format!(
                "idnma releases probe failed: {}",
                last_lines(&String::from_utf8_lossy(&output.stderr), 1)
            ));
            None
        }
        Err(error) => {
            log_event(&format!("idnma releases probe failed: {error}"));
            None
        }
    }
}

/// Change l'état, puis repose la barre de menus et repeint les fenêtres : la ligne « Installer » est grisée par
/// l'absence de son gestionnaire, que la fenêtre ne pose qu'au rendu.
fn set_status(cx: &mut App, change: impl FnOnce(&mut IdnmaStatus)) {
    change(cx.default_global::<IdnmaStatus>());
    menus::rebuild(cx);
    cx.refresh_windows();
}

/// Sonde la version installée et la dernière release hors du fil d'interface. La version locale arrive seule, sans
/// attendre le réseau.
pub fn refresh(cx: &mut App) {
    let installed = cx.background_spawn(async { probe_installed() });
    let latest = cx.background_spawn(async { probe_latest() });
    cx.spawn(async move |cx| {
        let installed = installed.await;
        cx.update(|cx| set_status(cx, |status| status.installed = installed));
        let latest = latest.await;
        cx.update(|cx| set_status(cx, |status| status.latest = latest));
    })
    .detach();
}

/// La ligne « Installer le réseau IDNMA » : lance le script du site, puis dit la version posée, ou pourquoi c'est raté.
pub fn install(window: &mut Window, cx: &mut App) {
    if !cx.default_global::<IdnmaStatus>().can_install() {
        return;
    }
    set_status(cx, |status| status.installing = true);
    log_event("idnma install started");
    let installing = cx.background_spawn(async {
        let output = run_in_login_shell(INSTALL_COMMAND)?;
        let installed = probe_installed();
        Ok::<_, std::io::Error>((output, installed))
    });
    let window = window.window_handle();
    cx.spawn(async move |cx| {
        let result = installing.await;
        cx.update(|cx| {
            let installed = match &result {
                Ok((_, installed)) => *installed,
                Err(_) => cx.default_global::<IdnmaStatus>().installed,
            };
            set_status(cx, |status| {
                status.installing = false;
                status.installed = installed;
            });
            let (message, detail) = match result {
                Ok((output, Installed::Present(version))) if output.status.success() => {
                    log_event(&format!("idnma install succeeded version={version}"));
                    (
                        format!("IdnmA {version} installé."),
                        "Les agents se lancent par idnma run <agent>.".to_string(),
                    )
                }
                Ok((output, _)) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let mut detail = last_lines(&stderr, MAX_FAILURE_LINES);
                    if detail.is_empty() {
                        detail = last_lines(&stdout, MAX_FAILURE_LINES);
                    }
                    log_event(&format!(
                        "idnma install failed code={:?} detail={detail}",
                        output.status.code()
                    ));
                    ("L'installation d'IdnmA a échoué.".to_string(), detail)
                }
                Err(error) => {
                    log_event(&format!("idnma install failed: {error}"));
                    (
                        "L'installation d'IdnmA a échoué.".to_string(),
                        error.to_string(),
                    )
                }
            };
            let _ = window.update(cx, |_, window, cx| {
                alert(window, cx, &message, Some(&detail));
            });
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    const V040: Version = Version(0, 4, 0);

    fn status(installed: Installed, latest: Option<Version>) -> IdnmaStatus {
        IdnmaStatus {
            installed,
            latest,
            installing: false,
        }
    }

    #[test]
    fn versions_order_by_field_not_by_text() {
        assert!(Version(0, 10, 0) > Version(0, 9, 3));
        assert_eq!(parse_version("v1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(parse_version("0.5.0-rc1"), None);
    }

    #[test]
    fn the_version_is_found_past_shell_noise() {
        let stdout = "Welcome back\nidnma 0.4.0\n";
        assert_eq!(parse_version_output(stdout), Some(V040));
        assert_eq!(parse_version_output("zsh: command not found: idnma"), None);
    }

    #[test]
    fn the_latest_release_is_the_highest_plain_tag() {
        let stdout = "a1\trefs/tags/v0.9.0\nb2\trefs/tags/v0.10.0\nc3\trefs/tags/v0.11.0-rc1\nd4\trefs/tags/vnext\n";
        assert_eq!(latest_from_ls_remote(stdout), Some(Version(0, 10, 0)));
        assert_eq!(latest_from_ls_remote(""), None);
    }

    #[test]
    fn install_is_greyed_only_when_installed_and_up_to_date() {
        assert!(!status(Installed::Present(V040), Some(V040)).can_install());
        assert!(!status(Installed::Present(Version(0, 5, 0)), Some(V040)).can_install());
        assert!(status(Installed::Present(Version(0, 3, 9)), Some(V040)).can_install());
        assert!(status(Installed::Missing, Some(V040)).can_install());
        // Hors ligne, rien ne prouve qu'IdnmA est à jour.
        assert!(status(Installed::Present(V040), None).can_install());
        let mut busy = status(Installed::Missing, Some(V040));
        busy.installing = true;
        assert!(!busy.can_install());
    }

    #[test]
    fn the_help_line_names_the_version_or_its_absence() {
        assert_eq!(
            status(Installed::Present(V040), Some(V040)).label(),
            "IdnmA 0.4.0"
        );
        assert_eq!(
            status(Installed::Missing, Some(V040)).label(),
            "IdnmA non installé"
        );
        assert_eq!(
            status(Installed::Present(Version(0, 3, 0)), Some(V040)).label(),
            "IdnmA 0.3.0 (0.4.0 disponible)"
        );
    }
}
