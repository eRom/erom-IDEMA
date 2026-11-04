//! La couche `claude` de la fenêtre Personnaliser : résolution du binaire, lectures de `~/.claude.json` et des sorties
//! `--json`, constructeurs d'arguments et séquence d'édition d'un serveur MCP.
//! IdemA n'écrit jamais `~/.claude.json` ni `~/.claude/settings.json` : toute écriture passe par le CLI, portée `user`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// Emplacements de `claude` essayés dans l'ordre ; le premier, avec `~`, se résout sur `$HOME` (même logique que
/// `CANDIDATES` dans `idema-tmux`).
pub const CLAUDE_CANDIDATES: &[&str] = &[
    "~/.local/bin/claude",
    "/opt/homebrew/bin/claude",
    "/usr/local/bin/claude",
];

/// Vrai si `path` est un fichier régulier avec au moins un bit d'exécution.
fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Trouve `claude` : `IDEMA_CLAUDE_BIN` (`env_override`) gagne s'il est fourni et non vide, sinon le premier candidat
/// exécutable. `Err` porte les chemins essayés, pour l'état vide « CLI Claude introuvable ».
pub fn resolve_claude(home: &Path, env_override: Option<&OsStr>) -> Result<PathBuf, Vec<PathBuf>> {
    if let Some(value) = env_override.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    let candidates: Vec<PathBuf> = CLAUDE_CANDIDATES
        .iter()
        .map(|candidate| match candidate.strip_prefix("~/") {
            Some(relative) => home.join(relative),
            None => PathBuf::from(candidate),
        })
        .collect();
    match candidates.iter().find(|candidate| is_executable(candidate)) {
        Some(found) => Ok(found.clone()),
        None => Err(candidates),
    }
}

/// Lit les serveurs MCP de portée utilisateur : la clé racine `mcpServers` de `~/.claude.json`, jamais celles des
/// projets. Fichier ou clé absent = aucun serveur ; JSON invalide = message d'erreur.
pub fn read_mcp_servers(claude_json: &Path) -> Result<BTreeMap<String, Value>, String> {
    let text = match fs::read_to_string(claude_json) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(format!("{}: {error}", claude_json.display())),
    };
    let root: Value = serde_json::from_str(&text)
        .map_err(|error| format!("{}: {error}", claude_json.display()))?;
    match root.get("mcpServers") {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(servers)) => Ok(servers
            .iter()
            .map(|(name, server)| (name.clone(), server.clone()))
            .collect()),
        Some(_) => Err(format!(
            "{}: mcpServers n'est pas un objet",
            claude_json.display()
        )),
    }
}

/// Un plugin installé en portée utilisateur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginEntry {
    /// `nom@marketplace`.
    pub id: String,
    pub name: String,
    pub marketplace: String,
    pub enabled: bool,
}

/// Analyse `claude plugin list --json` : garde la portée `user`, un seul élément par `id`, actifs d'abord puis ordre
/// alphabétique.
pub fn parse_plugins(json: &str) -> Result<Vec<PluginEntry>, String> {
    let items: Vec<Value> = serde_json::from_str(json).map_err(|error| error.to_string())?;
    let mut seen = BTreeSet::new();
    let mut plugins = Vec::new();
    for item in &items {
        if item["scope"] != "user" {
            continue;
        }
        let Some(id) = item["id"].as_str() else {
            continue;
        };
        if !seen.insert(id.to_string()) {
            continue;
        }
        let (name, marketplace) = id.split_once('@').unwrap_or((id, ""));
        plugins.push(PluginEntry {
            id: id.to_string(),
            name: name.to_string(),
            marketplace: marketplace.to_string(),
            enabled: item["enabled"].as_bool().unwrap_or(false),
        });
    }
    plugins.sort_by(|a, b| b.enabled.cmp(&a.enabled).then_with(|| a.id.cmp(&b.id)));
    Ok(plugins)
}

/// Un marketplace connu du CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceEntry {
    pub name: String,
    pub source: String,
    /// `repo`, sinon `url`, sinon `path`, sinon `installLocation`.
    pub location: String,
    /// Vrai si l'utilisateur l'a déclaré dans son `settings.json` : seuls ceux-là se retirent depuis Idema.
    pub removable: bool,
}

