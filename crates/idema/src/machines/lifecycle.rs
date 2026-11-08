//! Les décisions pures de la vie d'une session distante : l'agent à lancer, la suite d'une chute de client, l'épingle de
//! taille et le réveil du Mac.

use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};

use idema_tmux::remote::{RemoteSpec, control_check_command, control_exit_command};
use idema_tmux::server::{ServerError, TmuxServer};

use super::idnma::IdnmaAgentInfo;
use crate::config::AgentEntry;
use crate::hud;
use crate::lifecycle::{AfterClose, after_close};

/// Au-delà de ce saut de l'horloge murale, par rapport au pas de la minuterie, le Mac s'est endormi.
const WAKE_MARGIN: Duration = Duration::from_secs(10);

/// Une relecture des sessions qui n'a pas rendu au bout de ce temps : le lien est tenu pour tombé. ssh ne le dira qu'après
/// ses 30 s de `ServerAlive`, et pendant ce temps une saisie s'accumulerait dans un tuyau qui ne part plus.
pub const LINK_STALL: Duration = Duration::from_secs(6);

/// La relecture partie à `started` (s'il y en a une en vol) est-elle bloquée ?
pub fn link_stalled(started: Option<Instant>, now: Instant) -> bool {
    started.is_some_and(|started| now.saturating_duration_since(started) > LINK_STALL)
}

/// La clé d'agent d'une commande de lanceur `idnma run <clé> [options]`. `None` pour toute autre commande.
pub fn idnma_key_of(command: &str) -> Option<&str> {
    let mut words = command.split_whitespace();
    (words.next()? == "idnma" && words.next()? == "run")
        .then(|| words.next())
        .flatten()
}

/// L'agent par défaut d'une machine : celui que l'agent par défaut local désigne (`idnma run <clé>`) si la machine a cette
/// clé, sinon son premier agent. Un agent dont la clé n'est pas un mot simple (`idnma_run_command` la refuse) n'est jamais
/// le défaut. `None` : la machine n'en a aucun, le shell seul.
pub fn default_remote_agent<'a>(
    local_default: Option<&AgentEntry>,
    agents: &'a [IdnmaAgentInfo],
) -> Option<&'a IdnmaAgentInfo> {
    let runnable = |agent: &&IdnmaAgentInfo| idnma_run_command(&agent.key).is_some();
    local_default
        .and_then(|entry| idnma_key_of(&entry.command))
        .and_then(|key| {
            agents
                .iter()
                .filter(runnable)
                .find(|agent| agent.key == key)
        })
        .or_else(|| agents.iter().find(runnable))
}

/// Ce que l'espace de travail fait d'une session distante dont le client est tombé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteAfterClose {
    /// Le tmux de la machine a répondu : la décision de `after_close`.
    Close(AfterClose),
    /// ssh n'a pas répondu : ni présente ni absente, la machine est perdue. L'entrée et sa vue restent.
    MachineLost,
}

/// La suite d'une chute de client sur une machine. `result` est la réponse de `has-session` : une erreur de transport
/// n'est pas une session disparue.
pub fn after_remote_close(
    result: &Result<bool, ServerError>,
    attempts: u8,
    alive_for: Option<Duration>,
) -> RemoteAfterClose {
    match result {
        Ok(exists) => RemoteAfterClose::Close(after_close(*exists, attempts, alive_for)),
        Err(_) => RemoteAfterClose::MachineLost,
    }
}

/// La commande tapée au prompt de la machine pour lancer l'agent `key` de son `~/.idnma.yml`. `None` pour une clé qui n'est
/// pas un mot simple : la clé vient d'un fichier distant, et elle serait lue par un shell.
pub fn idnma_run_command(key: &str) -> Option<String> {
    let plain = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    plain.then(|| format!("idnma run {key}"))
}

