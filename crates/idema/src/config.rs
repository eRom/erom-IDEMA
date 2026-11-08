//! `~/.config/idema/config.toml` (spec §3.4, §4.3, §8) : réglages et entrées du lanceur. L'utilisateur l'édite à la
//! main ; Idema le relit dès qu'il change et garde la dernière version valide quand la nouvelle ne l'est pas.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use regex::Regex;
use serde::Deserialize;

use crate::settings::nav::fold;

/// Contenu écrit quand le fichier n'existe pas : les agents d'`idnma run`, ceux de `~/.idnma.yml`.
pub const DEFAULT_CONFIG: &str = r#"# Réglages d'IdemA. Le fichier est relu dès qu'il change ; une erreur s'affiche sous la liste des projets.
ui_font_size = 20
terminal_font_family = "Hack Nerd Font Mono"
terminal_font_size = 16
terminal_line_height = 1.55
sound_on_wait = true
# tmux_path = "/opt/homebrew/bin/tmux"

# Entrées du lanceur. Le menu les range par `harness` (le moteur qui fait tourner le modèle), puis par nom, chaque
# harness en sous-menu ; une entrée sans `harness` reste au premier niveau. « Shell seul » est intégré à IdemA. Le logo
# d'une session qui fait tourner une entrée est celui de son nom en minuscules (claude, codex, deepseek, gemini, glm,
# grok, kimi, mistral, muse, nvidia, qwen, xiaomi), ou celui de `icon`.
[[agents]]
name = "Gemini"
harness = "Antigravity"
command = "idnma run agy"

[[agents]]
name = "Claude"
harness = "Claude Code"
command = "idnma run claude"
default = true

[[agents]]
name = "Deepseek"
harness = "Claude Code"
command = "idnma run deepseek"

[[agents]]
name = "GLM"
harness = "Claude Code"
command = "idnma run glm"

[[agents]]
name = "Kimi"
harness = "Claude Code"
command = "idnma run kimi"

[[agents]]
name = "Muse"
harness = "Claude Code"
command = "idnma run muse"

[[agents]]
name = "Qwen"
harness = "Claude Code"
command = "idnma run qwen"

[[agents]]
name = "Codex"
harness = "Codex"
command = "idnma run codex"

[[agents]]
name = "Grok"
harness = "Grok Build"
command = "idnma run grok"

# Bouillon de neurones : des sessions Claude Code d'idéation, hors projet, pilotables depuis l'app mobile (Remote
# Control). La section est masquée tant que [ideation] est absente. `dir` est obligatoire ; `model` et `effort`
# passent à claude tels quels ; `label` nomme les bouillons (« Claude » par défaut).
# [ideation]
# dir = "~/ideation"
# model = "opus"
# effort = "high"
# label = "Claude"
"#;

/// Nom de l'entrée intégrée « Shell seul », et nom par défaut de ses sessions.
pub const SHELL_ENTRY: &str = "Shell seul";
pub const SHELL_SESSION_NAME: &str = "Shell";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEntry {
    pub name: String,
    /// Le moteur qui fait tourner le modèle, sous-menu du lanceur. Absent : l'entrée reste au premier niveau.
    #[serde(default)]
    pub harness: Option<String>,
    pub command: String,
    #[serde(default)]
    pub default: bool,
    /// Le logo de la barre latérale ; à défaut, le nom de l'entrée en minuscules.
    #[serde(default)]
    pub icon: Option<String>,
}

impl AgentEntry {
    /// Le nom du logo de l'entrée : `icon`, sinon son nom en minuscules.
    pub fn icon_name(&self) -> String {
        self.icon
            .clone()
            .unwrap_or_else(|| self.name.to_lowercase())
    }

    /// Le harness de l'entrée, sans ses blancs ; un harness vide n'en est pas un.
    pub fn harness_name(&self) -> Option<&str> {
        self.harness
            .as_deref()
            .map(str::trim)
            .filter(|harness| !harness.is_empty())
    }
}

/// Un groupe d'entrées du lanceur : les rangs dans `agents`, triés par nom. `harness` vaut `None` pour les entrées qui
/// n'en déclarent pas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessGroup {
    pub harness: Option<String>,
    pub agents: Vec<usize>,
}

/// Les entrées par harness : harness et noms par ordre alphabétique, sans casse ni accents, les entrées sans harness en
/// dernier. Deux homonymes gardent l'ordre du fichier.
pub fn harness_groups(agents: &[AgentEntry]) -> Vec<HarnessGroup> {
    let mut groups: Vec<HarnessGroup> = Vec::new();
    for (index, agent) in agents.iter().enumerate() {
        let harness = agent.harness_name();
        match groups
            .iter_mut()
            .find(|group| group.harness.as_deref() == harness)
        {
            Some(group) => group.agents.push(index),
            None => groups.push(HarnessGroup {
                harness: harness.map(str::to_owned),
                agents: vec![index],
            }),
        }
    }
    for group in &mut groups {
        group
            .agents
            .sort_by_cached_key(|&index| fold(&agents[index].name));
    }
    groups
        .sort_by_cached_key(|group| (group.harness.is_none(), group.harness.as_deref().map(fold)));
    groups
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_ui_font_size")]
    pub ui_font_size: f32,
    #[serde(default = "default_terminal_font_family")]
    pub terminal_font_family: String,
    #[serde(default = "default_terminal_font_size")]
    pub terminal_font_size: f32,
    #[serde(default = "default_terminal_line_height")]
    pub terminal_line_height: f32,
    #[serde(default = "default_sound_on_wait")]
    pub sound_on_wait: bool,
    #[serde(default)]
    pub tmux_path: Option<PathBuf>,
    #[serde(default)]
    pub agents: Vec<AgentEntry>,
    /// Section `[commit_message]` : absente, la fonction est éteinte.
    #[serde(default)]
    pub commit_message: Option<CommitMessageConfig>,
    /// Section `[browser]` : les favoris de la page d'accueil du navigateur. Absente, aucun favori.
    #[serde(default)]
    pub browser: Option<BrowserConfig>,
    /// Section `[ideation]` : les sessions d'idéation (bouillon de neurones). Absente, la fonction est masquée.
    #[serde(default)]
    pub ideation: Option<IdeationConfig>,
    /// Section `[companion]` : le compagnon iPhone. Absente, il n'existe pas (ni prise, ni fil).
    #[serde(default)]
    pub companion: Option<CompanionConfig>,
    /// Section `[machines]` : le nom de Localhost et les machines distantes. Absente, Localhost seul.
    #[serde(default)]
    pub machines: Option<MachinesConfig>,
}

