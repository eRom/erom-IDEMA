//! Le contrôleur côté interface : il suit la section `[companion]` de la config et démarre, arrête ou redémarre le
//! service en conséquence. Rien ici ne bloque le fil d'interface.

use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use futures::channel::mpsc;
use idema_tmux::server::TmuxServer;
use tokio::sync::watch;

use super::service::{self, ServiceDeps, ServiceHandle};
use super::{ListenState, PaneCommand, ServiceEvent, ServiceStatus};
use crate::config::CompanionConfig;

/// Un service qui tourne (ou démarre), avec la config qui l'a lancé.
struct Running {
    handle: ServiceHandle,
    config: CompanionConfig,
    // Lus par les tâches suivantes (événements vers le service, état pour Personnaliser).
    #[allow(dead_code, reason = "lu par les tâches 2 à 11 du compagnon")]
    events: tokio::sync::mpsc::UnboundedSender<ServiceEvent>,
    #[allow(dead_code, reason = "lu par les tâches 2 à 11 du compagnon")]
    status: watch::Receiver<ServiceStatus>,
}

#[derive(Default)]
pub struct CompanionController {
    running: Option<Running>,
}

impl CompanionController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Démarre, arrête ou redémarre selon `config` ; ne démarre qu'avec `tmux` présent. Rend la file de commandes du
    /// nouveau service quand il en démarre un, pour que l'espace de travail lance sa tâche de traitement.
    ///
    /// Un redémarrage (port changé, tmux changé) signale l'arrêt de l'ancien service et lance le nouveau tout de suite :
    /// c'est le nouveau fil qui joint l'ancien avant de lier sa prise, donc le fil d'interface n'attend jamais.
    pub fn sync(
        &mut self,
        config: Option<&CompanionConfig>,
        tmux: Option<Arc<TmuxServer>>,
        dir: &Path,
        scratchpad: &Path,
    ) -> Option<mpsc::UnboundedReceiver<PaneCommand>> {
        let wanted = config.zip(tmux);
        if let (Some(running), Some((config, _))) = (&self.running, &wanted)
            && running.config == **config
        {
            return None;
        }
        let previous = self.stop();
        let (config, tmux) = wanted?;
        let (events, events_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (commands, commands_receiver) = mpsc::unbounded();
        let (status, status_receiver) = watch::channel(ServiceStatus {
            listen: ListenState::Starting,
        });
        let handle = service::start(ServiceDeps {
            dir: dir.to_path_buf(),
            version: env!("CARGO_PKG_VERSION"),
            config: config.clone(),
            tmux,
            scratchpad: scratchpad.to_path_buf(),
            events: events_receiver,
            commands,
            status,
            previous,
        });
        self.running = Some(Running {
            handle,
            config: config.clone(),
            events,
            status: status_receiver,
        });
        Some(commands_receiver)
    }

    /// Signale l'arrêt du service en cours et rend son fil, que le suivant joindra.
    fn stop(&mut self) -> Option<JoinHandle<()>> {
        self.running.take().map(|running| running.handle.stop())
    }

    pub fn is_on(&self) -> bool {
        self.running.is_some()
    }

    /// Envoie un événement au service ; sans effet éteint.
    #[allow(dead_code, reason = "premier émetteur à la tâche 2")]
    pub fn send(&self, event: ServiceEvent) {
        if let Some(running) = &self.running {
            let _ = running.events.send(event);
        }
    }

    /// L'état du service pour Personnaliser ; `None` éteint.
    #[allow(dead_code, reason = "lu par Personnaliser (tâche 11)")]
    pub fn status(&self) -> Option<watch::Receiver<ServiceStatus>> {
        self.running.as_ref().map(|running| running.status.clone())
    }
}