/// Les commandes qui épinglent la taille de la fenêtre `window` (`@3`) : IdemA prend le lead tant qu'il l'affiche. Toutes
/// au niveau de la fenêtre, jamais global.
pub fn pin_commands(window: &str, columns: u16, lines: u16) -> Vec<Vec<String>> {
    vec![
        words(&["set-option", "-w", "-t", window, "@idema_size_pin", "1"]),
        words(&["set-option", "-w", "-t", window, "window-size", "manual"]),
        vec![
            "resize-window".into(),
            "-t".into(),
            window.into(),
            "-x".into(),
            columns.to_string(),
            "-y".into(),
            lines.to_string(),
        ],
    ]
}

/// Ce que la vue veut de l'épingle de taille à cet instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinWant {
    /// La session est affichée : sa fenêtre visible `window` (`@<n>`) à cette taille.
    Pin {
        window: u32,
        columns: u16,
        lines: u16,
    },
    /// La session n'est pas (ou plus) affichée : plus d'épingle.
    Release,
    /// Affichée, mais la fenêtre visible ou la taille ne sont pas connues encore : rien à changer.
    Hold,
}

/// Une écriture d'épingle à envoyer sur le client de contrôle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinAction {
    Pin {
        window: u32,
        columns: u16,
        lines: u16,
    },
    Unpin {
        window: u32,
    },
}

impl PinAction {
    /// La ligne du client de contrôle.
    pub fn line(&self) -> String {
        match *self {
            Self::Pin {
                window,
                columns,
                lines,
            } => control_line(&pin_commands(&format!("@{window}"), columns, lines)),
            Self::Unpin { window } => control_line(&unpin_commands(&format!("@{window}"))),
        }
    }
}

/// L'épingle de taille d'une vue : la fenêtre que cette vue a épinglée et dont elle doit encore la retirer. Pure : la vue
/// demande un plan, envoie ses actions, et ne note (`sent`) que celles dont l'écriture a réussi. Une épingle n'est donc
/// jamais oubliée avant son retrait, quel que soit le chemin (lien tombé puis revenu, reconnexion, fenêtre changée).
#[derive(Debug, Default)]
pub struct SizePin {
    pinned: Option<u32>,
}

impl SizePin {
    /// La fenêtre épinglée par cette vue, s'il y en a une.
    pub fn pinned(&self) -> Option<u32> {
        self.pinned
    }

    /// Les écritures à faire pour passer de l'état noté à `want`. Un client sorti ne reçoit rien : l'état est gardé et la
    /// reconnexion refait le plan. Poser rejoue toujours la taille ; une autre fenêtre déjà épinglée est libérée d'abord.
    pub fn plan(&self, want: PinWant, client_alive: bool) -> Vec<PinAction> {
        if !client_alive {
            return Vec::new();
        }
        match want {
            PinWant::Hold => Vec::new(),
            PinWant::Release => self
                .pinned
                .map(|window| PinAction::Unpin { window })
                .into_iter()
                .collect(),
            PinWant::Pin {
                window,
                columns,
                lines,
            } => {
                let mut actions = Vec::new();
                if let Some(old) = self.pinned.filter(|old| *old != window) {
                    actions.push(PinAction::Unpin { window: old });
                }
                actions.push(PinAction::Pin {
                    window,
                    columns,
                    lines,
                });
                actions
            }
        }
    }

    /// Note une action dont l'écriture a réussi.
    pub fn sent(&mut self, action: &PinAction) {
        match *action {
            PinAction::Pin { window, .. } => self.pinned = Some(window),
            PinAction::Unpin { window } => {
                if self.pinned == Some(window) {
                    self.pinned = None;
                }
            }
        }
    }
}

/// Les commandes qui rendent la taille de la fenêtre `window` à tmux et à Ghostty.
pub fn unpin_commands(window: &str) -> Vec<Vec<String>> {
    vec![
        words(&["set-option", "-w", "-u", "-t", window, "window-size"]),
        words(&["set-option", "-w", "-u", "-t", window, "@idema_size_pin"]),
    ]
}

