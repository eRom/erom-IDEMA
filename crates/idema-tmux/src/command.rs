//! Construction des commandes envoyées à tmux, et lecture de sa version.

use crate::protocol::PaneId;

/// Taille maximale d'un envoi `send-keys -H`, en octets, pour garder des lignes de commande courtes.
const SEND_KEYS_CHUNK: usize = 256;

/// Commandes `send-keys -H` qui écrivent ces octets tels quels dans le pane.
/// `-H` évite tout échappement : chaque octet part en hexadécimal.
pub fn send_keys_hex(pane: PaneId, bytes: &[u8]) -> Vec<String> {
    bytes
        .chunks(SEND_KEYS_CHUNK)
        .map(|chunk| {
            let hex: Vec<String> = chunk.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("send-keys -t %{} -H {}", pane.0, hex.join(" "))
        })
        .collect()
}

/// Retire l'option utilisateur `key` (`@…`) d'un pane. tmux l'accepte aussi quand l'option n'est pas posée (tmux 3.8).
pub fn unset_pane_option(pane: PaneId, key: &str) -> String {
    format!("set-option -p -u -t %{} {key}", pane.0)
}

/// Met une valeur entre apostrophes pour le parseur de commandes de tmux.
/// Contrat à vérifier contre un vrai tmux (tâche 4) : `set -g @x <quote(v)>` puis `show -gv @x` rend `v`.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Prépare une valeur libre pour la ligne de commande de tmux. tmux prend un argument terminé par « ; » pour
/// la fin d'une commande et retire ce « ; » ; un « \; » final donne un « ; » littéral (sonde du 15/10).
pub fn tmux_arg(value: &str) -> String {
    match value.strip_suffix(';') {
        Some(head) => format!("{head}\\;"),
        None => value.to_string(),
    }
}

/// Déclare à tmux la couleur du terminal d'un pane (`osc` 10 : texte, 11 : fond) : un programme qui l'interroge par
/// `OSC 10 ; ?` ou `OSC 11 ; ?` reçoit celle d'Idema, pas un repli (AC-40). Forme X11, deux chiffres doublés par canal
/// (`0x15` donne `1515`). Les échappements restent écrits en quatre caractères (`\033`) : tmux les lit lui-même (sonde du 16/10).
pub fn color_report(pane: PaneId, osc: u8, rgb: [u8; 3]) -> String {
    let [red, green, blue] = rgb;
    format!(
        r#"refresh-client -r "%{}:\033]{osc};rgb:{red:02x}{red:02x}/{green:02x}{green:02x}/{blue:02x}{blue:02x}\033\\""#,
        pane.0
    )
}

/// Nom de l'abonnement qui suit `pane-border-status` de chaque fenêtre de la session (AC-38).
pub const BORDER_WATCH: &str = "idema-border";

/// Abonnement de fenêtre (`@*` : toutes les fenêtres de la session attachée, la cachée comprise) : tmux envoie une
/// ligne `« @<n> <valeur> »` par fenêtre dont `pane-border-status` change, une seule par changement (sonde du 16/10).
pub fn border_subscription() -> String {
    format!("refresh-client -B '{BORDER_WATCH}:@*:#{{window_id}} #{{pane-border-status}}'")
}

/// Remise à `off` pour la valeur d'un `%subscription-changed` de `BORDER_WATCH` (« @3 top »). `None` quand la bordure
/// est déjà éteinte, ou que la valeur n'a pas la forme attendue : la remise à `off` ne donne ainsi pas de boucle,
/// puisque tmux la signale une dernière fois en `off` et que ce `off` ne déclenche rien.
pub fn border_reset(value: &str) -> Option<String> {
    let (window, status) = value.split_once(' ')?;
    let digits = window.strip_prefix('@')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let status = status.trim();
    if status.is_empty() || status == "off" {
        return None;
    }
    Some(format!("set-option -w -t {window} pane-border-status off"))
}

/// Lit la sortie de `tmux -V` : « tmux 3.8 », « tmux 3.8a », « tmux next-3.9 ».
pub fn parse_version(output: &str) -> Option<(u32, u32)> {
    let version = output
        .trim()
        .rsplit(' ')
        .next()?
        .trim_start_matches("next-");
    let (major, rest) = version.split_once('.')?;
    let minor: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Idema exige tmux 3.8 au minimum (spec §3.1).
pub fn is_supported(version: (u32, u32)) -> bool {
    version >= (3, 8)
}

/// Ce qu'Idema suit d'un pane par un abonnement `refresh-client -B`, que tmux réévalue au plus une fois par
/// seconde et ne signale qu'au changement (spec §5.1, AC-26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneWatch {
    /// `pane_current_command` : le processus au premier plan (repli sans OSC 133, confirmation de fermeture).
    Command,
    /// `@idema_agent` : l'entrée du lanceur du pane (spec §4.2).
    Agent,
    /// `pane_current_path` : le dossier courant du pane, contre lequel se résout un chemin relatif (spec §4.7).
    Path,
}

