//! Le contrôleur côté interface : il suit la section `[companion]` de la config et démarre, arrête ou redémarre le
//! service en conséquence. Rien ici ne bloque le fil d'interface.

use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use futures::channel::mpsc;
use idema_tmux::server::TmuxServer;
use tokio::sync::watch;

use super::auth::test_routes_enabled;
use super::fleet::Fleet;
use super::service::{self, ServiceDeps, ServiceHandle};
use super::{PaneCommand, ServiceEvent, ServiceStatus};
use crate::config::CompanionConfig;

/// Un service qui tourne (ou démarre), avec la config qui l'a lancé.
struct Running {
    handle: ServiceHandle,
    config: CompanionConfig,
    events: tokio::sync::mpsc::UnboundedSender<ServiceEvent>,
    status: watch::Receiver<ServiceStatus>,
    /// Le dernier instantané de la flotte : la route `fleet` le lit, chaque flux `events` s'y abonne.
    fleet: watch::Sender<Arc<Fleet>>,
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
        let (fleet, fleet_receiver) = watch::channel(Arc::new(Fleet::default()));
        let handle = service::start(ServiceDeps {
            dir: dir.to_path_buf(),
            version: env!("CARGO_PKG_VERSION"),
            config: config.clone(),
            tmux,
            scratchpad: scratchpad.to_path_buf(),
            events: events_receiver,
            commands,
            status,
            fleet: fleet_receiver,
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
            fleet,
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

    /// Publie l'instantané de la flotte ; sans effet éteint, et sans effet quand il ne dit rien de plus que le
    /// précédent (l'heure de construction ne compte pas) : aucun flux n'est réveillé pour rien.
    pub fn publish(&self, fleet: Fleet) {
        let Some(running) = &self.running else {
            return;
        };
        running.fleet.send_if_modified(|current| {
            if !current.differs_from(&fleet) {
                return false;
            }
            *current = Arc::new(fleet);
            true
        });
    }

    /// Envoie un événement au service ; sans effet éteint.
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

    /// Éteint, publier ne fait rien ; allumé, seul un instantané qui change quelque chose réveille les abonnés.
    #[test]
    fn publishing_wakes_subscribers_only_for_a_real_change() {
        let port = super::super::test_support::free_port();
        let home = super::super::test_support::TempDir::new("controller-publish");
        let scratchpad = home.0.join("scratchpad.md");
        let mut controller = CompanionController::new();
        let fleet = |generated_at, badge| Fleet {
            mac: "Mac".into(),
            generated_at,
            badge,
            ..Fleet::default()
        };
        controller.publish(fleet(1, 0));
        assert!(!controller.is_on(), "off: nothing to publish to");

        controller.sync(Some(&config(port)), Some(tmux()), &home.0, &scratchpad);
        let mut subscriber = controller.running.as_ref().unwrap().fleet.subscribe();
        controller.publish(fleet(1, 0));
        assert!(subscriber.has_changed().unwrap(), "the first snapshot");
        assert_eq!(subscriber.borrow_and_update().badge, 0);

        controller.publish(fleet(2, 0));
        assert!(
            !subscriber.has_changed().unwrap(),
            "only the build time moved"
        );
        controller.publish(fleet(3, 4));
        assert!(subscriber.has_changed().unwrap());
        assert_eq!(subscriber.borrow_and_update().badge, 4);

        controller.sync(None, Some(tmux()), &home.0, &scratchpad);
        controller.stopping.take().unwrap().join().unwrap();
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