/// Les deux seules écritures de serveur permises sur une machine : `pane_key_mode` les exige (sans elles, Maj+Entrée se
/// perd), et Ghostty ne les laisse pas voir. Posées à la connexion de la machine.
pub fn extended_keys_commands() -> Vec<Vec<String>> {
    vec![
        words(&["set-option", "-s", "extended-keys", "on"]),
        words(&["set-option", "-s", "extended-keys-format", "csi-u"]),
    ]
}

/// Des commandes tmux sur une seule ligne du client de contrôle (`;` entre deux).
pub fn control_line(commands: &[Vec<String>]) -> String {
    commands
        .iter()
        .map(|command| command.join(" "))
        .collect::<Vec<_>>()
        .join(" ; ")
}

/// Le Mac sort-il de veille ? L'horloge murale a avancé de plus de 10 s de plus que l'horloge monotone (`Instant`, qui sur
/// macOS ne compte pas le temps de veille) : une minuterie en retard (App Nap, fil d'interface bloqué) fait avancer les
/// deux de la même durée et n'est pas un réveil. Une horloge murale qui recule (réglage manuel) n'en est pas un non plus.
/// `monotonic` : le temps écoulé de l'horloge monotone entre les deux relevés.
pub fn is_wake_jump(previous: SystemTime, now: SystemTime, monotonic: Duration) -> bool {
    now.duration_since(previous)
        .is_ok_and(|wall| wall.saturating_sub(monotonic) > WAKE_MARGIN)
}

/// La ligne grisée d'un menu de machine sans `~/.idnma.yml`.
pub const NO_IDNMA_NOTE: &str = "Aucun ~/.idnma.yml sur cette machine";

/// Une ligne du menu des agents d'une machine (clic droit sur son « + »).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteMenuLine {
    /// Un agent du fichier : sa clé (ce que `idnma run` reçoit), son texte (`label`, sinon la clé).
    Agent {
        key: String,
        text: String,
        default: bool,
        /// Faux : la clé n'est pas un mot simple, l'entrée est grisée (elle serait lue par le shell de la machine).
        runnable: bool,
    },
    Note(&'static str),
    Separator,
    Shell {
        default: bool,
    },
}

/// Le menu des agents d'une machine : ceux de son `~/.idnma.yml` dans l'ordre du fichier, puis « Shell seul ». `agents` à
/// `None` : pas de fichier, la ligne grisée explique. `default_key` : la clé de l'agent par défaut, cochée ; sans elle, le
/// shell seul l'est.
pub fn remote_menu_lines(
    agents: Option<&[IdnmaAgentInfo]>,
    default_key: Option<&str>,
) -> Vec<RemoteMenuLine> {
    let mut lines = Vec::new();
    match agents {
        None => lines.push(RemoteMenuLine::Note(NO_IDNMA_NOTE)),
        Some(agents) => {
            lines.extend(agents.iter().map(|agent| RemoteMenuLine::Agent {
                key: agent.key.clone(),
                text: agent.label.clone().unwrap_or_else(|| agent.key.clone()),
                default: default_key == Some(agent.key.as_str()),
                runnable: idnma_run_command(&agent.key).is_some(),
            }));
            if !agents.is_empty() {
                lines.push(RemoteMenuLine::Separator);
            }
        }
    }
    let default_is_agent = lines
        .iter()
        .any(|line| matches!(line, RemoteMenuLine::Agent { default: true, .. }));
    lines.push(RemoteMenuLine::Shell {
        default: !default_is_agent,
    });
    lines
}

/// Le format de `list-windows -a` qui montre les épingles de taille.
pub const PINNED_WINDOWS_FORMAT: &str = "#{session_name}\t#{window_id}\t#{@idema_size_pin}";

