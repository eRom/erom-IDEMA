//! Lecture des sources sur disque qui disent qui tourne où : fichiers de session de Claude Code, manifestes de la
//! Caserne, environnement de lancement d'un processus, agents connus d'idnma. Du parsing pur, aucun effet de bord.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::restore::claude::is_uuid;

/// Une session Claude Code interactive vivante, telle que son fichier `~/.claude/sessions/<pid>.json` la désigne.
/// Contrairement à `ClaudeSession`, aucun pane tmux n'est exigé : c'est justement une session hors d'IdemA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    /// PID du processus `claude`.
    pub pid: i32,
    /// Identifiant de la conversation, toujours un UUID : il sera tapé dans un shell.
    pub session_id: String,
    pub cwd: PathBuf,
    pub name: Option<String>,
    /// Date de démarrage du processus telle que Claude l'écrit (`procStart`) : sert à reconnaître un PID réutilisé.
    pub proc_start: String,
}

/// Un vrai fichier de session pèse quelques centaines d'octets ; au-delà, il n'est pas lu.
pub(crate) const MAX_SESSION_FILE_BYTES: u64 = 64 * 1024;

/// Lit un fichier de session vivante. `None` si le JSON est cassé, si `kind` n'est pas `interactive`, si `entrypoint`
/// n'est pas `cli` (`sdk-cli` est un Dispatch, hors périmètre après les sondes réelles), si `sessionId` n'est pas un
/// UUID, si `pid` n'est pas strictement positif, si `cwd` ou `procStart` manquent, ou si `expected_pid` est donné et
/// diffère de `pid` (le nom du fichier doit dire la vérité).
pub fn parse_live_session(text: &str, expected_pid: Option<i32>) -> Option<LiveSession> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    // Un PID est strictement positif : 0 et les négatifs visent des groupes de processus, jamais un `claude`.
    let pid = i32::try_from(value.get("pid")?.as_i64()?)
        .ok()
        .filter(|pid| *pid > 0)?;
    if expected_pid.is_some_and(|expected| expected != pid) {
        return None;
    }
    if value.get("kind")?.as_str()? != "interactive" {
        return None;
    }
    if value.get("entrypoint")?.as_str()? != "cli" {
        return None;
    }
    let session_id = value.get("sessionId")?.as_str()?;
    if !is_uuid(session_id) {
        return None;
    }
    let cwd = value.get("cwd")?.as_str().filter(|cwd| !cwd.is_empty())?;
    let proc_start = value
        .get("procStart")?
        .as_str()
        .filter(|start| !start.is_empty())?;
    let name = value
        .get("name")
        .and_then(|name| name.as_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    Some(LiveSession {
        pid,
        session_id: session_id.to_string(),
        cwd: PathBuf::from(cwd),
        name,
        proc_start: proc_start.to_string(),
    })
}

/// Un processus qui déclare tenir une conversation, quel que soit son genre (`interactive`, `background`...) et son
/// point d'entrée (`cli`, `sdk-cli`...). C'est la vue large : elle sert à savoir qui écrit déjà dans une conversation,
/// pas à lister des sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHolder {
    pub pid: i32,
    pub session_id: String,
    /// Absent d'un fichier qui ne le porte pas : le recoupement avec le processus vivant n'est alors pas possible.
    pub proc_start: Option<String>,
}

/// Lit un fichier de session sans filtrer sur `kind` ni `entrypoint`. `None` si le JSON est cassé, si `pid` n'est pas
/// strictement positif ou si `sessionId` n'est pas un UUID. Un `procStart` absent ou vide vaut `None`.
pub fn parse_session_holder(text: &str) -> Option<SessionHolder> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let pid = i32::try_from(value.get("pid")?.as_i64()?)
        .ok()
        .filter(|pid| *pid > 0)?;
    let session_id = value.get("sessionId")?.as_str()?;
    if !is_uuid(session_id) {
        return None;
    }
    let proc_start = value
        .get("procStart")
        .and_then(|start| start.as_str())
        .filter(|start| !start.is_empty())
        .map(str::to_string);
    Some(SessionHolder {
        pid,
        session_id: session_id.to_string(),
        proc_start,
    })
}