/// Section `[machines]` : Localhost est toujours là, les machines distantes viennent de `[[machines.remote]]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachinesConfig {
    /// Le nom de Localhost dans l'interface.
    #[serde(default)]
    pub local_label: Option<String>,
    /// Les machines distantes, dans l'ordre du fichier.
    #[serde(default)]
    pub remote: Vec<RemoteMachineEntry>,
}

/// Une machine distante : un nom libre et ce que Romain tape après `ssh`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteMachineEntry {
    pub label: String,
    /// Alias de `~/.ssh/config` ou `utilisateur@hôte` ; c'est l'identité de la machine.
    pub connection: String,
}

/// Le nom de Localhost tant que `[machines] local_label` ne le change pas.
pub(crate) const DEFAULT_LOCAL_LABEL: &str = "Localhost";

/// Pourquoi une connexion est refusée, `None` si elle convient. `others` : les connexions des autres machines.
pub fn connection_problem(connection: &str, others: &[&str]) -> Option<&'static str> {
    if connection.trim().is_empty() {
        Some("La connexion est vide.")
    } else if connection
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        Some("La connexion ne peut contenir ni espace ni caractère de contrôle.")
    } else if connection.starts_with('-') {
        // `ssh` la lirait comme une option.
        Some("La connexion ne peut pas commencer par un tiret.")
    } else if connection == crate::machines::key::LOCAL_MACHINE {
        // La clé de Localhost : `machine_of` ne saurait plus distinguer les deux.
        Some("Ce nom est réservé à la machine locale.")
    } else if others.contains(&connection) {
        Some("Cette connexion est déjà celle d'une autre machine.")
    } else {
        None
    }
}

/// Pourquoi un label est refusé, `None` s'il convient.
pub fn label_problem(label: &str) -> Option<&'static str> {
    label.trim().is_empty().then_some("Le label est vide.")
}

/// Section `[companion]` : le service local que la page installée sur l'iPhone interroge.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompanionConfig {
    /// Port local, lié à `127.0.0.1` seulement.
    #[serde(default = "default_companion_port")]
    pub port: u16,
    /// Adresse tailnet à montrer quand la détection échoue.
    #[serde(default)]
    pub public_url: Option<String>,
    /// Notifier le passage d'une session en « attend ».
    #[serde(default = "default_true")]
    pub notify_waiting: bool,
    /// Notifier le passage d'une session en « erreur ».
    #[serde(default = "default_true")]
    pub notify_error: bool,
    /// Romain est absent après ce délai sans clavier ni souris ; `0` : toujours absent.
    #[serde(default = "default_away_after")]
    pub away_after_seconds: u64,
}

fn default_companion_port() -> u16 {
    3910
}

fn default_true() -> bool {
    true
}

fn default_away_after() -> u64 {
    120
}

/// Section `[ideation]` : où vivent les sessions d'idéation et comment `claude` y est lancé.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdeationConfig {
    /// Dossier des sessions : absolu, `~` ou `~/...`.
    pub dir: String,
    /// Le modèle passé à `claude`, tel quel.
    #[serde(default)]
    pub model: Option<String>,
    /// L'effort passé à `claude`, tel quel.
    #[serde(default)]
    pub effort: Option<String>,
    /// Le nom des bouillons dans l'interface.
    #[serde(default = "default_ideation_label")]
    pub label: String,
}

fn default_ideation_label() -> String {
    "Claude".into()
}

impl IdeationConfig {
    /// Le dossier des sessions, `~` remplacé par `$HOME`.
    pub fn dir_path(&self) -> PathBuf {
        let home = || PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        match self.dir.strip_prefix("~/") {
            Some(rest) => home().join(rest),
            None if self.dir == "~" => home(),
            None => PathBuf::from(&self.dir),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserConfig {
    #[serde(default)]
    pub favorites: Vec<Favorite>,
}

/// Un favori : un nom et une adresse `http`, `https` ou `file`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Favorite {
    pub name: String,
    pub url: String,
}

/// Section `[commit_message]` : le message de commit rédigé par un modèle Gemini (spec B-11).
#[derive(Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitMessageConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_commit_message_model")]
    pub model: String,
    #[serde(default = "default_include_project_rules")]
    pub include_project_rules: bool,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// La section absente ou vide : éteinte, sans clé, avec le modèle et les règles de projet par défaut.
impl Default for CommitMessageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            model: default_commit_message_model(),
            include_project_rules: default_include_project_rules(),
            instructions: None,
            endpoint: None,
        }
    }
}

fn default_commit_message_model() -> String {
    "gemini-3.5-flash-lite".into()
}

fn default_include_project_rules() -> bool {
    true
}

/// `Debug` à la main : la clé d'API ne doit apparaître ni dans un journal ni dans un message d'erreur.
impl fmt::Debug for CommitMessageConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommitMessageConfig")
            .field("enabled", &self.enabled)
            .field("api_key", &self.api_key.as_ref().map(|_| "…"))
            .field("model", &self.model)
            .field("include_project_rules", &self.include_project_rules)
            .field("instructions", &self.instructions)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

fn default_ui_font_size() -> f32 {
    20.
}

fn default_terminal_font_family() -> String {
    "Hack Nerd Font Mono".into()
}

fn default_terminal_font_size() -> f32 {
    16.
}

fn default_terminal_line_height() -> f32 {
    1.55
}

fn default_sound_on_wait() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        toml::from_str(DEFAULT_CONFIG).expect("the default config parses")
    }
}

impl Config {
    /// L'agent du `+` et de `⌘T` : l'entrée marquée `default`, sinon la première, sinon « Shell seul ».
    pub fn default_agent(&self) -> Option<&AgentEntry> {
        self.agents
            .iter()
            .find(|agent| agent.default)
            .or_else(|| self.agents.first())
    }

