//! Le serveur tmux d'Idema : binaire, configuration, environnement et sessions (spec §3.1, §3.4, §3.5).

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::command::{is_supported, parse_version, tmux_arg};
use crate::layout::{Preset, plan_preset, preset_layout};
use crate::protocol::{PaneId, WindowId};
use crate::remote::{RemoteSpec, forbidden_on_remote, remote_command, remote_tmux_line};

/// Contenu de `~/.config/idema/tmux.conf`, réécrit à chaque lancement (spec §3.5).
///
/// `mouse on` ne change rien pour Idema : un client en mode contrôle n'envoie pas de souris à tmux, et les octets de
/// souris qu'Idema pose par `send-keys -H` arrivent au pane à l'identique (sonde du 21/10). Il fait taire l'avis de
/// Claude Code « tmux detected · … add 'set -g mouse on' ». `focus-events` reste `off` : en `on`, tmux envoie lui-même
/// ESC [ I et ESC [ O aux panes d'un client de contrôle, qu'il croit toujours au premier plan, à l'attache et à chaque
/// changement de pane (sonde du 21/10), ce qui contredirait les rapports d'Idema (`PaneView::report_focus`).
///
/// `exit-empty off` garde le serveur en vie quand sa dernière session meurt : la capture de l'instantané voit alors un
/// serveur vide (la règle des 10 s retire la session), au lieu d'une erreur qui la laisserait au gardien et la
/// ferait « restaurer » au lancement suivant, sans redémarrage. Un vrai arrêt du Mac tue toujours le serveur.
pub const TMUX_CONF: &str = "\
set -g status off
set -g set-titles off
set -g default-terminal \"tmux-256color\"
set -g history-limit 10000
set -g mouse on
set -g focus-events off
set -g extended-keys on
set -g extended-keys-format csi-u
set -g pane-border-status off
set -g remain-on-exit off
set -g update-environment \"SSH_AUTH_SOCK\"
set -g window-size latest
set -s exit-empty off
";

/// Emplacements de tmux essayés avant le `PATH` (spec §3.5).
const CANDIDATES: &[&str] = &["/opt/homebrew/bin/tmux", "/usr/local/bin/tmux"];

#[derive(Debug)]
pub enum ServerError {
    NotFound,
    Unsupported(String),
    Io(std::io::Error),
    Tmux {
        args: String,
        stderr: String,
    },
    /// Le dossier du projet a disparu : refusé avant tmux, qui ouvrirait le shell dans `$HOME` sans rien dire.
    ProjectGone(PathBuf),
    /// Une commande qui toucherait à l'état global d'un tmux distant : refusée avant tout processus.
    RemoteForbidden(String),
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "tmux introuvable"),
            Self::Unsupported(version) => write!(f, "tmux 3.8 requis, trouvé : {version}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Tmux { args, stderr } => write!(f, "tmux {args} : {stderr}"),
            Self::ProjectGone(path) => write!(f, "no such directory: {}", path.display()),
            Self::RemoteForbidden(reason) => write!(f, "remote tmux refused: {reason}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<std::io::Error> for ServerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Trouve tmux : `override_path` (option `tmux_path`), puis les emplacements connus, puis le `PATH`.
pub fn locate_tmux(
    override_path: Option<&Path>,
    path_var: Option<&str>,
) -> Result<PathBuf, ServerError> {
    if let Some(path) = override_path {
        return if path.is_file() {
            Ok(path.to_path_buf())
        } else {
            Err(ServerError::NotFound)
        };
    }
    let from_path = path_var.into_iter().flat_map(|value| {
        value
            .split(':')
            .map(|dir| Path::new(dir).join("tmux"))
            .collect::<Vec<_>>()
    });
    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .chain(from_path)
        .find(|candidate| candidate.is_file())
        .ok_or(ServerError::NotFound)
}

/// Une session d'Idema listée par `list-sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSession {
    pub name: String,
    pub project: String,
    pub created: u128,
    /// `@idema_layout` : l'identifiant du preset courant (spec §4.4), vide s'il manque.
    pub layout: String,
    /// `@idema_kind == "ideation"` : la session est un bouillon de neurones.
    pub ideation: bool,
    /// `@idema_remote_name` : le nom Remote Control du bouillon, vide s'il manque.
    pub remote_name: String,
    /// `@idema_name` : le nom affiché dans la barre.
    pub display_name: String,
}

/// Format de `list-sessions -F` lu par `parse_sessions`. Le nom affiché vient en dernier : il est libre.
pub const SESSIONS_FORMAT: &str = "#{session_name}\t#{@idema_project}\t#{@idema_created}\t#{@idema_layout}\t#{@idema_kind}\t#{@idema_remote_name}\t#{@idema_name}";

/// Format de `list-sessions -F` d'un serveur distant, lu par `parse_remote_sessions`. Le nom affiché vient en
/// dernier : il est libre.
pub const REMOTE_SESSIONS_FORMAT: &str = "#{session_name}\t#{@idema_project}\t#{@idema_created}\t#{session_created}\t#{@idema_layout}\t#{@idema_kind}\t#{@idema_remote_name}\t#{@idema_name}";

/// Lit la sortie de `list-sessions -F REMOTE_SESSIONS_FORMAT`. Aucune session n'est écartée : celles qui n'ont pas
/// été créées par Idema ont un `project` vide. `created` vient de `@idema_created`, sinon de `session_created`
/// (secondes) en millisecondes. Une session distante n'est jamais un bouillon de neurones.
pub fn parse_remote_sessions(listing: &str) -> Vec<ListedSession> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(8, '\t');
            let name = fields.next().filter(|name| !name.is_empty())?.to_string();
            let project = fields.next().unwrap_or_default().to_string();
            let idema_created: Option<u128> = fields.next().and_then(|value| value.parse().ok());
            let tmux_created: Option<u128> = fields.next().and_then(|value| value.parse().ok());
            let created = idema_created
                .or(tmux_created.map(|seconds| seconds * 1000))
                .unwrap_or(0);
            let layout = fields.next().unwrap_or_default().to_string();
            let _kind = fields.next();
            let remote_name = fields.next().unwrap_or_default().to_string();
            let display_name = match fields.next() {
                Some(value) if !value.is_empty() => value.to_string(),
                _ => name.clone(),
            };
            Some(ListedSession {
                name,
                project,
                created,
                layout,
                ideation: false,
                remote_name,
                display_name,
            })
        })
        .collect()
}

/// Lit la sortie de `list-sessions -F SESSIONS_FORMAT`.
/// Une session sans `@idema_project`, créée hors d'Idema, est écartée (spec §3.4).
pub fn parse_sessions(listing: &str) -> Vec<ListedSession> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(7, '\t');
            let name = fields.next()?.to_string();
            let project = fields.next()?.to_string();
            if project.is_empty() {
                return None;
            }
            let created = fields
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let layout = fields.next().unwrap_or_default().to_string();
            let ideation = fields.next() == Some("ideation");
            let remote_name = fields.next().unwrap_or_default().to_string();
            let display_name = fields.next().unwrap_or_default().to_string();
            Some(ListedSession {
                name,
                project,
                created,
                layout,
                ideation,
                remote_name,
                display_name,
            })
        })
        .collect()
}

/// Un pane listé par `list-panes -a`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedPane {
    pub session: String,
    pub pane: PaneId,
    /// Pane actif de la fenêtre active : celui qu'Idema montre.
    pub active: bool,
    /// `@idema_agent` : l'entrée du lanceur utilisée.
    pub agent: String,
    /// `@idnma_agent` : la clé d'agent du dernier `idnma run` lancé dans le pane (`glm`), vide sans idnma.
    pub idnma_agent: String,
}

/// Format de `list-panes -a -F` lu par `parse_panes`.
pub const PANES_FORMAT: &str =
    "#{session_name}\t#{pane_id}\t#{window_active}#{pane_active}\t#{@idema_agent}\t#{@idnma_agent}";

pub fn parse_panes(listing: &str) -> Vec<ListedPane> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(5, '\t');
            let session = fields.next()?.to_string();
            let pane = PaneId(fields.next()?.strip_prefix('%')?.parse().ok()?);
            let active = fields.next()? == "11";
            let agent = fields.next().unwrap_or_default().to_string();
            let idnma_agent = fields.next().unwrap_or_default().to_string();
            Some(ListedPane {
                session,
                pane,
                active,
                agent,
                idnma_agent,
            })
        })
        .collect()
}

/// Un pane d'une session listé par `list-panes -s`, avec sa fenêtre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPane {
    pub window: WindowId,
    /// Fenêtre courante de la session.
    pub window_active: bool,
    /// `@idema_stash` : la fenêtre cachée (spec §4.4).
    pub stash: bool,
    pub window_width: u16,
    pub window_height: u16,
    pub pane: PaneId,
    /// Pane actif de sa fenêtre.
    pub pane_active: bool,
    /// `@idema_hidden` : l'instant, en millisecondes, où Idema a caché le pane ; 0 s'il manque.
    pub hidden_at: u128,
}

/// Format de `list-panes -s -F` lu par `parse_session_panes`, dans l'ordre des fenêtres puis des panes.
pub const SESSION_PANES_FORMAT: &str = "#{window_id}\t#{window_active}\t#{@idema_stash}\t#{window_width}\t\
#{window_height}\t#{pane_id}\t#{pane_active}\t#{@idema_hidden}";

pub fn parse_session_panes(listing: &str) -> Vec<SessionPane> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let window = WindowId(fields.next()?.strip_prefix('@')?.parse().ok()?);
            let window_active = fields.next()? == "1";
            let stash = fields.next()? == "1";
            let window_width = fields.next()?.parse().ok()?;
            let window_height = fields.next()?.parse().ok()?;
            let pane = PaneId(fields.next()?.strip_prefix('%')?.parse().ok()?);
            let pane_active = fields.next()? == "1";
            let hidden_at = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            Some(SessionPane {
                window,
                window_active,
                stash,
                window_width,
                window_height,
                pane,
                pane_active,
                hidden_at,
            })
        })
        .collect()
}

