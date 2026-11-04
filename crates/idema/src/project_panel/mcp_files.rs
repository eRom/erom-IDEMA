//! Lecture et écriture des serveurs MCP du projet dans les cinq fichiers des harnais.
//!
//! Chaque fichier est traduit vers une forme commune, celle de `.mcp.json` (un `serde_json::Value`) :
//! stdio `{"type":"stdio","command","args","env"}`, HTTP `{"type":"http","url","headers"}`. Module pur : aucun accès
//! au disque, le texte entre et le texte sort. Seule la section MCP d'un fichier change à l'écriture ; le reste
//! (clés, ordre, commentaires TOML) est conservé.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item, Table, TableLike, TomlError};

/// Les cinq harnais de codage dont le volet Projet lit la config MCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Harness {
    Claude,
    Codex,
    Grok,
    OpenCode,
    Antigravity,
}

impl Harness {
    pub const ALL: [Harness; 5] = [
        Harness::Claude,
        Harness::Codex,
        Harness::Grok,
        Harness::OpenCode,
        Harness::Antigravity,
    ];

    /// Libellé court de la puce.
    pub fn chip(self) -> &'static str {
        match self {
            Harness::Claude => "CLAUDE",
            Harness::Codex => "CODEX",
            Harness::Grok => "GROK",
            Harness::OpenCode => "OPENCODE",
            Harness::Antigravity => "AGY",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Harness::Claude => "Claude",
            Harness::Codex => "Codex",
            Harness::Grok => "Grok",
            Harness::OpenCode => "OpenCode",
            Harness::Antigravity => "Antigravity",
        }
    }

    /// Chemin du fichier, relatif à la racine du projet.
    pub fn mcp_file(self) -> &'static str {
        match self {
            Harness::Claude => ".mcp.json",
            Harness::Codex => ".codex/config.toml",
            Harness::Grok => ".grok/config.toml",
            Harness::OpenCode => "opencode.json",
            Harness::Antigravity => ".agents/mcp_config.json",
        }
    }

    fn is_toml(self) -> bool {
        matches!(self, Harness::Codex | Harness::Grok)
    }
}

/// Un serveur lu dans un fichier, traduit vers la forme commune.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadServer {
    /// Forme commune. `type` est toujours posé quand il se déduit (`command` : stdio, `url` : http) ; les autres
    /// clés ne figurent que si le fichier les porte (voir `normalize` pour les compléter).
    pub core: Value,
    /// Clés du fichier hors forme commune (`startup_timeout_sec`, `oauth`, `cwd`...), drapeau de désactivation exclu.
    pub extras: Vec<String>,
    /// `disabled: true` (OpenCode v2, Antigravity), `enabled = false` (Codex, Grok, OpenCode ancienne forme).
    pub disabled: bool,
    /// Faux pour un type que la forme commune ne porte pas (`sse`) ou une définition inexploitable.
    pub supported: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerChange {
    Put { name: String, core: Value },
    Remove { name: String },
}

/// Complète la forme commune : `type` déduit s'il manque (`command` : stdio, `url` : http), `args` et `env` (stdio)
/// ou `headers` (http) vides s'ils manquent. Sert à comparer deux définitions.
pub fn normalize(core: &Value) -> Value {
    let Some(object) = core.as_object() else {
        return core.clone();
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| infer_type(object).map(str::to_string));
    let mut out = Map::new();
    if let Some(kind) = &kind {
        out.insert("type".to_string(), Value::String(kind.clone()));
    }
    for (key, value) in object {
        if key != "type" {
            out.insert(key.clone(), value.clone());
        }
    }
    match kind.as_deref() {
        Some("stdio") => {
            out.entry("args")
                .or_insert_with(|| Value::Array(Vec::new()));
            out.entry("env")
                .or_insert_with(|| Value::Object(Map::new()));
        }
        Some("http") => {
            out.entry("headers")
                .or_insert_with(|| Value::Object(Map::new()));
        }
        _ => {}
    }
    Value::Object(out)
}