    /// Le nom de Localhost : `local_label`, sinon « Localhost ».
    pub fn local_machine_label(&self) -> &str {
        self.machines
            .as_ref()
            .and_then(|machines| machines.local_label.as_deref())
            .unwrap_or(DEFAULT_LOCAL_LABEL)
    }

    /// Les machines distantes, dans l'ordre du fichier.
    pub fn remote_machines(&self) -> &[RemoteMachineEntry] {
        self.machines
            .as_ref()
            .map_or(&[], |machines| machines.remote.as_slice())
    }
}

/// Ce qu'une config rechargée oblige à refaire : l'échelle de l'interface, la police du terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReloadEffects {
    pub theme: bool,
    pub terminal_font: bool,
}

impl Config {
    /// Ce qui change de `previous` à `self`. `tmux_path` n'y est pas : il ne sert qu'au lancement suivant.
    pub fn reload_effects(&self, previous: &Config) -> ReloadEffects {
        ReloadEffects {
            theme: self.ui_font_size != previous.ui_font_size,
            terminal_font: self.terminal_font_family != previous.terminal_font_family
                || self.terminal_font_size != previous.terminal_font_size
                || self.terminal_line_height != previous.terminal_line_height,
        }
    }
}

/// D'où vient le refus : le fichier n'a pas pu être lu, `toml` n'a pas pu l'analyser, ou IdemA refuse une valeur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    Read,
    Syntax,
    Invalid,
}

/// Une config refusée : la ligne fautive quand l'analyseur la connaît.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub kind: ConfigErrorKind,
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "config.toml, ligne {line} : {}", self.message),
            None => write!(f, "config.toml : {}", self.message),
        }
    }
}

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError {
        kind: ConfigErrorKind::Invalid,
        line: None,
        message: message.into(),
    }
}

/// Lit et valide le texte d'une config.
pub fn parse_config(text: &str) -> Result<Config, ConfigError> {
    let config: Config = toml::from_str(text).map_err(|error| ConfigError {
        kind: ConfigErrorKind::Syntax,
        line: error
            .span()
            .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1),
        message: redact_api_key(text, error.message().trim()),
    })?;
    validate(&config, text)?;
    Ok(config)
}

/// `[[browser.favorites]]`, avec ou sans espaces autour ou dans le nom, avec ou sans commentaire final.
fn is_favorite_header(line: &str) -> bool {
    let code = line.split('#').next().unwrap_or("");
    let squeezed: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    squeezed == "[[browser.favorites]]"
}

/// La ligne de l'en-tête du `index`-ième `[[browser.favorites]]`, pour situer un favori refusé. `None` si le
/// favori n'est pas écrit sous cette forme (table en ligne, par exemple).
fn favorite_line(text: &str, index: usize) -> Option<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| is_favorite_header(line))
        .nth(index)
        .map(|(position, _)| position + 1)
}

/// La ligne de la clé `key` de la `index`-ième `[[machines.remote]]` (ou de `[machines]` quand `index` est `None`), pour
/// situer une machine refusée. Repli sur l'en-tête de l'entrée ; `None` si elle n'est pas écrite sous cette forme.
fn machine_line(text: &str, index: Option<usize>, key: &str) -> Option<usize> {
    let squeeze = |line: &str| -> String {
        let code = line.split('#').next().unwrap_or("");
        code.chars().filter(|c| !c.is_whitespace()).collect()
    };
    let mut seen_remote = 0;
    let mut inside = false;
    let mut header = None;
    for (position, line) in text.lines().enumerate() {
        let squeezed = squeeze(line);
        if squeezed.starts_with('[') {
            inside = match index {
                Some(wanted) if squeezed == "[[machines.remote]]" => {
                    seen_remote += 1;
                    let hit = seen_remote == wanted + 1;
                    if hit {
                        header = Some(position + 1);
                    }
                    hit
                }
                None => squeezed == "[machines]",
                _ => false,
            };
        } else if inside
            && squeezed
                .strip_prefix(key)
                .is_some_and(|rest| rest.starts_with('='))
        {
            return Some(position + 1);
        }
    }
    header
}

