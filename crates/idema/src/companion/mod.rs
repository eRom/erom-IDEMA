//! Le compagnon iPhone : un petit service HTTP dans le process d'IdemA, lié à `127.0.0.1`, que la page installée sur
//! le téléphone interroge (par `tailscale serve`). Un fil dédié porte son propre runtime tokio ; le contrôleur
//! (`controller`) le démarre, l'arrête et le redémarre selon la section `[companion]` de la config. Section absente :
//! aucune prise, aucun fil, aucune ligne de journal.

pub mod controller;
mod server;
pub mod service;

pub use controller::CompanionController;

/// Ce que le service dit de lui à l'espace de travail (et, plus tard, à Personnaliser).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub listen: ListenState,
}

/// Où en est la prise du service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenState {
    /// Le service démarre : la prise n'est pas encore liée.
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

/// Ce que l'espace de travail envoie au service. Les tâches suivantes ajoutent les variantes.
#[derive(Debug)]
pub enum ServiceEvent {}

/// Ce que le service demande au fil d'interface. Les tâches suivantes ajoutent les variantes.
#[derive(Debug)]
pub enum PaneCommand {}