fn infer_type(object: &Map<String, Value>) -> Option<&'static str> {
    if object.contains_key("command") {
        Some("stdio")
    } else if object.contains_key("url") {
        Some("http")
    } else {
        None
    }
}

/// Un cœur validé : ce que les écrivains savent traduire.
enum Transport {
    Stdio {
        command: String,
        args: Option<Vec<String>>,
        env: Option<Vec<(String, String)>>,
    },
    Http {
        url: String,
        headers: Option<Vec<(String, String)>>,
    },
}

fn parse_core(core: &Value) -> Result<Transport, String> {
    let object = core
        .as_object()
        .ok_or("la définition du serveur doit être un objet JSON")?;
    let kind = match object.get("type") {
        Some(value) => value
            .as_str()
            .ok_or("`type` doit être une chaîne")?
            .to_string(),
        None => infer_type(object)
            .ok_or("ni `command` ni `url` : type de serveur introuvable")?
            .to_string(),
    };
    match kind.as_str() {
        "stdio" => Ok(Transport::Stdio {
            command: text_field(object, "command")?.ok_or("`command` est obligatoire en stdio")?,
            args: list_field(object, "args")?,
            env: table_field(object, "env")?,
        }),
        "http" => Ok(Transport::Http {
            url: text_field(object, "url")?.ok_or("`url` est obligatoire en http")?,
            headers: table_field(object, "headers")?,
        }),
        other => Err(format!("type `{other}` non pris en charge")),
    }
}

/// Vérifie qu'un bloc saisi est un serveur que les écrivains savent traduire : `parse_core`, et un `command` (stdio) ou
/// un `url` (http) qui n'est pas vide une fois les espaces ôtés. Un seul message, celui du premier défaut. Rend le bloc
/// à écrire, inchangé.
pub fn validate_core(core: &Value) -> Result<Value, String> {
    match parse_core(core)? {
        Transport::Stdio { command, .. } if command.trim().is_empty() => {
            Err("`command` ne peut pas être vide en stdio".to_string())
        }
        Transport::Http { url, .. } if url.trim().is_empty() => {
            Err("`url` ne peut pas être vide en http".to_string())
        }
        _ => Ok(core.clone()),
    }
}

fn text_field(object: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("`{key}` doit être une chaîne")),
    }
}

fn list_field(object: &Map<String, Value>, key: &str) -> Result<Option<Vec<String>>, String> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or_else(|| format!("`{key}` doit être une liste de chaînes")),
        Some(_) => Err(format!("`{key}` doit être une liste de chaînes")),
    }
}

fn table_field(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<(String, String)>>, String> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::Object(entries)) => entries
            .iter()
            .map(|(name, value)| value.as_str().map(|text| (name.clone(), text.to_string())))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or_else(|| format!("les valeurs de `{key}` doivent être des chaînes")),
        Some(_) => Err(format!("`{key}` doit être un objet de chaînes")),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Lecture

pub fn read_servers(harness: Harness, text: &str) -> Result<BTreeMap<String, ReadServer>, String> {
    if harness.is_toml() {
        read_toml(harness, text)
    } else {
        read_json(harness, text)
    }
}

/// Façon dont un format plat (tout sauf OpenCode) nomme les clés du cœur.
struct FlatDialect {
    /// Clés qui portent l'URL, par ordre de priorité.
    url_keys: &'static [&'static str],
    headers_key: &'static str,
    /// `type` fait partie de la définition (Claude) ou est une clé propre au harnais (les autres).
    type_is_core: bool,
    flag: Flag,
}

enum Flag {
    None,
    /// `disabled: true` désactive.
    Disabled,
    /// `enabled = false` désactive.
    Enabled,
}

