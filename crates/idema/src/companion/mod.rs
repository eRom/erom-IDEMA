//! Le compagnon iPhone : un petit service HTTP dans le process d'IdemA, lié à `127.0.0.1`, que la page installée sur
//! le téléphone interroge (par `tailscale serve`). Un fil dédié porte son propre runtime tokio ; le contrôleur
//! (`controller`) le démarre, l'arrête et le redémarre selon la section `[companion]` de la config. Section absente :
//! aucune prise, aucun fil, aucune ligne de journal.

mod access;
mod auth;
pub mod controller;
mod devices;
pub mod fleet;
mod server;
pub mod service;
mod sse;
#[cfg(test)]
mod test_support;
mod timeline;
mod web_assets;

use std::time::SystemTime;

pub use controller::CompanionController;

/// Ce que le service dit de lui à l'espace de travail (et, plus tard, à Personnaliser).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceStatus {
    pub listen: ListenState,
    /// Le code d'appairage en cours, s'il y en a un de valide.
    pub pairing: Option<PairingDisplay>,
    /// Les appareils appairés, du plus ancien au plus récent.
    pub devices: Vec<DeviceSummary>,
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
    #[allow(dead_code, reason = "émis par Personnaliser (tâche 11)")]
    StartPairing,
    /// Annuler le code en cours.
    #[allow(dead_code, reason = "émis par Personnaliser (tâche 11)")]
    CancelPairing,
    /// Oublier un appareil et son abonnement : sa requête suivante prend 401.
    ForgetDevice { id: String },
}

/// Ce que le service demande au fil d'interface. Les tâches suivantes ajoutent les variantes.
#[derive(Debug)]
pub enum PaneCommand {}
