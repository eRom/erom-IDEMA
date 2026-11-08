//! L'observateur du chemin réseau du Mac (spec ssh-tmux §3.2) : au retour du réseau, les machines distantes se
//! reconnectent tout de suite au lieu d'attendre la fin de leur délai (jusqu'à 60 s).
//!
//! `nw_path_monitor` (Network.framework) plutôt que la crate `system-configuration` : l'API pousse chaque changement de
//! chemin sur une file `dispatch` à elle, sans fil ni boucle d'exécution CF à entretenir, et `block2` est déjà une
//! dépendance (même FFI C que `signpost.rs`, aucune crate de plus). Pas de sondage périodique.

use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;

use block2::{DynBlock, RcBlock};
use futures::channel::mpsc::UnboundedSender;

/// `nw_path_status_satisfied` : un chemin utilisable existe. Tout autre état (`invalid`, `unsatisfied`, `satisfiable`)
/// est « pas de réseau » pour nous.
const NW_PATH_STATUS_SATISFIED: i32 = 1;

const QUEUE_LABEL: &CStr = c"com.erom.idema.network-path";

/// Le chemin réseau tel que l'observateur le rend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStatus {
    Satisfied,
    NotSatisfied,
}

impl PathStatus {
    /// Le `nw_path_status_t` brut de Network.framework.
    pub fn from_raw(raw: i32) -> Self {
        if raw == NW_PATH_STATUS_SATISFIED {
            Self::Satisfied
        } else {
            Self::NotSatisfied
        }
    }
}

/// Le réseau vient-il de revenir ? Vrai seulement au passage à « satisfait » depuis un état non satisfait : la première
/// notification (aucun état avant) et une répétition de « satisfait » (changement de Wi-Fi à Ethernet, par exemple) ne
/// déclenchent rien.
pub fn recovered(previous: Option<PathStatus>, current: PathStatus) -> bool {
    previous == Some(PathStatus::NotSatisfied) && current == PathStatus::Satisfied
}

type Monitor = *mut c_void;

#[link(name = "Network", kind = "framework")]
unsafe extern "C" {
    fn nw_path_monitor_create() -> Monitor;
    fn nw_path_monitor_set_queue(monitor: Monitor, queue: *mut c_void);
    fn nw_path_monitor_set_update_handler(
        monitor: Monitor,
        handler: &DynBlock<dyn Fn(*mut c_void)>,
    );
    fn nw_path_monitor_start(monitor: Monitor);
    fn nw_path_monitor_cancel(monitor: Monitor);
    fn nw_path_get_status(path: *mut c_void) -> i32;
    fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> *mut c_void;
}

/// L'observateur en marche. Le lâcher l'arrête.
pub struct NetworkMonitor {
    monitor: Monitor,
}

// SAFETY: le moniteur n'est touché que pour l'annuler à la chute ; Network.framework le permet de n'importe quel fil.
unsafe impl Send for NetworkMonitor {}

impl NetworkMonitor {
    /// Démarre l'observateur : chaque retour du réseau (`recovered`) envoie `()` sur `sender`, depuis la file `dispatch`
    /// de l'observateur, jamais depuis le fil d'interface.
    pub fn start(sender: UnboundedSender<()>) -> Self {
        let previous = Mutex::new(None::<PathStatus>);
        let handler = RcBlock::new(move |path: *mut c_void| {
            // SAFETY: Network.framework passe un `nw_path_t` valide pour la durée de l'appel.
            let current = PathStatus::from_raw(unsafe { nw_path_get_status(path) });
            let mut previous = previous
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let back = recovered(*previous, current);
            *previous = Some(current);
            if back {
                let _ = sender.unbounded_send(());
            }
        });
        // SAFETY: `nw_path_monitor_create` rend un objet neuf ; la file est sérielle (`attr` nul) et le bloc est copié par
        // `set_update_handler`, il survit à `handler`. L'objet et la file vivent autant que le process : IdemA n'ouvre
        // qu'un observateur.
        let monitor = unsafe {
            let monitor = nw_path_monitor_create();
            let queue = dispatch_queue_create(QUEUE_LABEL.as_ptr(), std::ptr::null_mut());
            nw_path_monitor_set_queue(monitor, queue);
            nw_path_monitor_set_update_handler(monitor, &handler);
            nw_path_monitor_start(monitor);
            monitor
        };
        Self { monitor }
    }
}

impl Drop for NetworkMonitor {
    fn drop(&mut self) {
        // SAFETY: `monitor` vient de `nw_path_monitor_create` et n'est annulé qu'ici.
        unsafe { nw_path_monitor_cancel(self.monitor) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_return_from_a_lost_network_counts() {
        use PathStatus::{NotSatisfied, Satisfied};
        // Le passage non satisfait -> satisfait déclenche.
        assert!(recovered(Some(NotSatisfied), Satisfied));
        // La première notification, au démarrage, n'est pas un retour.
        assert!(!recovered(None, Satisfied));
        assert!(!recovered(None, NotSatisfied));
        // Rester dans le même état, ou perdre le réseau, ne déclenche rien.
        assert!(!recovered(Some(Satisfied), Satisfied));
        assert!(!recovered(Some(NotSatisfied), NotSatisfied));
        assert!(!recovered(Some(Satisfied), NotSatisfied));
    }

    #[test]
    fn the_real_monitor_starts_reads_the_first_path_and_stops() {
        let (sender, mut receiver) = futures::channel::mpsc::unbounded::<()>();
        let monitor = NetworkMonitor::start(sender);
        // Laisse passer la première notification de Network.framework : lire son statut ne doit pas planter, et ce
        // premier état n'est pas un retour du réseau.
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(monitor);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn only_status_one_is_a_network() {
        // invalid, unsatisfied, satisfiable : pas de réseau utilisable.
        for raw in [0, 2, 3, -1, 99] {
            assert_eq!(PathStatus::from_raw(raw), PathStatus::NotSatisfied);
        }
        assert_eq!(PathStatus::from_raw(1), PathStatus::Satisfied);
    }
}