fn dialect(harness: Harness) -> FlatDialect {
    match harness {
        Harness::Claude => FlatDialect {
            url_keys: &["url"],
            headers_key: "headers",
            type_is_core: true,
            flag: Flag::None,
        },
        Harness::Antigravity => FlatDialect {
            url_keys: &["serverUrl", "url"],
            headers_key: "headers",
            type_is_core: false,
            flag: Flag::Disabled,
        },
        Harness::Codex => FlatDialect {
            url_keys: &["url"],
            headers_key: "http_headers",
            type_is_core: false,
            flag: Flag::Enabled,
        },
        // OpenCode a son propre lecteur : ce dialecte n'est jamais demandé pour lui.
        Harness::Grok | Harness::OpenCode => FlatDialect {
            url_keys: &["url"],
            headers_key: "headers",
            type_is_core: false,
            flag: Flag::Enabled,
        },
    }
}

fn read_json(harness: Harness, text: &str) -> Result<BTreeMap<String, ReadServer>, String> {
    let root = parse_json_root(text)?;
    let mut servers = BTreeMap::new();
    if harness == Harness::OpenCode {
        read_opencode(&root, &mut servers)?;
        return Ok(servers);
    }
    let Some(section) = root.get("mcpServers") else {
        return Ok(servers);
    };
    let section = section
        .as_object()
        .ok_or("`mcpServers` doit être un objet")?;
    let dialect = dialect(harness);
    for (name, entry) in section {
        let entry = entry
            .as_object()
            .ok_or_else(|| format!("`mcpServers.{name}` doit être un objet"))?;
        servers.insert(name.clone(), translate_flat(entry, &dialect));
    }
    Ok(servers)
}

fn parse_json_root(text: &str) -> Result<Map<String, Value>, String> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    // Les messages de serde_json donnent la ligne et la colonne, jamais le contenu de la ligne fautive.
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(root)) => Ok(root),
        Ok(_) => Err("JSON invalide : la racine doit être un objet".to_string()),
        Err(error) => Err(format!("JSON invalide : {error}")),
    }
}

fn translate_flat(entry: &Map<String, Value>, dialect: &FlatDialect) -> ReadServer {
    let url_key = dialect
        .url_keys
        .iter()
        .find(|key| entry.contains_key(**key))
        .copied();
    let mut body = Map::new();
    let mut extras = Vec::new();
    let mut disabled = false;
    for (key, value) in entry {
        match key.as_str() {
            "command" | "args" | "env" => {
                body.insert(key.clone(), value.clone());
            }
            "type" if dialect.type_is_core => {
                body.insert(key.clone(), value.clone());
            }
            _ if Some(key.as_str()) == url_key => {
                body.insert("url".to_string(), value.clone());
            }
            _ if key == dialect.headers_key => {
                body.insert("headers".to_string(), value.clone());
            }
            "disabled" if matches!(dialect.flag, Flag::Disabled) => {
                disabled = value.as_bool() == Some(true);
            }
            "enabled" if matches!(dialect.flag, Flag::Enabled) => {
                disabled = value.as_bool() == Some(false);
            }
            // Une variante d'URL écartée (`url` quand `serverUrl` est là) n'est pas une clé « en plus ».
            _ if dialect.url_keys.contains(&key.as_str()) => {}
            _ => extras.push(key.clone()),
        }
    }
    finish_read(body, extras, disabled)
}

/// Pose `type` en tête quand il se déduit, puis juge si la forme commune porte la définition.
fn finish_read(mut body: Map<String, Value>, extras: Vec<String>, disabled: bool) -> ReadServer {
    let core = if body.contains_key("type") {
        body
    } else {
        let mut core = Map::new();
        if let Some(kind) = infer_type(&body) {
            core.insert("type".to_string(), Value::String(kind.to_string()));
        }
        core.append(&mut body);
        core
    };
    let core = Value::Object(core);
    let supported = parse_core(&core).is_ok();
    ReadServer {
        core,
        extras,
        disabled,
        supported,
    }
}

