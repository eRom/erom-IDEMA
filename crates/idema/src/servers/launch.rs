//! Lecture de la configuration des serveurs d'un projet : `.claude/launch.json` (le format de Claude Desktop, avec
//! commentaires) et, à défaut, le script `dev` de `package.json`. IdemA ne l'écrit jamais.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Un serveur configuré, prêt à être lancé : `program` et `args` sont la commande finale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    /// Chemin absolu du dossier de lancement.
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// `None` pour l'entrée `dev` de `package.json`, dont le port se découvre au lancement.
    pub port: Option<u16>,
    /// L'adresse qu'ouvre « ouvrir », à la place de `http://localhost:<port>`.
    pub url: Option<String>,
    pub source: EntrySource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntrySource {
    LaunchJson,
    /// Le texte du script `dev` de `package.json`, qui sert à reconnaître un serveur déjà lancé.
    PackageDev {
        script: String,
    },
}

/// Une entrée ou un fichier illisible, montré tel quel sur la page d'accueil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryError {
    pub file: PathBuf,
    pub line: Option<usize>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectConfig {
    pub entries: Vec<ConfigEntry>,
    pub errors: Vec<EntryError>,
}

const LAUNCH_JSON: &str = ".claude/launch.json";
const WORKSPACE_FOLDER: &str = "${workspaceFolder}";

/// `launch.json` présent : ses entrées seules. Absent : l'entrée `dev` de `package.json`. Ni l'un ni l'autre : vide.
pub fn read_project_config(project: &Path) -> ProjectConfig {
    let launch = project.join(LAUNCH_JSON);
    match fs::read_to_string(&launch) {
        Ok(text) => return parse_launch_json(&text, project, &launch),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return ProjectConfig {
                entries: Vec::new(),
                errors: vec![EntryError {
                    file: launch,
                    line: None,
                    message: format!("fichier illisible : {error}"),
                }],
            };
        }
        Err(_) => {}
    }

    let package = project.join("package.json");
    let Ok(text) = fs::read_to_string(&package) else {
        return ProjectConfig::default();
    };
    let lockfiles: Vec<&str> = ["bun.lock", "bun.lockb", "pnpm-lock.yaml"]
        .into_iter()
        .filter(|name| project.join(name).exists())
        .collect();
    match parse_package_dev(&text, &lockfiles, project, &package) {
        Some(Ok(entry)) => ProjectConfig {
            entries: vec![entry],
            errors: Vec::new(),
        },
        Some(Err(error)) => ProjectConfig {
            entries: Vec::new(),
            errors: vec![error],
        },
        None => ProjectConfig::default(),
    }
}

/// Lit un `launch.json` : une entrée fautive donne une erreur avec sa ligne, les autres se lisent quand même. Un
/// fichier qui n'est pas du JSON donne une seule erreur et aucune entrée. Les champs inconnus sont ignorés.
pub fn parse_launch_json(text: &str, project: &Path, file: &Path) -> ProjectConfig {
    let cleaned = strip_jsonc(text);
    let root: Value = match serde_json::from_str(&cleaned) {
        Ok(root) => root,
        Err(error) => {
            return ProjectConfig {
                entries: Vec::new(),
                errors: vec![EntryError {
                    file: file.to_path_buf(),
                    line: Some(error.line()),
                    message: format!("JSON invalide : {error}"),
                }],
            };
        }
    };

    let file_error = |message: &str| ProjectConfig {
        entries: Vec::new(),
        errors: vec![EntryError {
            file: file.to_path_buf(),
            line: None,
            message: message.to_string(),
        }],
    };
    let Some(configurations) = root.get("configurations") else {
        return file_error("le tableau « configurations » est absent");
    };
    let Some(configurations) = configurations.as_array() else {
        return file_error("« configurations » doit être un tableau");
    };

    let lines = element_lines(&cleaned);
    let mut config = ProjectConfig::default();
    for (index, value) in configurations.iter().enumerate() {
        match read_entry(value, project) {
            Ok(entry) => config.entries.push(entry),
            Err(message) => config.errors.push(EntryError {
                file: file.to_path_buf(),
                line: lines.get(index).copied(),
                message,
            }),
        }
    }
    config
}

