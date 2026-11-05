//! L'état d'accès du service : les appareils appairés et le code d'appairage en cours. Il vit sur le fil du service
//! (seul écrivain de `devices.json`) et publie ce que Personnaliser doit voir dans `ServiceStatus`. Les méthodes qui
//! écrivent sur disque sont appelées depuis `spawn_blocking`, jamais depuis une tâche du runtime.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::rngs::OsRng;
use tokio::sync::watch;

use super::auth::{PairOutcome, PairingCodes, new_token, token_hash};
use super::devices::{Device, DeviceStore, Subscription, device_name};
use super::{DeviceSummary, PairingDisplay, ServiceEvent, ServiceStatus};
use crate::hud;

/// L'appareil qui a présenté un cookie valide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub id: String,
    pub name: String,
    /// L'appareil a un abonnement aux notifications.
    pub subscribed: bool,
}

impl From<&Device> for Caller {
    fn from(device: &Device) -> Self {
        Self {
            id: device.id.clone(),
            name: device.name.clone(),
            subscribed: device.subscription.is_some(),
        }
    }
}

/// Un appairage réussi : le jeton à poser en cookie (base64url, jamais gardé) et l'appareil créé.
pub struct Paired {
    pub token: String,
    pub caller: Caller,
}

/// Le jeton ne s'écrit jamais, ni au journal ni dans une assertion qui échoue.
impl std::fmt::Debug for Paired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Paired")
            .field("token", &"<masqué>")
            .field("caller", &self.caller)
            .finish()
    }
}

/// Pourquoi un appairage est refusé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairRefusal {
    WrongCode,
    Expired,
    Locked,
    NoCode,
    /// Le code était bon, mais `devices.json` n'a pas pu être écrit : personne n'est appairé.
    Store,
}

impl PairRefusal {
    /// Le code d'erreur que la page lit (`error` dans la réponse).
    pub fn error_code(self) -> &'static str {
        match self {
            Self::WrongCode => "wrong_code",
            Self::Expired => "expired",
            Self::Locked => "locked",
            Self::NoCode => "no_code",
            Self::Store => "store_failed",
        }
    }
}

struct State {
    devices: DeviceStore,
    codes: PairingCodes,
}

pub struct Access {
    /// Les appareils (lus par la garde, sur le runtime) et les codes. Jamais tenu pendant une écriture sur disque :
    /// une écriture lente ne doit pas figer la reconnaissance d'un cookie.
    state: Mutex<State>,
    /// Une écriture de `devices.json` à la fois (le fil du service reste l'unique écrivain). Les méthodes qui écrivent
    /// le prennent d'abord, puis copient la liste, l'écrivent hors de `state`, et ne la mettent en place qu'ensuite :
    /// la mémoire ne devance jamais le disque, et personne d'autre que ces méthodes ne change la liste entre-temps.
    /// Ordre des verrous : `writer` puis `state`, jamais l'inverse.
    writer: Mutex<()>,
    status: Arc<watch::Sender<ServiceStatus>>,
}