/// OpenCode : forme v2 `mcp.servers.<nom>` (drapeau `disabled`) et ancienne forme `mcp.<nom>` (drapeau `enabled`).
/// À nom égal, la forme v2 l'emporte.
fn read_opencode(
    root: &Map<String, Value>,
    servers: &mut BTreeMap<String, ReadServer>,
) -> Result<(), String> {
    let Some(mcp) = root.get("mcp") else {
        return Ok(());
    };
    let mcp = mcp.as_object().ok_or("`mcp` doit être un objet")?;
    if let Some(section) = mcp.get("servers") {
        let section = section
            .as_object()
            .ok_or("`mcp.servers` doit être un objet")?;
        for (name, entry) in section {
            let entry = entry
                .as_object()
                .ok_or_else(|| format!("`mcp.servers.{name}` doit être un objet"))?;
            servers.insert(name.clone(), translate_opencode(entry, false));
        }
    }
    for (name, entry) in mcp {
        if name == "servers" || servers.contains_key(name) {
            continue;
        }
        // Une clé de `mcp` qui n'est pas une définition de serveur (pas de `type`) n'est pas à nous.
        if let Some(entry) = entry.as_object().filter(|entry| entry.contains_key("type")) {
            servers.insert(name.clone(), translate_opencode(entry, true));
        }
    }
    Ok(())
}

fn translate_opencode(entry: &Map<String, Value>, legacy: bool) -> ReadServer {
    let flag_key = if legacy { "enabled" } else { "disabled" };
    let kind = entry.get("type").and_then(Value::as_str);
    let (core_keys, core_type): (&[&str], Option<&str>) = match kind {
        Some("local") => (&["command", "environment"], Some("stdio")),
        Some("remote") => (&["url", "headers"], Some("http")),
        _ => (&[], None),
    };
    let mut body = Map::new();
    body.insert(
        "type".to_string(),
        match core_type {
            Some(core_type) => Value::String(core_type.to_string()),
            None => entry.get("type").cloned().unwrap_or(Value::Null),
        },
    );
    let mut extras = Vec::new();
    let mut disabled = false;
    for (key, value) in entry {
        if key == "type" {
            continue;
        }
        if key == flag_key {
            let flag = value.as_bool();
            disabled = if legacy {
                flag == Some(false)
            } else {
                flag == Some(true)
            };
        } else if core_keys.contains(&key.as_str()) {
            match key.as_str() {
                "command" => split_command(value, &mut body),
                "environment" => {
                    body.insert("env".to_string(), value.clone());
                }
                _ => {
                    body.insert(key.clone(), value.clone());
                }
            }
        } else {
            extras.push(key.clone());
        }
    }
    let core = Value::Object(body);
    let supported = core_type.is_some() && parse_core(&core).is_ok();
    ReadServer {
        core,
        extras,
        disabled,
        supported,
    }
}

/// `command` d'OpenCode est un tableau `[programme, argument...]`. Une valeur inexploitable passe telle quelle.
fn split_command(value: &Value, body: &mut Map<String, Value>) {
    let parts: Option<Vec<&str>> = value
        .as_array()
        .and_then(|items| items.iter().map(Value::as_str).collect());
    match parts.as_deref() {
        Some([program, rest @ ..]) => {
            body.insert("command".to_string(), Value::String((*program).to_string()));
            if !rest.is_empty() {
                body.insert(
                    "args".to_string(),
                    Value::Array(
                        rest.iter()
                            .map(|argument| Value::String((*argument).to_string()))
                            .collect(),
                    ),
                );
            }
        }
        _ => {
            body.insert("command".to_string(), value.clone());
        }
    }
}

fn read_toml(harness: Harness, text: &str) -> Result<BTreeMap<String, ReadServer>, String> {
    let document = parse_toml(text)?;
    let mut servers = BTreeMap::new();
    let Some(section) = document.get("mcp_servers") else {
        return Ok(servers);
    };
    let section = section
        .as_table_like()
        .ok_or("`mcp_servers` doit être une table")?;
    let dialect = dialect(harness);
    for (name, entry) in section.iter() {
        let entry = entry
            .as_table_like()
            .ok_or_else(|| format!("`mcp_servers.{name}` doit être une table"))?;
        let object: Map<String, Value> = entry
            .iter()
            .map(|(key, item)| (key.to_string(), item_to_json(item)))
            .collect();
        servers.insert(name.to_string(), translate_flat(&object, &dialect));
    }
    Ok(servers)
}