/// Analyse `claude plugin marketplace list --json` ; `removable` vaut vrai pour les noms de `user_declared`.
pub fn parse_marketplaces(
    json: &str,
    user_declared: &BTreeSet<String>,
) -> Result<Vec<MarketplaceEntry>, String> {
    let items: Vec<Value> = serde_json::from_str(json).map_err(|error| error.to_string())?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let name = item["name"].as_str()?;
            let location = ["repo", "url", "path", "installLocation"]
                .iter()
                .find_map(|key| item[*key].as_str())
                .unwrap_or_default();
            Some(MarketplaceEntry {
                name: name.to_string(),
                source: item["source"].as_str().unwrap_or_default().to_string(),
                location: location.to_string(),
                removable: user_declared.contains(name),
            })
        })
        .collect())
}

/// Les noms de marketplaces que l'utilisateur déclare (`extraKnownMarketplaces`) ; fichier, clé ou JSON absent = vide.
pub fn user_declared_marketplaces(settings_json: &Path) -> BTreeSet<String> {
    let Ok(text) = fs::read_to_string(settings_json) else {
        return BTreeSet::new();
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return BTreeSet::new();
    };
    root.get("extraKnownMarketplaces")
        .and_then(Value::as_object)
        .map(|declared| declared.keys().cloned().collect())
        .unwrap_or_default()
}

/// Un identifiant de plugin valide : exactement un `@`, deux côtés non vides, aucun espace.
pub fn validate_plugin_id(id: &str) -> Result<(), &'static str> {
    if id.chars().any(char::is_whitespace) {
        return Err("L'identifiant ne doit pas contenir d'espace.");
    }
    match id.split_once('@') {
        Some((name, marketplace))
            if !name.is_empty() && !marketplace.is_empty() && !marketplace.contains('@') =>
        {
            Ok(())
        }
        _ => Err("Format attendu : nom@marketplace."),
    }
}

/// Ce que le CLI reçoit en position d'argument et qu'une saisie ne doit pas pouvoir faire passer pour une option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliArgument {
    /// Le nom d'un serveur MCP.
    Name,
    /// L'identifiant `nom@marketplace` d'un plugin.
    Id,
    /// La source d'un marketplace.
    Source,
}

/// Refuse une valeur qui commence par un tiret : le CLI la lirait comme une option (`mcp add-json --help …` affiche
/// l'aide, sort en 0 et n'ajoute rien, sans message). Garde commune au nom MCP, à l'identifiant de plugin et à la source.
pub fn reject_leading_dash(value: &str, argument: CliArgument) -> Result<(), &'static str> {
    if !value.starts_with('-') {
        return Ok(());
    }
    Err(match argument {
        CliArgument::Name => "Le nom ne doit pas commencer par un tiret.",
        CliArgument::Id => "L'identifiant ne doit pas commencer par un tiret.",
        CliArgument::Source => "La source ne doit pas commencer par un tiret.",
    })
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// `claude mcp add-json <name> <json> -s user`.
pub fn mcp_add_args(name: &str, json: &str) -> Vec<String> {
    owned(&["mcp", "add-json", name, json, "-s", "user"])
}

/// `claude mcp remove <name> -s user`.
pub fn mcp_remove_args(name: &str) -> Vec<String> {
    owned(&["mcp", "remove", name, "-s", "user"])
}

/// `claude plugin enable|disable <id> -s user --json`.
pub fn plugin_toggle_args(id: &str, enable: bool) -> Vec<String> {
    let verb = if enable { "enable" } else { "disable" };
    owned(&["plugin", verb, id, "-s", "user", "--json"])
}

/// `claude plugin install <id> -s user`.
pub fn plugin_install_args(id: &str) -> Vec<String> {
    owned(&["plugin", "install", id, "-s", "user"])
}

/// `claude plugin uninstall <id> -s user`.
pub fn plugin_uninstall_args(id: &str) -> Vec<String> {
    owned(&["plugin", "uninstall", id, "-s", "user"])
}

/// `claude plugin marketplace add <source> --scope user --json`.
pub fn marketplace_add_args(source: &str) -> Vec<String> {
    owned(&[
        "plugin",
        "marketplace",
        "add",
        source,
        "--scope",
        "user",
        "--json",
    ])
}

