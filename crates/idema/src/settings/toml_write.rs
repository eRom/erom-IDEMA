//! Éditions de `config.toml` qui gardent commentaires et mise en forme : une seule modification à la fois, validée
//! par `parse_config` avant d'être écrite. Le disque fait foi : chaque écriture relit le fichier.

use std::fs;
use std::io;
use std::path::Path;

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, TableLike, Value};

use crate::config::{
    AgentEntry, Config, ConfigError, ConfigErrorKind, Favorite, RemoteMachineEntry, parse_config,
};

/// Une modification d'une clé de `config.toml`. `None` ou une chaîne vide retire la clé (la valeur par défaut
/// d'IdemA reprend la main).
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigEdit {
    UiFontSize(f32),
    TerminalFontFamily(String),
    TerminalFontSize(f32),
    TerminalLineHeight(f32),
    SoundOnWait(bool),
    TmuxPath(Option<String>),
    /// Remplace tout le tableau `[[agents]]` ; l'appelant garantit qu'au plus une entrée porte `default`.
    Agents(Vec<AgentEntry>),
    CommitEnabled(bool),
    CommitApiKey(Option<String>),
    CommitModel(String),
    CommitIncludeProjectRules(bool),
    CommitInstructions(Option<String>),
    CommitEndpoint(Option<String>),
    /// Remplace tout le tableau `[[browser.favorites]]`.
    Favorites(Vec<Favorite>),
    /// Vrai : crée la table `[companion]` avec le port par défaut si elle manque, garde une table existante. Faux :
    /// retire toute la table (le compagnon n'existe plus).
    CompanionEnabled(bool),
    /// Les trois clés ci-dessous se posent dans la table existante et ne la créent jamais : sans elle, le compagnon est
    /// éteint, et poser une clé l'allumerait.
    CompanionNotifyWaiting(bool),
    CompanionNotifyError(bool),
    /// `None` retire la clé (le délai par défaut reprend la main).
    CompanionAwayAfter(Option<u64>),
    /// Remplace tout le tableau `[[machines.remote]]` ; la table `[machines]` qui reste vide est retirée.
    RemoteMachines(Vec<RemoteMachineEntry>),
    /// Le nom de Localhost sous `[machines]` ; `None` retire la clé (et la table, si elle reste vide).
    LocalMachineLabel(Option<String>),
}

/// Le port que la table `[companion]` reçoit quand l'interrupteur l'ajoute (celui de la spec, §3.2).
pub const COMPANION_DEFAULT_PORT: u16 = 3910;

/// Applique `edit` au texte d'une config. Refuse un `text` qui ne passe pas `parse_config`, et rend le nouveau texte
/// avec sa `Config` validée (une valeur refusée par la validation revient en erreur, rien n'est à défaire).
pub fn apply_edit(text: &str, edit: &ConfigEdit) -> Result<(String, Config), ConfigError> {
    parse_config(text)?;
    let mut document: DocumentMut =
        text.parse()
            .map_err(|error: toml_edit::TomlError| ConfigError {
                kind: ConfigErrorKind::Syntax,
                line: None,
                message: error.message().to_string(),
            })?;
    apply(&mut document, edit);
    let rendered = document.to_string();
    let config = parse_config(&rendered)?;
    Ok((rendered, config))
}

/// Relit le fichier (absent : base vide), applique `edit`, puis écrit de façon atomique : fichier temporaire voisin,
/// puis `rename`. Un fichier de base invalide ou une valeur refusée ne touche pas au disque.
pub fn write_edit(path: &Path, edit: &ConfigEdit) -> Result<Config, ConfigError> {
    // Un `config.toml` rangé par un gestionnaire de dotfiles est un lien : on écrit la cible, pas le lien.
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let base = match fs::read_to_string(&target) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(io_error("lecture impossible", &error)),
    };
    let (text, config) = apply_edit(&base, edit)?;
    write_atomically(&target, &text).map_err(|error| io_error("écriture impossible", &error))?;
    Ok(config)
}

fn io_error(what: &str, error: &io::Error) -> ConfigError {
    ConfigError {
        kind: ConfigErrorKind::Read,
        line: None,
        message: format!("{what} ({error})"),
    }
}

