//! Le contrôleur côté interface : il suit la section `[companion]` de la config et démarre, arrête ou redémarre le
//! service en conséquence. Rien ici ne bloque le fil d'interface.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime};

use futures::channel::mpsc;
use idema_term::activity::ActivityState;
use idema_tmux::server::TmuxServer;
use tokio::sync::watch;

use super::auth::test_routes_enabled;
use super::fleet::Fleet;
use super::input::RecentCommands;
use super::push_send::{OVERRIDE_VAR, override_endpoint};
use super::service::{self, ServiceDeps, ServiceHandle};
use super::timeline::Timeline;
use super::{PaneCommand, ServiceEvent, ServiceStatus};
use crate::config::CompanionConfig;
use crate::hud;

/// Un service qui tourne (ou démarre), avec la config qui l'a lancé.
struct Running {
    handle: ServiceHandle,
    config: CompanionConfig,
    events: tokio::sync::mpsc::UnboundedSender<ServiceEvent>,
    status: watch::Receiver<ServiceStatus>,
    /// Le dernier instantané de la flotte : la route `fleet` le lit, chaque flux `events` s'y abonne.
    fleet: watch::Sender<Arc<Fleet>>,
}

pub struct CompanionController {
    running: Option<Running>,
    /// La chronologie des transitions qui comptent. Elle appartient au contrôleur, donc à IdemA : un service qui
    /// redémarre (port changé) la retrouve, et son instant de démarrage est celui du process. Perdue avec lui.
    timeline: Arc<Mutex<Timeline>>,
    /// Les `command_id` tapés dans la dernière minute : un service qui redémarre ne fait pas oublier une saisie, et
    /// l'espace de travail, seul à taper, est seul à y toucher.
    recent: RecentCommands,
    /// Romain est absent (inactivité ou écran verrouillé). Faux tant que rien ne tourne : le compagnon éteint, rien ne
    /// change au « vu » d'IdemA. Gardé d'un redémarrage du service à l'autre (port changé) : Romain n'a pas bougé.
    away: bool,
    /// Le fil d'un service arrêté que personne n'a encore joint. Il n'existe que tant que rien ne tourne : un
    /// service qui démarre le reçoit comme `previous` et le joint avant de lier sa prise, donc une réactivation
    /// rapide ne dispute jamais le port à l'ancien fil.
    stopping: Option<JoinHandle<()>>,
}

impl Default for CompanionController {
    fn default() -> Self {
        Self::new()
    }
}

impl CompanionController {
    pub fn new() -> Self {
        // Le démarrage d'IdemA : maintenant, moins le temps écoulé depuis le début de `main`.
        let started_at = SystemTime::now()
            .checked_sub(hud::since_process_start())
            .unwrap_or_else(SystemTime::now);
        Self::with_start(started_at)
    }