impl PaneWatch {
    fn prefix(self) -> &'static str {
        match self {
            Self::Command => "idema-cmd-",
            Self::Agent => "idema-agent-",
            Self::Path => "idema-path-",
        }
    }

    fn format(self) -> &'static str {
        match self {
            Self::Command => "#{pane_current_command}",
            Self::Agent => "#{@idema_agent}",
            Self::Path => "#{pane_current_path}",
        }
    }

    /// Commande d'abonnement, à envoyer au client de contrôle de la session du pane.
    pub fn subscribe(self, pane: PaneId) -> String {
        format!(
            "refresh-client -B '{}{}:%{}:{}'",
            self.prefix(),
            pane.0,
            pane.0,
            self.format()
        )
    }

    /// Retire l'abonnement : `refresh-client -B` avec le nom seul (pane retiré du suivi).
    pub fn unsubscribe(self, pane: PaneId) -> String {
        format!("refresh-client -B {}{}", self.prefix(), pane.0)
    }

    /// Reconnaît le nom d'un abonnement de `subscribe` dans `%subscription-changed`.
    pub fn parse(name: &str) -> Option<(Self, PaneId)> {
        [Self::Command, Self::Agent, Self::Path]
            .into_iter()
            .find_map(|watch| {
                let number = name.strip_prefix(watch.prefix())?.parse().ok()?;
                Some((watch, PaneId(number)))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colour_report_names_the_pane_and_the_colour_in_x11_form() {
        let line = color_report(PaneId(3), 11, [0x15, 0x15, 0x16]);
        assert_eq!(
            line,
            r#"refresh-client -r "%3:\033]11;rgb:1515/1515/1616\033\\""#
        );
        // Les échappements partent en quatre caractères : aucun octet ESC dans la ligne envoyée à tmux.
        assert!(line.bytes().all(|byte| byte != 0x1b));
    }

    #[test]
    fn only_a_border_that_is_not_off_is_reset() {
        assert_eq!(
            border_reset("@3 top").as_deref(),
            Some("set-option -w -t @3 pane-border-status off")
        );
        assert_eq!(
            border_reset("@3 bottom").as_deref(),
            Some("set-option -w -t @3 pane-border-status off")
        );
        assert_eq!(border_reset("@3 off"), None);
        assert_eq!(border_reset(""), None);
        assert_eq!(border_reset("top"), None);
    }

    #[test]
    fn pane_watch_names_round_trip() {
        for watch in [PaneWatch::Command, PaneWatch::Agent, PaneWatch::Path] {
            let line = watch.subscribe(PaneId(7));
            let name = line
                .trim_start_matches("refresh-client -B '")
                .split(':')
                .next()
                .unwrap();
            assert_eq!(PaneWatch::parse(name), Some((watch, PaneId(7))));
        }
        assert_eq!(PaneWatch::parse("idema-size-7"), None);
        assert_eq!(PaneWatch::parse("idema-cmd-x"), None);
    }

    #[test]
    fn unsubscribing_names_the_subscription_without_a_format() {
        for watch in [PaneWatch::Command, PaneWatch::Agent, PaneWatch::Path] {
            let subscribe = watch.subscribe(PaneId(7));
            let name = subscribe
                .trim_start_matches("refresh-client -B '")
                .split(':')
                .next()
                .unwrap();
            assert_eq!(
                watch.unsubscribe(PaneId(7)),
                format!("refresh-client -B {name}")
            );
        }
    }

    #[test]
    fn send_keys_hex_encodes_every_byte() {
        assert_eq!(
            send_keys_hex(PaneId(3), b"\x1b[A"),
            vec!["send-keys -t %3 -H 1b 5b 41".to_string()]
        );
    }

    #[test]
    fn send_keys_hex_encodes_utf8_bytes() {
        assert_eq!(
            send_keys_hex(PaneId(1), "é".as_bytes()),
            vec!["send-keys -t %1 -H c3 a9".to_string()]
        );
    }

    #[test]
    fn send_keys_hex_splits_long_input() {
        let commands = send_keys_hex(PaneId(0), &[b'a'; 600]);
        assert_eq!(commands.len(), 3);
        assert!(
            commands
                .iter()
                .all(|command| command.starts_with("send-keys -t %0 -H 61"))
        );
    }

    #[test]
    fn quote_wraps_and_escapes_apostrophes() {
        assert_eq!(
            quote("/Users/r/dev/mon projet"),
            "'/Users/r/dev/mon projet'"
        );
        assert_eq!(quote("l'atelier"), r"'l'\''atelier'");
    }

    #[test]
    fn tmux_arg_escapes_only_a_trailing_semicolon() {
        assert_eq!(tmux_arg("fin;"), r"fin\;");
        assert_eq!(tmux_arg("a;b"), "a;b");
        assert_eq!(tmux_arg("Claude 2"), "Claude 2");
    }

    #[test]
    fn parse_version_reads_release_patch_and_next() {
        assert_eq!(parse_version("tmux 3.8\n"), Some((3, 8)));
        assert_eq!(parse_version("tmux 3.8a"), Some((3, 8)));
        assert_eq!(parse_version("tmux next-3.9"), Some((3, 9)));
        assert_eq!(parse_version("tmux 3.10"), Some((3, 10)));
        assert_eq!(parse_version("not tmux"), None);
    }

    #[test]
    fn minimum_version_is_three_eight() {
        assert!(is_supported((3, 8)));
        assert!(is_supported((4, 0)));
        assert!(!is_supported((3, 7)));
    }
}
