//! Le contrôleur côté interface : il suit la section `[companion]` de la config et démarre, arrête ou redémarre le
//! service en conséquence. Rien ici ne bloque le fil d'interface.

use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use futures::channel::mpsc;
use idema_tmux::server::TmuxServer;
use tokio::sync::watch;

use super::auth::test_routes_enabled;
use super::service::{self, ServiceDeps, ServiceHandle};
use super::{PaneCommand, ServiceEvent, ServiceStatus};
use crate::config::CompanionConfig;

/// Un service qui tourne (ou démarre), avec la config qui l'a lancé.
struct Running {
    handle: ServiceHandle,
    config: CompanionConfig,
    events: tokio::sync::mpsc::UnboundedSender<ServiceEvent>,
    status: watch::Receiver<ServiceStatus>,
}

#[derive(Default)]
pub struct CompanionController {
    running: Option<Running>,
    /// Le fil d'un service arrêté que personne n'a encore joint. Il n'existe que tant que rien ne tourne : un
    /// service qui démarre le reçoit comme `previous` et le joint avant de lier sa prise, donc une réactivation
    /// rapide ne dispute jamais le port à l'ancien fil.
    stopping: Option<JoinHandle<()>>,
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
        let previous = self.stop().or_else(|| self.stopping.take());
        let Some((config, tmux)) = wanted else {
            self.stopping = previous;
            return None;
        };
        let (events, events_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (commands, commands_receiver) = mpsc::unbounded();
        let (status, status_receiver) = watch::channel(ServiceStatus::default());
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
            test_routes: test_routes_enabled(
                std::env::var_os("IDEMA_HOME").as_deref(),
                std::env::var_os("IDEMA_COMPANION_TEST_ROUTES").as_deref(),
            ),
        })?;
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
    #[allow(
        dead_code,
        reason = "premier émetteur à la tâche 3 (transitions de la flotte)"
    )]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(port: u16) -> CompanionConfig {
        toml::from_str(&format!("port = {port}")).unwrap()
    }

    fn tmux() -> Arc<TmuxServer> {
        Arc::new(TmuxServer::new(
            "/nonexistent/tmux".into(),
            "idema-test-controller",
            "/nonexistent/tmux.conf".into(),
            Vec::new(),
        ))
    }

    /// Retirer la section garde le fil de l'ancien service ; la réactiver le lui fait joindre avant de lier le port.
    #[test]
    fn a_stopped_service_is_joined_by_the_next_one() {
        let port = super::super::test_support::free_port();
        let home = super::super::test_support::TempDir::new("controller");
        let scratchpad = home.0.join("scratchpad.md");
        let mut controller = CompanionController::new();
        let section = config(port);

        assert!(
            controller
                .sync(Some(&section), Some(tmux()), &home.0, &scratchpad)
                .is_some()
        );
        assert!(controller.is_on());
        assert!(controller.stopping.is_none());

        assert!(
            controller
                .sync(None, Some(tmux()), &home.0, &scratchpad)
                .is_none()
        );
        assert!(!controller.is_on());
        assert!(
            controller.stopping.is_some(),
            "the stopped thread is kept, not dropped"
        );

        // Réactivée aussitôt : le nouveau service reçoit l'ancien fil.
        assert!(
            controller
                .sync(Some(&section), Some(tmux()), &home.0, &scratchpad)
                .is_some()
        );
        assert!(controller.stopping.is_none(), "handed to the new service");

        // Retirée de nouveau, on joint pour rendre le port et le dossier avant la fin du test.
        controller.sync(None, Some(tmux()), &home.0, &scratchpad);
        controller.stopping.take().unwrap().join().unwrap();
    }
}