/// Lit les `<pid>.json` du dossier avec `parse` ; tout fichier illisible, étranger ou dont le nom ne dit pas le `pid`
/// est ignoré. `parse` reçoit le texte et le `pid` du nom du fichier.
fn read_session_dir<T>(dir: &Path, parse: impl Fn(&str, i32) -> Option<T>) -> Vec<T> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        // `file_type` ne suit pas les liens : un lien vers `/dev/zero`, un FIFO ou un dossier sont écartés.
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| {
            entry
                .metadata()
                .is_ok_and(|meta| meta.len() <= MAX_SESSION_FILE_BYTES)
        })
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| {
            let expected_pid = path.file_stem()?.to_str()?.parse::<i32>().ok()?;
            let text = std::fs::read_to_string(&path).ok()?;
            parse(&text, expected_pid)
        })
        .collect()
}

/// Lit les `<pid>.json` du dossier ; tout fichier illisible, étranger ou dont le nom ne dit pas le `pid` est ignoré.
pub fn read_live_sessions(dir: &Path) -> Vec<LiveSession> {
    read_session_dir(dir, |text, pid| parse_live_session(text, Some(pid)))
}

/// Comme `read_live_sessions`, sans filtre sur le genre ni le point d'entrée : tous ceux qui déclarent une conversation.
pub fn read_session_holders(dir: &Path) -> Vec<SessionHolder> {
    read_session_dir(dir, |text, pid| {
        parse_session_holder(text).filter(|holder| holder.pid == pid)
    })
}

/// Le manifeste d'une session lancée par la Caserne : l'agent, son moteur et son état.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaserneManifest {
    pub agent: String,
    /// `None` quand le manifeste porte `"engine": null`.
    pub engine: Option<String>,
    pub model: Option<String>,
    pub pid: i32,
    /// Vrai tant que `ended_at` est nul ou absent.
    pub open: bool,
}

/// Lit un manifeste de la Caserne. `None` si le JSON est cassé, si `harness` n'est pas `claude`, si `agent` manque ou
/// si `pid` n'est pas strictement positif.
pub fn parse_manifest(text: &str) -> Option<CaserneManifest> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("harness")?.as_str()? != "claude" {
        return None;
    }
    let pid = i32::try_from(value.get("pid")?.as_i64()?)
        .ok()
        .filter(|pid| *pid > 0)?;
    let agent = value
        .get("agent")?
        .as_str()
        .filter(|agent| !agent.is_empty())?;
    let optional = |key: &str| {
        value
            .get(key)
            .and_then(|field| field.as_str())
            .map(str::to_string)
    };
    Some(CaserneManifest {
        agent: agent.to_string(),
        engine: optional("engine"),
        // Un modèle vide ne dit rien : il vaut absent, sinon il passerait pour un `--model ''` à relancer.
        model: optional("model").filter(|model| !model.is_empty()),
        pid,
        open: value.get("ended_at").is_none_or(|ended| ended.is_null()),
    })
}

/// Ce que l'environnement de lancement d'un processus révèle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchEnv {
    /// Vrai si `ANTHROPIC_BASE_URL` est posée : `claude` parle alors à un fournisseur tiers, pas à Anthropic.
    pub third_party: bool,
}

/// Cherche le mot exact ` ANTHROPIC_BASE_URL=` dans une ligne de `ps eww`. La ligne n'est ni gardée ni journalisée :
/// elle contient des secrets, seul le booléen en sort.
pub fn parse_launch_env(line: &str) -> LaunchEnv {
    LaunchEnv {
        third_party: line.contains(" ANTHROPIC_BASE_URL="),
    }
}