/// Le script `dev` de `package.json` : `None` sans script `dev`, une erreur si le fichier ou le script est illisible.
/// Le gestionnaire se déduit des fichiers de verrou présents (`lockfiles` : leurs noms, sans chemin).
pub fn parse_package_dev(
    package: &str,
    lockfiles: &[&str],
    project: &Path,
    file: &Path,
) -> Option<Result<ConfigEntry, EntryError>> {
    let error = |line: Option<usize>, message: String| EntryError {
        file: file.to_path_buf(),
        line,
        message,
    };
    let root: Value = match serde_json::from_str(package) {
        Ok(root) => root,
        Err(parse) => {
            return Some(Err(error(
                Some(parse.line()),
                format!("JSON invalide : {parse}"),
            )));
        }
    };
    let dev = root.get("scripts")?.get("dev")?;
    let Some(script) = dev.as_str() else {
        return Some(Err(error(
            None,
            "le script « dev » doit être une chaîne".to_string(),
        )));
    };

    let manager = if lockfiles
        .iter()
        .any(|name| matches!(*name, "bun.lock" | "bun.lockb"))
    {
        "bun"
    } else if lockfiles.contains(&"pnpm-lock.yaml") {
        "pnpm"
    } else {
        "npm"
    };
    Some(Ok(ConfigEntry {
        name: "dev".to_string(),
        program: manager.to_string(),
        args: vec!["run".to_string(), "dev".to_string()],
        cwd: project.to_path_buf(),
        env: Vec::new(),
        port: None,
        url: None,
        source: EntrySource::PackageDev {
            script: script.to_string(),
        },
    }))
}

/// Retire les commentaires `//…` et `/*…*/` hors des chaînes, puis les virgules finales avant `]` ou `}`. Les sauts
/// de ligne sont gardés : les numéros de ligne des erreurs restent ceux du fichier.
pub fn strip_jsonc(text: &str) -> String {
    strip_trailing_commas(&strip_comments(text))
}

fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                while chars.peek().is_some_and(|next| *next != '\n') {
                    chars.next();
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = ' ';
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                    }
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
                // Un commentaire sépare deux jetons, comme une espace.
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

fn strip_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        index += 1;
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(escaped) = chars.get(index) {
                    out.push(*escaped);
                    index += 1;
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ',' => {
                let next = chars[index..].iter().find(|next| !next.is_whitespace());
                out.push(if matches!(next, Some(']' | '}')) {
                    ' '
                } else {
                    ','
                });
            }
            _ => out.push(c),
        }
    }
    out
}

/// La ligne (à partir de 1) du premier caractère de chaque élément du tableau `configurations` du texte nettoyé.
fn element_lines(cleaned: &str) -> Vec<usize> {
    let mut lines = Vec::new();
    let mut stack: Vec<char> = Vec::new();
    let mut line = 1;
    let mut in_string = false;
    let mut escaped = false;
    let mut current_string = String::new();
    let mut last_string = String::new();
    let mut key = String::new();
    let mut in_configurations = false;
    let mut expect_element = false;

    for c in cleaned.chars() {
        if c == '\n' {
            line += 1;
        }
        if in_string {
            if escaped {
                escaped = false;
                current_string.push(c);
            } else if c == '\\' {
                escaped = true;
                current_string.push(c);
            } else if c == '"' {
                in_string = false;
                last_string = std::mem::take(&mut current_string);
            } else {
                current_string.push(c);
            }
            continue;
        }
        if c.is_whitespace() {
            continue;
        }
        let at_element_level = in_configurations && stack.len() == 2;
        if at_element_level && expect_element && c != ',' && c != ']' {
            lines.push(line);
            expect_element = false;
        }
        match c {
            '"' => in_string = true,
            ':' if stack.len() == 1 => key = last_string.clone(),
            ',' if at_element_level => expect_element = true,
            '{' | '[' => {
                if c == '[' && stack.len() == 1 && key == "configurations" {
                    in_configurations = true;
                    expect_element = true;
                }
                stack.push(c);
            }
            '}' | ']' => {
                stack.pop();
                if stack.len() == 1 {
                    in_configurations = false;
                }
            }
            _ => {}
        }
    }
    lines
}