/// Les fenêtres dont IdemA a épinglé la taille (`@idema_size_pin` à 1) et qu'il n'affiche pas : un IdemA tombé les a
/// laissées. `spared` : la session que cette instance affiche et sa fenêtre visible (`@<n>`), seule épargnée : sa vue
/// repose son épingle, et une autre fenêtre épinglée de la même session est périmée.
pub fn windows_to_unpin(listing: &str, spared: Option<(&str, u32)>) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let (session, window, pin) = (fields.next()?, fields.next()?, fields.next()?);
            let number: u32 = window.strip_prefix('@')?.parse().ok()?;
            let keeps = spared == Some((session, number));
            (pin == "1" && !keeps).then(|| window.to_owned())
        })
        .collect()
}

/// Pose les deux écritures de serveur permises (`extended-keys`). Bloquant (ssh), hors du fil d'interface. Sans serveur tmux
/// sur la machine (aucune session), tmux refuse : la création d'une session la repose, la connexion suivante aussi.
pub fn set_extended_keys(server: &TmuxServer) {
    for command in extended_keys_commands() {
        let args: Vec<&str> = command.iter().map(String::as_str).collect();
        if let Err(error) = server.run(&args) {
            hud::log_event(&format!("remote server option skipped: {error}"));
        }
    }
}

/// Les fenêtres épinglées de la machine (`list-windows -a`) : une ligne `session<TAB>@fenêtre<TAB>épingle` par fenêtre.
/// Bloquant (ssh), hors du fil d'interface. Sans serveur tmux, il n'y en a aucune.
pub fn list_pinned_windows(server: &TmuxServer) -> String {
    server
        .run(&["list-windows", "-a", "-F", PINNED_WINDOWS_FORMAT])
        .unwrap_or_default()
}

