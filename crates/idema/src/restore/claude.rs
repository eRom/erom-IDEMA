//! Lecture des fichiers de session vivants de Claude Code (`~/.claude/sessions/<pid>.json`) : ils disent quelle
//! conversation tourne dans quel pane de tmux. IdemA n'y écrit jamais.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use idema_tmux::protocol::PaneId;
use idema_tmux::server::CapturedPane;

use crate::servers::scan::Process;
use crate::servers::stop::program_names;

/// Une conversation Claude Code vivante, telle que son fichier de session la désigne.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSession {
    /// PID du processus `claude`.
    pub pid: i32,
    /// Identifiant de la conversation, toujours un UUID : il sera tapé dans un shell.
    pub session_id: String,
    /// Nom de la session tmux qui porte le pane.
    pub tmux_session: String,
    /// Le pane de tmux où tourne `claude`.
    pub pane: PaneId,
}

/// Le dossier des fichiers de session : `$CLAUDE_CONFIG_DIR/sessions` si la variable est posée, sinon
/// `$HOME/.claude/sessions`. `None` sans l'un ni l'autre.
pub fn sessions_dir(home: Option<&OsStr>, config_dir: Option<&OsStr>) -> Option<PathBuf> {
    match config_dir {
        Some(dir) => Some(Path::new(dir).join("sessions")),
        None => home.map(|home| Path::new(home).join(".claude").join("sessions")),
    }
}

/// Vrai pour un UUID au format 8-4-4-4-12 chiffres hexadécimaux, sans rien d'autre.
pub(crate) fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// Lit `<session>:@<fenêtre>.%<pane>` : le dernier `:` sépare la session, le premier `.` suivant sépare la fenêtre.
fn parse_tmux_location(text: &str) -> Option<(String, PaneId)> {
    let (session, rest) = text.rsplit_once(':')?;
    let (window, pane) = rest.split_once('.')?;
    let window = window.strip_prefix('@')?;
    let pane = pane.strip_prefix('%')?;
    let all_digits =
        |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if session.is_empty() || !all_digits(window) || !all_digits(pane) {
        return None;
    }
    // La fenêtre ne sert pas, mais un numéro qui ne tient pas dans un `u32` n'est pas celui de tmux.
    window.parse::<u32>().ok()?;
    Some((session.to_string(), PaneId(pane.parse().ok()?)))
}

/// Lit un fichier de session. Seuls `pid`, `sessionId` et `tmux` comptent ; les autres champs passent. `None` si l'un
/// manque, a le mauvais type, ou si `sessionId` n'est pas un UUID.
pub fn parse_claude_session(text: &str) -> Option<ClaudeSession> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    // Un PID est strictement positif : 0 et les négatifs visent des groupes de processus, jamais un `claude`.
    let pid = i32::try_from(value.get("pid")?.as_i64()?)
        .ok()
        .filter(|pid| *pid > 0)?;
    let session_id = value.get("sessionId")?.as_str()?;
    if !is_uuid(session_id) {
        return None;
    }
    let (tmux_session, pane) = parse_tmux_location(value.get("tmux")?.as_str()?)?;
    Some(ClaudeSession {
        pid,
        session_id: session_id.to_string(),
        tmux_session,
        pane,
    })
}

/// Lit les `*.json` du dossier ; tout fichier illisible ou étranger est ignoré.
pub fn read_claude_sessions(dir: &Path) -> Vec<ClaudeSession> {
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
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|text| parse_claude_session(&text))
        .collect()
}

/// Un vrai fichier de session pèse quelques centaines d'octets ; au-delà, il n'est pas lu.
const MAX_SESSION_FILE_BYTES: u64 = 64 * 1024;

/// Vrai si la commande est celle de Claude Code : `claude` lancé par son nom ou par un chemin, ou derrière un
/// interpréteur (`node /…/claude`).
pub(crate) fn is_claude_command(command: &str) -> bool {
    program_names(command).contains(&"claude")
}