impl Access {
    /// Prend le magasin ouvert et publie tout de suite la liste des appareils.
    pub fn new(devices: DeviceStore, status: Arc<watch::Sender<ServiceStatus>>) -> Self {
        let access = Self {
            state: Mutex::new(State {
                devices,
                codes: PairingCodes::default(),
            }),
            writer: Mutex::new(()),
            status,
        };
        access.publish(&access.lock());
        access
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Un panique pendant qu'on tenait le verrou ne rend pas l'état incohérent (chaque changement est atomique).
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Met `ServiceStatus` à jour : le code encore bon et la liste des appareils.
    fn publish(&self, state: &State) {
        let now = Instant::now();
        let pairing = state
            .codes
            .pending(now)
            .map(|(code, deadline)| PairingDisplay {
                code: code.to_owned(),
                expires_at: SystemTime::now() + deadline.saturating_duration_since(now),
            });
        let devices = state
            .devices
            .devices()
            .iter()
            .map(|device| DeviceSummary {
                id: device.id.clone(),
                name: device.name.clone(),
                paired_at: device.paired_at,
                last_seen_at: device.last_seen_at,
                subscribed: device.subscription.is_some(),
            })
            .collect();
        self.status.send_modify(|status| {
            status.pairing = pairing;
            status.devices = devices;
        });
    }

    /// Un geste venu de l'espace de travail.
    pub fn handle(&self, event: ServiceEvent) {
        match event {
            ServiceEvent::StartPairing => {
                self.start_pairing();
            }
            ServiceEvent::CancelPairing => {
                let mut state = self.lock();
                state.codes.cancel();
                self.publish(&state);
            }
            ServiceEvent::ForgetDevice { id } => {
                self.forget(&id);
            }
            // Les épisodes, la présence et la config ne regardent pas l'accès : la boucle des notifications
            // (`notify::run`) les reçoit avant lui (`service::control_loop`).
            ServiceEvent::PhoneInteraction { .. }
            | ServiceEvent::Presence { .. }
            | ServiceEvent::EpisodeOpened { .. }
            | ServiceEvent::EpisodeClosed { .. }
            | ServiceEvent::ConfigChanged(_)
            | ServiceEvent::RecheckTailnet => {}
        }
    }

    /// Émet un code d'appairage (le précédent cesse de valoir) et le rend avec son échéance.
    pub fn start_pairing(&self) -> PairingDisplay {
        let mut state = self.lock();
        let now = Instant::now();
        let (code, deadline) = state.codes.issue(now, &mut OsRng);
        self.publish(&state);
        PairingDisplay {
            code,
            expires_at: SystemTime::now() + deadline.saturating_duration_since(now),
        }
    }

    /// L'échéance du code en cours : à cet instant `expire_pairing` fait disparaître l'affichage.
    pub fn pairing_deadline(&self) -> Option<Instant> {
        self.lock().codes.deadline()
    }

    pub fn expire_pairing(&self) {
        let mut state = self.lock();
        state.codes.expire(Instant::now());
        self.publish(&state);
    }

    /// Essaie un code d'appairage. Bon : crée l'appareil et rend le jeton, que seul ce retour contient.
    pub fn pair(&self, code: &str, user_agent: Option<&str>) -> Result<Paired, PairRefusal> {
        let outcome = {
            let mut state = self.lock();
            let outcome = state.codes.verify(code, Instant::now());
            self.publish(&state);
            outcome
        };
        let result = match outcome {
            PairOutcome::Accepted => {
                let token = new_token(&mut OsRng);
                let name = device_name(user_agent);
                match self
                    .write_devices(|store| store.add(name, &token_hash(&token), SystemTime::now()))
                {
                    Ok(id) => Ok(Paired {
                        token: URL_SAFE_NO_PAD.encode(token),
                        caller: Caller {
                            id,
                            name: name.to_owned(),
                            subscribed: false,
                        },
                    }),
                    Err(error) => {
                        log_write_failure("add", &error);
                        Err(PairRefusal::Store)
                    }
                }
            }
            PairOutcome::Wrong => Err(PairRefusal::WrongCode),
            PairOutcome::Expired => Err(PairRefusal::Expired),
            PairOutcome::Locked => Err(PairRefusal::Locked),
            PairOutcome::NoCode => Err(PairRefusal::NoCode),
        };
        hud::log_event(if result.is_ok() {
            "companion pair ok"
        } else {
            "companion pair refused"
        });
        result
    }

    /// L'appareil dont le cookie porte ce jeton (base64url), et si sa dernière activité est à écrire.
    pub fn authenticate(&self, token_b64url: &str) -> Option<(Caller, bool)> {
        let token = URL_SAFE_NO_PAD.decode(token_b64url).ok()?;
        if token.len() != 32 {
            return None;
        }
        let state = self.lock();
        let device = state.devices.by_token_hash(&token_hash(&token))?;
        let due = state.devices.touch_due(&device.id, SystemTime::now());
        Some((Caller::from(device), due))
    }

    /// Note l'activité de l'appareil (une écriture au plus par minute).
    pub fn touch(&self, id: &str) {
        let _writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if !self.lock().devices.touch_due(id, SystemTime::now()) {
            return;
        }
        if let Err(error) = self.write_devices_held(|store| store.touch(id, SystemTime::now())) {
            log_write_failure("touch", &error);
        }
    }

    /// Oublie l'appareil et son abonnement. Rend vrai s'il existait.
    pub fn forget(&self, id: &str) -> bool {
        match self.write_devices(|store| store.forget(id)) {
            Ok(true) => {
                hud::log_event("companion device forgotten");
                true
            }
            Ok(false) => false,
            Err(error) => {
                log_write_failure("forget", &error);
                false
            }
        }
    }

    /// L'abonnement push de l'appareil, s'il en a un.
    pub fn subscription_of(&self, id: &str) -> Option<Subscription> {
        self.lock()
            .devices
            .devices()
            .iter()
            .find(|device| device.id == id)
            .and_then(|device| device.subscription.clone())
    }

    /// Les abonnements push de tous les appareils, du plus ancien appairage au plus récent : à chacun sa notification.
    pub fn subscriptions(&self) -> Vec<Subscription> {
        self.lock()
            .devices
            .devices()
            .iter()
            .filter_map(|device| device.subscription.clone())
            .collect()
    }

    /// Pose l'abonnement de l'appareil (un `endpoint` n'est gardé qu'une fois, chez le dernier qui l'a donné). Écrit
    /// hors du verrou d'état. `NotFound` si l'appareil a été oublié entre-temps.
    pub fn set_subscription(&self, id: &str, subscription: Subscription) -> io::Result<()> {
        self.write_devices(|store| store.set_subscription(id, subscription))
            .inspect_err(|error| log_write_failure("subscribe", error))
    }

    /// Retire l'abonnement de cet appareil, s'il en avait un. Rend vrai s'il y en avait un.
    pub fn clear_subscription(&self, id: &str) -> io::Result<bool> {
        self.write_devices(|store| {
            let held = store
                .devices()
                .iter()
                .find(|device| device.id == id)
                .and_then(|device| device.subscription.as_ref())
                .map(|subscription| subscription.endpoint.clone());
            match held {
                Some(endpoint) => store.remove_endpoint(&endpoint),
                None => Ok(false),
            }
        })
        .inspect_err(|error| log_write_failure("unsubscribe", error))
    }

    /// Retire l'abonnement qui porte cet `endpoint`, chez qui qu'il soit (le service push l'a déclaré mort). Rend
    /// vrai s'il y en avait un.
    pub fn remove_endpoint(&self, endpoint: &str) -> io::Result<bool> {
        self.write_devices(|store| store.remove_endpoint(endpoint))
            .inspect_err(|error| log_write_failure("remove_endpoint", error))
    }

    /// Change la liste des appareils et l'écrit, hors du verrou d'état (voir `writer`).
    fn write_devices<R>(
        &self,
        change: impl FnOnce(&mut DeviceStore) -> io::Result<R>,
    ) -> io::Result<R> {
        let _writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        self.write_devices_held(change)
    }

    /// Comme `write_devices`, `writer` déjà tenu par l'appelant. `change` s'applique à une copie du magasin, qui écrit
    /// le fichier lui-même et revient en arrière s'il échoue ; la copie ne remplace le magasin qu'une fois écrite.
    fn write_devices_held<R>(
        &self,
        change: impl FnOnce(&mut DeviceStore) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut working = self.lock().devices.clone();
        let result = change(&mut working)?;
        let mut state = self.lock();
        state.devices = working;
        self.publish(&state);
        Ok(result)
    }
}

/// Une écriture de `devices.json` a échoué : la liste n'a pas bougé. Seul le genre d'erreur est écrit au journal.
fn log_write_failure(operation: &str, error: &io::Error) {
    hud::log_event(&format!(
        "companion devices write failed op={operation} kind={:?}",
        error.kind()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::companion::test_support::TempDir;

    fn access(home: &TempDir) -> (Access, watch::Receiver<ServiceStatus>) {
        let (status, receiver) = watch::channel(ServiceStatus::default());
        let access = Access::new(DeviceStore::open(&home.0).unwrap(), Arc::new(status));
        (access, receiver)
    }

    #[test]
    fn a_code_pairs_one_device_and_the_status_follows() {
        let home = TempDir::new("access");
        let (access, status) = access(&home);
        assert!(status.borrow().pairing.is_none());

        let display = access.start_pairing();
        assert_eq!(
            status.borrow().pairing.as_ref().map(|p| p.code.clone()),
            Some(display.code.clone())
        );

        let paired = access
            .pair(
                &display.code,
                Some("Mozilla/5.0 (iPhone; CPU iPhone OS 19_0)"),
            )
            .expect("the right code pairs");
        assert_eq!(paired.caller.name, "iPhone");
        assert!(status.borrow().pairing.is_none(), "the code is single use");
        let listed = status.borrow().devices.clone();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, paired.caller.id);
        assert!(!listed[0].subscribed);

        // Le cookie ouvre l'accès ; un autre jeton, non.
        let (caller, _) = access.authenticate(&paired.token).expect("a paired token");
        assert_eq!(caller, paired.caller);
        assert!(access.authenticate("AAAA").is_none());
        let other = URL_SAFE_NO_PAD.encode([7u8; 32]);
        assert!(access.authenticate(&other).is_none());

        assert_eq!(
            access.pair(&display.code, None).unwrap_err(),
            PairRefusal::NoCode
        );
    }

    #[test]
    fn refusals_say_why() {
        let home = TempDir::new("access");
        let (access, status) = access(&home);
        assert_eq!(
            access.pair("123456", None).unwrap_err(),
            PairRefusal::NoCode
        );
        let display = access.start_pairing();
        let wrong = if display.code == "000000" {
            "111111"
        } else {
            "000000"
        };
        for _ in 0..5 {
            assert_eq!(
                access.pair(wrong, None).unwrap_err(),
                PairRefusal::WrongCode
            );
        }
        assert_eq!(
            access.pair(&display.code, None).unwrap_err(),
            PairRefusal::Locked
        );
        assert!(
            status.borrow().pairing.is_none(),
            "a locked code leaves the screen"
        );
        assert!(status.borrow().devices.is_empty());
    }

    #[test]
    fn the_events_drive_the_pairing_state() {
        let home = TempDir::new("access");
        let (access, status) = access(&home);
        access.handle(ServiceEvent::StartPairing);
        let first = status.borrow().pairing.clone().expect("a code is shown");
        access.handle(ServiceEvent::StartPairing);
        let second = status.borrow().pairing.clone().unwrap();
        assert_ne!(first.code, second.code, "a new code replaces the old one");
        assert!(access.pairing_deadline().is_some());
        access.handle(ServiceEvent::CancelPairing);
        assert!(status.borrow().pairing.is_none());
        assert!(access.pairing_deadline().is_none());
    }

    #[test]
    fn forgetting_a_device_cuts_its_cookie_and_keeps_the_other() {
        let home = TempDir::new("access");
        let (access, status) = access(&home);
        let pair = |agent: &str| {
            let display = access.start_pairing();
            access.pair(&display.code, Some(agent)).unwrap()
        };
        let phone = pair("iPhone");
        let tablet = pair("iPad");
        assert_eq!(status.borrow().devices.len(), 2);

        access.handle(ServiceEvent::ForgetDevice {
            id: phone.caller.id.clone(),
        });
        assert!(access.authenticate(&phone.token).is_none());
        assert!(access.authenticate(&tablet.token).is_some());
        assert_eq!(status.borrow().devices.len(), 1);
        assert!(!access.forget(&phone.caller.id));
    }

    #[test]
    fn a_new_access_on_the_same_folder_knows_the_devices() {
        let home = TempDir::new("access");
        let token = {
            let (access, _) = access(&home);
            let display = access.start_pairing();
            access.pair(&display.code, Some("iPhone")).unwrap().token
        };
        let (access, status) = access(&home);
        assert!(access.authenticate(&token).is_some());
        assert_eq!(status.borrow().devices.len(), 1, "listed from the start");
    }

    #[test]
    fn a_paired_token_does_not_leak_through_debug() {
        let home = TempDir::new("access");
        let (access, _) = access(&home);
        let display = access.start_pairing();
        let paired = access.pair(&display.code, Some("iPhone")).unwrap();
        let printed = format!("{paired:?}");
        assert!(!printed.contains(&paired.token), "{printed}");
        assert!(printed.contains("iPhone"), "the device is still shown");
    }

    /// Une écriture de `devices.json` qui traîne (disque lent) ne fige pas la reconnaissance d'un cookie : le fichier
    /// voisin est un tube sans lecteur, donc l'écriture reste bloquée tant que le test ne l'ouvre pas en lecture.
    #[test]
    fn a_slow_devices_write_never_blocks_authentication() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::sync::mpsc;
        use std::time::Duration;

        let home = TempDir::new("access");
        let (access, _) = access(&home);
        let access = Arc::new(access);
        let pair = |agent: &str| {
            let display = access.start_pairing();
            access.pair(&display.code, Some(agent)).unwrap()
        };
        let phone = pair("iPhone");
        let tablet = pair("iPad");

        let fifo = home.0.join("companion").join("devices.json.tmp");
        let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);

        let (started, started_receiver) = mpsc::channel();
        let forgetting = {
            let access = Arc::clone(&access);
            let id = phone.caller.id.clone();
            std::thread::spawn(move || {
                started.send(()).unwrap();
                access.forget(&id)
            })
        };
        started_receiver.recv().unwrap();
        // Laisse l'oubli atteindre l'ouverture bloquée du fichier voisin.
        std::thread::sleep(Duration::from_millis(200));

        let (done, done_receiver) = mpsc::channel();
        {
            let access = Arc::clone(&access);
            let token = tablet.token.clone();
            std::thread::spawn(move || {
                let known = access.authenticate(&token).is_some();
                let deadline = access.pairing_deadline();
                done.send((known, deadline)).unwrap();
            });
        }
        let answered = done_receiver.recv_timeout(Duration::from_secs(2));

        // Libère l'écriture (le tube est vidé, `sync_all` sur un tube échoue : l'oubli est alors refusé).
        let mut drain = std::fs::File::open(&fifo).unwrap();
        std::io::Read::read_to_end(&mut drain, &mut Vec::new()).unwrap();
        forgetting.join().unwrap();

        let (known, deadline) = answered.expect("authenticate answered while the write was stuck");
        assert!(known);
        assert!(deadline.is_none());
    }
}