/// Masque dans `message` ce qui pourrait être la clé d'API : `toml` recopie parfois la valeur fautive dans son message
/// (« invalid type: string "…", expected a boolean »), et ce message finit à l'écran et dans le journal.
/// Deux filets, indépendants de la forme d'écriture (`api_key = ...`, `commit_message.api_key = ...`, table en ligne,
/// clé entre guillemets) : toute valeur recopiée entre guillemets, apostrophes ou accents graves devient `…` dès que le
/// texte parle de `commit_message` ; et la valeur de chaque ligne `api_key = "..."` est remplacée où qu'elle apparaisse.
fn redact_api_key(text: &str, message: &str) -> String {
    let mut redacted = message.to_string();
    let line_value = Regex::new(r#"(?m)^\s*api_key\s*=\s*(?:"([^"\n]*)|'([^'\n]*))"#)
        .expect("the api_key pattern compiles");
    for captures in line_value.captures_iter(text) {
        let value = captures
            .get(1)
            .or_else(|| captures.get(2))
            .map_or("", |m| m.as_str());
        if !value.is_empty() {
            redacted = redacted.replace(value, "…");
        }
    }
    if text.contains("commit_message") {
        // Les guillemets restent : le catalogue de `errors.rs` reconnaît « unknown field ` » et « invalid type: ».
        let quoted = Regex::new(r#""(?:[^"\\]|\\.)*"|'[^']*'|`[^`]*`"#)
            .expect("the quoted-value pattern compiles");
        redacted = quoted
            .replace_all(&redacted, |captures: &regex::Captures| {
                let quote = &captures[0][..1];
                format!("{quote}…{quote}")
            })
            .into_owned();
    }
    redacted
}

fn validate(config: &Config, text: &str) -> Result<(), ConfigError> {
    let in_range =
        |value: f32, low: f32, high: f32| value.is_finite() && (low..=high).contains(&value);
    if !in_range(config.ui_font_size, 10., 40.) {
        return Err(invalid("ui_font_size doit être entre 10 et 40"));
    }
    if !in_range(config.terminal_font_size, 8., 40.) {
        return Err(invalid("terminal_font_size doit être entre 8 et 40"));
    }
    if !in_range(config.terminal_line_height, 1., 3.) {
        return Err(invalid("terminal_line_height doit être entre 1 et 3"));
    }
    for agent in &config.agents {
        if agent.name.trim().is_empty() || agent.command.trim().is_empty() {
            return Err(invalid("chaque agent a un nom et une commande"));
        }
        // La commande part au shell suivie d'Entrée : un caractère de contrôle la couperait ou l'altérerait.
        if agent
            .name
            .chars()
            .chain(agent.command.chars())
            .chain(agent.harness.iter().flat_map(|harness| harness.chars()))
            .any(char::is_control)
        {
            return Err(invalid(format!(
                "l'agent « {} » contient un caractère de contrôle",
                agent.name.escape_debug()
            )));
        }
        if agent.name == SHELL_ENTRY {
            return Err(invalid(format!("« {SHELL_ENTRY} » est réservé à IdemA")));
        }
    }
    let defaults: Vec<&str> = config
        .agents
        .iter()
        .filter(|agent| agent.default)
        .map(|agent| agent.name.as_str())
        .collect();
    if defaults.len() > 1 {
        return Err(invalid(format!(
            "un seul agent par défaut ({})",
            defaults.join(", ")
        )));
    }
    if let Some(ideation) = &config.ideation {
        let dir = ideation.dir.as_str();
        if !(dir.starts_with('/') || dir == "~" || dir.starts_with("~/")) {
            return Err(invalid(
                "[ideation] dir doit être un chemin absolu ou commencer par ~/",
            ));
        }
        for (key, value) in [
            ("dir", Some(dir)),
            ("model", ideation.model.as_deref()),
            ("effort", ideation.effort.as_deref()),
            ("label", Some(ideation.label.as_str())),
        ] {
            let Some(value) = value else { continue };
            if value.trim().is_empty() {
                return Err(invalid(format!("[ideation] {key} ne peut pas être vide")));
            }
            if value.chars().any(char::is_control) {
                return Err(invalid(format!(
                    "[ideation] {key} contient un caractère de contrôle"
                )));
            }
        }
    }
    if config
        .companion
        .as_ref()
        .is_some_and(|companion| companion.port == 0)
    {
        return Err(invalid("[companion] port doit être entre 1 et 65535"));
    }
    // Une adresse illisible ferait admettre toute origine `*.ts.net` en écriture (le repli quand aucune adresse n'est
    // connue) : mieux vaut refuser la config que dégrader la garde en silence. Elle n'est pas répétée dans le message.
    if let Some(address) = config
        .companion
        .as_ref()
        .and_then(|companion| companion.public_url.as_deref())
        && !url::Url::parse(address)
            .is_ok_and(|parsed| parsed.scheme() == "https" && parsed.host_str().is_some())
    {
        return Err(invalid(
            "[companion] public_url doit être une adresse https valide, par exemple https://mac.tailnet.ts.net:8455",
        ));
    }
    let favorites = config.browser.iter().flat_map(|browser| &browser.favorites);
    for (index, favorite) in favorites.enumerate() {
        let refusal = if favorite.name.trim().is_empty() {
            Some(format!("favori n° {} sans nom", index + 1))
        } else if !matches!(
            url::Url::parse(&favorite.url)
                .as_ref()
                .map(url::Url::scheme),
            Ok("http" | "https" | "file")
        ) {
            // Le message est tout ce que l'écran montre (pas de « Ligne N » pour une erreur sémantique) :
            // il situe donc le favori lui-même.
            Some(format!(
                "adresse de favori invalide (favori n° {} « {} ») : {}",
                index + 1,
                favorite.name.trim(),
                favorite.url
            ))
        } else {
            None
        };
        if let Some(message) = refusal {
            return Err(ConfigError {
                line: favorite_line(text, index),
                ..invalid(message)
            });
        }
    }
    if let Some(machines) = &config.machines {
        if let Some(problem) = machines.local_label.as_deref().and_then(label_problem) {
            return Err(ConfigError {
                line: machine_line(text, None, "local_label"),
                ..invalid(format!("[machines] local_label : {problem}"))
            });
        }
        for (index, machine) in machines.remote.iter().enumerate() {
            // Une connexion répétée est refusée sur l'entrée qui la répète, pas sur la première.
            let earlier: Vec<&str> = machines.remote[..index]
                .iter()
                .map(|other| other.connection.as_str())
                .collect();
            let refusal = if let Some(problem) = label_problem(&machine.label) {
                Some(("label", problem))
            } else {
                connection_problem(&machine.connection, &earlier)
                    .map(|problem| ("connection", problem))
            };
            if let Some((key, problem)) = refusal {
                return Err(ConfigError {
                    line: machine_line(text, Some(index), key),
                    ..invalid(format!("machine n° {} : {problem}", index + 1))
                });
            }
        }
    }
    Ok(())
}

/// Crée le fichier avec `DEFAULT_CONFIG` s'il n'existe pas.
pub fn ensure_config_file(path: &Path) -> std::io::Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => std::io::Write::write_all(&mut file, DEFAULT_CONFIG.as_bytes()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Lit le fichier : `Ok` si la config est valide, l'erreur à afficher sinon.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|error| ConfigError {
        kind: ConfigErrorKind::Read,
        line: None,
        message: format!("lecture impossible ({error})"),
    })?;
    parse_config(&text)
}

/// Repère qu'un fichier a changé, par sa date et sa taille : relu à chaque pas de la minuterie.
#[derive(Debug)]
pub struct FileWatch {
    path: PathBuf,
    seen: Option<(SystemTime, u64)>,
}

impl FileWatch {
    /// Le premier appel à `changed` répond vrai si le fichier existe.
    pub fn new(path: PathBuf) -> Self {
        Self { path, seen: None }
    }

    pub fn changed(&mut self) -> bool {
        let current = std::fs::metadata(&self.path)
            .ok()
            .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
        if current.is_some() && current != self.seen {
            self.seen = current;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_launchers_are_idnma_agents_with_claude_by_default() {
        let config = Config::default();
        let defaults: Vec<&AgentEntry> =
            config.agents.iter().filter(|agent| agent.default).collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].name, "Claude");
        assert!(
            config
                .agents
                .iter()
                .all(|agent| agent.command.starts_with("idnma run "))
        );
    }