/// Un pane du serveur lu pour l'instantané de restauration : sa session, sa fenêtre, ses options `@idema_*` et
/// `@idnma_agent`, le PID de son shell et son dossier courant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedPane {
    pub session: String,
    pub window: WindowId,
    /// `@idema_stash` : le pane est dans la fenêtre cachée.
    pub stash: bool,
    pub pane: PaneId,
    /// Pane actif de sa fenêtre.
    pub pane_active: bool,
    /// Fenêtre courante de sa session.
    pub window_active: bool,
    pub pane_pid: i32,
    /// `@idema_hidden` : l'instant, en millisecondes, où Idema a caché le pane ; 0 s'il manque.
    pub hidden_at: u128,
    /// `@idema_agent` : l'entrée du lanceur utilisée.
    pub agent: String,
    /// `@idnma_agent` : la clé d'agent du dernier `idnma run` lancé dans le pane, vide sans idnma.
    pub idnma_agent: String,
    /// `@idema_resume` : `<conversation> <commande de relance>` d'une session ramenée dans le pane, vide s'il manque.
    pub resume: String,
    /// `pane_current_path` : le dossier courant du pane.
    pub cwd: PathBuf,
}

/// Format de `list-panes -a -F` lu par `parse_capture_panes`. Le dossier vient en dernier : c'est un texte libre.
/// `@idema_resume` le précède : il porte des espaces et des apostrophes mais jamais de tabulation ni de caractère de
/// contrôle (`relaunch_command` les refuse), donc il ne déplace aucun champ.
pub const CAPTURE_FORMAT: &str = "#{session_name}\t#{window_id}\t#{window_active}\t#{@idema_stash}\t#{pane_id}\t\
#{pane_active}\t#{pane_pid}\t#{@idema_hidden}\t#{@idema_agent}\t#{@idnma_agent}\t#{@idema_resume}\t#{pane_current_path}";

/// Lit la sortie de `list-panes -a -F CAPTURE_FORMAT`. Une ligne tronquée est écartée.
pub fn parse_capture_panes(listing: &str) -> Vec<CapturedPane> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(12, '\t');
            let session = fields.next()?.to_string();
            let window = WindowId(fields.next()?.strip_prefix('@')?.parse().ok()?);
            let window_active = fields.next()? == "1";
            let stash = fields.next()? == "1";
            let pane = PaneId(fields.next()?.strip_prefix('%')?.parse().ok()?);
            let pane_active = fields.next()? == "1";
            let pane_pid = fields.next()?.parse().ok()?;
            let hidden_at = fields.next()?.parse().unwrap_or(0);
            let agent = fields.next()?.to_string();
            let idnma_agent = fields.next()?.to_string();
            let resume = fields.next()?.to_string();
            let cwd = PathBuf::from(fields.next()?);
            Some(CapturedPane {
                session,
                window,
                stash,
                pane,
                pane_active,
                window_active,
                pane_pid,
                hidden_at,
                agent,
                idnma_agent,
                resume,
                cwd,
            })
        })
        .collect()
}

/// Taille minimale d'une session restaurée : en dessous, les `split-window` d'un grand preset manquent de place.
const MIN_RESTORE_COLUMNS: u16 = 80;
const MIN_RESTORE_LINES: u16 = 24;

/// Nom de la fenêtre cachée créée par `break-pane` (elle est aussi marquée `@idema_stash`).
const STASH_NAME: &str = "idema-stash";

/// Ce qu'il faut pour passer une session à un preset.
#[derive(Debug, Clone, Copy)]
pub struct PresetChange<'a> {
    pub session: &'a str,
    /// Dossier du projet, où s'ouvrent les shells neufs.
    pub project: &'a str,
    pub preset: Preset,
    /// Les panes de la session, tels que `list-panes -s` les donne (`parse_session_panes`).
    pub panes: &'a [SessionPane],
    /// Le pane qui a le focus dans Idema, jamais caché ; le pane actif de tmux par défaut.
    pub focused: Option<PaneId>,
    /// Horodatage des panes cachés : `now_ms`, `now_ms + 1`… dans l'ordre où ils le sont.
    pub now_ms: u128,
}

/// Une suite de commandes tmux d'une seule invocation : `;` entre deux commandes.
fn command_line(commands: Vec<Vec<String>>) -> Vec<String> {
    let mut args = Vec::new();
    for (index, command) in commands.into_iter().enumerate() {
        if index > 0 {
            args.push(";".to_string());
        }
        args.extend(command);
    }
    args
}

/// Valeurs `@idema_*` d'une session neuve, communes à la création et à la restauration.
struct SessionOptions<'a> {
    project: &'a str,
    display_name: &'a str,
    layout: &'a str,
    created: u128,
    /// `@idema_agent` du premier pane.
    agent: &'a str,
    remote_name: Option<&'a str>,
}

/// `new-session` détaché qui imprime l'identifiant du premier pane. Le `-c` développe les formats de tmux : `#` se
/// double pour arriver tel quel (sonde du 15/10).
fn new_session_command(name: &str, cwd: &str, columns: u16, lines: u16) -> Vec<String> {
    words(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-s",
        name,
        "-c",
        &tmux_arg(&cwd.replace('#', "##")),
        "-x",
        &columns.to_string(),
        "-y",
        &lines.to_string(),
    ])
}

/// Les `set-option` d'une session neuve, à joindre à `new-session` dans la même invocation. `@idema_project` vient
/// en dernier : la barre reconnaît une session à cette option, qui ne la montre donc jamais à moitié construite. Les
/// valeurs passent après `--` : un nom qui commence par « - » ne doit pas devenir une option.
fn session_option_commands(target: &str, options: &SessionOptions) -> Vec<Vec<String>> {
    let session =
        |key: &str, value: &str| words(&["set-option", "-t", target, "--", key, &tmux_arg(value)]);
    let mut commands = vec![
        session("@idema_name", options.display_name),
        session("@idema_layout", options.layout),
        session("@idema_created", &options.created.to_string()),
        words(&[
            "set-option",
            "-p",
            "-t",
            target,
            "--",
            "@idema_agent",
            &tmux_arg(options.agent),
        ]),
    ];
    // Un bouillon porte deux options de plus.
    if let Some(remote_name) = options.remote_name {
        commands.push(session("@idema_kind", "ideation"));
        commands.push(session("@idema_remote_name", remote_name));
    }
    commands.push(session("@idema_project", options.project));
    commands
}

/// L'identifiant `%<n>` que `new-session -P` ou `split-window -P` imprime sur sa première ligne.
fn created_pane_id(output: &str, command: &str) -> Result<PaneId, ServerError> {
    output
        .lines()
        .next()
        .and_then(|id| id.strip_prefix('%'))
        .and_then(|number| number.parse().ok())
        .map(PaneId)
        .ok_or_else(|| ServerError::Tmux {
            args: command.into(),
            stderr: format!("no pane id in {output:?}"),
        })
}