fn read_entry(value: &Value, project: &Path) -> Result<ConfigEntry, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "l'entrée doit être un objet".to_string())?;

    let name = optional_string(object, "name")?
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "le champ « name » est absent".to_string())?;
    let port = match object.get("port") {
        None => return Err("le champ « port » est absent".to_string()),
        Some(port) => port
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| "« port » doit être un entier entre 1 et 65535".to_string())?,
    };

    // `program` remplace `runtimeExecutable` : la commande devient `node <program> <args…>`.
    let (program, args) = if let Some(script) = optional_string(object, "program")? {
        let mut args = vec![script.to_string()];
        args.extend(string_list(object, "args")?);
        ("node".to_string(), args)
    } else if let Some(executable) = optional_string(object, "runtimeExecutable")? {
        (executable.to_string(), string_list(object, "runtimeArgs")?)
    } else {
        return Err("« runtimeExecutable » ou « program » est absent".to_string());
    };

    let cwd = resolve_cwd(optional_string(object, "cwd")?, project);
    let env = read_env(object)?;
    let url = optional_string(object, "url")?.map(str::to_string);

    Ok(ConfigEntry {
        name: name.to_string(),
        program,
        args,
        cwd,
        env,
        port: Some(port),
        url,
        source: EntrySource::LaunchJson,
    })
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(format!("« {key} » doit être une chaîne")),
    }
}

fn string_list(object: &Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("« {key} » ne doit contenir que des chaînes"))
            })
            .collect(),
        Some(_) => Err(format!("« {key} » doit être un tableau de chaînes")),
    }
}

fn read_env(object: &Map<String, Value>) -> Result<Vec<(String, String)>, String> {
    match object.get("env") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(variables)) => variables
            .iter()
            .map(|(name, value)| {
                value
                    .as_str()
                    .map(|value| (name.clone(), value.to_string()))
                    .ok_or_else(|| {
                        format!("la variable « {name} » de « env » doit être une chaîne")
                    })
            })
            .collect(),
        Some(_) => Err("« env » doit être un objet".to_string()),
    }
}