    #[test]
    fn missing_settings_take_their_defaults() {
        let config =
            parse_config("[[agents]]\nname = \"Test\"\ncommand = \"echo idema-ok\"\n").unwrap();
        assert_eq!(config.ui_font_size, 20.);
        assert!(config.sound_on_wait);
        assert_eq!(config.default_agent().unwrap().name, "Test");
    }

    fn agent(name: &str, harness: Option<&str>) -> AgentEntry {
        AgentEntry {
            name: name.into(),
            harness: harness.map(str::to_owned),
            command: format!("run {name}"),
            default: false,
            icon: None,
        }
    }

    /// Les groupes en clair : le harness et les noms de ses entrées.
    fn labels(agents: &[AgentEntry]) -> Vec<(Option<String>, Vec<&str>)> {
        harness_groups(agents)
            .into_iter()
            .map(|group| {
                let names = group
                    .agents
                    .iter()
                    .map(|&index| agents[index].name.as_str())
                    .collect();
                (group.harness, names)
            })
            .collect()
    }

    #[test]
    fn the_harness_key_is_read_and_stays_optional() {
        let config = parse_config(
            "[[agents]]\nname = \"Opus\"\nharness = \"Claude Code\"\ncommand = \"c\"\n\n[[agents]]\nname = \"Solo\"\ncommand = \"s\"\n",
        )
        .unwrap();
        assert_eq!(config.agents[0].harness_name(), Some("Claude Code"));
        assert_eq!(config.agents[1].harness_name(), None);
    }

    #[test]
    fn agents_group_by_harness_in_alphabetical_order() {
        let agents = [
            agent("Sonnet", Some("Claude Code")),
            agent("Gemini Flash", Some("Antigravity")),
            agent("deepseek", Some("Claude Code")),
            agent("Élan", Some("Claude Code")),
            agent("Gemini Argon", Some("Antigravity")),
        ];
        assert_eq!(
            labels(&agents),
            [
                (
                    Some("Antigravity".to_owned()),
                    vec!["Gemini Argon", "Gemini Flash"]
                ),
                (
                    Some("Claude Code".to_owned()),
                    vec!["deepseek", "Élan", "Sonnet"]
                ),
            ]
        );
    }

    #[test]
    fn agents_without_a_harness_come_last_and_a_blank_harness_is_none() {
        let agents = [
            agent("Z", None),
            agent("A", Some("   ")),
            agent("B", Some("  zed  ")),
        ];
        assert_eq!(
            labels(&agents),
            [(Some("zed".to_owned()), vec!["B"]), (None, vec!["A", "Z"]),]
        );
    }

    #[test]
    fn homonyms_stay_apart_across_harnesses() {
        let agents = [
            agent("Deepseek", Some("Opencode")),
            agent("Deepseek", Some("Claude Code")),
        ];
        let groups = harness_groups(&agents);
        assert_eq!(groups[0].agents, [1]);
        assert_eq!(groups[1].agents, [0]);
    }

    #[test]
    fn the_default_config_gives_every_agent_a_harness() {
        assert!(
            Config::default()
                .agents
                .iter()
                .all(|agent| agent.harness_name().is_some())
        );
    }

    #[test]
    fn a_control_character_in_a_harness_is_refused() {
        let error =
            parse_config("[[agents]]\nname = \"A\"\nharness = \"x\\ny\"\ncommand = \"c\"\n")
                .unwrap_err();
        assert_eq!(error.kind, ConfigErrorKind::Invalid);
    }

    #[test]
    fn a_syntax_error_names_its_line() {
        let error = parse_config("ui_font_size = 20\nsound_on_wait = peut-être\n").unwrap_err();
        assert_eq!(error.line, Some(2));
        assert!(error.to_string().starts_with("config.toml, ligne 2 : "));
    }

    #[test]
    fn an_unknown_key_is_refused_with_its_line() {
        let error = parse_config("ui_font_size = 20\nsound_on_wiat = false\n").unwrap_err();
        assert_eq!(error.line, Some(2));
    }

    #[test]
    fn semantic_errors_are_refused() {
        assert!(parse_config("ui_font_size = 400\n").is_err());
        assert!(parse_config("[[agents]]\nname = \"A\"\ncommand = \"\"\n").is_err());
        assert!(parse_config("[[agents]]\nname = \"A\"\ncommand = \"a\\nb\"\n").is_err());
        assert!(
            parse_config(
                "[[agents]]\nname = \"A\"\ncommand = \"a\"\ndefault = true\n[[agents]]\nname = \"B\"\ncommand = \"b\"\ndefault = true\n"
            )
            .is_err()
        );
        assert!(parse_config("[[agents]]\nname = \"Shell seul\"\ncommand = \"zsh\"\n").is_err());
    }

    #[test]
    fn reload_effects_name_only_what_changed() {
        let base = Config::default();
        let none = ReloadEffects {
            theme: false,
            terminal_font: false,
        };
        assert_eq!(base.reload_effects(&base), none);

        let mut ui = base.clone();
        ui.ui_font_size = 16.;
        assert_eq!(
            ui.reload_effects(&base),
            ReloadEffects {
                theme: true,
                ..none
            }
        );

        for edit in [
            |c: &mut Config| c.terminal_font_family = "Menlo".into(),
            |c: &mut Config| c.terminal_font_size = 18.,
            |c: &mut Config| c.terminal_line_height = 1.3,
        ] {
            let mut config = base.clone();
            edit(&mut config);
            assert_eq!(
                config.reload_effects(&base),
                ReloadEffects {
                    terminal_font: true,
                    ..none
                }
            );
        }

        // Les agents, le son et `tmux_path` ne demandent aucun rafraîchissement global.
        let mut other = base.clone();
        other.sound_on_wait = !base.sound_on_wait;
        other.tmux_path = Some("/opt/homebrew/bin/tmux".into());
        other.agents.clear();
        assert_eq!(other.reload_effects(&base), none);
    }

    #[test]
    fn without_agents_there_is_no_default_agent() {
        assert!(parse_config("").unwrap().default_agent().is_none());
    }