fn words(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

/// Arguments de l'invocation de tmux qui applique le preset (spec §4.4) : cacher les panes en trop dans la
/// fenêtre cachée (créée par le premier `break-pane`), rejoindre les cachés, créer des shells neufs, puis la
/// chaîne du preset et `@idema_layout`. Un `select-layout tiled` après chaque déplacement garde de la place pour le
/// suivant. Tout reste détaché (`-d`) : le focus ne bouge pas. tmux imprime l'identifiant des shells créés (`-P`).
/// `None` sans fenêtre visible, ou si elle est trop petite pour le preset.
pub fn preset_arguments(change: &PresetChange) -> Option<Vec<String>> {
    let visible = change
        .panes
        .iter()
        .find(|pane| pane.window_active && !pane.stash)?;
    let window = format!("@{}", visible.window.0);
    let visible_panes: Vec<PaneId> = change
        .panes
        .iter()
        .filter(|pane| pane.window == visible.window)
        .map(|pane| pane.pane)
        .collect();
    let hidden: Vec<(PaneId, u128)> = change
        .panes
        .iter()
        .filter(|pane| pane.stash)
        .map(|pane| (pane.pane, pane.hidden_at))
        .collect();
    let focused = change.focused.or_else(|| {
        change
            .panes
            .iter()
            .find(|pane| pane.window == visible.window && pane.pane_active)
            .map(|pane| pane.pane)
    });
    let layout = preset_layout(change.preset, visible.window_width, visible.window_height)?;
    let plan = plan_preset(&visible_panes, focused, &hidden, change.preset.panes());
    let pane = |id: PaneId| format!("%{}", id.0);
    let mut commands: Vec<Vec<String>> = Vec::new();

    let mut stash = change
        .panes
        .iter()
        .find(|pane| pane.stash)
        .map(|pane| format!("@{}", pane.window.0));
    for (offset, hidden) in plan.hide.iter().enumerate() {
        let stamp = (change.now_ms + offset as u128).to_string();
        let source = pane(*hidden);
        match &stash {
            Some(target) => {
                commands.push(words(&["join-pane", "-d", "-s", &source, "-t", target]));
                commands.push(words(&["select-layout", "-t", target, "tiled"]));
            }
            None => {
                commands.push(words(&[
                    "break-pane",
                    "-d",
                    "-s",
                    &source,
                    // Sans `-t`, hors client, tmux crée la fenêtre dans la session la plus récemment active.
                    "-t",
                    &exact(change.session),
                    "-n",
                    STASH_NAME,
                ]));
                // Une option de fenêtre visée par un pane s'applique à la fenêtre de ce pane : la cachée, neuve.
                commands.push(words(&[
                    "set-option",
                    "-w",
                    "-t",
                    &source,
                    "@idema_stash",
                    "1",
                ]));
                stash = Some(source.clone());
            }
        }
        commands.push(words(&[
            "set-option",
            "-p",
            "-t",
            &source,
            "@idema_hidden",
            &stamp,
        ]));
    }

    let mut last = visible_panes
        .iter()
        .rev()
        .find(|candidate| !plan.hide.contains(candidate))
        .copied()?;
    for rejoined in &plan.rejoin {
        let source = pane(*rejoined);
        // Joint après le dernier pane visible : l'ordre du layout reste celui d'avant la réduction.
        commands.push(words(&[
            "join-pane",
            "-d",
            "-s",
            &source,
            "-t",
            &pane(last),
        ]));
        commands.push(words(&[
            "set-option",
            "-p",
            "-u",
            "-t",
            &source,
            "@idema_hidden",
        ]));
        commands.push(words(&["select-layout", "-t", &window, "tiled"]));
        last = *rejoined;
    }
    let directory = tmux_arg(&change.project.replace('#', "##"));
    for _ in 0..plan.create {
        commands.push(words(&[
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &pane(last),
            "-c",
            &directory,
        ]));
        commands.push(words(&["select-layout", "-t", &window, "tiled"]));
    }
    commands.push(words(&["select-layout", "-t", &window, &layout]));
    commands.push(words(&[
        "set-option",
        "-t",
        &exact(change.session),
        "--",
        "@idema_layout",
        change.preset.id(),
    ]));
    Some(command_line(commands))
}

/// Arguments qui sortent un pane de la fenêtre cachée quand la visible a disparu et que tmux montre la cachée
/// (spec §4.4, « s'il ne reste aucun pane visible mais des panes cachés, il en rejoint un ») : le plus récemment
/// caché devient la fenêtre visible, seul. `None` s'il y a une fenêtre visible, ou rien de caché.
pub fn restore_arguments(session: &str, panes: &[SessionPane]) -> Option<Vec<String>> {
    if panes.iter().any(|pane| pane.window_active && !pane.stash) {
        return None;
    }
    let stash: Vec<&SessionPane> = panes
        .iter()
        .filter(|pane| pane.stash && pane.window_active)
        .collect();
    let latest = stash.iter().max_by_key(|pane| pane.hidden_at)?;
    let source = format!("%{}", latest.pane.0);
    let mut commands = Vec::new();
    if stash.len() > 1 {
        // Sans `-d`, la fenêtre neuve devient la courante : la cachée garde les autres.
        // `-t` vise la session : hors client, tmux choisirait sinon la plus récemment active.
        commands.push(words(&["break-pane", "-s", &source, "-t", &exact(session)]));
    } else {
        commands.push(words(&[
            "set-option",
            "-w",
            "-u",
            "-t",
            &source,
            "@idema_stash",
        ]));
    }
    commands.push(words(&[
        "set-option",
        "-p",
        "-u",
        "-t",
        &source,
        "@idema_hidden",
    ]));
    commands.push(words(&[
        "set-option",
        "-t",
        &exact(session),
        "--",
        "@idema_layout",
        Preset::Single.id(),
    ]));
    Some(command_line(commands))
}

/// Ce qu'il faut pour créer une session (spec §4.2, étapes 1 et 2).
#[derive(Debug, Clone, Copy)]
pub struct NewSession<'a> {
    pub project: &'a Path,
    pub display_name: &'a str,
    /// Nom de l'entrée du lanceur, écrit dans `@idema_agent`.
    pub agent: &'a str,
    pub columns: u16,
    pub lines: u16,
    /// `Some(nom)` fait de la session un bouillon de neurones : `@idema_kind` = `ideation` et
    /// `@idema_remote_name` = le nom Remote Control.
    pub remote_name: Option<&'a str>,
}

/// Une session créée : son nom tmux, son premier pane et son horodatage de création.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    pub name: String,
    pub pane: PaneId,
    pub created: u128,
}

/// Un pane à recréer par `restore_session`.
#[derive(Debug, Clone, Copy)]
pub struct RestorePane<'a> {
    /// Dossier où s'ouvre le shell. Il doit exister : tmux ouvre sinon le shell dans `$HOME` sans rien dire, et le
    /// repli sur le projet revient à l'appelant.
    pub cwd: &'a Path,
    /// Nom de l'entrée du lanceur, écrit dans `@idema_agent` du pane.
    pub agent: &'a str,
    /// `Some(instant)` : le pane est caché, et `@idema_hidden` reprend cet instant sauvé.
    pub hidden_at: Option<u128>,
    /// Le pane actif de la fenêtre visible (sans effet sur un pane caché).
    pub active: bool,
    /// `Some(valeur)` : écrite dans `@idema_resume` du pane (`<conversation> <commande de relance>`), pour qu'un second
    /// redémarrage rejoue la même commande.
    pub resume: Option<&'a str>,
}

/// Ce qu'il faut pour recréer une session à l'identique (spec restauration §3.4) : même nom tmux, mêmes options
/// `@idema_*`, mêmes panes dans le même ordre, même preset.
#[derive(Debug, Clone, Copy)]
pub struct RestoreSession<'a> {
    /// Nom tmux imposé (`idema-<8 hex>`).
    pub name: &'a str,
    pub project: &'a Path,
    pub display_name: &'a str,
    /// `@idema_created` : l'instant de création d'origine, qui fixe l'ordre dans la barre.
    pub created: u128,
    /// `@idema_layout` : l'identifiant du preset.
    pub layout: &'a str,
    /// `Some(nom)` fait de la session un bouillon de neurones, comme pour `NewSession`.
    pub remote_name: Option<&'a str>,
    pub columns: u16,
    pub lines: u16,
    /// Les panes dans l'ordre d'origine, cachés compris ; il en faut au moins un de visible.
    pub panes: &'a [RestorePane<'a>],
}

/// Ce qu'il faut pour lancer un serveur de dev dans une session cachée (spec P4 §5.3).
#[derive(Debug, Clone, Copy)]
pub struct NewServerSession<'a> {
    /// Dossier du projet, écrit dans `@idema_server_project`.
    pub project: &'a Path,
    /// Nom de l'entrée de configuration (`launch.json`, `package.json`), écrit dans `@idema_server_entry`.
    pub entry: &'a str,
    /// Dossier de lancement de la commande.
    pub cwd: &'a Path,
    pub program: &'a str,
    pub args: &'a [String],
    /// Variables ajoutées à l'environnement du pane (`PORT` compris, passé par l'appelant).
    pub env: &'a [(String, String)],
}

/// Une session serveur listée : jamais une session de la barre (elle n'a pas `@idema_project`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedServerSession {
    pub name: String,
    pub project: PathBuf,
    pub entry: String,
    pub pane: PaneId,
    /// PID du processus du pane ; 0 quand le pane est mort (tmux ne le donne plus).
    pub pane_pid: i32,
    /// La commande s'est terminée : le pane reste, avec sa sortie (`remain-on-exit`).
    pub dead: bool,
}

/// Format de `list-panes -a -F` lu par `parse_server_sessions`. L'entrée vient en dernier : elle est libre.
pub const SERVER_SESSIONS_FORMAT: &str = "#{session_name}\t#{pane_id}\t#{pane_pid}\t#{pane_dead}\t#{@idema_server_project}\t#{@idema_server_entry}";

/// Lit la sortie de `list-panes -a -F SERVER_SESSIONS_FORMAT`. Un pane d'une session sans
/// `@idema_server_project` (session de la barre, session étrangère) est écarté.
pub fn parse_server_sessions(listing: &str) -> Vec<ListedServerSession> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(6, '\t');
            let name = fields.next()?.to_string();
            let pane = PaneId(fields.next()?.strip_prefix('%')?.parse().ok()?);
            let raw_pid = fields.next()?;
            let dead = fields.next()? == "1";
            // Un pane mort n'a plus de PID (champ vide) ; un PID illisible sur un pane vivant écarte la ligne.
            let pane_pid = if dead && raw_pid.is_empty() {
                0
            } else {
                raw_pid.parse().ok()?
            };
            let project = fields.next()?;
            if project.is_empty() {
                return None;
            }
            let entry = fields.next().unwrap_or_default().to_string();
            Some(ListedServerSession {
                name,
                project: PathBuf::from(project),
                entry,
                pane,
                pane_pid,
                dead,
            })
        })
        .collect()
}

/// Format de `list-panes -a -F` lu par `parse_pane_pids`.
pub const PANE_PIDS_FORMAT: &str = "#{session_name}\t#{pane_id}\t#{pane_pid}";

/// Lit la sortie de `list-panes -a -F PANE_PIDS_FORMAT`. Un pane mort n'a plus de PID : il est écarté.
pub fn parse_pane_pids(listing: &str) -> Vec<(String, PaneId, i32)> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let name = fields.next()?.to_string();
            let pane = PaneId(fields.next()?.strip_prefix('%')?.parse().ok()?);
            let pid = fields.next()?.parse().ok()?;
            Some((name, pane, pid))
        })
        .collect()
}

