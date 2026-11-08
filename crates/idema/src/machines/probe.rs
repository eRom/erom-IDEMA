//! La sonde d'une machine distante : un seul appel ssh qui rend la version de tmux, `$HOME`, `$SHELL` et le
//! `~/.idnma.yml`, et le classement des échecs de ssh.

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use idema_tmux::remote::{RemoteSpec, remote_command};

use super::idnma::{IdnmaAgentInfo, parse_idnma_agent_list};

/// La version minimale de tmux distant.
const MIN_TMUX: (u32, u32) = (3, 8);

/// Longueur maximale du texte de `tmux -V` repris dans un message d'interface.
const MAX_SHOWN_VERSION: usize = 40;

/// Pourquoi une machine distante n'est pas utilisable.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteFailure {
    Unreachable,
    AuthRefused,
    HostKey,
    TmuxMissing,
    /// La sortie de `tmux -V`, telle quelle (`tmux 3.4`).
    TmuxTooOld(String),
}

impl RemoteFailure {
    /// Le texte d'interface complet d'une cause d'échec pour la machine `connection`.
    pub fn message(&self, connection: &str) -> String {
        match self {
            Self::Unreachable => "Injoignable".into(),
            Self::AuthRefused => "Authentification refusée".into(),
            Self::HostKey => format!(
                "Clé d'hôte inconnue ou changée : connecte-toi une fois avec ssh {connection}"
            ),
            Self::TmuxMissing => "tmux introuvable".into(),
            Self::TmuxTooOld(found) => format!("tmux 3.8 requis, trouvé : {found}"),
        }
    }

    /// La cause en deux ou trois mots, pour la barre repliée.
    pub fn short(&self) -> &'static str {
        match self {
            Self::Unreachable => "injoignable",
            Self::AuthRefused => "authentification refusée",
            Self::HostKey => "clé d'hôte",
            Self::TmuxMissing => "tmux introuvable",
            Self::TmuxTooOld(_) => "tmux trop ancien",
        }
    }
}

/// Classe l'échec d'un appel ssh (ou de la sonde). Le code 255 est celui de ssh lui-même : lui seul autorise à lire
/// `Permission denied` ou la clé d'hôte, car un `Permission denied` de tmux (socket d'un autre utilisateur) n'est pas
/// un refus d'authentification. 127 : commande absente côté distant. Tout le reste est « injoignable ».
/// Ne s'applique jamais à une erreur de tmux (`no server running` n'est pas un échec de machine).
pub fn classify_failure(exit: Option<i32>, stderr: &str) -> RemoteFailure {
    match exit {
        Some(255) => {
            if stderr.contains("Host key verification failed")
                || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
            {
                RemoteFailure::HostKey
            } else if stderr.contains("Permission denied") {
                RemoteFailure::AuthRefused
            } else {
                RemoteFailure::Unreachable
            }
        }
        Some(127) => RemoteFailure::TmuxMissing,
        _ => RemoteFailure::Unreachable,
    }
}

/// Ce que la sonde apprend d'une machine.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineFacts {
    pub tmux_version: (u32, u32),
    pub home: String,
    pub shell: String,
    /// `None` quand la machine n'a pas de `~/.idnma.yml`.
    pub agents: Option<Vec<IdnmaAgentInfo>>,
}

/// Le script de la sonde, lu tel quel par le shell distant. Le marqueur `idema-probe-v1` permet d'ignorer la
/// bannière qu'un `.bashrc` imprimerait avant.
pub const PROBE_SCRIPT: &str = "printf 'idema-probe-v1\\n'; tmux -V 2>/dev/null || printf 'NO_TMUX\\n'; printf '%s\\n%s\\n' \"$HOME\" \"$SHELL\"; if [ -r \"$HOME/.idnma.yml\" ]; then printf 'IDNMA\\n'; cat \"$HOME/.idnma.yml\"; else printf 'NO_IDNMA\\n'; fi";

