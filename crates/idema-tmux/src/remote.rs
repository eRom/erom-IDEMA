//! Transport distant de `TmuxServer` : tmux lancé par `/usr/bin/ssh`, maître multiplexé (spec ssh-tmux).

use std::path::PathBuf;
use std::process::Command;

use crate::server::reset_signal_state;

/// Le client ssh du système : jamais celui du `PATH`.
const SSH_BINARY: &str = "/usr/bin/ssh";

/// Cite un argument pour un shell POSIX : entre apostrophes, une apostrophe interne devient `'\''`.
pub fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Une machine distante et le canal ssh qui la dessert.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteSpec {
    /// Ce que `ssh` reçoit après `--` : un alias de `~/.ssh/config` ou `utilisateur@hôte`.
    pub destination: String,
    /// Dossier des sockets de contrôle (`/tmp/idema-<uid>-<8 hex>`, 0700).
    pub control_dir: PathBuf,
    /// Numéro du canal : un maître ssh par numéro, `%C-<slot>` dans le `ControlPath`.
    pub slot: u8,
    /// L'environnement complet du processus ssh (`HOME`, `USER`, `LOGNAME`, `PATH`, `SSH_AUTH_SOCK`, `LANG`).
    pub ssh_env: Vec<(String, String)>,
    /// `$HOME` de la machine distante.
    pub home: String,
    /// Le shell de connexion de la machine distante.
    pub shell: String,
}

/// Le `ControlPath` d'une connexion : le même texte pour ouvrir le maître et pour le fermer.
fn control_path(spec: &RemoteSpec) -> String {
    format!("{}/%C-{}", spec.control_dir.to_string_lossy(), spec.slot)
}

/// Les arguments de `ssh` jusqu'à la destination comprise, dans l'ordre imposé.
pub fn ssh_arguments(spec: &RemoteSpec, tty: bool) -> Vec<String> {
    let mut arguments = vec![(if tty { "-t" } else { "-T" }).to_string()];
    let options = [
        "BatchMode=yes".to_string(),
        "ConnectTimeout=5".to_string(),
        "ServerAliveInterval=10".to_string(),
        "ServerAliveCountMax=3".to_string(),
        "ForwardAgent=no".to_string(),
        "RemoteCommand=none".to_string(),
        "LogLevel=ERROR".to_string(),
        "ControlMaster=auto".to_string(),
        format!("ControlPath={}", control_path(spec)),
        "ControlPersist=120".to_string(),
    ];
    for option in options {
        arguments.push("-o".into());
        arguments.push(option);
    }
    arguments.push("--".into());
    arguments.push(spec.destination.clone());
    arguments
}

/// La ligne que le shell distant lit : `tmux -u` (sans `-u`, un nom de session non ASCII devient `_ _` sous
/// `env -i`, mesuré le 08/11), puis chaque argument cité.
pub fn remote_tmux_line(args: &[&str]) -> String {
    let mut line = String::from("tmux -u");
    for arg in args {
        line.push(' ');
        line.push_str(&shell_quote(arg));
    }
    line
}

/// Les arguments de `ssh` qui ferment le maître d'une connexion (`%C` doit correspondre à l'ouverture).
pub fn control_exit_arguments(spec: &RemoteSpec) -> Vec<String> {
    vec![
        "-o".into(),
        format!("ControlPath={}", control_path(spec)),
        "-O".into(),
        "exit".into(),
        "--".into(),
        spec.destination.clone(),
    ]
}

/// `ssh <options> -- <destination> <script>` : `script` est lu tel quel par le shell distant.
pub fn remote_command(spec: &RemoteSpec, script: &str, tty: bool) -> Command {
    let mut command = Command::new(SSH_BINARY);
    command
        .args(ssh_arguments(spec, tty))
        .arg(script)
        .env_clear()
        .envs(
            spec.ssh_env
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
    reset_signal_state(&mut command);
    command
}

/// Découpe `args` en commandes tmux. tmux sépare sur un argument `;` et sur un argument qui se termine par un
/// `;` non échappé (`\;` reste un `;` littéral).
fn split_commands<'a>(args: &[&'a str]) -> Vec<Vec<&'a str>> {
    let mut commands = vec![Vec::new()];
    for &arg in args {
        let (word, ends_command) = match arg.strip_suffix(';') {
            Some(_) if arg.ends_with("\\;") => (arg, false),
            Some(word) => (word, true),
            None => (arg, false),
        };
        if !word.is_empty()
            && let Some(current) = commands.last_mut()
        {
            current.push(word);
        }
        if ends_command {
            commands.push(Vec::new());
        }
    }
    commands.retain(|command| !command.is_empty());
    commands
}

/// Pourquoi tmux ne doit pas recevoir ces arguments sur une machine distante, ou `None` s'ils passent.
///
/// IdemA est l'invité de ce serveur : jamais d'arrêt, de rechargement de configuration ni d'option globale ou
/// serveur. Les drapeaux d'une commande se lisent avant son premier mot sans tiret, ou avant `--`.
pub fn forbidden_on_remote(args: &[&str]) -> Option<String> {
    for command in split_commands(args) {
        let name = command[0];
        match name {
            "kill-server" => return Some("kill-server".into()),
            "source-file" | "source" => return Some(format!("{name} reads a remote file")),
            "set-option" | "set" | "setw" | "set-window-option" | "set-environment" | "setenv" => {
                let rest = &command[1..];
                let flags: Vec<&str> = rest
                    .iter()
                    .take_while(|word| word.len() > 1 && word.starts_with('-') && **word != "--")
                    .copied()
                    .collect();
                let bundle: String = flags.iter().map(|flag| &flag[1..]).collect();
                if !bundle.contains(['g', 's']) {
                    continue;
                }
                if is_extended_keys_exception(name, &flags, &rest[flags.len()..]) {
                    continue;
                }
                return Some(format!(
                    "{name} {} changes global tmux state",
                    flags.join(" ")
                ));
            }
            _ => {}
        }
    }
    None
}