/// `claude plugin marketplace remove <name> --scope user` : jamais sans `--scope`.
pub fn marketplace_remove_args(name: &str) -> Vec<String> {
    owned(&["plugin", "marketplace", "remove", name, "--scope", "user"])
}

/// `claude plugin marketplace update [name]` : tous les marketplaces si `name` est absent.
pub fn marketplace_update_args(name: Option<&str>) -> Vec<String> {
    let mut args = owned(&["plugin", "marketplace", "update"]);
    args.extend(name.map(str::to_string));
    args
}

/// `claude plugin list --json`.
pub fn plugin_list_args() -> Vec<String> {
    owned(&["plugin", "list", "--json"])
}

/// `claude plugin marketplace list --json`.
pub fn marketplace_list_args() -> Vec<String> {
    owned(&["plugin", "marketplace", "list", "--json"])
}

/// Un échec du CLI : code de sortie (absent si le lancement a échoué ou si le processus a été tué) et stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub code: Option<i32>,
    pub stderr: String,
}

/// Lance `bin args…` et rend stdout sur code 0. Bloquant : à appeler hors du fil d'interface.
pub fn run_claude(bin: &Path, args: &[String]) -> Result<String, CliError> {
    let output = Command::new(bin)
        .args(args)
        .output()
        .map_err(|error| CliError {
            code: None,
            stderr: format!("{}: {error}", bin.display()),
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(CliError {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Le texte d'une erreur du CLI : son stderr tel quel ; vide, une phrase qui dit au moins que l'appel a échoué.
pub fn describe_failure(error: &CliError) -> String {
    let text = error.stderr.trim_end();
    if !text.is_empty() {
        return text.to_string();
    }
    match error.code {
        Some(code) => format!("claude a échoué (code {code})."),
        None => "claude s'est arrêté sans code de sortie.".to_string(),
    }
}

/// Édite un serveur MCP : `remove` puis `add-json new`. Si l'ajout échoue, l'ancien bloc est remis (`add-json old`)
/// et l'erreur de l'ajout est rendue ; si la restauration échoue aussi, la ligne « Ancien bloc non restauré : » et le
/// stderr de la restauration suivent celui de l'ajout. Si `remove` échoue, rien d'autre ne part.
pub fn edit_mcp_server(
    run: &mut dyn FnMut(&[String]) -> Result<String, CliError>,
    name: &str,
    old: &Value,
    new: &Value,
) -> Result<(), CliError> {
    run(&mcp_remove_args(name))?;
    if let Err(mut error) = run(&mcp_add_args(name, &new.to_string())) {
        // C'est l'erreur de l'ajout que l'utilisateur doit voir en premier ; une restauration ratée s'ajoute dessous,
        // sinon il croirait l'ancien bloc en place alors que le serveur a disparu.
        if let Err(restore) = run(&mcp_add_args(name, &old.to_string())) {
            error.stderr = format!(
                "{}\nAncien bloc non restauré : {}",
                describe_failure(&error),
                describe_failure(&restore)
            );
        }
        return Err(error);
    }
    Ok(())
}

/// Résumé d'une ligne d'un serveur MCP : l'`url` pour http/sse, sinon la commande suivie de ses arguments, chaque
/// chemin absolu raccourci à `…/<nom de fichier>`.
pub fn mcp_summary(server: &Value) -> String {
    if let Some(url) = server["url"].as_str() {
        return url.to_string();
    }
    let mut parts: Vec<String> = server["command"]
        .as_str()
        .map(str::to_string)
        .into_iter()
        .collect();
    if let Some(args) = server["args"].as_array() {
        parts.extend(args.iter().filter_map(Value::as_str).map(shorten_path));
    }
    parts.join(" ")
}

/// Un chemin absolu devient `…/<nom de fichier>` ; tout autre argument reste tel quel.
fn shorten_path(arg: &str) -> String {
    let path = Path::new(arg);
    match path.file_name().and_then(OsStr::to_str) {
        Some(file) if path.is_absolute() => format!("…/{file}"),
        _ => arg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("idema-claude-cli-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn missing_file_or_key_reads_as_no_servers() {
        let dir = temp_dir("no-servers");
        assert_eq!(
            read_mcp_servers(&dir.join("absent.json")),
            Ok(BTreeMap::new())
        );

        let no_key = dir.join("no-key.json");
        write_file(&no_key, r#"{"projects":{}}"#);
        assert_eq!(read_mcp_servers(&no_key), Ok(BTreeMap::new()));

        let invalid = dir.join("invalid.json");
        write_file(&invalid, "{");
        assert!(read_mcp_servers(&invalid).is_err());
    }

    #[test]
    fn only_root_mcp_servers_are_read() {
        let dir = temp_dir("root-only");
        let file = dir.join(".claude.json");
        write_file(
            &file,
            r#"{"mcpServers":{"a":{"type":"http","url":"https://a/mcp"}},
                "projects":{"/x":{"mcpServers":{"b":{"command":"bun"}}}}}"#,
        );
        let servers = read_mcp_servers(&file).unwrap();
        assert_eq!(servers.keys().collect::<Vec<_>>(), ["a"]);
        assert_eq!(servers["a"]["url"], "https://a/mcp");
    }

    const PLUGINS: &str = r#"[
        {"id":"b@m","version":"1","scope":"user","enabled":true,"installPath":"/p","projectEnabled":false},
        {"id":"a@m","version":"1","scope":"user","enabled":false,"installPath":"/p","projectEnabled":false},
        {"id":"a@m","version":"1","scope":"local","enabled":true,"installPath":"/p","projectPath":"/x"},
        {"id":"c@m","version":"1","scope":"user","enabled":true,"installPath":"/p","projectEnabled":false}
    ]"#;

    #[test]
    fn plugins_keep_user_scope_once_sorted_enabled_first() {
        let plugins = parse_plugins(PLUGINS).unwrap();
        let ids: Vec<&str> = plugins.iter().map(|plugin| plugin.id.as_str()).collect();
        assert_eq!(ids, ["b@m", "c@m", "a@m"]);
        let a = plugins.iter().find(|plugin| plugin.id == "a@m").unwrap();
        assert!(!a.enabled);
        assert_eq!(a.name, "a");
        assert_eq!(a.marketplace, "m");
        assert!(parse_plugins("not json").is_err());
    }

    #[test]
    fn marketplaces_are_removable_only_when_user_declared() {
        let json = r#"[
            {"name":"erom-marketplace","source":"github","repo":"eRom/erom-marketplace","installLocation":"/i/1"},
            {"name":"claude-plugins-official","source":"github","repo":"anthropics/claude-plugins-official","installLocation":"/i/2"},
            {"name":"local-one","source":"directory","path":"/some/dir","installLocation":"/i/3"},
            {"name":"remote-one","source":"git","url":"https://x/y.git","installLocation":"/i/4"},
            {"name":"bare-one","source":"settings","installLocation":"/i/5"}
        ]"#;
        let declared: BTreeSet<String> = ["erom-marketplace".to_string()].into();
        let entries = parse_marketplaces(json, &declared).unwrap();
        let removable: Vec<&str> = entries
            .iter()
            .filter(|entry| entry.removable)
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(removable, ["erom-marketplace"]);
        assert_eq!(entries[0].location, "eRom/erom-marketplace");
        assert_eq!(entries[0].source, "github");
        assert_eq!(entries[2].location, "/some/dir");
        assert_eq!(entries[3].location, "https://x/y.git");
        assert_eq!(entries[4].location, "/i/5");
        assert!(parse_marketplaces("{", &declared).is_err());
    }

    #[test]
    fn user_declared_marketplaces_come_from_extra_known_marketplaces() {
        let dir = temp_dir("declared");
        let file = dir.join("settings.json");
        write_file(
            &file,
            r#"{"extraKnownMarketplaces":{"one":{},"two":{}},"other":1}"#,
        );
        let declared = user_declared_marketplaces(&file);
        assert_eq!(declared.into_iter().collect::<Vec<_>>(), ["one", "two"]);
        assert!(user_declared_marketplaces(&dir.join("absent.json")).is_empty());
        write_file(&file, r#"{"hooks":{}}"#);
        assert!(user_declared_marketplaces(&file).is_empty());
    }

    #[test]
    fn plugin_id_needs_name_at_marketplace() {
        assert_eq!(validate_plugin_id("ok@m"), Ok(()));
        for bad in ["erom-devil", "a b@c", "@m", "n@", "a@b@c"] {
            assert!(validate_plugin_id(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn a_leading_dash_is_refused_for_every_argument_kind() {
        for argument in [CliArgument::Name, CliArgument::Id, CliArgument::Source] {
            for bad in ["-x", "--help", "--help@m", "-"] {
                let message = reject_leading_dash(bad, argument).unwrap_err();
                assert!(
                    message.ends_with("ne doit pas commencer par un tiret."),
                    "{message}"
                );
            }
            for good in ["a", "a-b", "x@m-1", "o/r", "", "a --b"] {
                assert_eq!(reject_leading_dash(good, argument), Ok(()), "{good}");
            }
        }
    }

    #[test]
    fn claude_override_wins_then_first_executable_candidate() {
        let home = temp_dir("resolve-home");
        let fake = home.join(".local/bin/claude");
        write_file(&fake, "#!/bin/sh\n");
        // Non exécutable : écarté.
        assert_ne!(resolve_claude(&home, None), Ok(fake.clone()));
        make_executable(&fake);
        assert_eq!(resolve_claude(&home, None), Ok(fake.clone()));

        let other = home.join("elsewhere/claude");
        write_file(&other, "#!/bin/sh\n");
        assert_eq!(
            resolve_claude(&home, Some(other.as_os_str())),
            Ok(other.clone())
        );
        // Un override vide ne compte pas.
        assert_eq!(resolve_claude(&home, Some(OsStr::new(""))), Ok(fake));

        let empty_home = temp_dir("resolve-empty");
        let tried = match resolve_claude(&empty_home, None) {
            Err(tried) => tried,
            // Une machine qui porte un vrai claude dans un candidat absolu : rien à vérifier ici.
            Ok(_) => return,
        };
        assert_eq!(tried.len(), 3);
        assert_eq!(tried[0], empty_home.join(".local/bin/claude"));
        assert_eq!(tried[1], PathBuf::from("/opt/homebrew/bin/claude"));
        assert_eq!(tried[2], PathBuf::from("/usr/local/bin/claude"));
    }

    type Calls = Rc<RefCell<Vec<Vec<String>>>>;

    /// Runner scripté : enregistre les appels, échoue sur ceux dont l'index figure dans `failing`.
    fn scripted(
        calls: &Calls,
        failing: &[usize],
    ) -> impl FnMut(&[String]) -> Result<String, CliError> {
        let failing = failing.to_vec();
        let calls = Rc::clone(calls);
        move |args: &[String]| {
            let index = calls.borrow().len();
            calls.borrow_mut().push(args.to_vec());
            if failing.contains(&index) {
                Err(CliError {
                    code: Some(1),
                    stderr: format!("boom {index}"),
                })
            } else {
                Ok(String::new())
            }
        }
    }

    #[test]
    fn mcp_edit_restores_the_old_block_when_the_add_fails() {
        let old = serde_json::json!({"type":"http","url":"https://old/mcp"});
        let new = serde_json::json!({"type":"http","url":"https://new/mcp"});
        let calls = Calls::default();
        let mut run = scripted(&calls, &[1]);
        let error = edit_mcp_server(&mut run, "srv", &old, &new).unwrap_err();
        assert_eq!(error.stderr, "boom 1");
        assert_eq!(calls.borrow().len(), 3);
        assert_eq!(calls.borrow()[0], mcp_remove_args("srv"));
        assert_eq!(calls.borrow()[1], mcp_add_args("srv", &new.to_string()));
        assert_eq!(calls.borrow()[2], mcp_add_args("srv", &old.to_string()));
    }

    #[test]
    fn mcp_edit_reports_a_failed_restore() {
        let old = serde_json::json!({"command":"a"});
        let new = serde_json::json!({"command":"b"});
        let calls = Calls::default();
        // Le retrait passe, l'ajout (appel 1) et la restauration (appel 2) échouent.
        let mut run = scripted(&calls, &[1, 2]);
        let error = edit_mcp_server(&mut run, "srv", &old, &new).unwrap_err();
        assert_eq!(calls.borrow().len(), 3);
        assert_eq!(error.stderr, "boom 1\nAncien bloc non restauré : boom 2");
        assert_eq!(error.code, Some(1));
    }

    #[test]
    fn mcp_edit_keeps_a_fallback_text_for_silent_failures() {
        let old = serde_json::json!({"command":"a"});
        let new = serde_json::json!({"command":"b"});
        // Le retrait passe, l'ajout et la restauration échouent sans rien écrire sur stderr.
        let mut index = 0;
        let mut run = |_: &[String]| {
            index += 1;
            match index {
                1 => Ok(String::new()),
                2 => Err(CliError {
                    code: Some(1),
                    stderr: String::new(),
                }),
                _ => Err(CliError {
                    code: None,
                    stderr: "\n".into(),
                }),
            }
        };
        let error = edit_mcp_server(&mut run, "srv", &old, &new).unwrap_err();
        assert_eq!(
            error.stderr,
            "claude a échoué (code 1).\nAncien bloc non restauré : claude s'est arrêté sans code de sortie."
        );
    }

    #[test]
    fn mcp_edit_stops_when_the_remove_fails() {
        let old = serde_json::json!({"command":"a"});
        let new = serde_json::json!({"command":"b"});
        let calls = Calls::default();
        let mut run = scripted(&calls, &[0]);
        let error = edit_mcp_server(&mut run, "srv", &old, &new).unwrap_err();
        assert_eq!(error.stderr, "boom 0");
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn mcp_edit_succeeds_with_remove_then_add() {
        let old = serde_json::json!({"command":"a"});
        let new = serde_json::json!({"command":"b"});
        let calls = Calls::default();
        let mut run = scripted(&calls, &[]);
        edit_mcp_server(&mut run, "srv", &old, &new).unwrap();
        assert_eq!(calls.borrow().len(), 2);
    }

    #[test]
    fn write_commands_always_carry_the_user_scope() {
        let writes = [
            mcp_add_args("n", "{}"),
            mcp_remove_args("n"),
            plugin_toggle_args("a@b", true),
            plugin_toggle_args("a@b", false),
            plugin_install_args("a@b"),
            plugin_uninstall_args("a@b"),
            marketplace_add_args("o/r"),
            marketplace_remove_args("n"),
        ];
        for args in writes {
            let scoped = args
                .windows(2)
                .any(|pair| (pair[0] == "-s" || pair[0] == "--scope") && pair[1] == "user");
            assert!(scoped, "user scope missing from {args:?}");
        }
        assert!(marketplace_remove_args("n").contains(&"--scope".to_string()));
        assert_eq!(plugin_toggle_args("a@b", true)[1], "enable");
        assert_eq!(plugin_toggle_args("a@b", false)[1], "disable");
        assert_eq!(
            marketplace_update_args(None),
            ["plugin", "marketplace", "update"]
        );
        assert_eq!(
            marketplace_update_args(Some("m")),
            ["plugin", "marketplace", "update", "m"]
        );
        assert_eq!(plugin_list_args(), ["plugin", "list", "--json"]);
        assert_eq!(
            marketplace_list_args(),
            ["plugin", "marketplace", "list", "--json"]
        );
    }

    #[test]
    fn summary_shows_url_or_short_command() {
        let http = serde_json::json!({"type":"http","url":"https://x/mcp"});
        assert_eq!(mcp_summary(&http), "https://x/mcp");
        let stdio = serde_json::json!({"type":"stdio","command":"bun","args":["run","/a/b/query-memory.ts"]});
        assert_eq!(mcp_summary(&stdio), "bun run …/query-memory.ts");
        let bare = serde_json::json!({"command":"npx"});
        assert_eq!(mcp_summary(&bare), "npx");
    }

    #[test]
    fn run_claude_returns_stdout_on_success_and_the_error_otherwise() {
        // `/bin/sh` tient lieu de binaire : jamais le vrai `claude`.
        let sh = Path::new("/bin/sh");
        let ok = run_claude(sh, &["-c".into(), "printf out".into()]).unwrap();
        assert_eq!(ok, "out");
        let error = run_claude(sh, &["-c".into(), "printf oops >&2; exit 3".into()]).unwrap_err();
        assert_eq!(error.code, Some(3));
        assert_eq!(error.stderr, "oops");
        let missing = run_claude(Path::new("/nonexistent/claude"), &[]).unwrap_err();
        assert_eq!(missing.code, None);
        assert!(!missing.stderr.is_empty());
    }
}