    #[test]
    fn ensure_config_file_writes_once_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("idema-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);
        ensure_config_file(&path).unwrap();
        assert_eq!(load_config(&path).unwrap(), Config::default());
        std::fs::write(&path, "sound_on_wait = false\n").unwrap();
        ensure_config_file(&path).unwrap();
        assert!(!load_config(&path).unwrap().sound_on_wait);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_watch_reports_each_change_once() {
        let dir = std::env::temp_dir().join(format!("idema-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);
        let mut watch = FileWatch::new(path.clone());
        assert!(!watch.changed());
        std::fs::write(&path, "a = 1\n").unwrap();
        assert!(watch.changed());
        assert!(!watch.changed());
        std::fs::write(&path, "a = 12\n").unwrap();
        assert!(watch.changed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_message_is_off_without_its_section() {
        assert!(parse_config("").unwrap().commit_message.is_none());
    }

    #[test]
    fn commit_message_settings_take_their_defaults() {
        let config =
            parse_config("[commit_message]\nenabled = true\napi_key = \"fake-key-for-tests\"\n")
                .unwrap();
        let section = config.commit_message.unwrap();
        assert!(section.enabled);
        assert_eq!(section.model, "gemini-3.5-flash-lite");
        assert!(section.include_project_rules);
        assert_eq!(section.instructions, None);
        assert_eq!(section.endpoint, None);
    }

    #[test]
    fn the_default_commit_message_section_is_the_one_an_empty_table_parses_to() {
        let parsed = parse_config("[commit_message]\n")
            .unwrap()
            .commit_message
            .unwrap();
        assert_eq!(CommitMessageConfig::default(), parsed);
    }

    #[test]
    fn an_unknown_commit_message_key_is_refused_with_its_line() {
        let error = parse_config("[commit_message]\nenabled = true\nmodle = \"x\"\n").unwrap_err();
        assert_eq!(error.line, Some(3));
    }

    #[test]
    fn the_api_key_never_shows_in_debug() {
        let config = parse_config("[commit_message]\napi_key = \"fake-key-for-tests\"\n").unwrap();
        assert!(!format!("{config:?}").contains("fake-key-for-tests"));
    }

    /// Chaque cas fait recopier une valeur par `toml` dans son message, sans que la ligne `api_key = "..."` du texte
    /// suffise à la retrouver : la clé collée dans un autre champ, les écritures pointées, la table en ligne, la
    /// clé entre guillemets, et une valeur non textuelle (le nombre témoin 123456789).
    #[test]
    fn a_config_error_never_shows_the_api_key() {
        let mut leaks = Vec::new();
        for (text, secret) in [
            (
                "[commit_message]\nenabled = \"fake-key-for-tests\"\n",
                "fake-key-for-tests",
            ),
            (
                "[commit_message]\nenabled = 'fake-key-for-tests'\n",
                "fake-key-for-tests",
            ),
            (
                "commit_message.api_key = \"fake-key-for-tests\"\ncommit_message.enabled = \"fake-key-for-tests\"\n",
                "fake-key-for-tests",
            ),
            (
                "commit_message = { api_key = \"fake-key-for-tests\", enabled = \"fake-key-for-tests\" }\n",
                "fake-key-for-tests",
            ),
            (
                "[commit_message]\n\"api_key\" = \"fake-key-for-tests\"\nenabled = \"fake-key-for-tests\"\n",
                "fake-key-for-tests",
            ),
            ("[commit_message]\napi_key = 123456789\n", "123456789"),
            (
                "[commit_message]\napi_key = \"fake-key-for-tests\"\nenabled = peut-être\n",
                "fake-key-for-tests",
            ),
        ] {
            let error = parse_config(text).unwrap_err();
            let explained = crate::errors::explain_config(&error);
            let shown = [
                error.to_string(),
                explained.text,
                explained.detail.unwrap_or_default(),
            ];
            if shown.iter().any(|line| line.contains(secret)) {
                leaks.push(format!("{text:?} -> {error}"));
            }
        }
        assert!(leaks.is_empty(), "leaks: {leaks:#?}");
    }

    /// Le masquage ne doit pas faire retomber `explain_config` sur le texte générique.
    #[test]
    fn a_masked_config_error_is_still_explained_from_the_catalogue() {
        let unknown = parse_config("[commit_message]\nmodle = \"x\"\n").unwrap_err();
        assert_eq!(
            crate::errors::explain_config(&unknown).text,
            "Ligne 2 : clé inconnue."
        );
        let wrong_type =
            parse_config("[commit_message]\nenabled = \"fake-key-for-tests\"\n").unwrap_err();
        assert_eq!(
            crate::errors::explain_config(&wrong_type).text,
            "Ligne 2 : valeur du mauvais type."
        );
    }

    #[test]
    fn browser_favorites_are_read_in_order() {
        let config = parse_config("[[browser.favorites]]\nname = \"Vercel\"\nurl = \"https://vercel.com/\"\n[[browser.favorites]]\nname = \"Local\"\nurl = \"http://localhost:8812/\"\n").unwrap();
        let names: Vec<_> = config
            .browser
            .unwrap()
            .favorites
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["Vercel", "Local"]);
    }

    #[test]
    fn the_browser_section_is_optional() {
        assert!(parse_config("").unwrap().browser.is_none());
    }

    #[test]
    fn an_unknown_favorite_key_is_refused_with_its_line() {
        let error = parse_config(
            "[[browser.favorites]]\nname = \"A\"\nurl = \"https://a.dev/\"\nicon = \"x\"\n",
        )
        .unwrap_err();
        assert_eq!(error.line, Some(4));
    }

    #[test]
    fn a_favorite_needs_a_name_and_a_web_or_file_url() {
        assert!(
            parse_config("[[browser.favorites]]\nname = \"\"\nurl = \"https://a.dev/\"\n").is_err()
        );
        assert!(
            parse_config("[[browser.favorites]]\nname = \"A\"\nurl = \"javascript:alert(1)\"\n")
                .is_err()
        );
        assert!(
            parse_config("[[browser.favorites]]\nname = \"A\"\nurl = \"file:///tmp/r.pdf\"\n")
                .is_ok()
        );
    }

    #[test]
    fn a_refused_favorite_points_at_its_own_entry() {
        let error = parse_config("[[browser.favorites]]\nname = \"A\"\nurl = \"https://a.dev/\"\n\n[[browser.favorites]]\nname = \" \"\nurl = \"https://b.dev/\"\n").unwrap_err();
        assert_eq!(error.kind, ConfigErrorKind::Invalid);
        assert_eq!(error.line, Some(5));
        assert_eq!(error.message, "favori n° 2 sans nom");
        let bad_url = parse_config("[[browser.favorites]]\nname = \"A\"\nurl = \"ftp://a.dev/\"\n")
            .unwrap_err();
        assert_eq!(bad_url.line, Some(1));
        assert_eq!(
            bad_url.message,
            "adresse de favori invalide (favori n° 1 « A ») : ftp://a.dev/"
        );
    }

    #[test]
    fn a_refused_favorite_names_its_position_and_name_on_screen() {
        let unnamed = parse_config("[[browser.favorites]]\nname = \"A\"\nurl = \"https://a.dev/\"\n[[browser.favorites]]\nname = \"\"\nurl = \"https://b.dev/\"\n").unwrap_err();
        assert_eq!(unnamed.message, "favori n° 2 sans nom");
        let bad_url = parse_config("[[browser.favorites]]\nname = \"A\"\nurl = \"https://a.dev/\"\n[[browser.favorites]]\nname = \"B\"\nurl = \"https://b.dev/\"\n[[browser.favorites]]\nname = \"Vercel\"\nurl = \"ftp://v.dev/\"\n").unwrap_err();
        assert_eq!(
            bad_url.message,
            "adresse de favori invalide (favori n° 3 « Vercel ») : ftp://v.dev/"
        );
        assert_eq!(
            crate::errors::explain_config(&bad_url).text,
            bad_url.message
        );
    }

    #[test]
    fn favorite_headers_with_a_comment_or_spaces_are_still_counted() {
        let commented = parse_config("[[browser.favorites]] # perso\nname = \"A\"\nurl = \"https://a.dev/\"\n\n[[browser.favorites]]\nname = \"\"\nurl = \"https://b.dev/\"\n").unwrap_err();
        assert_eq!(commented.line, Some(5));
        assert_eq!(commented.message, "favori n° 2 sans nom");
        let spaced = parse_config("[[ browser.favorites ]]\nname = \"A\"\nurl = \"https://a.dev/\"\n[[ browser . favorites ]]   # x\nname = \"B\"\nurl = \"nope\"\n").unwrap_err();
        assert_eq!(spaced.line, Some(4));
        // Une ligne qui n'est qu'un commentaire ou une valeur de chaîne n'est pas un en-tête.
        let decoy = parse_config("# [[browser.favorites]]\n[[browser.favorites]]\nname = \"[[browser.favorites]]\"\nurl = \"nope\"\n").unwrap_err();
        assert_eq!(decoy.line, Some(2));
    }

    #[test]
    fn ideation_is_off_without_its_section() {
        assert_eq!(parse_config(DEFAULT_CONFIG).unwrap().ideation, None);
        assert!(DEFAULT_CONFIG.contains("# [ideation]"));
    }

    #[test]
    fn romain_ideation_section_parses() {
        let text = "[ideation]\ndir = \"/Users/Shared/erom-ideation\"\nmodel = \"fable\"\neffort = \"max\"\nlabel = \"Fable\"\n";
        let section = parse_config(text).unwrap().ideation.unwrap();
        assert_eq!(section.dir, "/Users/Shared/erom-ideation");
        assert_eq!(section.model.as_deref(), Some("fable"));
        assert_eq!(section.effort.as_deref(), Some("max"));
        assert_eq!(section.label, "Fable");
    }

    #[test]
    fn ideation_label_defaults_to_claude() {
        let section = parse_config("[ideation]\ndir = \"/x\"\n")
            .unwrap()
            .ideation
            .unwrap();
        assert_eq!(section.label, "Claude");
        assert_eq!(section.model, None);
        assert_eq!(section.effort, None);
    }

    #[test]
    fn ideation_refusals() {
        let cases = [
            ("dir = \"rel/x\"", ConfigErrorKind::Invalid),
            ("dir = \"\"", ConfigErrorKind::Invalid),
            ("dir = \"  \"", ConfigErrorKind::Invalid),
            ("dir = \"/x\"\nmodel = \"\"", ConfigErrorKind::Invalid),
            (
                "dir = \"/x\"\nmodel = \"a\\u0007b\"",
                ConfigErrorKind::Invalid,
            ),
            ("dir = \"/x\"\neffort = \"\\n\"", ConfigErrorKind::Invalid),
            ("dir = \"/x\"\nlabel = \"\"", ConfigErrorKind::Invalid),
            ("dir = \"/x\"\ncolour = \"x\"", ConfigErrorKind::Syntax),
        ];
        for (body, kind) in cases {
            let error = parse_config(&format!("[ideation]\n{body}\n")).unwrap_err();
            assert_eq!(error.kind, kind, "{body}: {error}");
        }
    }

    #[test]
    fn ideation_dir_expands_the_home() {
        let home = std::env::var("HOME").expect("HOME is set");
        let dir_path = |dir: &str| {
            parse_config(&format!("[ideation]\ndir = \"{dir}\"\n"))
                .unwrap()
                .ideation
                .unwrap()
                .dir_path()
        };
        assert_eq!(dir_path("~/idees"), PathBuf::from(&home).join("idees"));
        assert_eq!(dir_path("~"), PathBuf::from(&home));
        assert_eq!(dir_path("/abs"), PathBuf::from("/abs"));
    }

    #[test]
    fn browser_settings_are_empty_without_the_section() {
        let settings = crate::browser::BrowserSettings::from_config(&parse_config("").unwrap());
        assert!(settings.0.is_empty());
        let config =
            parse_config("[[browser.favorites]]\nname = \"A\"\nurl = \"https://a.dev/\"\n")
                .unwrap();
        assert_eq!(
            crate::browser::BrowserSettings::from_config(&config)
                .0
                .len(),
            1
        );
    }

    #[test]
    fn companion_section_absent_means_off() {
        assert_eq!(parse_config("").unwrap().companion, None);
    }

    #[test]
    fn companion_section_takes_its_defaults() {
        let section = parse_config("[companion]\n").unwrap().companion.unwrap();
        assert_eq!(section.port, 3910);
        assert!(section.notify_waiting);
        assert!(section.notify_error);
        assert_eq!(section.away_after_seconds, 120);
        assert_eq!(section.public_url, None);
    }

    #[test]
    fn companion_section_rejects_unknown_keys() {
        assert!(parse_config("[companion]\nfoo = 1\n").is_err());
    }

    #[test]
    fn companion_section_reads_every_field() {
        let text = "[companion]\nport = 4000\npublic_url = \"https://mac.tailnet.ts.net:8455\"\nnotify_waiting = false\nnotify_error = false\naway_after_seconds = 0\n";
        let section = parse_config(text).unwrap().companion.unwrap();
        assert_eq!(section.port, 4000);
        assert_eq!(
            section.public_url.as_deref(),
            Some("https://mac.tailnet.ts.net:8455")
        );
        assert!(!section.notify_waiting);
        assert!(!section.notify_error);
        assert_eq!(section.away_after_seconds, 0);
    }

    #[test]
    fn companion_port_zero_is_refused() {
        let error = parse_config("[companion]\nport = 0\n").unwrap_err();
        assert_eq!(error.kind, ConfigErrorKind::Invalid, "{error}");
    }

    #[test]
    fn companion_public_url_must_be_a_valid_https_address() {
        for bad in [
            "",
            "not a url",
            "mac.tailnet.ts.net:8455",
            "http://mac.tailnet.ts.net:8455",
            "ftp://mac.tailnet.ts.net",
            "https://",
        ] {
            let error =
                parse_config(&format!("[companion]\npublic_url = {bad:?}\n")).expect_err(bad);
            assert_eq!(error.kind, ConfigErrorKind::Invalid, "{bad}: {error}");
            assert!(error.to_string().contains("public_url"), "{bad}: {error}");
        }
        for good in [
            "https://mac.tailnet.ts.net",
            "https://mac.tailnet.ts.net:8455",
            "https://mac.tailnet.ts.net:8455/",
        ] {
            let parsed = parse_config(&format!("[companion]\npublic_url = {good:?}\n"));
            assert!(parsed.is_ok(), "{good}");
        }
        assert!(parse_config("[companion]\n").is_ok(), "no URL stays valid");
    }

    #[test]
    fn companion_changes_reload_without_any_global_refresh() {
        let base = Config::default();
        let mut other = base.clone();
        other.companion = parse_config("[companion]\n").unwrap().companion;
        assert_ne!(other, base);
        assert_eq!(
            other.reload_effects(&base),
            ReloadEffects {
                theme: false,
                terminal_font: false
            }
        );
    }

    const MACHINES_EXAMPLE: &str = r#"[machines]
local_label = "Localhost"

[[machines.remote]]
label = "VPS Hostinger"
connection = "caserne-claude"

[[machines.remote]]
label = "Dell"
connection = "dell-lan"
"#;

    #[test]
    fn machines_section_reads_the_spec_example() {
        let config = parse_config(MACHINES_EXAMPLE).unwrap();
        let machines = config.machines.as_ref().unwrap();
        assert_eq!(machines.local_label.as_deref(), Some("Localhost"));
        assert_eq!(
            config.remote_machines(),
            [
                RemoteMachineEntry {
                    label: "VPS Hostinger".into(),
                    connection: "caserne-claude".into()
                },
                RemoteMachineEntry {
                    label: "Dell".into(),
                    connection: "dell-lan".into()
                },
            ]
        );
    }

    #[test]
    fn an_absent_machines_section_means_localhost_alone() {
        let config = parse_config("").unwrap();
        assert_eq!(config.local_machine_label(), "Localhost");
        assert!(config.remote_machines().is_empty());
        let renamed = parse_config("[machines]\nlocal_label = \"Mon Mac\"\n").unwrap();
        assert_eq!(renamed.local_machine_label(), "Mon Mac");
    }

    #[test]
    fn an_unknown_machine_key_is_refused() {
        let entry = "[[machines.remote]]\nlabel = \"Dell\"\nconnection = \"dell-lan\"\nport = 22\n";
        assert!(parse_config(entry).is_err());
        assert!(parse_config("[machines]\nport = 22\n").is_err());
    }

    #[test]
    fn connection_problems_are_named() {
        assert_eq!(connection_problem("", &[]), Some("La connexion est vide."));
        let blank = Some("La connexion ne peut contenir ni espace ni caractère de contrôle.");
        assert_eq!(connection_problem("a b", &[]), blank);
        assert_eq!(connection_problem("a\tb", &[]), blank);
        assert_eq!(
            connection_problem("-oProxyCommand=x", &[]),
            Some("La connexion ne peut pas commencer par un tiret.")
        );
        assert_eq!(
            connection_problem("local", &[]),
            Some("Ce nom est réservé à la machine locale.")
        );
        assert_eq!(connection_problem("locale", &[]), None);
        assert_eq!(
            connection_problem("dell-lan", &["dell-lan"]),
            Some("Cette connexion est déjà celle d'une autre machine.")
        );
        for fine in ["caserne-claude", "llm@192.168.1.20", "ssh://u@h:2222"] {
            assert_eq!(connection_problem(fine, &["dell-lan"]), None, "{fine}");
        }
        assert_eq!(label_problem("  "), Some("Le label est vide."));
        assert_eq!(label_problem("Dell"), None);
    }

    #[test]
    fn a_refused_machine_is_located_on_its_own_line() {
        let text = "[machines]\n\n[[machines.remote]]\nlabel = \"A\"\nconnection = \"same\"\n\n[[machines.remote]]\nlabel = \"B\"\nconnection = \"same\"\n";
        let error = parse_config(text).unwrap_err();
        assert_eq!(error.kind, ConfigErrorKind::Invalid);
        assert_eq!(error.line, Some(9));
        assert!(error.message.contains("déjà celle d'une autre machine"));
        let dashed = "[[machines.remote]]\nlabel = \"A\"\nconnection = \"-oX\"\n";
        assert_eq!(parse_config(dashed).unwrap_err().line, Some(3));
        let reserved = "[[machines.remote]]\nlabel = \"A\"\nconnection = \"local\"\n";
        assert_eq!(parse_config(reserved).unwrap_err().line, Some(3));
        let unlabeled = "[[machines.remote]]\nlabel = \" \"\nconnection = \"ok\"\n";
        assert_eq!(parse_config(unlabeled).unwrap_err().line, Some(2));
    }
}