fn parse_toml(text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>()
        .map_err(|error| toml_error(text, &error))
}

/// Message, ligne et colonne seulement : la ligne fautive peut porter un secret (`env`, `headers`).
fn toml_error(text: &str, error: &TomlError) -> String {
    let message = error.message();
    match error.span() {
        Some(span) => {
            let before = &text[..span.start.min(text.len())];
            let line = before.matches('\n').count() + 1;
            let column = before
                .rsplit('\n')
                .next()
                .map_or(0, |last| last.chars().count())
                + 1;
            format!("TOML invalide : {message} (ligne {line}, colonne {column})")
        }
        None => format!("TOML invalide : {message}"),
    }
}

fn item_to_json(item: &Item) -> Value {
    match item {
        Item::None => Value::Null,
        Item::Value(value) => toml_value_to_json(value),
        Item::Table(table) => table_to_json(table),
        Item::ArrayOfTables(tables) => {
            Value::Array(tables.iter().map(|table| table_to_json(table)).collect())
        }
    }
}

fn table_to_json(table: &dyn TableLike) -> Value {
    Value::Object(
        table
            .iter()
            .map(|(key, item)| (key.to_string(), item_to_json(item)))
            .collect(),
    )
}

fn toml_value_to_json(value: &toml_edit::Value) -> Value {
    match value {
        toml_edit::Value::String(text) => Value::String(text.value().clone()),
        toml_edit::Value::Integer(number) => Value::from(*number.value()),
        toml_edit::Value::Float(number) => {
            serde_json::Number::from_f64(*number.value()).map_or(Value::Null, Value::Number)
        }
        toml_edit::Value::Boolean(flag) => Value::Bool(*flag.value()),
        toml_edit::Value::Datetime(date) => Value::String(date.value().to_string()),
        toml_edit::Value::Array(items) => {
            Value::Array(items.iter().map(toml_value_to_json).collect())
        }
        toml_edit::Value::InlineTable(table) => table_to_json(table),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Écriture

/// Une valeur à poser dans une entrée de serveur.
enum Field {
    Text(String),
    List(Vec<String>),
    Table(Vec<(String, String)>),
}

impl Field {
    fn to_json(&self) -> Value {
        match self {
            Field::Text(text) => Value::String(text.clone()),
            Field::List(items) => Value::Array(items.iter().cloned().map(Value::String).collect()),
            Field::Table(pairs) => Value::Object(
                pairs
                    .iter()
                    .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                    .collect(),
            ),
        }
    }
}

type Assignments = Vec<(&'static str, Field)>;

/// Les clés que le cœur gouverne dans l'entrée d'un harnais : toute clé de cette liste que le nouveau cœur ne pose
/// pas est retirée, les autres clés de l'entrée (`extras`, drapeau de désactivation) ne sont jamais touchées.
fn owned_keys(harness: Harness) -> &'static [&'static str] {
    match harness {
        Harness::Claude => &["type", "command", "args", "env", "url", "headers"],
        Harness::Antigravity => &["command", "args", "env", "url", "serverUrl", "headers"],
        Harness::Codex => &["command", "args", "env", "url", "http_headers"],
        Harness::Grok => &["command", "args", "env", "url", "headers"],
        Harness::OpenCode => &["type", "command", "environment", "url", "headers"],
    }
}

/// Les clés à poser pour `transport`, dans l'ordre d'écriture d'une entrée neuve.
fn assignments(harness: Harness, transport: &Transport) -> Assignments {
    let mut out: Assignments = Vec::new();
    match (harness, transport) {
        (Harness::OpenCode, Transport::Stdio { command, args, env }) => {
            out.push(("type", Field::Text("local".to_string())));
            let mut line = vec![command.clone()];
            line.extend(args.iter().flatten().cloned());
            out.push(("command", Field::List(line)));
            if let Some(env) = env {
                out.push(("environment", Field::Table(env.clone())));
            }
        }
        (Harness::OpenCode, Transport::Http { url, headers }) => {
            out.push(("type", Field::Text("remote".to_string())));
            out.push(("url", Field::Text(url.clone())));
            if let Some(headers) = headers {
                out.push(("headers", Field::Table(headers.clone())));
            }
        }
        (_, Transport::Stdio { command, args, env }) => {
            if harness == Harness::Claude {
                out.push(("type", Field::Text("stdio".to_string())));
            }
            out.push(("command", Field::Text(command.clone())));
            if let Some(args) = args {
                out.push(("args", Field::List(args.clone())));
            }
            if let Some(env) = env {
                out.push(("env", Field::Table(env.clone())));
            }
        }
        (_, Transport::Http { url, headers }) => {
            if harness == Harness::Claude {
                out.push(("type", Field::Text("http".to_string())));
            }
            let url_key = if harness == Harness::Antigravity {
                "serverUrl"
            } else {
                "url"
            };
            out.push((url_key, Field::Text(url.clone())));
            if let Some(headers) = headers {
                let headers_key = if harness == Harness::Codex {
                    "http_headers"
                } else {
                    "headers"
                };
                out.push((headers_key, Field::Table(headers.clone())));
            }
        }
    }
    out
}

/// Applique `changes` au texte d'un fichier (`None` : fichier absent, créé). Rend le nouveau texte complet.
/// Un fichier illisible rend `Err` et n'est jamais réécrit ; un `Put` dont le cœur n'est pas pris en charge aussi.
/// Un fichier absent qu'aucun `Put` ne concerne ne se crée pas : le résultat est vide.
pub fn write_servers(
    harness: Harness,
    original: Option<&str>,
    changes: &[ServerChange],
) -> Result<String, String> {
    let mut puts = Vec::new();
    for change in changes {
        if let ServerChange::Put { name, core } = change {
            puts.push((
                name.as_str(),
                parse_core(core).map_err(|error| format!("serveur `{name}` : {error}"))?,
            ));
        }
    }
    let blank = original.is_none_or(|text| text.trim().is_empty());
    if blank && puts.is_empty() {
        return Ok(String::new());
    }
    if harness.is_toml() {
        write_toml(harness, original, changes)
    } else {
        write_json(harness, original, blank, changes)
    }
}

fn write_json(
    harness: Harness,
    original: Option<&str>,
    blank: bool,
    changes: &[ServerChange],
) -> Result<String, String> {
    let mut root = parse_json_root(original.unwrap_or_default())?;
    if blank && harness == Harness::OpenCode {
        root.insert(
            "$schema".to_string(),
            Value::String("https://opencode.ai/config.json".to_string()),
        );
    }
    for change in changes {
        match change {
            ServerChange::Put { name, core } => {
                let transport = parse_core(core)?;
                let fields = assignments(harness, &transport);
                if harness == Harness::OpenCode {
                    put_opencode(&mut root, name, &fields)?;
                } else {
                    let section = json_section(&mut root, "mcpServers")?;
                    let entry = section
                        .entry(name.clone())
                        .or_insert_with(|| Value::Object(Map::new()))
                        .as_object_mut()
                        .ok_or_else(|| format!("`mcpServers.{name}` doit être un objet"))?;
                    apply_json(entry, harness, &fields);
                }
            }
            ServerChange::Remove { name } => remove_json(harness, &mut root, name)?,
        }
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|error| format!("sérialisation JSON impossible : {error}"))?;
    text.push('\n');
    Ok(text)
}

/// L'objet `key` de `parent`, créé s'il manque.
fn json_section<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>, String> {
    parent
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| format!("`{key}` doit être un objet"))
}

fn apply_json(entry: &mut Map<String, Value>, harness: Harness, fields: &Assignments) {
    for key in owned_keys(harness) {
        if !fields.iter().any(|(name, _)| name == key) {
            entry.shift_remove(*key);
        }
    }
    for (key, field) in fields {
        // `insert` sur une clé existante garde sa place dans l'objet.
        entry.insert((*key).to_string(), field.to_json());
    }
}

fn put_opencode(
    root: &mut Map<String, Value>,
    name: &str,
    fields: &Assignments,
) -> Result<(), String> {
    let mcp = json_section(root, "mcp")?;
    let in_v2 = mcp
        .get("servers")
        .and_then(Value::as_object)
        .is_some_and(|servers| servers.contains_key(name));
    // Seule la forme v2 s'écrit : une entrée de l'ancienne forme y migre au premier enregistrement.
    let legacy = if in_v2 || name == "servers" {
        None
    } else {
        match mcp.get(name) {
            Some(Value::Object(_)) => mcp.shift_remove(name),
            _ => None,
        }
    };
    let mut entry = match legacy {
        Some(Value::Object(legacy)) => migrate_legacy(legacy),
        _ => Map::new(),
    };
    let servers = json_section(mcp, "servers")?;
    if in_v2 {
        entry = servers
            .get(name)
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| format!("`mcp.servers.{name}` doit être un objet"))?;
    }
    // Une clé propre à l'autre type n'est pas valide dans celui-ci (`additionalProperties: false`, schéma v2) : `cwd`
    // n'existe que pour `local`, `oauth` que pour `remote`. Passer de l'un à l'autre ne doit pas la laisser en place.
    let writes_remote = fields.iter().any(|(key, field)| {
        *key == "type" && matches!(field, Field::Text(kind) if kind == "remote")
    });
    entry.shift_remove(if writes_remote { "cwd" } else { "oauth" });
    apply_json(&mut entry, Harness::OpenCode, fields);
    servers.insert(name.to_string(), Value::Object(entry));
    Ok(())
}