/// Lit `tmux 3.8`, `tmux 3.3a`, `tmux next-3.8` : majeur et mineur, suffixe de lettre ignoré.
fn parse_tmux_version(line: &str) -> Option<(u32, u32)> {
    let version = line.trim().strip_prefix("tmux ")?.trim();
    let version = version.strip_prefix("next-").unwrap_or(version);
    let (major, rest) = version.split_once('.')?;
    let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Lit la sortie de `PROBE_SCRIPT`.
pub fn parse_probe(output: &str) -> Result<MachineFacts, RemoteFailure> {
    let mut lines = output.lines().map(|line| line.trim_end_matches('\r'));
    // Une sortie sans marqueur n'est pas celle de la sonde : la machine n'a pas répondu comme attendu.
    lines
        .by_ref()
        .find(|line| *line == "idema-probe-v1")
        .ok_or(RemoteFailure::Unreachable)?;
    let tmux_line = lines.next().ok_or(RemoteFailure::Unreachable)?;
    if tmux_line == "NO_TMUX" {
        return Err(RemoteFailure::TmuxMissing);
    }
    let tmux_version = match parse_tmux_version(tmux_line) {
        Some(version) if version >= MIN_TMUX => version,
        // Une version illisible (`tmux master`) est traitée comme trop ancienne ; le texte affiché est borné.
        _ => {
            let shown: String = tmux_line.trim().chars().take(MAX_SHOWN_VERSION).collect();
            return Err(RemoteFailure::TmuxTooOld(shown));
        }
    };
    let home = lines.next().ok_or(RemoteFailure::Unreachable)?.to_string();
    let shell = lines.next().ok_or(RemoteFailure::Unreachable)?.to_string();
    let agents = match lines.next() {
        Some("IDNMA") => {
            let rest: Vec<&str> = lines.collect();
            Some(parse_idnma_agent_list(&rest.join("\n")))
        }
        Some("NO_IDNMA") => None,
        _ => return Err(RemoteFailure::Unreachable),
    };
    Ok(MachineFacts {
        tmux_version,
        home,
        shell,
        agents,
    })
}

/// Sonde la machine : bloquant (un appel ssh), à lancer hors du fil d'interface.
pub fn probe(spec: &RemoteSpec) -> Result<MachineFacts, RemoteFailure> {
    let output = remote_command(spec, PROBE_SCRIPT, false)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| RemoteFailure::Unreachable)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let exit = output.status.code();
        // Le stderr brut va au journal seulement, jamais à l'interface. Jamais le stdout : il porte `~/.idnma.yml`.
        let one_line = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
        crate::hud::log_event(&format!(
            "ssh probe failed destination={} exit={exit:?}: {one_line}",
            spec.destination
        ));
        return Err(classify_failure(exit, &stderr));
    }
    parse_probe(&String::from_utf8_lossy(&output.stdout))
}

/// L'attente avant le prochain essai de connexion : 2, 5, 15, 30 puis 60 s.
pub fn retry_delay(attempt: u32) -> Duration {
    const SECONDS: [u64; 5] = [2, 5, 15, 30, 60];
    Duration::from_secs(SECONDS[(attempt as usize).min(SECONDS.len() - 1)])
}

/// FNV-1a 64 bits.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Le dossier des sockets de contrôle ssh : `/tmp/idema-<uid>-<8 premiers hex du FNV-1a de idema_home>`. Court exprès :
/// ssh refuse un `ControlPath` de 104 octets ou plus. Ne touche pas au disque.
pub fn control_dir(idema_home: &Path, uid: u32) -> PathBuf {
    let hash = fnv1a_64(idema_home.as_os_str().as_encoded_bytes());
    PathBuf::from(format!("/tmp/idema-{uid}-{:08x}", hash >> 32))
}

/// Les seules variables du processus que ssh reçoit.
pub const SSH_ENV: [&str; 6] = ["HOME", "USER", "LOGNAME", "PATH", "SSH_AUTH_SOCK", "LANG"];

