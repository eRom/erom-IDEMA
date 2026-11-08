//! Le compagnon iPhone : un petit service HTTP dans le process d'IdemA, lié à `127.0.0.1`, que la page installée sur
//! le téléphone interroge (par `tailscale serve`). Un fil dédié porte son propre runtime tokio ; le contrôleur
//! (`controller`) le démarre, l'arrête et le redémarre selon la section `[companion]` de la config. Section absente :
//! aucune prise, aucun fil, aucune ligne de journal.

mod access;
mod auth;
pub mod controller;
mod devices;
pub mod episodes;
pub mod fleet;
pub mod input;
mod notify;
mod notify_text;
pub mod presence;
mod push_send;
pub mod scratchpad;
pub mod screen;
mod server;
pub mod service;
mod sse;
pub mod tailscale;
#[cfg(test)]
mod test_support;
mod timeline;
mod web_assets;
mod webpush;

use std::time::{Instant, SystemTime};

use tokio::sync::oneshot;

use crate::config::CompanionConfig;
use crate::machines::key::{SessionTarget, split_session_key};
pub use controller::CompanionController;
use episodes::EpisodeKind;
use input::RemoteInput;
use tailscale::TailnetState;

/// Ce que le service dit de lui à l'espace de travail (et, plus tard, à Personnaliser).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceStatus {
    pub listen: ListenState,
    /// Le code d'appairage en cours, s'il y en a un de valide.
    pub pairing: Option<PairingDisplay>,
    /// Les appareils appairés, du plus ancien au plus récent.
    pub devices: Vec<DeviceSummary>,
    /// L'adresse tailnet du service. `None` tant que la première détection n'a pas rendu sa réponse.
    pub tailnet: Option<TailnetState>,
}

/// Où en est la prise du service.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ListenState {
    /// Le service démarre : la prise n'est pas encore liée.
    #[default]
    Starting,
    Listening {
        port: u16,
    },
    /// La prise n'a pas pu être liée (port pris, par exemple) : le service ne tourne pas.
    Failed {
        port: u16,
        reason: String,
    },
}

/// Le code d'appairage à montrer dans Personnaliser, et l'instant où il cesse de valoir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingDisplay {
    pub code: String,
    pub expires_at: SystemTime,
}

/// Un appareil appairé, tel que Personnaliser le liste. Jamais le jeton ni l'empreinte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSummary {
    pub id: String,
    pub name: String,
    pub paired_at: SystemTime,
    pub last_seen_at: SystemTime,
    /// L'appareil a un abonnement aux notifications.
    pub subscribed: bool,
}

/// Ce que l'espace de travail envoie au service. Les tâches suivantes ajoutent les variantes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceEvent {
    /// Émettre un code d'appairage (le précédent, s'il y en avait un, cesse de valoir).
    StartPairing,
    /// Annuler le code en cours.
    CancelPairing,
    /// Oublier un appareil et son abonnement : sa requête suivante prend 401.
    ForgetDevice { id: String },
    /// Une saisie ou un acquittement venus du téléphone ont touché cette session : Romain agit depuis l'iPhone,
    /// l'anti-rafale de la session repart de zéro (`episodes::Episodes::phone_interaction`).
    PhoneInteraction { session: String },
    /// Romain est devenu absent (`away`) ou présent : seul le changement part, jamais un état répété.
    Presence { away: bool },
    /// Une session entre dans un épisode « attend » ou « erreur » (IdemA en sonnerait, ou elle passe en erreur) ; `pane`
    /// est le plus urgent de ses panes.
    EpisodeOpened {
        session: String,
        pane: u32,
        kind: EpisodeKind,
    },
    /// L'épisode est fini : l'état a quitté « attend » ou « erreur », ou la session a disparu.
    EpisodeClosed { session: String, kind: EpisodeKind },
    /// La section `[companion]` a changé sans changer de port : le service suit sans redémarrer (les flux restent
    /// ouverts). Un changement de port, lui, redémarre le service.
    ConfigChanged(CompanionConfig),
    /// Relancer la détection de l'adresse tailnet (le bouton « Vérifier » de Personnaliser). Plusieurs demandes
    /// rapprochées ne font qu'une détection de plus.
    RecheckTailnet,
}