/// Libère les épingles de `windows` (`@<n>`) : bloquant (ssh), hors du fil d'interface.
pub fn release_windows(server: &TmuxServer, windows: &[String]) {
    for window in windows {
        let mut args: Vec<String> = Vec::new();
        for (index, command) in unpin_commands(window).into_iter().enumerate() {
            if index > 0 {
                args.push(";".into());
            }
            args.extend(command);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match server.run(&args) {
            Ok(_) => hud::log_event(&format!("stale size pin released window={window}")),
            Err(error) => hud::log_event(&format!("stale size pin kept window={window}: {error}")),
        }
    }
}

/// Ferme le maître ssh de `spec` (`ssh -O exit`) : bloquant, hors du fil d'interface. Un maître déjà parti n'est pas une
/// erreur.
pub fn close_master(spec: &RemoteSpec) {
    let status = control_exit_command(spec)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Err(error) = status {
        hud::log_event(&format!(
            "ssh master close failed destination={}: {error}",
            spec.destination
        ));
    }
}

/// Le pid que `ssh -O check` annonce (« Master running (pid=1234) »).
pub fn parse_master_pid(stderr: &str) -> Option<u32> {
    let rest = stderr.split("(pid=").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Le pid du maître ssh de `spec`, lu par `ssh -O check` : bloquant, hors du fil d'interface. `None` quand la lecture
/// échoue : elle ne bloque jamais la connexion, le journal écrit alors `?`.
pub fn master_pid(spec: &RemoteSpec) -> Option<u32> {
    let output = control_check_command(spec)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .ok()?;
    parse_master_pid(&String::from_utf8_lossy(&output.stderr))
}

fn words(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentEntry;
    use crate::lifecycle::{AfterClose, REATTACH_ATTEMPTS, REATTACH_STABLE, after_close};
    use idema_tmux::command::border_reset;
    use idema_tmux::remote::forbidden_on_remote;
    use idema_tmux::server::ServerError;
    use std::time::{Duration, Instant, SystemTime};

    fn local(command: &str) -> AgentEntry {
        AgentEntry {
            name: "Local".into(),
            harness: None,
            command: command.into(),
            default: true,
            icon: None,
        }
    }

    fn remote(key: &str) -> IdnmaAgentInfo {
        IdnmaAgentInfo {
            key: key.into(),
            label: None,
            avatar: None,
        }
    }

    #[test]
    fn parse_master_pid_reads_the_check_answer() {
        assert_eq!(
            parse_master_pid("Master running (pid=4242)\r\n"),
            Some(4242)
        );
        assert_eq!(
            parse_master_pid("Control socket connect(/tmp/x): No such file or directory"),
            None
        );
        assert_eq!(parse_master_pid("Master running (pid=)"), None);
    }

    #[test]
    fn idnma_key_of_reads_the_launcher_command() {
        assert_eq!(idnma_key_of("idnma run claude"), Some("claude"));
        assert_eq!(idnma_key_of("idnma run glm --x"), Some("glm"));
        assert_eq!(idnma_key_of("  idnma   run   opus  "), Some("opus"));
        assert_eq!(idnma_key_of("bash"), None);
        assert_eq!(idnma_key_of("idnma run"), None);
        assert_eq!(idnma_key_of("idnma list claude"), None);
        assert_eq!(idnma_key_of(""), None);
    }

    #[test]
    fn the_default_remote_agent_follows_the_local_default() {
        let claude = local("idnma run claude");
        let machine = [remote("opus"), remote("claude")];
        assert_eq!(
            default_remote_agent(Some(&claude), &machine).map(|agent| agent.key.as_str()),
            Some("claude")
        );
        // La machine n'a pas la clé locale : son premier agent.
        let machine = [remote("opus"), remote("glm")];
        assert_eq!(
            default_remote_agent(Some(&claude), &machine).map(|agent| agent.key.as_str()),
            Some("opus")
        );
        // Une machine sans agent : le shell seul.
        assert_eq!(default_remote_agent(Some(&claude), &[]), None);
        assert_eq!(default_remote_agent(None, &[]), None);
        // Un défaut local qui n'est pas un lanceur `idnma run` : le premier de la machine.
        let bash = local("bash");
        let machine = [remote("opus"), remote("claude")];
        assert_eq!(
            default_remote_agent(Some(&bash), &machine).map(|agent| agent.key.as_str()),
            Some("opus")
        );
        assert_eq!(
            default_remote_agent(None, &machine).map(|agent| agent.key.as_str()),
            Some("opus")
        );
    }

    #[test]
    fn only_a_plain_agent_key_becomes_a_command_for_the_remote_shell() {
        assert_eq!(
            idnma_run_command("claude").as_deref(),
            Some("idnma run claude")
        );
        assert_eq!(
            idnma_run_command("glm-5.1_x").as_deref(),
            Some("idnma run glm-5.1_x")
        );
        for key in ["", "a b", "a;b", "$(x)", "`x`", "a'b", "a\nb", "a&b", "a|b"] {
            assert_eq!(idnma_run_command(key), None, "{key:?}");
        }
    }

    #[test]
    fn a_transport_error_is_a_lost_machine_not_a_gone_session() {
        for exists in [false, true] {
            for attempts in 0..=REATTACH_ATTEMPTS {
                for alive_for in [
                    None,
                    Some(Duration::from_millis(100)),
                    Some(REATTACH_STABLE),
                ] {
                    assert_eq!(
                        after_remote_close(&Ok(exists), attempts, alive_for),
                        RemoteAfterClose::Close(after_close(exists, attempts, alive_for)),
                        "exists={exists} attempts={attempts} alive_for={alive_for:?}"
                    );
                }
            }
        }
        // Les essais épuisés restent « injoignable » : le budget d'`after_close` est conservé.
        assert_eq!(
            after_remote_close(&Ok(true), REATTACH_ATTEMPTS, None),
            RemoteAfterClose::Close(AfterClose::Unreachable)
        );
        let io = Err(ServerError::Io(std::io::Error::other("broken pipe")));
        assert_eq!(
            after_remote_close(&io, 0, None),
            RemoteAfterClose::MachineLost
        );
        let timeout = Err(ServerError::Tmux {
            args: "has-session -t =idema-1".into(),
            stderr: "ssh: connect to host dell port 22: Operation timed out".into(),
        });
        assert_eq!(
            after_remote_close(&timeout, REATTACH_ATTEMPTS, Some(REATTACH_STABLE)),
            RemoteAfterClose::MachineLost
        );
    }

    #[test]
    fn pin_and_unpin_target_the_window_only() {
        let commands: Vec<Vec<String>> = pin_commands("@3", 132, 40)
            .into_iter()
            .chain(unpin_commands("@3"))
            .collect();
        for command in &commands {
            let words: Vec<&str> = command.iter().map(String::as_str).collect();
            assert_eq!(forbidden_on_remote(&words), None, "{words:?}");
            if words[0] == "set-option" {
                assert!(words.contains(&"-w") || words.contains(&"-wu"), "{words:?}");
                assert!(
                    !words
                        .iter()
                        .any(|word| word.contains('g') && word.starts_with('-')),
                    "{words:?}"
                );
            }
            assert!(words.contains(&"@3"), "{words:?}");
        }
        assert_eq!(
            pin_commands("@3", 132, 40),
            vec![
                vec!["set-option", "-w", "-t", "@3", "@idema_size_pin", "1"],
                vec!["set-option", "-w", "-t", "@3", "window-size", "manual"],
                vec!["resize-window", "-t", "@3", "-x", "132", "-y", "40"],
            ]
        );
        assert_eq!(
            unpin_commands("@3"),
            vec![
                vec!["set-option", "-w", "-u", "-t", "@3", "window-size"],
                vec!["set-option", "-w", "-u", "-t", "@3", "@idema_size_pin"],
            ]
        );
    }

    #[test]
    fn pin_commands_fit_one_control_client_line() {
        let line = control_line(&pin_commands("@12", 80, 24));
        assert_eq!(
            line,
            "set-option -w -t @12 @idema_size_pin 1 ; set-option -w -t @12 window-size manual ; resize-window -t @12 -x 80 -y 24"
        );
        assert!(!line.contains('\n'));
    }

    #[test]
    fn the_server_options_are_the_two_allowed_extended_keys() {
        let commands = extended_keys_commands();
        assert_eq!(commands.len(), 2);
        for command in &commands {
            let words: Vec<&str> = command.iter().map(String::as_str).collect();
            assert_eq!(forbidden_on_remote(&words), None, "{words:?}");
        }
    }

    #[test]
    fn a_listing_in_flight_for_too_long_reads_as_a_stalled_link() {
        let now = Instant::now();
        let started = now - Duration::from_secs(1);
        assert!(!link_stalled(None, now));
        assert!(!link_stalled(Some(started), now));
        assert!(link_stalled(
            Some(now - LINK_STALL - Duration::from_millis(1)),
            now
        ));
    }

    #[test]
    fn a_clock_jump_reads_as_a_wake() {
        let secs = Duration::from_secs;
        let before = SystemTime::UNIX_EPOCH + secs(1_000);
        // Un tick à l'heure.
        assert!(!is_wake_jump(before, before + secs(2), secs(2)));
        // Un tick en retard (App Nap, fil bloqué) : les deux horloges ont avancé autant, ce n'est pas un réveil.
        assert!(!is_wake_jump(before, before + secs(60), secs(60)));
        assert!(!is_wake_jump(before, before + secs(20), secs(20)));
        // Une veille : l'horloge murale a avancé de 60 s, la monotone de 2 s.
        assert!(is_wake_jump(before, before + secs(60), secs(2)));
        // Juste sous la marge de 10 s : pas un réveil.
        assert!(!is_wake_jump(before, before + secs(12), secs(2)));
        // Une horloge murale qui recule (réglage manuel) n'est pas un réveil.
        assert!(!is_wake_jump(before, before - secs(60), secs(2)));
    }

    #[test]
    fn a_remote_agent_the_shell_would_misread_is_neither_runnable_nor_the_default() {
        let agents = [remote("a;b"), remote("glm")];
        // Le défaut local désigne la clé refusée : on prend le premier agent qu'on sait lancer.
        let risky = local("idnma run a;b");
        assert_eq!(
            default_remote_agent(Some(&risky), &agents).map(|agent| agent.key.as_str()),
            Some("glm")
        );
        assert_eq!(
            default_remote_agent(None, &[remote("a b")]).map(|agent| agent.key.as_str()),
            None
        );
        let lines = remote_menu_lines(Some(&agents), None);
        assert!(matches!(
            &lines[0],
            RemoteMenuLine::Agent {
                runnable: false,
                default: false,
                ..
            }
        ));
        assert!(matches!(
            &lines[1],
            RemoteMenuLine::Agent { runnable: true, .. }
        ));
    }

    /// Le décor des scénarios de l'épingle : la fenêtre `@3`, 120x40.
    fn want(window: u32) -> PinWant {
        PinWant::Pin {
            window,
            columns: 120,
            lines: 40,
        }
    }

    /// Planifie puis applique, chaque action réussie étant notée.
    fn step(pin: &mut SizePin, want: PinWant, client_alive: bool) -> Vec<PinAction> {
        let plan = pin.plan(want, client_alive);
        apply(pin, plan.clone());
        plan
    }

    /// Applique un plan comme la vue : chaque action réussie est notée.
    fn apply(pin: &mut SizePin, plan: Vec<PinAction>) {
        for action in plan {
            pin.sent(&action);
        }
    }

    #[test]
    fn a_displayed_session_pins_its_visible_window_and_a_release_unpins_it() {
        let mut pin = SizePin::default();
        let plan = pin.plan(want(3), true);
        assert_eq!(
            plan,
            vec![PinAction::Pin {
                window: 3,
                columns: 120,
                lines: 40
            }]
        );
        apply(&mut pin, plan);
        assert_eq!(pin.pinned(), Some(3));
        // Plus rien à envoyer tant que rien ne change : seul `Pin` rejoue la taille.
        assert_eq!(pin.plan(PinWant::Hold, true), vec![]);
        let plan = pin.plan(PinWant::Release, true);
        assert_eq!(plan, vec![PinAction::Unpin { window: 3 }]);
        apply(&mut pin, plan);
        assert_eq!(pin.pinned(), None);
        assert_eq!(pin.plan(PinWant::Release, true), vec![]);
    }

    #[test]
    fn a_pin_is_forgotten_only_after_its_unpin_was_sent() {
        let mut pin = SizePin::default();
        step(&mut pin, want(3), true);
        // Client sorti : rien ne part, l'épingle reste due.
        assert_eq!(pin.plan(PinWant::Release, false), vec![]);
        assert_eq!(pin.pinned(), Some(3));
        // L'écriture a échoué (`sent` jamais appelé) : le plan suivant réessaie.
        let _lost = pin.plan(PinWant::Release, true);
        assert_eq!(pin.pinned(), Some(3));
        assert_eq!(
            pin.plan(PinWant::Release, true),
            vec![PinAction::Unpin { window: 3 }]
        );
    }

    #[test]
    fn a_session_left_while_its_link_was_down_unpins_once_the_link_is_back() {
        // Chemin A : relecture bloquée, l'utilisateur passe sur une session locale, la relecture répond.
        let mut pin = SizePin::default();
        step(&mut pin, want(3), true);
        // Pendant la coupure le client est resté vivant : le retrait part quand même (la file ne bloque plus).
        assert_eq!(
            pin.plan(PinWant::Release, true),
            vec![PinAction::Unpin { window: 3 }]
        );
    }

    #[test]
    fn a_window_that_changed_during_a_cut_loses_its_old_pin() {
        // La fenêtre épinglée est `@3`, la visible devient `@5` : l'ancienne est libérée avant que la nouvelle soit posée.
        let mut pin = SizePin::default();
        step(&mut pin, want(3), true);
        let plan = pin.plan(want(5), true);
        assert_eq!(
            plan,
            vec![
                PinAction::Unpin { window: 3 },
                PinAction::Pin {
                    window: 5,
                    columns: 120,
                    lines: 40
                }
            ]
        );
        apply(&mut pin, plan);
        assert_eq!(pin.pinned(), Some(5));
    }

    #[test]
    fn a_pin_resent_after_a_zone_resize_carries_the_new_size() {
        let mut pin = SizePin::default();
        step(&mut pin, want(3), true);
        let plan = pin.plan(
            PinWant::Pin {
                window: 3,
                columns: 90,
                lines: 30,
            },
            true,
        );
        assert_eq!(
            plan,
            vec![PinAction::Pin {
                window: 3,
                columns: 90,
                lines: 30
            }]
        );
    }

    #[test]
    fn an_unpin_command_line_matches_the_pin_that_was_sent() {
        let pin = PinAction::Pin {
            window: 3,
            columns: 120,
            lines: 40,
        };
        assert_eq!(pin.line(), control_line(&pin_commands("@3", 120, 40)));
        assert_eq!(
            PinAction::Unpin { window: 3 }.line(),
            control_line(&unpin_commands("@3"))
        );
    }

    fn labelled(key: &str, label: &str) -> IdnmaAgentInfo {
        IdnmaAgentInfo {
            key: key.into(),
            label: Some(label.into()),
            avatar: None,
        }
    }

    #[test]
    fn the_remote_menu_lists_the_file_in_order_by_label_else_key_then_the_shell() {
        let agents = [labelled("opus", "Opus 4"), remote("glm")];
        assert_eq!(
            remote_menu_lines(Some(&agents), Some("glm")),
            vec![
                RemoteMenuLine::Agent {
                    key: "opus".into(),
                    text: "Opus 4".into(),
                    default: false,
                    runnable: true,
                },
                RemoteMenuLine::Agent {
                    key: "glm".into(),
                    text: "glm".into(),
                    default: true,
                    runnable: true,
                },
                RemoteMenuLine::Separator,
                RemoteMenuLine::Shell { default: false },
            ]
        );
        // Sans agent par défaut, le shell seul est l'entrée cochée.
        assert_eq!(
            remote_menu_lines(Some(&agents), None).last(),
            Some(&RemoteMenuLine::Shell { default: true })
        );
    }

    #[test]
    fn a_machine_without_an_idnma_file_offers_the_shell_and_says_why() {
        assert_eq!(
            remote_menu_lines(None, None),
            vec![
                RemoteMenuLine::Note(NO_IDNMA_NOTE),
                RemoteMenuLine::Shell { default: true },
            ]
        );
        assert_eq!(NO_IDNMA_NOTE, "Aucun ~/.idnma.yml sur cette machine");
        // Un fichier présent mais sans agent : pas de note, le shell seul.
        assert_eq!(
            remote_menu_lines(Some(&[]), None),
            vec![RemoteMenuLine::Shell { default: true }]
        );
    }

    #[test]
    fn only_pinned_windows_of_sessions_that_are_not_displayed_are_released() {
        let listing =
            "work\t@1\t1\nwork\t@2\t\nold\t@7\t1\nother\t@9\t0\nwork\t@4\t1\n\nbroken line\n";
        // Seule la fenêtre visible de la session affichée est épargnée : son autre fenêtre épinglée (`@4`, restée d'une
        // coupure où la visible a changé) est libérée avec celles des autres sessions.
        assert_eq!(
            windows_to_unpin(listing, Some(("work", 1))),
            vec!["@7", "@4"]
        );
        assert_eq!(windows_to_unpin(listing, None), vec!["@1", "@7", "@4"]);
        assert!(windows_to_unpin("", None).is_empty());
    }

    #[test]
    fn the_border_reset_writes_only_a_change() {
        assert_eq!(border_reset("@1 off"), None);
        assert_eq!(border_reset("@1 "), None);
        assert!(border_reset("@1 top").is_some());
    }
}