/// Ancienne forme vers v2 : `enabled` devient `disabled` inversé, un `timeout` entier est abandonné (le `timeout` v2
/// est un objet), les autres clés passent telles quelles.
fn migrate_legacy(legacy: Map<String, Value>) -> Map<String, Value> {
    let mut entry = Map::new();
    for (key, value) in legacy {
        match key.as_str() {
            "enabled" => {
                if let Some(flag) = value.as_bool() {
                    entry.insert("disabled".to_string(), Value::Bool(!flag));
                }
            }
            "timeout" if !value.is_object() => {}
            _ => {
                entry.insert(key, value);
            }
        }
    }
    entry
}

fn remove_json(harness: Harness, root: &mut Map<String, Value>, name: &str) -> Result<(), String> {
    if harness != Harness::OpenCode {
        if let Some(section) = root.get_mut("mcpServers") {
            section
                .as_object_mut()
                .ok_or("`mcpServers` doit être un objet")?
                .shift_remove(name);
        }
        return Ok(());
    }
    let Some(mcp) = root.get_mut("mcp") else {
        return Ok(());
    };
    let mcp = mcp.as_object_mut().ok_or("`mcp` doit être un objet")?;
    if let Some(servers) = mcp.get_mut("servers") {
        servers
            .as_object_mut()
            .ok_or("`mcp.servers` doit être un objet")?
            .shift_remove(name);
    }
    if name != "servers" && mcp.get(name).is_some_and(Value::is_object) {
        mcp.shift_remove(name);
    }
    Ok(())
}