/// Ce que le service demande au fil d'interface : une commande sur un pane, avec sa réponse `oneshot`.
#[derive(Debug)]
pub enum PaneCommand {
    /// Taper une touche ou un texte. `received_at` est l'instant de réception HTTP : une commande traitée plus de
    /// 1,5 s après est jetée sans rien taper (`input::is_expired`).
    Input {
        session: String,
        pane: u32,
        input: RemoteInput,
        command_id: String,
        received_at: Instant,
        reply: oneshot::Sender<InputOutcome>,
    },
    /// Acquitter ce pane seul (`mark_seen`). Réponse : le pane existe.
    Ack {
        session: String,
        pane: u32,
        origin: AckOrigin,
        reply: oneshot::Sender<bool>,
    },
    /// Lire ce que le pane accepte, pour l'écran. `None` : session ou pane inconnus.
    Probe {
        session: String,
        pane: u32,
        reply: oneshot::Sender<Option<PaneProbe>>,
    },
}

/// Qui acquitte : le téléphone (geste, ouverture de l'écran) ou la présence de Romain devant l'écran du téléphone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOrigin {
    Phone,
    Watched,
}

/// Ce que le fil d'interface a fait d'une saisie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    /// Tapée.
    Sent,
    /// La première écriture est partie mais l'Entrée demandée ne l'a pas été : trop tard pour répondre juste, pane
    /// disparu, client de contrôle sorti ou programme fini. La page dit « Envoyé sans Entrée » et relit l'écran.
    SentWithoutEnter,
    /// Ce `command_id` a déjà été tapé dans la minute : rien n'est retapé, la page le compte pour un succès.
    Duplicate,
    /// Rien ne tourne dans le pane (prompt du shell, ou programme encore inconnu) : fermé par défaut.
    NoProgram,
    ReadOnly,
    /// Le client de contrôle de la session est sorti : rien de ce qu'on lui écrirait n'arriverait.
    Disconnected,
    NoPane,
    UnknownSession,
    /// Arrivée sur le fil d'interface plus de 1,5 s après sa réception HTTP.
    Expired,
}

impl InputOutcome {
    /// Le mot du journal (`outcome=`) et de la réponse JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::SentWithoutEnter => "sent_without_enter",
            Self::Duplicate => "duplicate",
            Self::NoProgram => "no_program",
            Self::ReadOnly => "read_only",
            Self::Disconnected => "disconnected",
            Self::NoPane => "no_pane",
            Self::UnknownSession => "unknown_session",
            Self::Expired => "expired",
        }
    }
}

/// Une donnée venue d'une requête, sûre pour une ligne de journal : les caractères de contrôle (dont `\n`, qui fausserait
/// la ligne) sont remplacés par `?`, la longueur est bornée. Toute ligne `companion` qui porte une donnée de requête
/// passe par ici.
pub fn log_safe(text: &str) -> String {
    text.chars()
        .take(80)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Une clé de session telle qu'elle figure au journal : le nom tmux en local, `<connexion>:<nom tmux>` sur une machine
/// (jamais la clé qualifiée, dont le séparateur est un caractère de contrôle). Le nom passe par `log_safe`.
pub fn key_label(key: &str) -> String {
    match split_session_key(key) {
        SessionTarget::Local(name) => log_safe(name),
        SessionTarget::Remote { connection, tmux } => format!("{connection}:{}", log_safe(tmux)),
    }
}

/// Ce que le pane accepte, tel que l'écran le dit à la page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneProbe {
    /// Le pane prend-il une saisie (`input::accepts_input`) ?
    pub accepts_input: bool,
    /// Son état (`idle`, `running`, `waiting`, `error`).
    pub state: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_value_cannot_forge_a_journal_line() {
        assert_eq!(log_safe("idema-ab12cd34"), "idema-ab12cd34");
        let forged = log_safe("x\ncompanion pair ok\r\u{85}\u{1b}[31m\u{7f}");
        assert!(!forged.chars().any(char::is_control), "{forged:?}");
        assert!(forged.starts_with("x?companion pair ok"));
        assert_eq!(log_safe(&"a".repeat(500)).len(), 80);
    }

    #[test]
    fn a_session_key_is_journaled_by_its_machine_and_name() {
        use crate::machines::key::remote_session_key;
        assert_eq!(key_label("idema-ab12cd34"), "idema-ab12cd34");
        let label = key_label(&remote_session_key("caserne-claude", "test\nvpc"));
        assert_eq!(label, "caserne-claude:test?vpc");
        assert!(!label.chars().any(char::is_control), "{label:?}");
    }
}