pub(crate) fn write_atomically(target: &Path, text: &str) -> io::Result<()> {
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    if let Some(parent) = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(".idema-tmp");
    let temp = target.with_file_name(temp_name);

    let written = fs::write(&temp, text)
        .and_then(|()| match fs::metadata(target) {
            // Le fichier peut porter une clé d'API : le nouveau garde les droits de l'ancien.
            Ok(meta) => fs::set_permissions(&temp, meta.permissions()),
            Err(_) => Ok(()),
        })
        .and_then(|()| fs::rename(&temp, target));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

fn apply(document: &mut DocumentMut, edit: &ConfigEdit) {
    let root = document.as_table_mut();
    match edit {
        ConfigEdit::UiFontSize(value) => set(root, "ui_font_size", Some(number(*value))),
        ConfigEdit::TerminalFontFamily(value) => set(root, "terminal_font_family", text(value)),
        ConfigEdit::TerminalFontSize(value) => {
            set(root, "terminal_font_size", Some(number(*value)))
        }
        ConfigEdit::TerminalLineHeight(value) => {
            set(root, "terminal_line_height", Some(number(*value)));
        }
        ConfigEdit::SoundOnWait(value) => set(root, "sound_on_wait", Some(Value::from(*value))),
        ConfigEdit::TmuxPath(value) => set(root, "tmux_path", optional_text(value)),
        ConfigEdit::Agents(agents) => {
            let tables = agents.iter().map(agent_table).collect();
            replace_tables(document, &[], "agents", tables);
        }
        ConfigEdit::CommitEnabled(value) => set_commit(root, "enabled", Some(Value::from(*value))),
        ConfigEdit::CommitApiKey(value) => set_commit(root, "api_key", optional_text(value)),
        ConfigEdit::CommitModel(value) => set_commit(root, "model", text(value)),
        ConfigEdit::CommitIncludeProjectRules(value) => {
            set_commit(root, "include_project_rules", Some(Value::from(*value)));
        }
        ConfigEdit::CommitInstructions(value) => {
            set_commit(root, "instructions", optional_text(value))
        }
        ConfigEdit::CommitEndpoint(value) => set_commit(root, "endpoint", optional_text(value)),
        ConfigEdit::Favorites(favorites) => {
            let tables = favorites.iter().map(favorite_table).collect();
            replace_tables(document, &["browser"], "favorites", tables);
        }
        ConfigEdit::RemoteMachines(machines) => {
            let tables = machines.iter().map(machine_table).collect();
            replace_tables(document, &["machines"], "remote", tables);
            drop_empty_machines(document.as_table_mut());
        }
        ConfigEdit::LocalMachineLabel(value) => {
            if root.get("machines").is_none() {
                if value.is_none() {
                    return;
                }
                let mut section = Table::new();
                // Une ligne vide sépare la table de ce qui la précède, sauf sur un fichier vide.
                if !root.is_empty() {
                    section.decor_mut().set_prefix("\n");
                }
                root.insert("machines", Item::Table(section));
            }
            if let Some(section) = root.get_mut("machines").and_then(Item::as_table_like_mut) {
                set(section, "local_label", optional_text(value));
            }
            drop_empty_machines(root);
        }
        ConfigEdit::CompanionEnabled(true) => {
            if root.get("companion").is_none() {
                let mut section = Table::new();
                section.insert(
                    "port",
                    Item::Value(Value::from(i64::from(COMPANION_DEFAULT_PORT))),
                );
                // Une ligne vide sépare la table de ce qui la précède, sauf sur un fichier vide.
                if !root.is_empty() {
                    section.decor_mut().set_prefix("\n");
                }
                root.insert("companion", Item::Table(section));
            }
        }
        ConfigEdit::CompanionEnabled(false) => {
            root.remove("companion");
        }
        ConfigEdit::CompanionNotifyWaiting(value) => {
            set_companion(root, "notify_waiting", Some(Value::from(*value)));
        }
        ConfigEdit::CompanionNotifyError(value) => {
            set_companion(root, "notify_error", Some(Value::from(*value)));
        }
        ConfigEdit::CompanionAwayAfter(value) => set_companion(
            root,
            "away_after_seconds",
            value.map(|seconds| Value::from(i64::try_from(seconds).unwrap_or(i64::MAX))),
        ),
    }
}

/// Une clé de `[companion]`, dans la table existante seulement : la créer allumerait le compagnon.
fn set_companion(root: &mut Table, key: &str, value: Option<Value>) {
    if let Some(section) = root.get_mut("companion").and_then(Item::as_table_like_mut) {
        set(section, key, value);
    }
}

/// Un nombre de la config : entier quand il l'est (`16`, comme à la main), décimal sinon (`1.55`, sans le bruit
/// du passage de `f32` à `f64`).
fn number(value: f32) -> Value {
    if value.is_finite() && value.fract() == 0. && value.abs() < 1e9 {
        return Value::from(value as i64);
    }
    Value::from(value.to_string().parse::<f64>().unwrap_or(f64::from(value)))
}

fn text(value: &str) -> Option<Value> {
    (!value.is_empty()).then(|| Value::from(value))
}

fn optional_text(value: &Option<String>) -> Option<Value> {
    value.as_deref().and_then(text)
}

/// Pose `key` (ou le retire pour `None`) en gardant la clé en place et les commentaires de sa ligne.
fn set(table: &mut dyn TableLike, key: &str, value: Option<Value>) {
    let Some(mut value) = value else {
        table.remove(key);
        return;
    };
    match table.get_mut(key) {
        Some(item) => {
            if let Some(old) = item.as_value() {
                *value.decor_mut() = old.decor().clone();
            }
            *item = Item::Value(value);
        }
        None => {
            table.insert(key, Item::Value(value));
        }
    }
}

/// Une clé de `[commit_message]` ; la section est créée au besoin, mais jamais pour retirer une clé.
fn set_commit(root: &mut Table, key: &str, value: Option<Value>) {
    if root.get("commit_message").is_none() {
        if value.is_none() {
            return;
        }
        root.insert("commit_message", Item::Table(Table::new()));
    }
    if let Some(section) = root
        .get_mut("commit_message")
        .and_then(Item::as_table_like_mut)
    {
        set(section, key, value);
    }
}

fn agent_table(agent: &AgentEntry) -> Table {
    let mut table = Table::new();
    table.insert("name", Item::Value(Value::from(agent.name.as_str())));
    if let Some(harness) = &agent.harness {
        table.insert("harness", Item::Value(Value::from(harness.as_str())));
    }
    table.insert("command", Item::Value(Value::from(agent.command.as_str())));
    // `default` n'est écrit que sur l'entrée qui le porte : les autres l'omettent.
    if agent.default {
        table.insert("default", Item::Value(Value::from(true)));
    }
    if let Some(icon) = &agent.icon {
        table.insert("icon", Item::Value(Value::from(icon.as_str())));
    }
    table
}

fn machine_table(machine: &RemoteMachineEntry) -> Table {
    let mut table = Table::new();
    table.insert("label", Item::Value(Value::from(machine.label.as_str())));
    table.insert(
        "connection",
        Item::Value(Value::from(machine.connection.as_str())),
    );
    table
}

/// Retire `[machines]` quand elle ne porte plus rien : ni `local_label` ni machine distante.
fn drop_empty_machines(root: &mut Table) {
    let empty = root
        .get("machines")
        .and_then(Item::as_table_like)
        .is_some_and(|section| section.is_empty());
    if empty {
        root.remove("machines");
    }
}

fn favorite_table(favorite: &Favorite) -> Table {
    let mut table = Table::new();
    table.insert("name", Item::Value(Value::from(favorite.name.as_str())));
    table.insert("url", Item::Value(Value::from(favorite.url.as_str())));
    table
}

/// Remplace tout le tableau de tables `key` (sous les sections `parents`). Les commentaires qui précèdent le premier
/// en-tête restent devant le premier nouvel en-tête ; une liste vide retire la clé et range ces commentaires en fin
/// de fichier plutôt que de les perdre.
fn replace_tables(document: &mut DocumentMut, parents: &[&str], key: &str, tables: Vec<Table>) {
    let mut parent: &mut dyn TableLike = document.as_table_mut();
    for name in parents {
        if parent.get(name).is_none() {
            if tables.is_empty() {
                return;
            }
            // Section créée pour porter le tableau : pas d'en-tête `[browser]` seul, juste `[[browser.favorites]]`.
            let mut section = Table::new();
            section.set_implicit(true);
            parent.insert(name, Item::Table(section));
        }
        let Some(next) = parent.get_mut(name).and_then(Item::as_table_like_mut) else {
            return;
        };
        parent = next;
    }

    let (lead, position) = match parent.get(key).and_then(Item::as_array_of_tables) {
        Some(old) => {
            let first = old.iter().next();
            (
                first
                    .and_then(|table| table.decor().prefix())
                    .and_then(|prefix| prefix.as_str())
                    .unwrap_or_default()
                    .to_string(),
                first.and_then(Table::position),
            )
        }
        None => (String::new(), None),
    };

    if tables.is_empty() {
        parent.remove(key);
        if lead.contains('#') {
            let trailing = format!("{}{lead}", document.trailing().as_str().unwrap_or_default());
            document.set_trailing(trailing);
        }
        return;
    }

    let mut array = ArrayOfTables::new();
    for (index, mut table) in tables.into_iter().enumerate() {
        // Les nouveaux en-têtes prennent la place de l'ancien premier, l'ordre du fichier ne bouge pas.
        table.set_position(position);
        let prefix = if index == 0 && !lead.is_empty() {
            lead.clone()
        } else {
            "\n".to_string()
        };
        table.decor_mut().set_prefix(prefix);
        array.push(table);
    }
    parent.insert(key, Item::ArrayOfTables(array));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AgentEntry, ConfigErrorKind, DEFAULT_CONFIG, Favorite, RemoteMachineEntry, parse_config,
    };
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("idema-toml-write-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn agent(name: &str, default: bool) -> AgentEntry {
        AgentEntry {
            name: name.into(),
            harness: None,
            command: format!("idnma run {}", name.to_lowercase()),
            default,
            icon: None,
        }
    }

    fn favorite(name: &str) -> Favorite {
        Favorite {
            name: name.into(),
            url: format!("https://{}.test/", name.to_lowercase()),
        }
    }

    #[test]
    fn a_scalar_edit_keeps_every_comment() {
        let (text, config) =
            apply_edit(DEFAULT_CONFIG, &ConfigEdit::TerminalFontSize(18.)).unwrap();
        assert_eq!(config.terminal_font_size, 18.);

        let before: Vec<&str> = DEFAULT_CONFIG.lines().collect();
        let after: Vec<&str> = text.lines().collect();
        assert_eq!(before.len(), after.len());
        let changed: Vec<(&&str, &&str)> = before
            .iter()
            .zip(&after)
            .filter(|(old, new)| old != new)
            .collect();
        assert_eq!(changed.len(), 1, "exactly one line changes: {changed:?}");
        assert_eq!(*changed[0].0, "terminal_font_size = 16");
        assert_eq!(*changed[0].1, "terminal_font_size = 18");
        let comments = |lines: &[&str]| lines.iter().filter(|line| line.contains('#')).count();
        assert_eq!(comments(&before), comments(&after));
    }

    #[test]
    fn editing_a_missing_section_creates_it() {
        let (text, config) =
            apply_edit("ui_font_size = 14\n", &ConfigEdit::CommitModel("x".into())).unwrap();
        assert_eq!(config.commit_message.unwrap().model, "x");
        assert!(parse_config(&text).is_ok());
    }

    #[test]
    fn the_harness_survives_a_rewrite_of_the_agents() {
        let entry = AgentEntry {
            harness: Some("Claude Code".into()),
            ..agent("Opus", false)
        };
        let (text, config) = apply_edit("", &ConfigEdit::Agents(vec![entry])).unwrap();
        assert!(text.contains("harness = \"Claude Code\""));
        assert_eq!(config.agents[0].harness_name(), Some("Claude Code"));
    }

    #[test]
    fn default_agent_is_exclusive() {
        let agents = vec![agent("Claude", false), agent("Codex", true)];
        let (text, config) = apply_edit(DEFAULT_CONFIG, &ConfigEdit::Agents(agents)).unwrap();
        let defaults: Vec<&str> = config
            .agents
            .iter()
            .filter(|entry| entry.default)
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(defaults, ["Codex"]);
        assert_eq!(text.matches("default = true").count(), 1);
        assert!(!text.contains("default = false"));
    }

    #[test]
    fn removing_the_last_agent_leaves_no_array() {
        let (text, config) = apply_edit(DEFAULT_CONFIG, &ConfigEdit::Agents(vec![])).unwrap();
        assert!(!text.contains("[[agents]]"));
        assert!(config.agents.is_empty());
    }

    #[test]
    fn favorites_reorder_round_trips() {
        let base = "[[browser.favorites]]\nname = \"A\"\nurl = \"https://a.test/\"\n\n[[browser.favorites]]\nname = \"B\"\nurl = \"https://b.test/\"\n";
        let reversed = vec![favorite("B"), favorite("A")];
        let (text, config) = apply_edit(base, &ConfigEdit::Favorites(reversed)).unwrap();
        let names: Vec<String> = config
            .browser
            .unwrap()
            .favorites
            .into_iter()
            .map(|favorite| favorite.name)
            .collect();
        assert_eq!(names, ["B", "A"]);
        assert!(parse_config(&text).is_ok());
    }

    fn remote(label: &str, connection: &str) -> RemoteMachineEntry {
        RemoteMachineEntry {
            label: label.into(),
            connection: connection.into(),
        }
    }

    #[test]
    fn remote_machines_replace_the_array_and_keep_comments() {
        let base = format!(
            "{DEFAULT_CONFIG}\n# machines\n[[machines.remote]]\nlabel = \"Dell\"\nconnection = \"dell-lan\"\n"
        );
        let machines = vec![remote("Dell", "dell-lan"), remote("Pi", "pi@raspberry")];
        let (text, config) =
            apply_edit(&base, &ConfigEdit::RemoteMachines(machines.clone())).unwrap();
        assert_eq!(config.remote_machines(), machines.as_slice());
        // Le commentaire au-dessus de `[[agents]]` (et celui de la première machine) survivent.
        assert!(text.contains("# Entrées du lanceur"), "{text}");
        assert!(text.contains("# machines"), "{text}");
        assert!(text.contains("[[agents]]"), "{text}");
    }

    #[test]
    fn an_empty_machines_table_disappears() {
        let base = "ui_font_size = 14\n\n[machines]\n\n[[machines.remote]]\nlabel = \"Dell\"\nconnection = \"dell-lan\"\n";
        let (text, config) = apply_edit(base, &ConfigEdit::RemoteMachines(vec![])).unwrap();
        assert!(!text.contains("machines"), "{text}");
        assert!(config.machines.is_none());
        // Avec un `local_label`, la table reste.
        let named = "[machines]\nlocal_label = \"Mon Mac\"\n\n[[machines.remote]]\nlabel = \"Dell\"\nconnection = \"dell-lan\"\n";
        let (text, config) = apply_edit(named, &ConfigEdit::RemoteMachines(vec![])).unwrap();
        assert!(text.contains("local_label = \"Mon Mac\""), "{text}");
        assert_eq!(config.local_machine_label(), "Mon Mac");
        assert!(config.remote_machines().is_empty());
    }

    #[test]
    fn local_label_is_written_under_machines() {
        let base = "ui_font_size = 14\n";
        let (text, config) =
            apply_edit(base, &ConfigEdit::LocalMachineLabel(Some("Mon Mac".into()))).unwrap();
        assert!(
            text.contains("[machines]\nlocal_label = \"Mon Mac\""),
            "{text}"
        );
        assert_eq!(config.local_machine_label(), "Mon Mac");
        // Les machines distantes déjà écrites gardent leur place sous la même table.
        let with_remote = "[[machines.remote]]\nlabel = \"Dell\"\nconnection = \"dell-lan\"\n";
        let (text, config) = apply_edit(
            with_remote,
            &ConfigEdit::LocalMachineLabel(Some("Mon Mac".into())),
        )
        .unwrap();
        assert_eq!(config.local_machine_label(), "Mon Mac");
        assert_eq!(config.remote_machines().len(), 1, "{text}");
        // Vider le label retire la clé, et la table vide avec elle.
        let (text, config) = apply_edit(&text, &ConfigEdit::LocalMachineLabel(None)).unwrap();
        assert!(!text.contains("local_label"), "{text}");
        assert_eq!(config.local_machine_label(), "Localhost");
        assert_eq!(config.remote_machines().len(), 1);
        let (text, config) =
            apply_edit("ui_font_size = 14\n", &ConfigEdit::LocalMachineLabel(None)).unwrap();
        assert!(
            !text.contains("machines"),
            "never created to remove a key: {text}"
        );
        assert!(config.machines.is_none());
    }

    #[test]
    fn enabling_adds_the_section_with_the_default_port() {
        let (text, config) = apply_edit("", &ConfigEdit::CompanionEnabled(true)).unwrap();
        assert_eq!(text, "[companion]\nport = 3910\n");
        assert_eq!(config.companion.unwrap().port, 3910);

        // Sur un fichier qui a déjà du contenu, la table arrive après une ligne vide et le reste ne bouge pas.
        let (text, config) = apply_edit(
            "ui_font_size = 14 # taille\n",
            &ConfigEdit::CompanionEnabled(true),
        )
        .unwrap();
        assert_eq!(
            text,
            "ui_font_size = 14 # taille\n\n[companion]\nport = 3910\n"
        );
        assert!(config.companion.is_some());
    }

    #[test]
    fn enabling_keeps_an_existing_section_untouched() {
        let base = "[companion]\nport = 4000\nnotify_error = false # sans bruit\n";
        let (text, config) = apply_edit(base, &ConfigEdit::CompanionEnabled(true)).unwrap();
        assert_eq!(text, base);
        assert_eq!(config.companion.unwrap().port, 4000);
    }

    #[test]
    fn disabling_removes_the_section_and_keeps_the_rest() {
        let base = "# en-tête\nui_font_size = 14\n\n# le compagnon\n[companion]\nport = 4000\nnotify_error = false\n\n[commit_message]\nenabled = true # actif\n";
        let (text, config) = apply_edit(base, &ConfigEdit::CompanionEnabled(false)).unwrap();
        assert_eq!(config.companion, None);
        assert!(!text.contains("[companion]") && !text.contains("port = 4000"));
        assert!(!text.contains("notify_error"));
        assert!(text.contains("# en-tête\nui_font_size = 14"));
        assert!(text.contains("[commit_message]\nenabled = true # actif"));

        // Éteindre un compagnon déjà éteint ne change rien.
        let (again, _) = apply_edit(&text, &ConfigEdit::CompanionEnabled(false)).unwrap();
        assert_eq!(again, text);
    }

    #[test]
    fn notify_toggles_write_their_keys() {
        let base = "[companion]\nport = 3910\n";
        let (text, config) = apply_edit(base, &ConfigEdit::CompanionNotifyWaiting(false)).unwrap();
        assert!(text.contains("notify_waiting = false"));
        assert!(!config.companion.as_ref().unwrap().notify_waiting);
        let (text, config) = apply_edit(&text, &ConfigEdit::CompanionNotifyError(false)).unwrap();
        assert!(text.contains("notify_error = false"));
        let section = config.companion.unwrap();
        assert!(!section.notify_waiting && !section.notify_error);

        // Une clé déjà là est remplacée en place, avec le commentaire de sa ligne.
        let commented = "[companion]\nnotify_waiting = false # calme\n";
        let (text, _) = apply_edit(commented, &ConfigEdit::CompanionNotifyWaiting(true)).unwrap();
        assert_eq!(text, "[companion]\nnotify_waiting = true # calme\n");
    }

    #[test]
    fn away_after_writes_then_clears_its_key() {
        let base = "[companion]\nport = 3910\n";
        let (text, config) = apply_edit(base, &ConfigEdit::CompanionAwayAfter(Some(0))).unwrap();
        assert!(text.contains("away_after_seconds = 0"));
        assert_eq!(config.companion.unwrap().away_after_seconds, 0);
        let (text, config) = apply_edit(&text, &ConfigEdit::CompanionAwayAfter(None)).unwrap();
        assert!(!text.contains("away_after_seconds"));
        assert_eq!(config.companion.unwrap().away_after_seconds, 120);
    }

    #[test]
    fn the_companion_keys_never_create_the_section() {
        for edit in [
            ConfigEdit::CompanionNotifyWaiting(false),
            ConfigEdit::CompanionNotifyError(false),
            ConfigEdit::CompanionAwayAfter(Some(30)),
        ] {
            let (text, config) = apply_edit("ui_font_size = 14\n", &edit).unwrap();
            assert_eq!(text, "ui_font_size = 14\n", "{edit:?}");
            assert_eq!(config.companion, None);
        }
    }

    #[test]
    fn a_rejected_value_returns_the_validation_error() {
        let error = apply_edit(DEFAULT_CONFIG, &ConfigEdit::UiFontSize(99.)).unwrap_err();
        assert_eq!(error.kind, ConfigErrorKind::Invalid);
    }

    #[test]
    fn an_invalid_base_is_never_written() {
        let dir = temp_dir("invalid-base");
        let path = dir.join("config.toml");
        fs::write(&path, "ui_font_size = \n").unwrap();
        let result = write_edit(&path, &ConfigEdit::TerminalFontSize(18.));
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"ui_font_size = \n");
    }

    #[test]
    fn clearing_tmux_path_removes_the_key() {
        let base = "ui_font_size = 14\ntmux_path = \"/x\"\n";
        let (text, config) = apply_edit(base, &ConfigEdit::TmuxPath(None)).unwrap();
        assert!(!text.contains("tmux_path"));
        assert_eq!(config.tmux_path, None);
    }

    #[test]
    fn replacing_agents_keeps_the_header_comments_and_the_section_order() {
        let base = format!(
            "{DEFAULT_CONFIG}\n# commit\n[commit_message]\nenabled = true # actif\n\n# favoris\n[[browser.favorites]]\nname = \"A\"\nurl = \"https://a.test/\"\n"
        );
        let agents = vec![agent("Claude", true), agent("Codex", false)];
        let (text, config) = apply_edit(&base, &ConfigEdit::Agents(agents)).unwrap();
        assert_eq!(config.agents.len(), 2);

        let lead = text.find("# Entrées du lanceur").unwrap();
        let agents_at = text.find("[[agents]]").unwrap();
        let commit_at = text.find("[commit_message]").unwrap();
        let favorites_at = text.find("[[browser.favorites]]").unwrap();
        assert!(lead < agents_at && agents_at < commit_at && commit_at < favorites_at);
        assert!(text.contains("enabled = true # actif"));
        assert!(text.contains("# favoris"));
    }

    #[test]
    fn an_edit_keeps_the_ideation_section() {
        let base = format!("{DEFAULT_CONFIG}\n# bouillon\n[ideation]\ndir = \"/x\" # dossier\n");
        let (text, config) = apply_edit(&base, &ConfigEdit::UiFontSize(22.)).unwrap();
        assert!(text.contains("# bouillon"), "{text}");
        assert!(text.contains("[ideation]"), "{text}");
        assert!(text.contains("dir = \"/x\" # dossier"), "{text}");
        assert_eq!(config.ideation.unwrap().dir, "/x");
    }

    #[test]
    fn a_trailing_comment_on_the_edited_line_survives() {
        let (text, _) =
            apply_edit("ui_font_size = 20 # taille\n", &ConfigEdit::UiFontSize(22.)).unwrap();
        assert_eq!(text, "ui_font_size = 22 # taille\n");
    }

    #[test]
    fn a_fractional_value_is_written_without_float_noise() {
        let (text, config) =
            apply_edit("ui_font_size = 14\n", &ConfigEdit::TerminalLineHeight(1.55)).unwrap();
        assert!(text.contains("terminal_line_height = 1.55\n"), "{text}");
        assert_eq!(config.terminal_line_height, 1.55);
    }

    #[test]
    fn an_empty_string_removes_an_optional_key() {
        let base = "[commit_message]\nenabled = true\nendpoint = \"https://e.test\"\n";
        let (text, config) =
            apply_edit(base, &ConfigEdit::CommitEndpoint(Some(String::new()))).unwrap();
        assert!(!text.contains("endpoint"));
        assert_eq!(config.commit_message.unwrap().endpoint, None);
    }

    #[test]
    fn clearing_a_key_never_creates_its_section() {
        let (text, config) = apply_edit("", &ConfigEdit::CommitApiKey(None)).unwrap();
        assert!(!text.contains("commit_message"));
        assert!(config.commit_message.is_none());
    }

    #[test]
    fn write_edit_creates_a_missing_file_and_leaves_no_temporary() {
        let dir = temp_dir("missing-file");
        let path = dir.join("nested").join("config.toml");
        let config = write_edit(&path, &ConfigEdit::UiFontSize(18.)).unwrap();
        assert_eq!(config.ui_font_size, 18.);
        assert_eq!(fs::read_to_string(&path).unwrap(), "ui_font_size = 18\n");
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[test]
    fn write_edit_rereads_the_disk_before_applying() {
        let dir = temp_dir("reread");
        let path = dir.join("config.toml");
        fs::write(&path, "ui_font_size = 14\n").unwrap();
        write_edit(&path, &ConfigEdit::SoundOnWait(false)).unwrap();
        // L'utilisateur édite à la main entre deux écritures : la seconde part du disque, pas d'une copie en mémoire.
        fs::write(&path, "ui_font_size = 30\nsound_on_wait = false\n").unwrap();
        let config = write_edit(&path, &ConfigEdit::TerminalFontSize(12.)).unwrap();
        assert_eq!(config.ui_font_size, 30.);
        assert_eq!(config.terminal_font_size, 12.);
    }
}