/// `${workspaceFolder}` vaut la racine du projet où qu'il apparaisse ; un chemin relatif part de la racine, un chemin
/// absolu reste tel quel.
fn resolve_cwd(raw: Option<&str>, project: &Path) -> PathBuf {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return project.to_path_buf();
    };
    let expanded = PathBuf::from(raw.replace(WORKSPACE_FOLDER, &project.to_string_lossy()));
    if expanded.is_absolute() {
        expanded
    } else {
        project.join(expanded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "/work/cv";

    fn project() -> PathBuf {
        PathBuf::from(PROJECT)
    }

    fn launch_file() -> PathBuf {
        project().join(LAUNCH_JSON)
    }

    fn parse(text: &str) -> ProjectConfig {
        parse_launch_json(text, &project(), &launch_file())
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("idema-launch-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_claude_desktop_file_reads_every_complete_entry() {
        let text = r#"{
  "version": "0.0.1",
  "configurations": [
    { "name": "cv-dev", "runtimeExecutable": "npm", "runtimeArgs": ["run", "dev"], "port": 3100, "autoPort": false },
    { "name": "cv-prod", "runtimeExecutable": "npm", "runtimeArgs": ["run", "start"], "port": 3101 }
  ]
}"#;
        let config = parse(text);

        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert_eq!(config.entries.len(), 2);
        let first = &config.entries[0];
        assert_eq!(first.name, "cv-dev");
        assert_eq!(first.program, "npm");
        assert_eq!(first.args, ["run", "dev"]);
        assert_eq!(first.port, Some(3100));
        assert_eq!(first.cwd, project());
        assert_eq!(first.source, EntrySource::LaunchJson);
        assert_eq!(config.entries[1].port, Some(3101));
    }

    #[test]
    fn comments_and_trailing_commas_are_accepted() {
        let text = r#"{
  // commentaire de ligne
  "configurations": [
    /* bloc
       sur deux lignes */
    {
      "name": "web", // après une valeur
      "runtimeExecutable": "npm",
      "runtimeArgs": ["run", "dev",],
      "port": 3000,
      "url": "http://a//b",
    },
  ],
}"#;
        let config = parse(text);

        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert_eq!(config.entries.len(), 1);
        assert_eq!(config.entries[0].url.as_deref(), Some("http://a//b"));
        assert_eq!(config.entries[0].args, ["run", "dev"]);
    }

    #[test]
    fn stripping_keeps_strings_and_line_breaks() {
        let text = "{\n  \"a\": \"x,]\", // c\n  /* b\n c */ \"b\": [1,\n],\n}";
        let cleaned = strip_jsonc(text);

        assert_eq!(cleaned.lines().count(), text.lines().count());
        let value: Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(value["a"], "x,]");
        assert_eq!(value["b"], serde_json::json!([1]));
    }

    #[test]
    fn cwd_env_program_and_url_change_the_launch() {
        let text = r#"{ "configurations": [
  { "name": "api", "program": "server.js", "args": ["--x"], "port": 4000,
    "cwd": "${workspaceFolder}/web", "env": {"NODE_ENV": "development"}, "url": "http://localhost:4000/app" }
] }"#;
        let config = parse(text);

        assert!(config.errors.is_empty(), "{:?}", config.errors);
        let entry = &config.entries[0];
        assert_eq!(entry.cwd, project().join("web"));
        assert!(
            entry
                .env
                .contains(&("NODE_ENV".to_string(), "development".to_string()))
        );
        assert_eq!(entry.program, "node");
        assert_eq!(entry.args, ["server.js", "--x"]);
        assert_eq!(entry.url.as_deref(), Some("http://localhost:4000/app"));
    }

    #[test]
    fn a_relative_cwd_starts_at_the_project_and_an_absolute_one_is_kept() {
        let entry = |cwd: &str| {
            let text = format!(
                r#"{{ "configurations": [ {{ "name": "a", "runtimeExecutable": "x", "port": 1, "cwd": "{cwd}" }} ] }}"#
            );
            parse(&text).entries.remove(0).cwd
        };

        assert_eq!(entry("apps/web"), project().join("apps/web"));
        assert_eq!(entry("/srv/site"), PathBuf::from("/srv/site"));
        assert_eq!(entry("${workspaceFolder}"), project());
    }

    #[test]
    fn an_incomplete_entry_gives_an_error_with_its_line_and_the_others_still_read() {
        let text = r#"{
  "configurations": [
    { "name": "a", "runtimeExecutable": "npm", "port": 3000 },
    { "name": "b", "runtimeExecutable": "npm" },
    { "name": "c", "runtimeExecutable": "npm", "port": 3002 }
  ]
}"#;
        let config = parse(text);

        let names: Vec<&str> = config.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a", "c"]);
        assert_eq!(config.errors.len(), 1);
        assert_eq!(config.errors[0].line, Some(4));
        assert_eq!(config.errors[0].file, launch_file());
        assert!(config.errors[0].message.contains("port"));
    }

    #[test]
    fn a_bad_port_or_a_non_string_env_value_makes_the_entry_an_error() {
        let entry = |fields: &str| {
            let text = format!(
                r#"{{ "configurations": [ {{ "name": "a", "runtimeExecutable": "x", {fields} }} ] }}"#
            );
            parse(&text)
        };

        for fields in [
            r#""port": 0"#,
            r#""port": 65536"#,
            r#""port": -3"#,
            r#""port": 3000.5"#,
            r#""port": "3000""#,
            r#""port": 3000, "env": {"N": 1}"#,
        ] {
            let config = entry(fields);
            assert!(config.entries.is_empty(), "{fields}");
            assert_eq!(config.errors.len(), 1, "{fields}");
            assert_eq!(config.errors[0].line, Some(1), "{fields}");
        }
        assert_eq!(entry(r#""port": 65535"#).entries.len(), 1);
    }

    #[test]
    fn invalid_json_gives_one_error_and_no_entry() {
        let config = parse("{\n  \"configurations\": [\n    { \"name\": \"a\",");

        assert!(config.entries.is_empty());
        assert_eq!(config.errors.len(), 1);
        assert_eq!(config.errors[0].line, Some(3));
    }

    #[test]
    fn the_dev_script_follows_the_lockfile() {
        let package = r#"{ "name": "beta", "scripts": { "dev": "python3 -m http.server 8813" } }"#;
        let read = |lockfiles: &[&str]| {
            parse_package_dev(
                package,
                lockfiles,
                &project(),
                &project().join("package.json"),
            )
            .expect("un script dev")
            .expect("une entrée")
        };

        let bun = read(&["bun.lock"]);
        assert_eq!(
            (bun.program.as_str(), bun.args.as_slice()),
            ("bun", ["run".to_string(), "dev".to_string()].as_slice())
        );
        assert_eq!(read(&["bun.lockb"]).program, "bun");
        assert_eq!(read(&["pnpm-lock.yaml"]).program, "pnpm");
        let npm = read(&[]);
        assert_eq!(npm.program, "npm");
        assert_eq!(npm.name, "dev");
        assert_eq!(npm.port, None);
        assert_eq!(npm.cwd, project());
        assert_eq!(
            npm.source,
            EntrySource::PackageDev {
                script: "python3 -m http.server 8813".to_string()
            }
        );
    }

    #[test]
    fn a_package_without_dev_script_gives_nothing() {
        let file = project().join("package.json");
        let read = |package: &str| parse_package_dev(package, &[], &project(), &file);

        assert!(read(r#"{ "name": "x" }"#).is_none());
        assert!(read(r#"{ "scripts": { "build": "tsc" } }"#).is_none());
        assert!(read(r#"{ "scripts": { "dev": 3 } }"#).is_some_and(|result| result.is_err()));
        assert!(read("{ nope").is_some_and(|result| result.is_err()));
    }

    #[test]
    fn launch_json_wins_over_package_json() {
        let dir = temp_dir("wins");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(".claude/launch.json"),
            r#"{ "configurations": [ { "name": "web", "runtimeExecutable": "npm", "port": 3000 } ] }"#,
        )
        .unwrap();
        fs::write(
            dir.join("package.json"),
            r#"{ "scripts": { "dev": "next dev" } }"#,
        )
        .unwrap();

        let config = read_project_config(&dir);

        let names: Vec<&str> = config.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["web"]);
        assert_eq!(config.entries[0].source, EntrySource::LaunchJson);
        assert!(config.errors.is_empty());
    }

    #[test]
    fn without_launch_json_the_dev_script_is_read_and_an_empty_folder_gives_nothing() {
        let dir = temp_dir("fallback");
        assert_eq!(read_project_config(&dir), ProjectConfig::default());

        fs::write(
            dir.join("package.json"),
            r#"{ "scripts": { "dev": "next dev" } }"#,
        )
        .unwrap();
        fs::write(dir.join("bun.lock"), "").unwrap();
        let config = read_project_config(&dir);

        assert_eq!(config.entries.len(), 1);
        assert_eq!(config.entries[0].program, "bun");
        assert_eq!(config.entries[0].cwd, dir);
    }
}