/// Le nombre de générations entre `ancestor` et `pid` (0 si c'est le même processus), en remontant la chaîne des
/// `ppid`. `None` si `pid` n'en descend pas. Un cycle ou un PID absent arrête la remontée.
fn depth_below(pid: i32, ancestor: i32, processes: &HashMap<i32, Process>) -> Option<usize> {
    let mut current = pid;
    for depth in 0..=processes.len() {
        let process = processes.get(&current)?;
        if current == ancestor {
            return Some(depth);
        }
        current = process.ppid;
    }
    None
}

/// Vrai si `pid` est `ancestor` ou en descend, en remontant la chaîne des `ppid`. Un cycle ou un PID absent arrête la
/// remontée.
pub(crate) fn descends_from(pid: i32, ancestor: i32, processes: &HashMap<i32, Process>) -> bool {
    depth_below(pid, ancestor, processes).is_some()
}

/// La conversation de chaque pane, par `(session tmux, pane)`. Un fichier compte si son `pid` est un processus connu
/// qui est le shell du pane désigné ou en descend, et dont la commande est celle de Claude Code. Le second contrôle
/// écarte un fichier resté d'un `claude` mort dont le PID a été réutilisé par un sous-processus du même pane (après un
/// redémarrage, les sessions gardent leur nom et les panes repartent de `%0`). Deux fichiers pour un même pane : le
/// `claude` le moins profond sous le shell du pane gagne (un `claude -p` lancé par la conversation, qui hérite de
/// `TMUX_PANE`, ne lui prend pas la place), puis, à profondeur égale, le plus grand `pid`.
///
/// Ce contrôle de commande protège aussi Codex, Grok et agy : `idnma run codex|grok|agy` inscrit lui aussi un fichier
/// dans ce dossier, avec un `sessionId` aléatoire et un `tmux` valide qui pointe le pane (erom-agence-network,
/// `register.ts`). Seul le nom du programme (`bun …/idnma run codex`, pas `claude`) empêche IdemA de retaper
/// `idnma run codex --resume <uuid bidon>`.
pub fn conversations(
    sessions: &[ClaudeSession],
    panes: &[CapturedPane],
    processes: &HashMap<i32, Process>,
) -> HashMap<(String, PaneId), String> {
    // Par pane : (profondeur sous le shell du pane, pid, sessionId) du meilleur fichier vu.
    let mut winners: HashMap<(String, PaneId), (usize, i32, &str)> = HashMap::new();
    for session in sessions {
        let Some(pane) = panes
            .iter()
            .find(|pane| pane.session == session.tmux_session && pane.pane == session.pane)
        else {
            continue;
        };
        let is_claude = processes
            .get(&session.pid)
            .is_some_and(|process| is_claude_command(&process.command));
        if !is_claude {
            continue;
        }
        let Some(depth) = depth_below(session.pid, pane.pane_pid, processes) else {
            continue;
        };
        let key = (session.tmux_session.clone(), session.pane);
        match winners.get(&key) {
            Some((kept_depth, kept_pid, _))
                if *kept_depth < depth || (*kept_depth == depth && *kept_pid >= session.pid) => {}
            _ => {
                winners.insert(key, (depth, session.pid, &session.session_id));
            }
        }
    }
    winners
        .into_iter()
        .map(|(key, (_, _, session_id))| (key, session_id.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use idema_tmux::protocol::WindowId;

    const SAMPLE: &str = r#"{"pid":2886,"sessionId":"44a13315-a76d-4dba-9be9-4b275aba8388","cwd":"/Users/me/dev/erom-tool-idema","startedAt":1793184343878,"procStart":"Wed Oct 28 10:45:42 2026","version":"2.1.312","peerProtocol":1,"kind":"interactive","entrypoint":"cli","tmux":"idema-054bcf18:@8.%8","messagingSocketPath":"/tmp/cc-socks/2886.sock","name":"claude-vulcain-o66q","nameSource":"user","status":"busy"}"#;

    const ID_A: &str = "44a13315-a76d-4dba-9be9-4b275aba8388";
    const ID_B: &str = "11111111-2222-3333-4444-555555555555";

    fn session_json(session_id: &str, tmux: &str) -> String {
        format!(r#"{{"pid":1,"sessionId":"{session_id}","tmux":"{tmux}"}}"#)
    }

    fn process_with(pid: i32, ppid: i32, command: &str) -> (i32, Process) {
        let process = Process {
            pid,
            ppid,
            pgid: pid,
            command: command.to_string(),
        };
        (pid, process)
    }

    /// Un processus `claude` tel que `ps -o command=` le montre.
    fn claude_process(pid: i32, ppid: i32) -> (i32, Process) {
        process_with(
            pid,
            ppid,
            "claude --model opus --dangerously-skip-permissions",
        )
    }

    fn process(pid: i32, ppid: i32) -> (i32, Process) {
        process_with(pid, ppid, "zsh")
    }

    fn pane(session: &str, pane: u32, pane_pid: i32) -> CapturedPane {
        CapturedPane {
            session: session.to_string(),
            window: WindowId(1),
            stash: false,
            pane: PaneId(pane),
            pane_active: true,
            window_active: true,
            pane_pid,
            hidden_at: 0,
            agent: String::new(),
            idnma_agent: String::new(),
            resume: String::new(),
            cwd: PathBuf::from("/"),
        }
    }

    fn claude(pid: i32, session_id: &str, tmux_session: &str, pane: u32) -> ClaudeSession {
        ClaudeSession {
            pid,
            session_id: session_id.to_string(),
            tmux_session: tmux_session.to_string(),
            pane: PaneId(pane),
        }
    }

    #[test]
    fn a_real_session_file_parses() {
        let session = parse_claude_session(SAMPLE).expect("the sample parses");
        assert_eq!(session.pid, 2886);
        assert_eq!(session.session_id, ID_A);
        assert_eq!(session.tmux_session, "idema-054bcf18");
        assert_eq!(session.pane, PaneId(8));
    }

    #[test]
    fn missing_fields_or_a_malformed_tmux_field_give_nothing() {
        let no_session_id = r#"{"pid":1,"tmux":"s:@1.%1"}"#;
        let no_tmux = format!(r#"{{"pid":1,"sessionId":"{ID_A}"}}"#);
        let no_pid = format!(r#"{{"sessionId":"{ID_A}","tmux":"s:@1.%1"}}"#);
        let string_pid = format!(r#"{{"pid":"1","sessionId":"{ID_A}","tmux":"s:@1.%1"}}"#);
        for text in [
            no_session_id,
            &no_tmux,
            &no_pid,
            &string_pid,
            "not json",
            "",
        ] {
            assert_eq!(parse_claude_session(text), None, "{text}");
        }
        for tmux in [
            "abc", "s:@1", "s:@1.8", "s:1.%1", "s:@.%1", "s:@1.%", "s:@x.%1", "s:@1.%x", ":@1.%1",
        ] {
            assert_eq!(
                parse_claude_session(&session_json(ID_A, tmux)),
                None,
                "{tmux}"
            );
        }
        assert!(parse_claude_session(&session_json(ID_A, "s:@1.%1")).is_some());
    }

    #[test]
    fn a_session_id_that_is_not_a_uuid_is_ignored() {
        let with_trailing_space = format!("{ID_A} ");
        let with_newline = format!("{ID_A}\\n");
        for id in [
            "x; rm -rf ~",
            "44a13315",
            &with_trailing_space,
            &with_newline,
            "44a13315-a76d-4dba-9be9-4b275abaZ388",
            "44a13315a76d4dba9be94b275aba8388",
            "",
        ] {
            assert_eq!(
                parse_claude_session(&session_json(id, "s:@1.%1")),
                None,
                "{id:?}"
            );
        }
    }

    #[test]
    fn the_sessions_dir_follows_claude_config_dir() {
        let home = OsStr::new("/h");
        let config = OsStr::new("/c");
        assert_eq!(
            sessions_dir(Some(home), None),
            Some(PathBuf::from("/h/.claude/sessions"))
        );
        assert_eq!(
            sessions_dir(Some(home), Some(config)),
            Some(PathBuf::from("/c/sessions"))
        );
        assert_eq!(
            sessions_dir(None, Some(config)),
            Some(PathBuf::from("/c/sessions"))
        );
        assert_eq!(sessions_dir(None, None), None);
    }

    #[test]
    fn read_claude_sessions_skips_unreadable_and_foreign_files() {
        let dir =
            std::env::temp_dir().join(format!("idema-claude-sessions-{}-read", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("1.json"), SAMPLE).unwrap();
        std::fs::write(dir.join("2.json"), "{ broken").unwrap();
        std::fs::write(dir.join("3.abc.key"), SAMPLE).unwrap();

        let sessions = read_claude_sessions(&dir);
        let missing = read_claude_sessions(&dir.join("absent"));
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, ID_A);
        assert!(missing.is_empty());
    }

    #[test]
    fn a_conversation_belongs_to_its_pane_only_when_its_pid_descends_from_it() {
        let processes: HashMap<i32, Process> = [
            process(10, 1),
            claude_process(11, 10),
            claude_process(50, 1),
        ]
        .into_iter()
        .collect();
        let id_other = "22222222-2222-2222-2222-222222222222";
        let id_gone = "33333333-3333-3333-3333-333333333333";
        let sessions = [
            claude(11, ID_A, "s", 1),
            claude(50, id_other, "s", 2),
            claude(99, id_gone, "s", 3),
        ];
        let panes = [pane("s", 1, 10), pane("s", 2, 20), pane("s", 3, 30)];

        let map = conversations(&sessions, &panes, &processes);

        assert_eq!(map.len(), 1);
        assert_eq!(
            map.get(&("s".to_string(), PaneId(1))).map(String::as_str),
            Some(ID_A)
        );
    }

    #[test]
    fn the_largest_pid_wins_at_equal_depth() {
        // Deux `claude` frères sous le shell du pane : même profondeur, le plus grand pid l'emporte.
        let processes: HashMap<i32, Process> = [
            process(10, 1),
            claude_process(11, 10),
            claude_process(12, 10),
        ]
        .into_iter()
        .collect();
        let panes = [pane("s", 1, 10)];
        for sessions in [
            [claude(12, ID_B, "s", 1), claude(11, ID_A, "s", 1)],
            [claude(11, ID_A, "s", 1), claude(12, ID_B, "s", 1)],
        ] {
            let map = conversations(&sessions, &panes, &processes);

            assert_eq!(
                map.get(&("s".to_string(), PaneId(1))).map(String::as_str),
                Some(ID_B)
            );
        }
    }

    #[test]
    fn the_claude_closest_to_the_pane_wins() {
        // La conversation interactive (11) lance un `claude -p` (12, pid plus grand) qui écrit lui aussi un fichier :
        // la conversation reste celle de 11, quel que soit l'ordre de lecture.
        let processes: HashMap<i32, Process> = [
            process(10, 1),
            claude_process(11, 10),
            claude_process(12, 11),
        ]
        .into_iter()
        .collect();
        let panes = [pane("s", 1, 10)];
        for sessions in [
            [claude(12, ID_B, "s", 1), claude(11, ID_A, "s", 1)],
            [claude(11, ID_A, "s", 1), claude(12, ID_B, "s", 1)],
        ] {
            let map = conversations(&sessions, &panes, &processes);

            assert_eq!(
                map.get(&("s".to_string(), PaneId(1))).map(String::as_str),
                Some(ID_A)
            );
        }
    }

    #[test]
    fn a_peer_file_written_by_idnma_for_codex_is_ignored() {
        // `idnma run codex` inscrit un fichier valide qui pointe le pane, avec un sessionId qui n'a rien d'une
        // conversation Claude : seule la commande du process l'écarte.
        let processes: HashMap<i32, Process> = [
            process(10, 1),
            process_with(11, 10, "bun /Users/x/.bun/bin/idnma run codex"),
        ]
        .into_iter()
        .collect();
        let panes = [pane("s", 1, 10)];

        let map = conversations(&[claude(11, ID_A, "s", 1)], &panes, &processes);

        assert!(map.is_empty());
    }

    #[test]
    fn a_pane_of_another_tmux_session_or_a_cyclic_chain_gives_nothing() {
        let processes: HashMap<i32, Process> = [
            process(10, 1),
            claude_process(11, 10),
            claude_process(20, 21),
            process(21, 20),
        ]
        .into_iter()
        .collect();
        let panes = [pane("other", 1, 10), pane("s", 2, 99)];
        let sessions = [claude(11, ID_A, "s", 1), claude(20, ID_B, "s", 2)];

        assert!(conversations(&sessions, &panes, &processes).is_empty());
    }

    #[test]
    fn a_recycled_pid_that_is_not_claude_is_ignored() {
        // Après un redémarrage, un vieux `2886.json` retombe sur le pane %8 neuf : le PID 2886 appartient maintenant à un
        // sous-processus du pane, pas à Claude Code.
        let processes: HashMap<i32, Process> = [
            process(10, 1),
            process_with(11, 10, "node mcp-server.js"),
            process_with(12, 10, "node /Users/x/.local/bin/mcp-server.js --claude"),
        ]
        .into_iter()
        .collect();
        let panes = [pane("s", 1, 10)];

        assert!(conversations(&[claude(11, ID_A, "s", 1)], &panes, &processes).is_empty());
        assert!(conversations(&[claude(12, ID_A, "s", 1)], &panes, &processes).is_empty());
    }

    #[test]
    fn a_claude_started_by_path_counts() {
        let processes: HashMap<i32, Process> = [
            process(10, 1),
            process_with(
                11,
                10,
                "/Users/x/.local/bin/claude --resume 44a13315-a76d-4dba-9be9-4b275aba8388",
            ),
            process_with(20, 1, "zsh"),
            process_with(21, 20, "node /Users/x/.nvm/bin/claude --model opus"),
        ]
        .into_iter()
        .collect();
        let panes = [pane("s", 1, 10), pane("s", 2, 20)];
        let sessions = [claude(11, ID_A, "s", 1), claude(21, ID_B, "s", 2)];

        let map = conversations(&sessions, &panes, &processes);

        assert_eq!(
            map.get(&("s".to_string(), PaneId(1))).map(String::as_str),
            Some(ID_A)
        );
        assert_eq!(
            map.get(&("s".to_string(), PaneId(2))).map(String::as_str),
            Some(ID_B)
        );
    }

    #[test]
    fn a_claude_that_is_the_pane_process_itself_counts() {
        // `idnma run` exécute `claude` à la place du shell : le PID du pane est celui de Claude Code.
        let processes: HashMap<i32, Process> = [claude_process(10, 1)].into_iter().collect();
        let panes = [pane("s", 1, 10)];

        let map = conversations(&[claude(10, ID_A, "s", 1)], &panes, &processes);

        assert_eq!(
            map.get(&("s".to_string(), PaneId(1))).map(String::as_str),
            Some(ID_A)
        );
    }

    #[test]
    fn a_tmux_session_name_may_contain_colons_and_dots() {
        let session = parse_claude_session(&session_json(ID_A, "a.b:c:@1.%2")).unwrap();
        assert_eq!(session.tmux_session, "a.b:c");
        assert_eq!(session.pane, PaneId(2));
    }

    #[test]
    fn an_out_of_range_pane_or_pid_gives_nothing() {
        for tmux in [
            "s:@1.%4294967296",
            "s:@1.%+5",
            "s:@4294967296.%1",
            "s:@+1.%1",
        ] {
            assert_eq!(
                parse_claude_session(&session_json(ID_A, tmux)),
                None,
                "{tmux}"
            );
        }
        for pid in ["-1", "0", "2147483648", "9999999999999999999", "1.5"] {
            let text = format!(r#"{{"pid":{pid},"sessionId":"{ID_A}","tmux":"s:@1.%1"}}"#);
            assert_eq!(parse_claude_session(&text), None, "{pid}");
        }
    }

    #[test]
    fn read_claude_sessions_skips_directories_links_and_oversized_files() {
        let dir = std::env::temp_dir().join(format!(
            "idema-claude-sessions-{}-kinds",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("dir.json")).unwrap();
        std::fs::write(dir.join("1.json"), SAMPLE).unwrap();
        let padding = " ".repeat(MAX_SESSION_FILE_BYTES as usize);
        std::fs::write(dir.join("big.json"), format!("{SAMPLE}{padding}")).unwrap();
        std::os::unix::fs::symlink(dir.join("1.json"), dir.join("link.json")).unwrap();

        let sessions = read_claude_sessions(&dir);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(sessions.len(), 1);
    }
}