/// Arguments de l'invocation de tmux qui crée la session serveur `name` : `new-session`, puis les deux options
/// de session et `remain-on-exit` sur ce seul pane, le tout dans une seule invocation. tmux traite la suite de
/// commandes sans repasser par sa boucle d'événements : une commande qui se termine aussitôt ne détruit pas la
/// session avant que `remain-on-exit` soit posé (sonde du 20/10, vingt essais de `echo boom; exit 3`).
///
/// La commande est la fin de la ligne de `new-session` : avec plusieurs arguments, tmux 3.8 les passe à `exec`
/// tels quels, sans les recoller dans une chaîne de shell. Le shell de l'utilisateur (`shell`), interactif et de
/// connexion comme celui des sessions de la barre, la lance par `exec "$0" "$@"` : il lit ses fichiers de connexion
/// et le `.zshrc` (donc le `PATH` de l'utilisateur : bun, node), puis cède la place au programme, dont les
/// arguments restent des arguments séparés. Sans `-i`, le `.zshrc` serait sauté (sonde du 20/10 : ni bun ni le
/// même node que les sessions).
pub fn server_session_arguments(
    name: &str,
    shell: &str,
    request: &NewServerSession,
) -> Vec<String> {
    let target = exact(name);
    let project = request.project.to_string_lossy();
    let cwd = request.cwd.to_string_lossy();
    let mut args = words(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-s",
        name,
        "-c",
        // `-c` développe les formats de tmux : `#` se double pour arriver tel quel. Ni `-e` ni la commande
        // ne développent quoi que ce soit (sonde du 20/10).
        &tmux_arg(&cwd.replace('#', "##")),
    ]);
    for (key, value) in request.env {
        args.push("-e".into());
        args.push(tmux_arg(&format!("{key}={value}")));
    }
    // Ce que tmux prend pour la fin d'un argument (« ; » final) est échappé, chaque valeur reste un argument.
    args.extend(words(&["--", shell, "-l", "-i", "-c", r#"exec "$0" "$@""#]));
    args.push(tmux_arg(request.program));
    args.extend(request.args.iter().map(|arg| tmux_arg(arg)));
    let commands = vec![
        words(&[
            "set-option",
            "-t",
            &target,
            "--",
            "@idema_server_project",
            &tmux_arg(&project),
        ]),
        words(&[
            "set-option",
            "-t",
            &target,
            "--",
            "@idema_server_entry",
            &tmux_arg(request.entry),
        ]),
        words(&["set-option", "-p", "-t", &target, "remain-on-exit", "on"]),
        // Sans la ligne « Pane is dead (status N, date) » que tmux écrit en anglais sous la sortie d'un pane mort : le journal d'un
        // démarrage raté montre la sortie du serveur seule (sonde du 20/10, tmux 3.8).
        words(&[
            "set-option",
            "-p",
            "-t",
            &target,
            "remain-on-exit-format",
            "",
        ]),
    ];
    for command in commands {
        args.push(";".into());
        args.extend(command);
    }
    args
}

/// Cible exacte d'une session : sans `=`, tmux accepterait un préfixe du nom. Le `:` final la rend valable
/// aussi là où tmux attend un pane ou une fenêtre (`set-option`, `list-panes`, `attach-session`) : sondes du 15/10.
pub fn exact(session: &str) -> String {
    format!("={session}:")
}

/// tmux sans serveur répond « no server running on … » ou « error connecting to … » (sonde du 15/10) :
/// pour une liste, c'est une liste vide, même si le serveur s'arrête entre deux commandes.
fn is_no_server(error: &ServerError) -> bool {
    matches!(error, ServerError::Tmux { stderr, .. }
        if stderr.starts_with("no server running") || stderr.starts_with("error connecting to"))
}

/// Comme `is_no_server`, plus « no current target » : `list-panes -a` sur un serveur vivant sans aucune session
/// (`exit-empty off`) répond ainsi (sonde du 28/10). Pour une liste de panes, c'est aussi une liste vide.
fn is_no_server_or_no_pane(error: &ServerError) -> bool {
    is_no_server(error)
        || matches!(error, ServerError::Tmux { stderr, .. } if stderr.starts_with("no current target"))
}

/// Le serveur tmux d'Idema : `tmux -L <socket> -f <config>` (`-S` pour un chemin absolu), lancé avec l'environnement filtré.
pub struct TmuxServer {
    transport: Transport,
    env: Vec<(String, String)>,
}

/// Où tourne tmux : en local sur le serveur dédié d'Idema, ou sur une machine distante par ssh.
enum Transport {
    Local {
        binary: PathBuf,
        socket: String,
        config: PathBuf,
    },
    Remote(RemoteSpec),
}

impl TmuxServer {
    pub fn new(
        binary: PathBuf,
        socket: impl Into<String>,
        config: PathBuf,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            transport: Transport::Local {
                binary,
                socket: socket.into(),
                config,
            },
            env,
        }
    }

    /// Un serveur tmux distant : le tmux du `PATH` de la machine, serveur par défaut, lancé par ssh. Son
    /// environnement est celui du shell distant.
    pub fn remote(spec: RemoteSpec) -> Self {
        let env = vec![
            ("HOME".to_string(), spec.home.clone()),
            ("SHELL".to_string(), spec.shell.clone()),
        ];
        Self {
            transport: Transport::Remote(spec),
            env,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.transport, Transport::Remote(_))
    }

    pub fn remote_spec(&self) -> Option<&RemoteSpec> {
        match &self.transport {
            Transport::Remote(spec) => Some(spec),
            Transport::Local { .. } => None,
        }
    }

    /// L'environnement de la liste blanche, intégration zsh comprise : celui des panes, que le volet Terminal
    /// reprend (spec §3.1).
    pub fn environment(&self) -> &[(String, String)] {
        &self.env
    }

    /// Commande `tmux -L <socket> -f <config> <args…>`, environnement remplacé par la liste blanche. Sur un
    /// serveur distant : `ssh … -- <destination> "tmux -u '<arg>'…"`, le tmux du `PATH` de la machine, sans `-L`
    /// ni `-f`, environnement remplacé par celui de ssh.
    pub fn command(&self, args: &[&str]) -> Command {
        let (binary, socket, config) = match &self.transport {
            Transport::Local {
                binary,
                socket,
                config,
            } => (binary, socket, config),
            Transport::Remote(spec) => return remote_command(spec, &remote_tmux_line(args), false),
        };
        let mut command = Command::new(binary);
        // Un chemin absolu désigne le fichier socket lui-même (`-S`) : les tests le posent dans leur dossier jetable,
        // que le nettoyage retire. Un nom court passe par `-L` et laisse son fichier dans le dossier de tmux.
        let flag = if Path::new(socket).is_absolute() {
            "-S"
        } else {
            "-L"
        };
        command
            .arg(flag)
            .arg(socket)
            .arg("-f")
            .arg(config)
            .args(args)
            .env_clear()
            .envs(
                self.env
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            );
        reset_signal_state(&mut command);
        command
    }

    /// Sur un serveur distant, refuse avant tout processus une commande qui toucherait à l'état global de tmux.
    fn guard_remote(&self, args: &[&str]) -> Result<(), ServerError> {
        if self.is_remote()
            && let Some(reason) = forbidden_on_remote(args)
        {
            return Err(ServerError::RemoteForbidden(reason));
        }
        Ok(())
    }

    /// Lance une commande ponctuelle et renvoie sa sortie, sans le saut de ligne final. Refusée d'abord si elle
    /// toucherait à l'état global d'un tmux distant.
    pub fn run(&self, args: &[&str]) -> Result<String, ServerError> {
        self.guard_remote(args)?;
        let output = self.command(args).stdin(Stdio::null()).output()?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string())
        } else {
            Err(ServerError::Tmux {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }

    /// Vérifie que tmux est en 3.8 au moins (le tmux du `PATH` de la machine, pour un serveur distant). Un
    /// processus qui échoue (ssh injoignable, tmux absent) est une erreur `Tmux`, pas une version non supportée.
    pub fn check_version(&self) -> Result<(u32, u32), ServerError> {
        let output = match &self.transport {
            Transport::Local { binary, .. } => {
                Command::new(binary).arg("-V").env_clear().output()?
            }
            Transport::Remote(spec) => remote_command(spec, "tmux -V", false).output()?,
        };
        if !output.status.success() {
            return Err(ServerError::Tmux {
                args: "-V".into(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        match parse_version(&text) {
            Some(version) if is_supported(version) => Ok(version),
            _ => Err(ServerError::Unsupported(text)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.run(&["list-sessions"]).is_ok()
    }

    /// Écrit la configuration, et la recharge si le serveur tourne déjà. Refusé sur un serveur distant : IdemA n'écrit
    /// jamais la configuration de la machine.
    pub fn write_config(&self) -> Result<(), ServerError> {
        let Transport::Local { config, .. } = &self.transport else {
            return Err(ServerError::RemoteForbidden("write-config".into()));
        };
        if let Some(dir) = config.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(config, TMUX_CONF)?;
        if self.is_running() {
            self.run(&["source-file", &config.to_string_lossy()])?;
        }
        Ok(())
    }

    /// Pose une variable dans l'environnement global d'un serveur déjà lancé : les panes créés ensuite la
    /// reçoivent. Un serveur lancé plus tard la reçoit par l'environnement de la commande.
    pub fn set_global_environment(&self, key: &str, value: &str) -> Result<(), ServerError> {
        match self.run(&["set-environment", "-g", key, &tmux_arg(value)]) {
            Err(error) if !is_no_server(&error) => Err(error),
            _ => Ok(()),
        }
    }

    /// Crée une session et pose ses options en une seule invocation de tmux : la session n'existe jamais
    /// sans `@idema_project`, que la barre lit pour la reconnaître.
    pub fn create_session(&self, request: &NewSession) -> Result<CreatedSession, ServerError> {
        // tmux 3.8 ouvre le shell dans `$HOME` quand `-c` pointe un dossier absent (sonde du 16/10) : on refuse avant.
        // Le dossier d'un projet distant n'existe pas sur ce Mac : la vérification ne vaut qu'en local.
        if !self.is_remote() && !request.project.is_dir() {
            return Err(ServerError::ProjectGone(request.project.to_path_buf()));
        }
        let project = request.project.to_string_lossy().to_string();
        let name = format!("idema-{:08x}", random_u32());
        let created = now_millis();
        let mut commands = vec![new_session_command(
            &name,
            &project,
            request.columns,
            request.lines,
        )];
        commands.extend(session_option_commands(
            &exact(&name),
            &SessionOptions {
                project: &project,
                display_name: request.display_name,
                layout: "single",
                created,
                agent: request.agent,
                remote_name: request.remote_name,
            },
        ));
        let args = command_line(commands);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.run(&args)?;
        let pane = created_pane_id(&output, "new-session")?;
        Ok(CreatedSession {
            name,
            pane,
            created,
        })
    }

    /// Recrée une session à l'identique (spec restauration §3.4) : `request.name`, mêmes options `@idema_*` que
    /// `create_session`, les panes dans l'ordre de `request.panes`, les cachés dans la fenêtre cachée avec leur
    /// `@idema_hidden` sauvé, le preset, le pane actif. Rend les identifiants des panes dans l'ordre de `request.panes`.
    /// Plusieurs invocations de tmux : la première crée la session (jamais visible à moitié construite), la suite en
    /// ajoute les panes. Une erreur après la création tue la session avant d'être rendue.
    pub fn restore_session(&self, request: &RestoreSession) -> Result<Vec<PaneId>, ServerError> {
        // Même garde que `create_session` : tmux ouvrirait sinon le shell dans `$HOME`.
        if !request.project.is_dir() {
            return Err(ServerError::ProjectGone(request.project.to_path_buf()));
        }
        let refused = |reason: &str| ServerError::Tmux {
            args: "restore-session".into(),
            stderr: reason.into(),
        };
        if request.panes.is_empty() {
            return Err(refused("no panes to restore"));
        }
        // Le premier pane visible porte la session : sa fenêtre est celle que l'utilisateur voit.
        let first = request
            .panes
            .iter()
            .position(|pane| pane.hidden_at.is_none())
            .ok_or_else(|| refused("no visible pane to restore"))?;
        let project = request.project.to_string_lossy().to_string();
        // Sous 80x24, le troisième `split-window` d'un preset à six panes échoue (sonde du 28/10) ;
        // `window-size latest` rend la taille du client à l'attache.
        let columns = request.columns.max(MIN_RESTORE_COLUMNS);
        let lines = request.lines.max(MIN_RESTORE_LINES);
        let mut commands = vec![new_session_command(
            request.name,
            &request.panes[first].cwd.to_string_lossy(),
            columns,
            lines,
        )];
        commands.extend(session_option_commands(
            &exact(request.name),
            &SessionOptions {
                project: &project,
                display_name: request.display_name,
                layout: request.layout,
                created: request.created,
                agent: request.panes[first].agent,
                remote_name: request.remote_name,
            },
        ));
        let args = command_line(commands);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        // Un échec ici n'a rien créé : surtout ne pas tuer une session qui porterait déjà ce nom.
        let first_pane = created_pane_id(&self.run(&args)?, "new-session")?;
        self.rebuild_panes(request, first, first_pane, (columns, lines))
            .inspect_err(|_| {
                let _ = self.kill_session(request.name);
            })
    }

    /// La suite de `restore_session`, une fois la session et son premier pane créés : les autres panes, la fenêtre
    /// cachée, le preset, le pane actif.
    fn rebuild_panes(
        &self,
        request: &RestoreSession,
        first: usize,
        first_pane: PaneId,
        (columns, lines): (u16, u16),
    ) -> Result<Vec<PaneId>, ServerError> {
        let pane = |id: PaneId| format!("%{}", id.0);
        // Chaque pane suivant se coupe depuis le précédent, puis `tiled` lui garde de la place (comme
        // `preset_arguments`). Les emplacements se remplissent dans l'ordre de création ; `first` est déjà juste.
        let mut ids = vec![first_pane; request.panes.len()];
        let mut last = first_pane;
        for (index, restored) in request.panes.iter().enumerate() {
            if index == first {
                continue;
            }
            let cwd = restored.cwd.to_string_lossy();
            let args = command_line(vec![
                words(&[
                    "split-window",
                    "-d",
                    "-P",
                    "-F",
                    "#{pane_id}",
                    "-t",
                    &pane(last),
                    "-c",
                    &tmux_arg(&cwd.replace('#', "##")),
                ]),
                words(&["select-layout", "-t", &pane(last), "tiled"]),
            ]);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            last = created_pane_id(&self.run(&args)?, "split-window")?;
            ids[index] = last;
        }

        let mut commands: Vec<Vec<String>> = Vec::new();
        for (index, restored) in request.panes.iter().enumerate() {
            if index != first {
                commands.push(words(&[
                    "set-option",
                    "-p",
                    "-t",
                    &pane(ids[index]),
                    "--",
                    "@idema_agent",
                    &tmux_arg(restored.agent),
                ]));
            }
        }
        // `@idema_resume` suit le pane, premier compris (la session ne porte que son `@idema_agent`) : posée ici, avant
        // que les cachés ne changent de fenêtre, elle les accompagne comme `@idema_agent`.
        for (index, restored) in request.panes.iter().enumerate() {
            if let Some(resume) = restored.resume {
                commands.push(words(&[
                    "set-option",
                    "-p",
                    "-t",
                    &pane(ids[index]),
                    "--",
                    "@idema_resume",
                    &tmux_arg(resume),
                ]));
            }
        }
        // Les cachés rejoignent la fenêtre cachée AVANT le preset : sa chaîne ne décrit que les panes visibles
        // (sonde du 28/10 : avec un pane de trop, `select-layout` échoue). Le premier caché crée la fenêtre
        // (`break-pane`), les suivants la rejoignent après le précédent : l'ordre d'origine est gardé.
        let mut stash: Option<PaneId> = None;
        for (index, restored) in request.panes.iter().enumerate() {
            let Some(hidden_at) = restored.hidden_at else {
                continue;
            };
            let source = pane(ids[index]);
            match stash {
                Some(previous) => {
                    commands.push(words(&[
                        "join-pane",
                        "-d",
                        "-s",
                        &source,
                        "-t",
                        &pane(previous),
                    ]));
                    commands.push(words(&["select-layout", "-t", &source, "tiled"]));
                }
                None => {
                    commands.push(words(&[
                        "break-pane",
                        "-d",
                        "-s",
                        &source,
                        "-t",
                        &exact(request.name),
                        "-n",
                        STASH_NAME,
                    ]));
                    // Une option de fenêtre visée par un pane s'applique à la fenêtre de ce pane : la cachée, neuve.
                    commands.push(words(&[
                        "set-option",
                        "-w",
                        "-t",
                        &source,
                        "@idema_stash",
                        "1",
                    ]));
                }
            }
            stash = Some(ids[index]);
            commands.push(words(&[
                "set-option",
                "-p",
                "-t",
                &source,
                "@idema_hidden",
                &hidden_at.to_string(),
            ]));
        }
        // Le preset s'applique si ses cellules comptent autant que les panes visibles ; sinon `tiled` (une chaîne
        // de trop ou de moins de cellules ferait échouer `select-layout`).
        let visible = request
            .panes
            .iter()
            .filter(|restored| restored.hidden_at.is_none())
            .count();
        let layout = Preset::from_id(request.layout)
            .filter(|preset| preset.panes() == visible)
            .and_then(|preset| preset_layout(preset, columns, lines))
            .unwrap_or_else(|| "tiled".to_string());
        commands.push(words(&["select-layout", "-t", &pane(first_pane), &layout]));
        if let Some(index) = request
            .panes
            .iter()
            .position(|restored| restored.active && restored.hidden_at.is_none())
        {
            commands.push(words(&["select-pane", "-t", &pane(ids[index])]));
        }
        let args = command_line(commands);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&args)?;
        Ok(ids)
    }

    /// Lance un serveur de dev dans une session cachée (spec P4 §5.3) : `idema-srv-<8 hex>`, sans `@idema_project`
    /// (la barre ne la voit donc pas), avec `@idema_server_project` et `@idema_server_entry`. Le pane garde sa
    /// sortie quand la commande se termine. Rien ne la tue à la sortie d'Idema : elle vit dans le serveur tmux.
    pub fn create_server_session(
        &self,
        request: &NewServerSession,
    ) -> Result<CreatedSession, ServerError> {
        // Même garde que `create_session` : tmux ouvrirait sinon le shell dans `$HOME`.
        if !request.cwd.is_dir() {
            return Err(ServerError::ProjectGone(request.cwd.to_path_buf()));
        }
        let name = format!("idema-srv-{:08x}", random_u32());
        let created = now_millis();
        let shell = self
            .env
            .iter()
            .find(|(key, _)| key == "SHELL")
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("/bin/zsh");
        let args = server_session_arguments(&name, shell, request);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        // L'erreur de tmux ne répète pas la ligne de commande : elle porte chaque `-e CLÉ=VALEUR` de l'entrée (des jetons, des
        // mots de passe de base de données), et le message va au journal de debug et sur la ligne du serveur.
        let output = self.run(&args).map_err(without_argv)?;
        let pane = output
            .lines()
            .next()
            .and_then(|id| id.strip_prefix('%'))
            .and_then(|number| number.parse().ok())
            .map(PaneId)
            .ok_or_else(|| ServerError::Tmux {
                args: "new-session".into(),
                stderr: format!("no pane id in {output:?}"),
            })?;
        Ok(CreatedSession {
            name,
            pane,
            created,
        })
    }

    /// Sessions serveur du tmux d'Idema, mortes comprises (`dead`). Sans serveur, la liste est vide.
    /// Pour lire le journal d'une session, capturer l'historique (`capture-pane -S - -J`) : tmux ajoute « Pane is
    /// dead » et fait remonter l'écran, une sortie courte quitte alors l'écran visible.
    pub fn server_sessions(&self) -> Result<Vec<ListedServerSession>, ServerError> {
        match self.run(&["list-panes", "-a", "-F", SERVER_SESSIONS_FORMAT]) {
            Ok(listing) => Ok(parse_server_sessions(&listing)),
            Err(error) if is_no_server_or_no_pane(&error) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// Nom de session, pane et PID de chaque pane vivant du serveur, sessions serveur comprises. Sans serveur, la
    /// liste est vide.
    pub fn pane_pids(&self) -> Result<Vec<(String, PaneId, i32)>, ServerError> {
        match self.run(&["list-panes", "-a", "-F", PANE_PIDS_FORMAT]) {
            Ok(listing) => Ok(parse_pane_pids(&listing)),
            Err(error) if is_no_server_or_no_pane(&error) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// Sessions d'Idema du serveur. Sans serveur, la liste est vide.
    pub fn sessions(&self) -> Result<Vec<ListedSession>, ServerError> {
        match self.run(&["list-sessions", "-F", SESSIONS_FORMAT]) {
            Ok(listing) => Ok(parse_sessions(&listing)),
            Err(error) if is_no_server(&error) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// Sessions et panes du serveur, lus pour l'instantané de restauration. Contrairement à `sessions()`, un serveur
    /// absent est une erreur (un serveur vivant mais vide est `Ok` vide) : l'appelant ne doit pas prendre « injoignable » pour « vide » (spec §3.3).
    pub fn capture(&self) -> Result<(Vec<ListedSession>, Vec<CapturedPane>), ServerError> {
        let listing = self.run(&["list-sessions", "-F", SESSIONS_FORMAT])?;
        // Un serveur vivant sans aucune session (`exit-empty off`) répond « no current target » à `list-panes -a`
        // (sonde du 28/10) : c'est un serveur vide, pas une erreur.
        if listing.trim().is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let panes = parse_capture_panes(&self.run(&["list-panes", "-a", "-F", CAPTURE_FORMAT])?);
        Ok((parse_sessions(&listing), panes))
    }

    /// Tous les panes du serveur. Sans serveur, la liste est vide.
    pub fn panes(&self) -> Result<Vec<ListedPane>, ServerError> {
        match self.run(&["list-panes", "-a", "-F", PANES_FORMAT]) {
            Ok(listing) => Ok(parse_panes(&listing)),
            Err(error) if is_no_server_or_no_pane(&error) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    pub fn has_session(&self, session: &str) -> bool {
        self.run(&["has-session", "-t", &exact(session)]).is_ok()
    }

    /// Comme `has_session`, mais une erreur autre que « session absente » (ssh injoignable, refus) n'est pas
    /// prise pour une absence : l'appelant ne doit pas croire qu'une session a disparu parce que le réseau a coupé.
    pub fn has_session_checked(&self, session: &str) -> Result<bool, ServerError> {
        match self.run(&["has-session", "-t", &exact(session)]) {
            Ok(_) => Ok(true),
            Err(error) if is_no_server(&error) => Ok(false),
            Err(ServerError::Tmux { stderr, .. }) if stderr.contains("can't find session") => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Sessions d'un serveur distant, étrangères comprises. Sans serveur, la liste est vide.
    pub fn remote_sessions(&self) -> Result<Vec<ListedSession>, ServerError> {
        match self.run(&["list-sessions", "-F", REMOTE_SESSIONS_FORMAT]) {
            Ok(listing) => Ok(parse_remote_sessions(&listing)),
            Err(error) if is_no_server(&error) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    pub fn kill_session(&self, session: &str) -> Result<(), ServerError> {
        self.run(&["kill-session", "-t", &exact(session)])
            .map(|_| ())
    }

    /// Option utilisateur d'une session (`@idema_name`…).
    pub fn set_session_option(
        &self,
        session: &str,
        key: &str,
        value: &str,
    ) -> Result<(), ServerError> {
        self.run(&[
            "set-option",
            "-t",
            &exact(session),
            "--",
            key,
            &tmux_arg(value),
        ])
        .map(|_| ())
    }

    /// Option utilisateur d'un pane (`@idema_agent`).
    pub fn set_pane_option(&self, pane: PaneId, key: &str, value: &str) -> Result<(), ServerError> {
        let target = format!("%{}", pane.0);
        self.run(&[
            "set-option",
            "-p",
            "-t",
            &target,
            "--",
            key,
            &tmux_arg(value),
        ])
        .map(|_| ())
    }

    pub fn kill(&self) -> Result<(), ServerError> {
        self.run(&["kill-server"]).map(|_| ())
    }

    /// Les panes de la session, fenêtre cachée comprise.
    pub fn session_panes(&self, session: &str) -> Result<Vec<SessionPane>, ServerError> {
        let target = exact(session);
        self.run(&[
            "list-panes",
            "-s",
            "-t",
            &target,
            "-F",
            SESSION_PANES_FORMAT,
        ])
        .map(|listing| parse_session_panes(&listing))
    }

    /// Si la fenêtre visible a disparu, en sort le pane caché le plus récent (`restore_arguments`). Vrai si
    /// quelque chose a été fait.
    pub fn restore_visible(&self, session: &str) -> Result<bool, ServerError> {
        let panes = self.session_panes(session)?;
        let Some(args) = restore_arguments(session, &panes) else {
            return Ok(false);
        };
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&args).map(|_| true)
    }

    /// Passe la session au preset (spec §4.4) et rend les shells créés, sur lesquels `@idema_agent` vaut
    /// `new_agent`. Une fenêtre visible disparue est d'abord rétablie.
    pub fn apply_preset(
        &self,
        session: &str,
        project: &str,
        preset: Preset,
        focused: Option<PaneId>,
        new_agent: &str,
    ) -> Result<Vec<PaneId>, ServerError> {
        self.restore_visible(session)?;
        let panes = self.session_panes(session)?;
        let now_ms = now_millis();
        let args = preset_arguments(&PresetChange {
            session,
            project,
            preset,
            panes: &panes,
            focused,
            now_ms,
        })
        .ok_or_else(|| ServerError::Tmux {
            args: format!("select-layout {}", preset.id()),
            stderr: "no visible window, or the window is too small for this layout".into(),
        })?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run_creating_shells(&args, new_agent)
    }

    /// Lance la ligne `args`, lit les `%<n>` qu'elle imprime (`split-window -P`) **même quand tmux échoue** : une
    /// ligne qui crée un shell puis bute sur la suite le laisse en vie, et il doit porter `new_agent` pour ne pas
    /// passer pour un agent inconnu. Le marquage tient en une invocation ; l'erreur de tmux, s'il y en a une, est
    /// rendue après.
    pub fn run_creating_shells(
        &self,
        args: &[&str],
        new_agent: &str,
    ) -> Result<Vec<PaneId>, ServerError> {
        self.guard_remote(args)?;
        let output = self.command(args).stdin(Stdio::null()).output()?;
        let created: Vec<PaneId> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().strip_prefix('%')?.parse().ok())
            .map(PaneId)
            .collect();
        let marked = if created.is_empty() {
            Ok(())
        } else {
            let commands = created
                .iter()
                .map(|pane| {
                    words(&[
                        "set-option",
                        "-p",
                        "-t",
                        &format!("%{}", pane.0),
                        "--",
                        "@idema_agent",
                        &tmux_arg(new_agent),
                    ])
                })
                .collect();
            let args = command_line(commands);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            self.run(&args).map(|_| ())
        };
        if !output.status.success() {
            return Err(ServerError::Tmux {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        marked?;
        Ok(created)
    }

    /// Zoome `pane` sur toute sa fenêtre, ou rétablit la fenêtre si elle est déjà zoomée (`resize-pane -Z`).
    pub fn toggle_zoom(&self, pane: PaneId) -> Result<(), ServerError> {
        self.run(&["resize-pane", "-Z", "-t", &format!("%{}", pane.0)])
            .map(|_| ())
    }

    /// Donne le focus de tmux à `pane` : `pane_active`, la méta de la barre et la relance s'y fient.
    pub fn select_pane(&self, pane: PaneId) -> Result<(), ServerError> {
        self.run(&["select-pane", "-t", &format!("%{}", pane.0)])
            .map(|_| ())
    }
}

/// Fait démarrer le process tmux avec un masque de signaux vide et les dispositions par défaut.
///
/// La std de Rust 1.98.1 (`library/std/src/sys/process/unix/unix.rs`, chemins fork/exec et `posix_spawn`)
/// fait hériter l'enfant du masque du fil appelant (« Inherit the signal mask from the parent rather than
/// resetting it ») et ne remet que SIGPIPE à `SIG_DFL`. Or un fil de GPUI peut avoir un masque bloqué : le
/// serveur tmux qu'il lance ignorait alors `kill-server`, SIGTERM, SIGHUP et ne récoltait jamais ses
/// zombies (porte du jalon 2, S-1). Un `pre_exec` remet donc tout à zéro dans l'enfant, avant l'exec ;
/// la présence d'un fermoir fait aussi passer la std du `posix_spawn` à fork/exec, ce qui est voulu.
pub fn reset_signal_state(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY : la fermeture ne fait que des appels async-signal-safe (`sigemptyset`, `pthread_sigmask`,
    // `signal`), sans allocation ni verrou, comme l'exige `pre_exec` entre le fork et l'exec.
    unsafe {
        command.pre_exec(|| {
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            if libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            for signal in [
                libc::SIGTERM,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGCHLD,
                libc::SIGPIPE,
            ] {
                if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Horodatage en millisecondes : il fixe l'ordre des sessions dans la barre (`@idema_created`).
fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// L'erreur d'une invocation de tmux sans la ligne de commande : le sous-commande seule (`new-session`) et le message de
/// tmux. Les autres erreurs passent telles quelles.
fn without_argv(error: ServerError) -> ServerError {
    match error {
        ServerError::Tmux { args, stderr } => ServerError::Tmux {
            args: args
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned(),
            stderr,
        },
        other => other,
    }
}

/// Suffixe de nom de session, sans dépendance : horloge et identifiant du process.
fn random_u32() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos ^ std::process::id().rotate_left(16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket_flag(socket: &str) -> (String, String) {
        let server = TmuxServer::new(
            PathBuf::from("tmux"),
            socket,
            PathBuf::from("/nonexistent/tmux.conf"),
            Vec::new(),
        );
        let command = server.command(&["list-sessions"]);
        let mut args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned());
        (args.next().unwrap(), args.next().unwrap())
    }

    #[test]
    fn a_named_socket_uses_l_and_an_absolute_path_uses_s() {
        assert_eq!(socket_flag("idema"), ("-L".into(), "idema".into()));
        assert_eq!(
            socket_flag("/var/folders/t/idema-test/sock"),
            ("-S".into(), "/var/folders/t/idema-test/sock".into())
        );
    }

    #[test]
    fn parse_sessions_keeps_idema_sessions_only() {
        let listing = "idema-1a\t/Users/r/dev/idema\t1760000000500\tgrid-2x2\t\t\tClaude\nhors-projet\t\t\t\t\t\t\nidema-2b\t/Users/r/dev/lintree\t1760000000100\tsingle\t\t\tShell";
        assert_eq!(
            parse_sessions(listing),
            vec![
                ListedSession {
                    name: "idema-1a".into(),
                    project: "/Users/r/dev/idema".into(),
                    created: 1_760_000_000_500,
                    layout: "grid-2x2".into(),
                    ideation: false,
                    remote_name: String::new(),
                    display_name: "Claude".into(),
                },
                ListedSession {
                    name: "idema-2b".into(),
                    project: "/Users/r/dev/lintree".into(),
                    created: 1_760_000_000_100,
                    layout: "single".into(),
                    ideation: false,
                    remote_name: String::new(),
                    display_name: "Shell".into(),
                },
            ]
        );
    }

    #[test]
    fn parse_sessions_reads_the_ideation_marks() {
        let listing = "idema-1\t/Users/Shared/erom-ideation\t5\tsingle\tideation\terom-ideation-2\tFable\nidema-2\t/p\t6\tsingle\t\t\tClaude";
        let listed = parse_sessions(listing);
        assert!(listed[0].ideation);
        assert_eq!(listed[0].remote_name, "erom-ideation-2");
        assert_eq!(listed[0].display_name, "Fable");
        assert!(!listed[1].ideation);
        assert_eq!(listed[1].remote_name, "");
    }

    #[test]
    fn parse_sessions_tolerates_missing_fields_and_keeps_tabs_in_the_name() {
        let listed = parse_sessions("idema-3c\t/p");
        assert_eq!(
            (listed[0].created, listed[0].display_name.as_str()),
            (0, "")
        );
        assert_eq!(
            parse_sessions("idema-4d\t/p\t1\tsingle\t\t\tun\tnom")[0].display_name,
            "un\tnom"
        );
    }

    #[test]
    fn parse_panes_reads_activity_agent_and_idnma_agent() {
        let listing = "idema-1a\t%3\t11\tGLM\tglm\nidema-1a\t%4\t10\t\t\nother\t%5\t01\tShell\t";
        assert_eq!(
            parse_panes(listing),
            vec![
                ListedPane {
                    session: "idema-1a".into(),
                    pane: PaneId(3),
                    active: true,
                    agent: "GLM".into(),
                    idnma_agent: "glm".into(),
                },
                ListedPane {
                    session: "idema-1a".into(),
                    pane: PaneId(4),
                    active: false,
                    agent: String::new(),
                    idnma_agent: String::new(),
                },
                ListedPane {
                    session: "other".into(),
                    pane: PaneId(5),
                    active: false,
                    agent: "Shell".into(),
                    idnma_agent: String::new(),
                },
            ]
        );
    }

    #[test]
    fn parse_panes_skips_malformed_lines() {
        assert!(parse_panes("s\tnot-a-pane\t11\t\t").is_empty());
    }

    fn session_pane(
        window: u32,
        active: bool,
        stash: bool,
        pane: u32,
        hidden_at: u128,
    ) -> SessionPane {
        SessionPane {
            window: WindowId(window),
            window_active: active,
            stash,
            window_width: 120,
            window_height: 40,
            pane: PaneId(pane),
            pane_active: false,
            hidden_at,
        }
    }

    #[test]
    fn parse_session_panes_reads_windows_stash_and_hidden_order() {
        let listing = "@0\t1\t\t120\t40\t%0\t1\t\n@1\t0\t1\t120\t40\t%3\t1\t100\nbroken";
        let panes = parse_session_panes(listing);
        assert_eq!(panes.len(), 2);
        assert_eq!(
            panes[0],
            SessionPane {
                pane_active: true,
                ..session_pane(0, true, false, 0, 0)
            }
        );
        assert_eq!(
            panes[1],
            SessionPane {
                pane_active: true,
                ..session_pane(1, false, true, 3, 100)
            }
        );
    }

    #[test]
    fn parse_capture_panes_reads_every_field() {
        let listing = "idema-1\t@2\t1\t\t%5\t1\t4242\t\tClaude\tclaude\t\t/p/a#b c\n\
                       idema-1\t@3\t0\t1\t%6\t0\t4300\t777\tShell seul\t\t\t/p\n\
                       idema-1\t@3\t0";
        let panes = parse_capture_panes(listing);
        assert_eq!(
            panes,
            vec![
                CapturedPane {
                    session: "idema-1".into(),
                    window: WindowId(2),
                    window_active: true,
                    stash: false,
                    pane: PaneId(5),
                    pane_active: true,
                    pane_pid: 4242,
                    hidden_at: 0,
                    agent: "Claude".into(),
                    idnma_agent: "claude".into(),
                    resume: String::new(),
                    cwd: PathBuf::from("/p/a#b c"),
                },
                CapturedPane {
                    session: "idema-1".into(),
                    window: WindowId(3),
                    window_active: false,
                    stash: true,
                    pane: PaneId(6),
                    pane_active: false,
                    pane_pid: 4300,
                    hidden_at: 777,
                    agent: "Shell seul".into(),
                    idnma_agent: String::new(),
                    resume: String::new(),
                    cwd: PathBuf::from("/p"),
                },
            ]
        );
    }

    #[test]
    fn parse_capture_panes_reads_a_resume_value_with_spaces_and_quotes() {
        let resume = "aaaa1111-bbbb-4ccc-8ddd-eeeeffff0000 idnma run 'claude' --model 'haiku' --remote-control 'it'\\''s me'";
        let listing = format!("idema-1\t@2\t1\t\t%5\t1\t4242\t\tOpus\tclaude\t{resume}\t/p/a b\n");
        let panes = parse_capture_panes(&listing);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].resume, resume);
        assert_eq!(panes[0].cwd, PathBuf::from("/p/a b"));
    }

    fn change<'a>(panes: &'a [SessionPane], preset: Preset, focused: u32) -> PresetChange<'a> {
        PresetChange {
            session: "idema-1",
            project: "/p/a#b",
            preset,
            panes,
            focused: Some(PaneId(focused)),
            now_ms: 500,
        }
    }

    #[test]
    fn shrinking_creates_the_stash_with_the_first_hidden_pane_then_joins_it() {
        let panes = [
            session_pane(0, true, false, 0, 0),
            session_pane(0, true, false, 1, 0),
            session_pane(0, true, false, 2, 0),
        ];
        let args = preset_arguments(&change(&panes, Preset::Single, 0)).unwrap();
        assert_eq!(
            args.join(" "),
            "break-pane -d -s %2 -t =idema-1: -n idema-stash ; set-option -w -t %2 @idema_stash 1 ; \
set-option -p -t %2 @idema_hidden 500 ; join-pane -d -s %1 -t %2 ; select-layout -t %2 tiled ; \
set-option -p -t %1 @idema_hidden 501 ; select-layout -t @0 aafd,120x40,0,0,0 ; \
set-option -t =idema-1: -- @idema_layout single"
        );
    }

    #[test]
    fn growing_rejoins_after_the_last_visible_pane_then_creates_shells_in_the_project() {
        let panes = [
            session_pane(0, true, false, 0, 0),
            session_pane(1, false, true, 3, 100),
            session_pane(1, false, true, 1, 102),
        ];
        let args = preset_arguments(&change(&panes, Preset::Grid2x2, 0)).unwrap();
        let line = args.join(" ");
        assert!(line.starts_with(
            "join-pane -d -s %1 -t %0 ; set-option -p -u -t %1 @idema_hidden ; select-layout -t @0 tiled ; \
join-pane -d -s %3 -t %1 ; set-option -p -u -t %3 @idema_hidden ; select-layout -t @0 tiled ; \
split-window -d -P -F #{pane_id} -t %3 -c /p/a##b ; select-layout -t @0 tiled ; select-layout -t @0 "
        ), "{line}");
        assert!(line.ends_with(" ; set-option -t =idema-1: -- @idema_layout grid-2x2"));
    }

    #[test]
    fn an_existing_stash_receives_the_hidden_panes() {
        let panes = [
            session_pane(0, true, false, 0, 0),
            session_pane(0, true, false, 4, 0),
            session_pane(1, false, true, 3, 100),
        ];
        let line = preset_arguments(&change(&panes, Preset::Single, 4))
            .unwrap()
            .join(" ");
        assert!(line.starts_with(
            "join-pane -d -s %0 -t @1 ; select-layout -t @1 tiled ; set-option -p -t %0 @idema_hidden 500 ; "
        ), "{line}");
    }

    #[test]
    fn no_visible_window_or_no_room_gives_no_arguments() {
        let stash_only = [session_pane(1, true, true, 3, 100)];
        assert_eq!(
            preset_arguments(&change(&stash_only, Preset::Single, 3)),
            None
        );
        let tiny = [SessionPane {
            window_width: 5,
            ..session_pane(0, true, false, 0, 0)
        }];
        assert_eq!(preset_arguments(&change(&tiny, Preset::Grid3x2, 0)), None);
    }

    #[test]
    fn restore_brings_back_the_most_recently_hidden_pane() {
        let several = [
            session_pane(1, true, true, 3, 100),
            session_pane(1, true, true, 5, 102),
        ];
        assert_eq!(
            restore_arguments("idema-1", &several).unwrap().join(" "),
            "break-pane -s %5 -t =idema-1: ; set-option -p -u -t %5 @idema_hidden ; set-option -t =idema-1: -- @idema_layout single"
        );
        let one = [session_pane(1, true, true, 3, 100)];
        assert_eq!(
            restore_arguments("idema-1", &one).unwrap().join(" "),
            "set-option -w -u -t %3 @idema_stash ; set-option -p -u -t %3 @idema_hidden ; set-option -t =idema-1: -- @idema_layout single"
        );
        let fine = [
            session_pane(0, true, false, 0, 0),
            session_pane(1, false, true, 3, 100),
        ];
        assert_eq!(restore_arguments("idema-1", &fine), None);
    }

    #[test]
    fn parse_server_sessions_keeps_server_panes_only_and_reads_dead_ones() {
        let listing = "idema-srv-1\t%7\t4242\t0\t/p/a b\tweb\nidema-srv-2\t%8\t\t1\t/p/c\tapi\tx\nidema-1a\t%3\t99\t0\t\t\nidema-srv-3\t%9\tpid?\t0\t/p/d\tbad\nidema-srv-4\t%10\t\t0\t/p/e\tlive-without-pid\nbroken";
        // Un pane vivant dont le PID est illisible ou absent n'est pas listé comme vivant avec le PID 0.
        assert_eq!(
            parse_server_sessions(listing),
            vec![
                ListedServerSession {
                    name: "idema-srv-1".into(),
                    project: PathBuf::from("/p/a b"),
                    entry: "web".into(),
                    pane: PaneId(7),
                    pane_pid: 4242,
                    dead: false,
                },
                ListedServerSession {
                    name: "idema-srv-2".into(),
                    project: PathBuf::from("/p/c"),
                    entry: "api\tx".into(),
                    pane: PaneId(8),
                    pane_pid: 0,
                    dead: true,
                },
            ]
        );
    }

    #[test]
    fn a_failed_server_session_creation_never_repeats_the_environment_of_the_entry() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("idema-fake-tmux-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Un « tmux » qui échoue en disant pourquoi sur sa sortie d'erreur, comme le vrai.
        let fake = dir.join("tmux");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'create session failed: boom' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let server = TmuxServer::new(
            fake,
            "idema-test",
            PathBuf::from("/nonexistent/tmux.conf"),
            Vec::new(),
        );
        let args = vec!["run".to_string(), "dev".to_string()];
        let env = vec![("DB_PASSWORD".to_string(), "s3cr3t-value-42".to_string())];
        let request = NewServerSession {
            project: &dir,
            entry: "web",
            cwd: &dir,
            program: "bun",
            args: &args,
            env: &env,
        };
        let error = server.create_server_session(&request).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("boom"), "the cause of tmux is kept: {text}");
        assert!(
            !text.contains("s3cr3t-value-42") && !text.contains("DB_PASSWORD"),
            "the environment leaked into the error: {text}"
        );
    }

    #[test]
    fn parse_pane_pids_skips_dead_and_malformed_panes() {
        let listing = "idema-1a\t%3\t99\nidema-srv-2\t%8\t\nidema-srv-3\t%9\t12\nbroken";
        assert_eq!(
            parse_pane_pids(listing),
            vec![
                ("idema-1a".to_string(), PaneId(3), 99),
                ("idema-srv-3".to_string(), PaneId(9), 12),
            ]
        );
    }

    #[test]
    fn server_session_arguments_pass_every_value_as_its_own_argument() {
        let args = vec!["a b".to_string(), "fin;".to_string()];
        let env = vec![("PORT".to_string(), "8899".to_string())];
        let request = NewServerSession {
            project: Path::new("/p/mon projet"),
            entry: "web",
            cwd: Path::new("/p/a#b"),
            program: "node",
            args: &args,
            env: &env,
        };
        let line = server_session_arguments("idema-srv-1", "/bin/zsh", &request);
        assert_eq!(
            line.join("\u{1f}"),
            [
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-s",
                "idema-srv-1",
                "-c",
                "/p/a##b",
                "-e",
                "PORT=8899",
                "--",
                "/bin/zsh",
                "-l",
                "-i",
                "-c",
                r#"exec "$0" "$@""#,
                "node",
                "a b",
                r"fin\;",
                ";",
                "set-option",
                "-t",
                "=idema-srv-1:",
                "--",
                "@idema_server_project",
                "/p/mon projet",
                ";",
                "set-option",
                "-t",
                "=idema-srv-1:",
                "--",
                "@idema_server_entry",
                "web",
                ";",
                "set-option",
                "-p",
                "-t",
                "=idema-srv-1:",
                "remain-on-exit",
                "on",
                ";",
                "set-option",
                "-p",
                "-t",
                "=idema-srv-1:",
                "remain-on-exit-format",
                "",
            ]
            .join("\u{1f}")
        );
        // Jamais la barre : aucune option `@idema_project`.
        assert!(!line.iter().any(|arg| arg == "@idema_project"));
    }

    #[test]
    fn locate_tmux_rejects_a_missing_override() {
        assert!(matches!(
            locate_tmux(Some(Path::new("/nonexistent/tmux")), None),
            Err(ServerError::NotFound)
        ));
    }

    #[test]
    fn parse_remote_sessions_keeps_foreign_sessions() {
        let foreign = ListedSession {
            name: "testvpc".into(),
            project: String::new(),
            created: 1_794_130_101_000,
            layout: String::new(),
            ideation: false,
            remote_name: String::new(),
            display_name: "testvpc".into(),
        };
        assert_eq!(
            parse_remote_sessions("testvpc\t\t\t1794130101\t\t\t\t"),
            vec![foreign.clone()]
        );
        // Dernière ligne tronquée par `run()` : les champs manquants sont vides.
        assert_eq!(
            parse_remote_sessions("testvpc\t\t\t1794130101"),
            vec![foreign]
        );
        let listing = "idema-0a1b2c3d\t/home/claude\t1794130000123\t1794130000\tsingle\t\t\tAudit\nx\tideation-like\t\t5\t\tideation\t\t";
        let sessions = parse_remote_sessions(listing);
        assert_eq!(
            sessions[0],
            ListedSession {
                name: "idema-0a1b2c3d".into(),
                project: "/home/claude".into(),
                created: 1_794_130_000_123,
                layout: "single".into(),
                ideation: false,
                remote_name: String::new(),
                display_name: "Audit".into(),
            }
        );
        assert!(!sessions[1].ideation);
        assert_eq!(sessions[1].created, 5000);
        assert_eq!(sessions[1].display_name, "x");
    }

    fn remote_server() -> TmuxServer {
        TmuxServer::remote(RemoteSpec {
            destination: "idema-no-such-host.invalid".into(),
            control_dir: PathBuf::from("/nonexistent/idema-control"),
            slot: 0,
            ssh_env: Vec::new(),
            home: "/home/claude".into(),
            shell: "/bin/bash".into(),
        })
    }

    #[test]
    fn a_remote_server_refuses_kill_server_and_config_writes() {
        let server = remote_server();
        assert!(matches!(
            server.kill(),
            Err(ServerError::RemoteForbidden(_))
        ));
        assert!(matches!(
            server.write_config(),
            Err(ServerError::RemoteForbidden(_))
        ));
        assert!(matches!(
            server.set_global_environment("A", "b"),
            Err(ServerError::RemoteForbidden(_))
        ));
        assert!(matches!(
            server.run(&["set-option", "-g", "status", "off"]),
            Err(ServerError::RemoteForbidden(_))
        ));
    }

    #[test]
    fn run_creating_shells_is_guarded_on_a_remote_server() {
        assert!(matches!(
            remote_server().run_creating_shells(&["set-option", "-g", "x", "y"], "glm"),
            Err(ServerError::RemoteForbidden(_))
        ));
    }

    #[test]
    fn a_remote_check_version_that_cannot_reach_the_host_is_a_tmux_error() {
        let server = TmuxServer::remote(RemoteSpec {
            destination: "nonexistent-host-idema.invalid".into(),
            control_dir: std::env::temp_dir().join("idema-no-control"),
            slot: 0,
            ssh_env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            home: "/home/claude".into(),
            shell: "/bin/bash".into(),
        });
        assert!(matches!(
            server.check_version(),
            Err(ServerError::Tmux { .. })
        ));
    }

    #[test]
    fn a_remote_server_environment_is_the_remote_shell() {
        let server = remote_server();
        assert!(server.is_remote());
        assert_eq!(
            server.environment(),
            [
                ("HOME".to_string(), "/home/claude".to_string()),
                ("SHELL".to_string(), "/bin/bash".to_string())
            ]
        );
        assert_eq!(
            server.remote_spec().map(|spec| spec.destination.as_str()),
            Some("idema-no-such-host.invalid")
        );
    }
}