/// Seule écriture serveur permise : sans `extended-keys`, tmux 3.8 laisse `#{pane_key_mode}` à `VT10x` et
/// Maj+Entrée se perd (mesuré le 08/11) ; Ghostty ne le laisse pas voir.
fn is_extended_keys_exception(name: &str, flags: &[&str], words: &[&str]) -> bool {
    matches!(name, "set-option" | "set")
        && flags == ["-s"]
        && matches!(
            words,
            ["extended-keys", "on"] | ["extended-keys-format", "csi-u"]
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn spec() -> RemoteSpec {
        RemoteSpec {
            destination: "caserne-claude".into(),
            control_dir: PathBuf::from("/tmp/idema-501-0a1b2c3d"),
            slot: 0,
            ssh_env: Vec::new(),
            home: "/home/claude".into(),
            shell: "/bin/bash".into(),
        }
    }

    #[test]
    fn shell_quote_round_trips_through_sh() {
        let inputs = [
            "plain",
            "mon audit",
            "l'audit",
            "$(touch /tmp/idema-x)",
            "a;b",
            "line\nbreak",
            "tab\tsep",
            "#{session_name}",
            "é ✳",
            "",
        ];
        for input in inputs {
            let output = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf '%s' {}", shell_quote(input)))
                .output()
                .expect("sh runs");
            assert_eq!(String::from_utf8_lossy(&output.stdout), input);
        }
        assert!(!Path::new("/tmp/idema-x").exists());
    }

    #[test]
    fn ssh_arguments_carry_the_imposed_options_in_order() {
        let expected: Vec<String> = [
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "ForwardAgent=no",
            "-o",
            "RemoteCommand=none",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "ControlMaster=auto",
            "-o",
            "ControlPath=/tmp/idema-501-0a1b2c3d/%C-0",
            "-o",
            "ControlPersist=120",
            "--",
            "caserne-claude",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(ssh_arguments(&spec(), false), expected);
        let mut with_tty = expected;
        with_tty[0] = "-t".into();
        assert_eq!(ssh_arguments(&spec(), true), with_tty);
    }

    #[test]
    fn remote_tmux_line_quotes_every_argument() {
        assert_eq!(
            remote_tmux_line(&["list-sessions", "-F", "#{session_name}\t#{x}"]),
            "tmux -u 'list-sessions' '-F' '#{session_name}\t#{x}'"
        );
    }

    #[test]
    fn control_exit_arguments_share_the_control_path() {
        let mut other = spec();
        other.slot = 3;
        assert_eq!(
            control_exit_arguments(&other),
            vec![
                "-o",
                "ControlPath=/tmp/idema-501-0a1b2c3d/%C-3",
                "-O",
                "exit",
                "--",
                "caserne-claude"
            ]
        );
    }

    #[test]
    fn remote_command_runs_ssh_with_the_script_last_and_a_replaced_environment() {
        let mut with_env = spec();
        with_env.ssh_env = vec![("HOME".into(), "/Users/r".into())];
        let command = remote_command(&with_env, "tmux -V", false);
        assert_eq!(command.get_program(), "/usr/bin/ssh");
        let arguments: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(arguments.last().map(String::as_str), Some("tmux -V"));
        assert_eq!(arguments[arguments.len() - 2], "caserne-claude");
        let envs: Vec<_> = command.get_envs().collect();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].0, "HOME");
    }

    #[test]
    fn forbidden_on_remote_refuses_global_and_server_writes() {
        let refused: &[&[&str]] = &[
            &["kill-server"],
            &["source-file", "x"],
            &["set-option", "-g", "status", "off"],
            &["set", "-gq", "mouse", "on"],
            &["setw", "-g", "x", "y"],
            &["set-option", "-s", "history-limit", "9"],
            &["list-sessions", ";", "set", "-g", "a", "b"],
        ];
        for args in refused {
            assert!(forbidden_on_remote(args).is_some(), "{args:?}");
        }
        let allowed: &[&[&str]] = &[
            &["set-option", "-w", "-t", "@1", "window-size", "manual"],
            &["set-option", "-p", "-t", "%3", "@idema_agent", "glm"],
            &["set-option", "-t", "=testvpc", "@idema_name", "Audit"],
            &["list-sessions"],
            &["set-option", "-s", "extended-keys", "on"],
            &["set-option", "-s", "extended-keys-format", "csi-u"],
            &[
                "new-session",
                "-d",
                "-s",
                "x",
                ";",
                "set-option",
                "-t",
                "=x:",
                "--",
                "@idema_name",
                "-s",
            ],
        ];
        for args in allowed {
            assert_eq!(forbidden_on_remote(args), None, "{args:?}");
        }
    }
}