/// La machine de la sonde : canal 0, dossier de contrôle de cette instance, et pour seul environnement les variables de
/// `SSH_ENV` que `var` connaît (`home` et `shell` distants sont inconnus tant qu'on ne les a pas sondés). Les réglages
/// (« Tester ») et les sessions s'en servent tous deux : ils partagent ainsi le maître `%C-0`.
pub fn probe_spec(
    connection: &str,
    idema_home: &Path,
    uid: u32,
    var: impl Fn(&str) -> Option<String>,
) -> RemoteSpec {
    RemoteSpec {
        destination: connection.to_string(),
        control_dir: control_dir(idema_home, uid),
        slot: 0,
        ssh_env: SSH_ENV
            .iter()
            .filter_map(|name| var(name).map(|value| ((*name).to_string(), value)))
            .collect(),
        home: String::new(),
        shell: String::new(),
    }
}

/// Crée le dossier de contrôle en 0700, ou vérifie celui qui existe : il doit appartenir à l'utilisateur courant et
/// n'être ouvert ni au groupe ni aux autres (sinon un tiers pourrait détourner un maître ssh).
pub fn ensure_control_dir(dir: &Path) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    // `symlink_metadata` : un lien symbolique posé par un tiers n'est jamais suivi.
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir() {
        return Err(io::Error::other("control dir is not a directory"));
    }
    // SAFETY: `geteuid` n'a aucune précondition.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control dir is owned by another user",
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control dir is open to group or others",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_failure_reads_the_captured_samples() {
        let timeout = "ssh: connect to host 10.255.255.1 port 22: Operation timed out";
        let dns = "ssh: Could not resolve hostname nonexistent-host-idema.invalid: nodename nor servname provided, or not known";
        let host_key = "No ED25519 host key is known for github.com and you have requested strict checking.\nHost key verification failed.";
        let changed = "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@";
        let auth = "git@github.com: Permission denied (publickey).";
        let missing = "/bin/bash: line 1: tmux-absent: command not found";
        let tmux_socket = "error connecting to /tmp/tmux-1003/default (Permission denied)";
        assert_eq!(
            classify_failure(Some(255), timeout),
            RemoteFailure::Unreachable
        );
        assert_eq!(classify_failure(Some(255), dns), RemoteFailure::Unreachable);
        assert_eq!(
            classify_failure(Some(255), host_key),
            RemoteFailure::HostKey
        );
        assert_eq!(classify_failure(Some(255), changed), RemoteFailure::HostKey);
        assert_eq!(
            classify_failure(Some(255), auth),
            RemoteFailure::AuthRefused
        );
        assert_eq!(
            classify_failure(Some(127), missing),
            RemoteFailure::TmuxMissing
        );
        assert_eq!(
            classify_failure(Some(1), tmux_socket),
            RemoteFailure::Unreachable
        );
        assert_eq!(classify_failure(None, auth), RemoteFailure::Unreachable);
    }

    #[test]
    fn failure_texts_match_the_interface_copy() {
        assert_eq!(
            RemoteFailure::HostKey.message("dell-lan"),
            "Clé d'hôte inconnue ou changée : connecte-toi une fois avec ssh dell-lan"
        );
        assert_eq!(
            RemoteFailure::TmuxTooOld("tmux 3.4".into()).message("x"),
            "tmux 3.8 requis, trouvé : tmux 3.4"
        );
        assert_eq!(RemoteFailure::Unreachable.message("x"), "Injoignable");
        assert_eq!(
            RemoteFailure::AuthRefused.message("x"),
            "Authentification refusée"
        );
        assert_eq!(RemoteFailure::TmuxMissing.message("x"), "tmux introuvable");
        assert_eq!(RemoteFailure::Unreachable.short(), "injoignable");
        assert_eq!(
            RemoteFailure::TmuxTooOld("t".into()).short(),
            "tmux trop ancien"
        );
    }

    const MACHINE: &str = "idema-probe-v1\ntmux 3.8\n/home/claude\n/bin/bash\nIDNMA\nagents:\n  glm:\n    label: GLM 5.3\n    avatar: glm\n";

    #[test]
    fn parse_probe_reads_a_machine() {
        let facts = parse_probe(MACHINE).unwrap();
        assert_eq!(facts.tmux_version, (3, 8));
        assert_eq!(facts.home, "/home/claude");
        assert_eq!(facts.shell, "/bin/bash");
        let agents = facts.agents.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].key, "glm");
        assert_eq!(agents[0].label.as_deref(), Some("GLM 5.3"));

        assert_eq!(
            parse_probe("idema-probe-v1\nNO_TMUX\n/home/x\n/bin/sh\nNO_IDNMA\n"),
            Err(RemoteFailure::TmuxMissing)
        );
        assert_eq!(
            parse_probe("idema-probe-v1\ntmux 3.4\n/home/x\n/bin/sh\nNO_IDNMA\n"),
            Err(RemoteFailure::TmuxTooOld("tmux 3.4".into()))
        );
        let no_idnma =
            parse_probe("idema-probe-v1\ntmux 3.8\n/home/x\n/bin/sh\nNO_IDNMA\n").unwrap();
        assert_eq!(no_idnma.agents, None);
    }

    #[test]
    fn parse_probe_skips_a_banner() {
        let with_banner = format!("Welcome to Linux Lite 7.8 llm\r\n\n{MACHINE}");
        assert_eq!(parse_probe(&with_banner), parse_probe(MACHINE));
    }

    #[test]
    fn parse_probe_reads_tmux_version_variants() {
        let probe_with =
            |tmux: &str| parse_probe(&format!("idema-probe-v1\n{tmux}\n/h\n/s\nNO_IDNMA\n"));
        assert_eq!(probe_with("tmux 3.10").unwrap().tmux_version, (3, 10));
        assert_eq!(probe_with("tmux next-3.8").unwrap().tmux_version, (3, 8));
        assert_eq!(probe_with("tmux 4.0a").unwrap().tmux_version, (4, 0));
        assert_eq!(
            probe_with("tmux 3.3a"),
            Err(RemoteFailure::TmuxTooOld("tmux 3.3a".into()))
        );
        assert_eq!(
            probe_with("tmux master"),
            Err(RemoteFailure::TmuxTooOld("tmux master".into()))
        );
    }

    #[test]
    fn parse_probe_truncates_a_long_version_text() {
        let long = format!("tmux {}", "x".repeat(100));
        let Err(RemoteFailure::TmuxTooOld(shown)) =
            parse_probe(&format!("idema-probe-v1\n{long}\n/h\n/s\nNO_IDNMA\n"))
        else {
            panic!("expected TmuxTooOld");
        };
        assert_eq!(shown.chars().count(), 40);
    }

    #[test]
    fn parse_probe_without_marker_is_unreachable() {
        assert_eq!(parse_probe("hello\n"), Err(RemoteFailure::Unreachable));
        assert_eq!(
            parse_probe("idema-probe-v1\n"),
            Err(RemoteFailure::Unreachable)
        );
    }

    #[test]
    fn retry_delay_follows_the_schedule() {
        let seconds: Vec<u64> = (0..6).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(seconds, vec![2, 5, 15, 30, 60, 60]);
        assert_eq!(retry_delay(u32::MAX).as_secs(), 60);
    }

    #[test]
    fn control_dir_is_short_and_stable() {
        let first = control_dir(Path::new("/Users/romain/.idema"), 501);
        assert_eq!(first, control_dir(Path::new("/Users/romain/.idema"), 501));
        assert_ne!(first, control_dir(Path::new("/Users/other/.idema"), 501));
        assert!(first.to_string_lossy().len() <= 40);
        assert!(first.to_string_lossy().starts_with("/tmp/idema-501-"));
    }

    #[test]
    fn ensure_control_dir_refuses_a_wide_mode() {
        let root = std::env::temp_dir().join(format!("idema-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let fresh = root.join("fresh");
        ensure_control_dir(&fresh).unwrap();
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Un dossier déjà conforme est accepté.
        ensure_control_dir(&fresh).unwrap();

        let wide = root.join("wide");
        std::fs::create_dir(&wide).unwrap();
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_control_dir(&wide).is_err());

        // Un lien symbolique n'est jamais suivi.
        let link = root.join("link");
        std::os::unix::fs::symlink(&fresh, &link).unwrap();
        assert!(ensure_control_dir(&link).is_err());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