    fn with_start(started_at: SystemTime) -> Self {
        Self {
            running: None,
            timeline: Arc::new(Mutex::new(Timeline::new(started_at))),
            recent: RecentCommands::default(),
            away: false,
            stopping: None,
        }
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
            self.away = false;
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
            timeline: Arc::clone(&self.timeline),
            previous,
            test_routes: test_routes_enabled(
                std::env::var_os("IDEMA_HOME").as_deref(),
                std::env::var_os("IDEMA_COMPANION_TEST_ROUTES").as_deref(),
            ),
            push_override: override_endpoint(
                std::env::var(OVERRIDE_VAR).ok().as_deref(),
                std::env::var("IDEMA_HOME").ok().as_deref(),
                std::env::var("IDEMA_COMPANION_TEST_ROUTES").ok().as_deref(),
            ),
        })?;
        self.running = Some(Running {
            handle,
            config: config.clone(),
            events,
            status: status_receiver,
            fleet,
        });
        // Un service redémarré (port changé) ne sait pas que Romain était déjà absent.
        if self.away {
            self.send(ServiceEvent::Presence { away: true });
        }
        Some(commands_receiver)
    }

    /// Signale l'arrêt du service en cours et rend son fil, que le suivant joindra.
    fn stop(&mut self) -> Option<JoinHandle<()>> {
        self.running.take().map(|running| running.handle.stop())
    }

    pub fn is_on(&self) -> bool {
        self.running.is_some()
    }

    /// Romain est-il présent ? Vrai compagnon éteint : le « vu » d'IdemA ne dépend alors de rien d'autre que de la
    /// fenêtre.
    pub fn present(&self) -> bool {
        // `away` peut survivre à un démarrage raté (port pris) : sans service, l'absence ne veut plus rien dire.
        self.running.is_none() || !self.away
    }

    /// Enregistre l'absence mesurée ; vrai quand l'état a changé. Sans effet éteint (aucune minuterie n'existe alors,
    /// et `present()` reste vrai).
    pub fn set_away(&mut self, away: bool) -> bool {
        if self.running.is_none() || self.away == away {
            return false;
        }
        self.away = away;
        true
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

    /// Une session a changé d'état. Rangée dans la chronologie tant que le compagnon est allumé (même si son service
    /// redémarre ou n'a pas pu lier sa prise) ; éteint, rien n'est gardé.
    pub fn record_transition(
        &self,
        session: &str,
        from: ActivityState,
        to: ActivityState,
        at: SystemTime,
    ) {
        if self.running.is_some() {
            self.lock_timeline().record(session, from, to, at);
        }
    }

    /// Une session perd son état suivi (vue remplacée ou perdue, session fermée) : voir `Timeline::reset`.
    pub fn record_reset(&self, session: &str, from: ActivityState, at: SystemTime) {
        if self.running.is_some() {
            self.lock_timeline().reset(session, from, at);
        }
    }

    fn lock_timeline(&self) -> std::sync::MutexGuard<'_, Timeline> {
        // Un panique pendant qu'on tenait le verrou ne rend pas la chronologie incohérente (chaque ajout est atomique).
        self.timeline.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ce `command_id` a-t-il été tapé dans la dernière minute ? Ne retient rien.
    pub fn command_typed_recently(&self, id: &str, now: Instant) -> bool {
        self.recent.seen(id, now)
    }

    /// Retient un `command_id` qui vient d'être tapé.
    pub fn remember_command(&mut self, id: &str, now: Instant) {
        self.recent.first_seen(id, now);
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

    /// La chronologie est celle d'IdemA : un service qui redémarre (port changé) la garde, avec le même instant de
    /// démarrage ; éteint, le contrôleur ne garde rien.
    #[test]
    fn the_timeline_lives_as_long_as_idema_not_as_long_as_a_service() {
        use idema_term::activity::ActivityState::{Idle, Running, Waiting};
        use std::time::{Duration, UNIX_EPOCH};

        let home = super::super::test_support::TempDir::new("controller-timeline");
        let scratchpad = home.0.join("scratchpad.md");
        let start = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let at = |seconds| start + Duration::from_secs(seconds);
        let mut controller = CompanionController::with_start(start);
        let events = |controller: &CompanionController| controller.lock_timeline().since(0);

        controller.record_transition("a", Running, Waiting, at(1));
        assert!(events(&controller).is_empty(), "off: nothing is kept");

        let first = super::super::test_support::free_port();
        controller.sync(Some(&config(first)), Some(tmux()), &home.0, &scratchpad);
        controller.record_transition("a", Running, Waiting, at(10));

        // Le port change : l'ancien service s'arrête, un nouveau démarre, la chronologie reste.
        let second = super::super::test_support::free_port();
        assert_ne!(first, second);
        controller.sync(Some(&config(second)), Some(tmux()), &home.0, &scratchpad);
        controller.record_transition("a", Waiting, Idle, at(20));
        controller.record_reset("b", Waiting, at(30));

        let kept: Vec<_> = events(&controller)
            .into_iter()
            .map(|event| (event.session, event.from, event.to))
            .collect();
        assert_eq!(
            kept,
            [
                ("a".into(), "running", "waiting"),
                ("a".into(), "waiting", "idle"),
                ("b".into(), "waiting", "idle"),
            ]
        );
        assert_eq!(controller.lock_timeline().started_at(), 1_800_000_000_000);

        controller.sync(None, Some(tmux()), &home.0, &scratchpad);
        controller.stopping.take().unwrap().join().unwrap();
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

    /// Éteint, Romain est toujours présent et rien ne s'enregistre ; allumé, `set_away` rend vrai au seul changement ;
    /// éteindre le compagnon rend la présence.
    #[test]
    fn presence_follows_the_service_and_is_always_true_when_off() {
        let port = super::super::test_support::free_port();
        let home = super::super::test_support::TempDir::new("controller-presence");
        let scratchpad = home.0.join("scratchpad.md");
        let mut controller = CompanionController::new();

        assert!(controller.present());
        assert!(!controller.set_away(true), "off: nothing is recorded");
        assert!(controller.present());

        controller.sync(Some(&config(port)), Some(tmux()), &home.0, &scratchpad);
        assert!(controller.present(), "on: present until measured away");
        assert!(controller.set_away(true));
        assert!(!controller.present());
        assert!(!controller.set_away(true), "no change, no event");
        assert!(controller.set_away(false));
        assert!(controller.present());

        assert!(controller.set_away(true));
        controller.sync(None, Some(tmux()), &home.0, &scratchpad);
        assert!(
            controller.present(),
            "turning the companion off ends the absence"
        );
        controller.stopping.take().unwrap().join().unwrap();
    }

    /// Un redémarrage raté (le fil n'a pas pu naître) laisse `away` vrai sans service : compagnon éteint, Romain est
    /// présent, quoi que `away` ait gardé.
    #[test]
    fn present_is_true_when_off_even_if_a_failed_restart_kept_away() {
        let port = super::super::test_support::free_port();
        let home = super::super::test_support::TempDir::new("controller-failed-restart");
        let scratchpad = home.0.join("scratchpad.md");
        let mut controller = CompanionController::new();
        controller.sync(Some(&config(port)), Some(tmux()), &home.0, &scratchpad);
        assert!(controller.set_away(true));
        assert!(!controller.present());

        // L'état laissé par `sync` quand `service::start` rend `None` : l'ancien service est arrêté, `away` reste.
        let stopped = controller.stop().expect("a service was running");
        stopped.join().unwrap();

        assert!(!controller.is_on());
        assert!(controller.away, "the failed start did not reset it");
        assert!(controller.present(), "no service, so Romain is present");
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