fn write_toml(
    harness: Harness,
    original: Option<&str>,
    changes: &[ServerChange],
) -> Result<String, String> {
    let text = original.unwrap_or_default();
    let mut document = parse_toml(text)?;
    for change in changes {
        match change {
            ServerChange::Put { name, core } => {
                let transport = parse_core(core)?;
                let fields = assignments(harness, &transport);
                put_toml(&mut document, harness, name, &fields)?;
            }
            ServerChange::Remove { name } => {
                if let Some(section) = document.get_mut("mcp_servers") {
                    section
                        .as_table_like_mut()
                        .ok_or("`mcp_servers` doit être une table")?
                        .remove(name);
                }
            }
        }
    }
    Ok(document.to_string())
}

fn put_toml(
    document: &mut DocumentMut,
    harness: Harness,
    name: &str,
    fields: &Assignments,
) -> Result<(), String> {
    if document.get("mcp_servers").is_none() {
        // Implicite : pas d'en-tête `[mcp_servers]` vide, seulement `[mcp_servers.<nom>]`.
        let mut table = Table::new();
        table.set_implicit(true);
        document.insert("mcp_servers", Item::Table(table));
    }
    let has_content = !document.to_string().trim().is_empty();
    let section = document
        .get_mut("mcp_servers")
        .and_then(Item::as_table_like_mut)
        .ok_or("`mcp_servers` doit être une table")?;
    match section.get_mut(name) {
        Some(item) => {
            let entry = item
                .as_table_like_mut()
                .ok_or_else(|| format!("`mcp_servers.{name}` doit être une table"))?;
            apply_toml(entry, harness, fields);
        }
        None => {
            let mut table = Table::new();
            if has_content {
                table.decor_mut().set_prefix("\n");
            }
            apply_toml(&mut table, harness, fields);
            section.insert(name, Item::Table(table));
        }
    }
    Ok(())
}