/// Les agents d'idnma et leurs moteurs, par nom de clé uniquement : aucune valeur du fichier n'est conservée.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdnmaAgents {
    engines_by_agent: HashMap<String, HashSet<String>>,
}

impl IdnmaAgents {
    /// Vrai si l'agent existe et, quand un moteur est demandé, s'il le déclare. Sans moteur, l'agent seul suffit.
    pub fn knows(&self, agent: &str, engine: Option<&str>) -> bool {
        match (self.engines_by_agent.get(agent), engine) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(engines), Some(engine)) => engines.contains(engine),
        }
    }
}

/// Le nom de la clé d'une ligne `clé: valeur`, sans guillemets. `None` pour un élément de liste ou une ligne sans `:`.
fn key_of(body: &str) -> Option<&str> {
    if body.starts_with('-') {
        return None;
    }
    let (key, _) = body.split_once(':')?;
    let key = key.trim().trim_matches(|c| c == '"' || c == '\'');
    (!key.is_empty()).then_some(key)
}

/// Lit `~/.idnma.yml` sans analyse YAML : on compte le retrait. Sous `agents:` (colonne 0), chaque clé au premier retrait
/// est un agent ; sous son sous-bloc `engines:`, chaque clé au premier retrait est un moteur. Le bloc finit à la clé
/// suivante en colonne 0. Seuls les noms de clés sont gardés, jamais une valeur.
pub fn parse_idnma_agents(text: &str) -> IdnmaAgents {
    let mut agents = IdnmaAgents::default();
    let mut in_agents = false;
    let mut agent_indent: Option<usize> = None;
    let mut current: Option<String> = None;
    let mut field_indent: Option<usize> = None;
    let mut in_engines = false;
    let mut engine_indent: Option<usize> = None;

    for line in text.lines() {
        let line = line.trim_end();
        let body = line.trim_start();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        let indent = line.len() - body.len();
        if indent == 0 {
            in_agents = key_of(body) == Some("agents");
            agent_indent = None;
            current = None;
            continue;
        }
        if !in_agents {
            continue;
        }
        let Some(key) = key_of(body) else {
            continue;
        };
        let top = *agent_indent.get_or_insert(indent);
        if indent == top {
            agents.engines_by_agent.entry(key.to_string()).or_default();
            current = Some(key.to_string());
            field_indent = None;
            in_engines = false;
            engine_indent = None;
            continue;
        }
        if indent < top {
            continue;
        }
        let Some(agent) = &current else {
            continue;
        };
        let field = *field_indent.get_or_insert(indent);
        if indent == field {
            in_engines = key == "engines";
            engine_indent = None;
        } else if in_engines {
            let engine = *engine_indent.get_or_insert(indent);
            if indent == engine
                && let Some(engines) = agents.engines_by_agent.get_mut(agent)
            {
                engines.insert(key.to_string());
            }
        }
    }
    agents
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "bbbb2222-cccc-4ddd-8eee-ffff00001111";

    fn session_json(extra: &str) -> String {
        format!(
            r#"{{"pid":16728,"sessionId":"{ID}","cwd":"/Users/x/dev/demo","kind":"interactive","entrypoint":"cli","procStart":"Mon Nov  2 18:01:12 2026"{extra}}}"#
        )
    }

    #[test]
    fn a_cli_session_file_gives_a_live_session() {
        let text = r#"{"pid":16728,"sessionId":"bbbb2222-cccc-4ddd-8eee-ffff00001111","cwd":"/Users/x/dev/demo","name":"demo-robot","kind":"interactive","entrypoint":"cli","procStart":"Mon Nov  2 18:01:12 2026","bridgeSessionId":"session_01x","status":"idle"}"#;
        let session = parse_live_session(text, Some(16728)).unwrap();
        assert_eq!(session.pid, 16728);
        assert_eq!(session.session_id, ID);
        assert_eq!(session.cwd, PathBuf::from("/Users/x/dev/demo"));
        assert_eq!(session.name.as_deref(), Some("demo-robot"));
        assert_eq!(session.proc_start, "Mon Nov  2 18:01:12 2026");
    }

    #[test]
    fn a_session_without_a_name_keeps_none() {
        assert_eq!(
            parse_live_session(&session_json(""), None).unwrap().name,
            None
        );
        let null_name = session_json(r#","name":null"#);
        assert_eq!(parse_live_session(&null_name, None).unwrap().name, None);
    }

    #[test]
    fn a_session_without_proc_start_is_dropped() {
        let text = format!(
            r#"{{"pid":1,"sessionId":"{ID}","cwd":"/x","kind":"interactive","entrypoint":"cli"}}"#
        );
        assert_eq!(parse_live_session(&text, None), None);
    }

    #[test]
    fn other_kinds_are_not_live_sessions() {
        for kind in ["teammate", "background"] {
            let text = session_json("").replace("interactive", kind);
            assert_eq!(parse_live_session(&text, None), None, "kind {kind}");
        }
    }

    #[test]
    fn a_foreign_or_unsafe_file_is_ignored() {
        let not_uuid = session_json("").replace(ID, "not-a-uuid; rm");
        assert_eq!(parse_live_session(&not_uuid, None), None);
        let zero_pid = session_json("").replace("16728", "0");
        assert_eq!(parse_live_session(&zero_pid, None), None);
        assert_eq!(parse_live_session("{ broken", None), None);
        let no_cwd = session_json("").replace(r#""cwd":"/Users/x/dev/demo","#, "");
        assert_eq!(parse_live_session(&no_cwd, None), None);
    }

    #[test]
    fn a_file_naming_another_pid_is_dropped() {
        let text = session_json("").replace("16728", "1");
        assert_eq!(parse_live_session(&text, Some(2)), None);
        assert!(parse_live_session(&text, Some(1)).is_some());

        let dir = std::env::temp_dir().join(format!(
            "idema-reclaim-sources-{}-pidname",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Le nom du fichier dit 2, le contenu dit 1 : écarté. Un nom non numérique est étranger : écarté aussi.
        std::fs::write(dir.join("2.json"), &text).unwrap();
        std::fs::write(dir.join("other.json"), &text).unwrap();
        std::fs::write(dir.join("1.json"), &text).unwrap();
        let sessions = read_live_sessions(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pid, 1);
    }

    #[test]
    fn only_the_cli_entrypoint_is_live() {
        assert!(parse_live_session(&session_json(""), None).is_some());
        for entrypoint in ["sdk-cli", "vscode"] {
            let other = session_json("").replace(r#""cli""#, &format!(r#""{entrypoint}""#));
            assert_eq!(parse_live_session(&other, None), None, "{entrypoint}");
        }
    }

    #[test]
    fn a_manifest_reads_agent_engine_model_and_state() {
        let open = r#"{"session_id":"cccc3333-dddd-4eee-8fff-000011112222","agent":"glm","engine":"default","model":"glm-5.3-flash:cloud[1m]","harness":"claude","name":"glm-beta-xyot","cwd":"/x","pid":9186,"ended_at":null}"#;
        let manifest = parse_manifest(open).unwrap();
        assert_eq!(manifest.agent, "glm");
        assert_eq!(manifest.engine.as_deref(), Some("default"));
        assert_eq!(manifest.model.as_deref(), Some("glm-5.3-flash:cloud[1m]"));
        assert_eq!(manifest.pid, 9186);
        assert!(manifest.open);

        let absent = open.replace(r#","ended_at":null"#, "");
        assert!(parse_manifest(&absent).unwrap().open);

        let ended = open.replace("null", r#""2026-11-02T18:00:00Z""#);
        assert!(!parse_manifest(&ended).unwrap().open);

        let no_engine = open.replace(r#""engine":"default""#, r#""engine":null"#);
        assert_eq!(parse_manifest(&no_engine).unwrap().engine, None);
    }

    #[test]
    fn an_empty_manifest_model_is_none() {
        let text = r#"{"agent":"glm","engine":null,"model":"","harness":"claude","pid":9186,"ended_at":null}"#;
        assert_eq!(parse_manifest(text).unwrap().model, None);
    }

    #[test]
    fn a_holder_is_read_whatever_its_kind_and_entrypoint() {
        let sdk = session_json("").replace(r#""cli""#, r#""sdk-cli""#);
        let holder = parse_session_holder(&sdk).unwrap();
        assert_eq!(holder.pid, 16728);
        assert_eq!(holder.session_id, ID);
        assert_eq!(
            holder.proc_start.as_deref(),
            Some("Mon Nov  2 18:01:12 2026")
        );

        let background = session_json("").replace("interactive", "background");
        assert!(parse_session_holder(&background).is_some());

        // Sans `procStart` (ou vide) : un détenteur quand même, sans date à recouper.
        let bare = format!(r#"{{"pid":7,"sessionId":"{ID}","kind":"daemon"}}"#);
        assert_eq!(parse_session_holder(&bare).unwrap().proc_start, None);
        let empty = format!(r#"{{"pid":7,"sessionId":"{ID}","procStart":""}}"#);
        assert_eq!(parse_session_holder(&empty).unwrap().proc_start, None);
    }

    #[test]
    fn a_foreign_holder_file_is_ignored() {
        let not_uuid = session_json("").replace(ID, "not-a-uuid; rm");
        assert_eq!(parse_session_holder(&not_uuid), None);
        let zero_pid = session_json("").replace("16728", "0");
        assert_eq!(parse_session_holder(&zero_pid), None);
        assert_eq!(parse_session_holder("{ broken"), None);
    }

    #[test]
    fn holders_are_read_from_a_directory_by_their_file_name() {
        let dir = std::env::temp_dir().join(format!(
            "idema-reclaim-sources-{}-holders",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sdk = session_json("").replace(r#""cli""#, r#""sdk-cli""#);
        // Le nom du fichier dit 2, le contenu dit 16728 : écarté, comme pour les sessions.
        std::fs::write(dir.join("2.json"), &sdk).unwrap();
        std::fs::write(dir.join("16728.json"), &sdk).unwrap();
        let holders = read_session_holders(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].pid, 16728);
    }

    #[test]
    fn a_manifest_of_another_harness_is_dropped() {
        let text = r#"{"agent":"codex","engine":null,"model":null,"harness":"codex","pid":9186,"ended_at":null}"#;
        assert_eq!(parse_manifest(text), None);
    }

    #[test]
    fn the_launch_env_only_sees_the_base_url() {
        let with = "claude x ANTHROPIC_MODEL=sonnet ANTHROPIC_BASE_URL=http://localhost:11434";
        assert!(parse_launch_env(with).third_party);
        let without = "claude x ANTHROPIC_MODEL=sonnet";
        assert!(!parse_launch_env(without).third_party);
        assert!(!parse_launch_env("echo ANTHROPIC_BASE_URLX=1").third_party);
    }

    #[test]
    fn idnma_agents_are_read_from_their_indentation() {
        let text = "agents:\n  claude:\n    label: x\n    engines:\n      default:\n        model: opus\n  agy:\n    engines:\n      default:\n      gflash:\nother:\n  nope:\n";
        let agents = parse_idnma_agents(text);
        assert!(agents.knows("claude", None));
        assert!(agents.knows("agy", Some("gflash")));
        assert!(agents.knows("agy", Some("default")));
        assert!(!agents.knows("claude", Some("gflash")));
        assert!(!agents.knows("nope", None));
        assert!(!IdnmaAgents::default().knows("claude", None));
    }
}