fn apply_toml(entry: &mut dyn TableLike, harness: Harness, fields: &Assignments) {
    for key in owned_keys(harness) {
        if !fields.iter().any(|(name, _)| name == key) {
            entry.remove(key);
        }
    }
    for (key, field) in fields {
        match field {
            Field::Text(text) => set_toml_value(entry, key, toml_edit::Value::from(text.as_str())),
            Field::List(items) => {
                let array: toml_edit::Array = items.iter().map(String::as_str).collect();
                set_toml_value(entry, key, toml_edit::Value::Array(array));
            }
            Field::Table(pairs) => set_toml_table(entry, key, pairs),
        }
    }
}

/// Remplace la valeur d'une clé en gardant sa place, son commentaire de tête et son commentaire de fin.
fn set_toml_value(entry: &mut dyn TableLike, key: &str, mut value: toml_edit::Value) {
    match entry.get_mut(key) {
        Some(item) => {
            if let Some(old) = item.as_value() {
                *value.decor_mut() = old.decor().clone();
            }
            *item = Item::Value(value);
        }
        None => {
            entry.insert(key, Item::Value(value));
        }
    }
}

/// Synchronise une sous-table (`env`, `headers`) : retire les clés disparues, pose les autres, crée la table si elle
/// manque (et qu'il y a quelque chose à y mettre).
fn set_toml_table(entry: &mut dyn TableLike, key: &str, pairs: &[(String, String)]) {
    match entry.get_mut(key) {
        Some(item) => match item.as_table_like_mut() {
            Some(existing) => sync_pairs(existing, pairs),
            None => *item = Item::Table(pairs_table(pairs)),
        },
        None => {
            if !pairs.is_empty() {
                entry.insert(key, Item::Table(pairs_table(pairs)));
            }
        }
    }
}

fn sync_pairs(existing: &mut dyn TableLike, pairs: &[(String, String)]) {
    let stale: Vec<String> = existing
        .iter()
        .map(|(key, _)| key.to_string())
        .filter(|key| !pairs.iter().any(|(name, _)| name == key))
        .collect();
    for key in stale {
        existing.remove(&key);
    }
    for (name, value) in pairs {
        set_toml_value(existing, name, toml_edit::Value::from(value.as_str()));
    }
}

fn pairs_table(pairs: &[(String, String)]) -> Table {
    let mut table = Table::new();
    table.decor_mut().set_prefix("\n");
    for (name, value) in pairs {
        table.insert(name, toml_edit::value(value.as_str()));
    }
    table
}

#[cfg(test)]
mod tests;
