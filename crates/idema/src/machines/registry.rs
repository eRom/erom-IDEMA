//! Le registre des machines distantes et le pool de maîtres ssh de chacune.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use gpui::Task;
use idema_tmux::remote::RemoteSpec;
use idema_tmux::server::TmuxServer;

use super::probe::{MachineFacts, RemoteFailure};
use crate::config::RemoteMachineEntry;

/// Canaux longs (un par volet de terminal ou client de contrôle) portés par un maître avant d'en ouvrir un autre :
/// sshd limite à 10 sessions par connexion (`MaxSessions`), on en garde 6 pour les longs et le reste pour les brefs.
const LONG_LIVED_PER_MASTER: usize = 6;

/// La porte du second canal bref de la machine `connection` : une par connexion pour tout le process, partagée entre
/// l'espace de travail (`RemoteMachine::adhoc`) et le fil du compagnon, qui n'a pas accès à l'espace de travail. Elle
/// survit au retrait puis à la remise d'une machine : un appel encore en vol ne se double pas d'un second.
pub fn adhoc_gate(connection: &str) -> Arc<Mutex<()>> {
    static GATES: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(gates.entry(connection.to_owned()).or_default())
}

/// L'état de la connexion à une machine.
#[derive(Debug, Clone, PartialEq)]
pub enum MachineStatus {
    Connecting,
    Connected,
    Failed(RemoteFailure),
}

/// Le bail d'un canal long : tant qu'il vit, son canal est compté sur son maître. Non clonable : un bail, un canal.
pub struct Lease {
    pub server: Arc<TmuxServer>,
    /// Compté par `Arc::strong_count` : sa disparition libère le canal.
    _token: Arc<()>,
}

impl Lease {
    /// Un bail hors pool : une session locale n'a aucun maître ssh à compter.
    pub fn unpooled(server: Arc<TmuxServer>) -> Self {
        Self {
            server,
            _token: Arc::new(()),
        }
    }
}

/// Un maître ssh : le serveur qui l'ouvre et le jeton que ses baux se partagent.
struct Master {
    slot: u8,
    server: Arc<TmuxServer>,
    token: Arc<()>,
}

impl Master {
    fn new(spec: RemoteSpec) -> Self {
        Self {
            slot: spec.slot,
            server: Arc::new(TmuxServer::remote(spec)),
            token: Arc::new(()),
        }
    }

    /// Les baux vivants : le maître garde lui-même une référence au jeton.
    fn leases(&self) -> usize {
        Arc::strong_count(&self.token) - 1
    }
}

/// Les maîtres ssh d'une machine. Le pool survit aux reconnexions : seuls les faits (`$HOME`, `$SHELL`) changent.
pub struct SlotPool {
    base: RemoteSpec,
    masters: Vec<Master>,
    brief: Arc<TmuxServer>,
}

impl SlotPool {
    pub fn new(base: RemoteSpec) -> Self {
        let base = RemoteSpec { slot: 0, ..base };
        Self {
            masters: vec![Master::new(base.clone())],
            brief: Arc::new(TmuxServer::remote(base.clone())),
            base,
        }
    }

    /// Un canal long : le premier maître qui en porte moins de 6, sinon un maître de plus (slot suivant).
    pub fn long_lived(&mut self) -> Lease {
        let index = match self
            .masters
            .iter()
            .position(|master| master.leases() < LONG_LIVED_PER_MASTER)
        {
            Some(index) => index,
            None => {
                let slot = u8::try_from(self.masters.len()).unwrap_or(u8::MAX);
                self.masters.push(Master::new(RemoteSpec {
                    slot,
                    ..self.base.clone()
                }));
                self.masters.len() - 1
            }
        };
        let master = &self.masters[index];
        Lease {
            server: Arc::clone(&master.server),
            _token: Arc::clone(&master.token),
        }
    }

    /// Le serveur des appels brefs : même spec que le maître 0 (donc même `ControlPath`), mais un `TmuxServer` à lui,
    /// pour que ses clones ne comptent jamais comme des baux.
    pub fn brief(&self) -> Arc<TmuxServer> {
        Arc::clone(&self.brief)
    }

    /// Les specs de tous les maîtres du pool, un par numéro de canal : ce qu'il faut pour les fermer (`ssh -O exit`) quand le
    /// Mac sort de veille et que leurs connexions sont mortes.
    pub fn master_specs(&self) -> Vec<RemoteSpec> {
        self.masters
            .iter()
            .filter_map(|master| master.server.remote_spec().cloned())
            .collect()
    }

    #[cfg(test)]
    pub fn slots(&self) -> usize {
        self.masters.len()
    }

    /// Met à jour `$HOME` et `$SHELL` après une reconnexion. Les baux déjà donnés gardent leur ancien serveur.
    pub fn update_facts(&mut self, home: String, shell: String) {
        self.base.home = home;
        self.base.shell = shell;
        for master in &mut self.masters {
            let spec = RemoteSpec {
                slot: master.slot,
                ..self.base.clone()
            };
            master.server = Arc::new(TmuxServer::remote(spec));
        }
        self.brief = Arc::new(TmuxServer::remote(self.base.clone()));
    }
}

/// Une machine distante déclarée dans la config, avec son état.
pub struct RemoteMachine {
    pub entry: RemoteMachineEntry,
    pub status: MachineStatus,
    /// La cause du dernier échec de connexion, gardée pendant l'essai suivant (`Connecting`) pour que la barre ne la
    /// perde pas à chaque tentative ; effacée par une connexion réussie.
    pub last_failure: Option<RemoteFailure>,
    /// La machine était connectée et se reconnecte (`lose_machine`, réveil du Mac) : la barre dit « Connexion… » dès la
    /// perte, sans attendre un premier échec. Effacée par une connexion réussie ; la première connexion reste muette.
    pub reconnecting: bool,
    pub facts: Option<MachineFacts>,
    /// Le nombre d'essais de connexion depuis le dernier succès (indice de `retry_delay`).
    pub attempt: u32,
    pub pool: Option<SlotPool>,
    /// Un sondage de connexion est en vol : un second n'est pas lancé tant que le premier n'a pas rendu.
    pub probing: bool,
    /// Une relecture des sessions est en vol (au plus une par machine : un canal bref, jamais empilé).
    pub refreshing: bool,
    /// Depuis quand la relecture en vol est partie : une relecture qui ne rend pas signale un lien figé (`link_stalled`).
    pub refresh_started: Option<Instant>,
    /// Le lien est tenu pour bloqué (une relecture ne rend pas, `link_stalled`) alors que la machine est encore
    /// `Connected` : la barre dit « Connexion… ». Effacé quand une relecture rend, que la machine est perdue ou
    /// reconnectée.
    pub link_stalled: bool,
    /// Une relecture a été demandée pendant la précédente : elle repart dès que celle-ci a rendu.
    pub refresh_again: bool,
    /// Les relectures de suite qui ont échoué : la connexion est tenue pour perdue au troisième échec.
    pub refresh_failures: u8,
    /// Le second canal bref : les commandes ponctuelles (fermer, renommer, `has-session`, lecture d'écran du compagnon)
    /// passent une à une (`adhoc_gate`).
    pub adhoc: Arc<Mutex<()>>,
    /// La relecture toutes les 2 s tant que la machine est connectée.
    pub poll: Option<Task<()>>,
    /// Le prochain essai de connexion, après `retry_delay`.
    pub retry: Option<Task<()>>,
}

impl RemoteMachine {
    /// Ce que la barre dit d'une machine qui ne répond pas : l'échec en cours, ou celui du dernier essai tant que le
    /// suivant est en vol. Rien pour une machine connectée, ni pour une première connexion.
    pub fn failure(&self) -> Option<&RemoteFailure> {
        match &self.status {
            MachineStatus::Failed(failure) => Some(failure),
            MachineStatus::Connecting => self.last_failure.as_ref(),
            MachineStatus::Connected => None,
        }
    }

    /// Lève le drapeau « lien bloqué ». Vrai à la première détection d'un blocage (la seule à journaliser), faux tant
    /// que le même blocage dure.
    pub fn mark_link_stalled(&mut self) -> bool {
        !std::mem::replace(&mut self.link_stalled, true)
    }

    /// Une machine qui vient d'être déclarée : elle se connecte.
    pub fn new(entry: RemoteMachineEntry) -> Self {
        let connection = entry.connection.clone();
        Self {
            entry,
            status: MachineStatus::Connecting,
            last_failure: None,
            reconnecting: false,
            facts: None,
            attempt: 0,
            pool: None,
            probing: false,
            refreshing: false,
            refresh_again: false,
            refresh_started: None,
            link_stalled: false,
            refresh_failures: 0,
            adhoc: adhoc_gate(&connection),
            poll: None,
            retry: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec() -> RemoteSpec {
        RemoteSpec {
            destination: "dell-lan".into(),
            control_dir: PathBuf::from("/tmp/idema-501-00000000"),
            slot: 0,
            ssh_env: Vec::new(),
            home: "/home/llm".into(),
            shell: "/bin/bash".into(),
        }
    }

    fn slot_of(lease: &Lease) -> u8 {
        lease.server.remote_spec().unwrap().slot
    }

    #[test]
    fn a_reconnecting_machine_keeps_its_last_failure() {
        let mut machine = RemoteMachine::new(RemoteMachineEntry {
            label: "Dell".into(),
            connection: "dell-lan".into(),
        });
        // Première connexion : rien à dire.
        assert_eq!(machine.failure(), None);
        machine.status = MachineStatus::Failed(RemoteFailure::Unreachable);
        machine.last_failure = Some(RemoteFailure::Unreachable);
        assert_eq!(machine.failure(), Some(&RemoteFailure::Unreachable));
        // L'essai suivant est en vol : la cause reste affichée.
        machine.status = MachineStatus::Connecting;
        assert_eq!(machine.failure(), Some(&RemoteFailure::Unreachable));
        // Connectée : plus d'échec.
        machine.status = MachineStatus::Connected;
        machine.last_failure = None;
        assert_eq!(machine.failure(), None);
    }

    #[test]
    fn a_stall_is_reported_once_until_it_is_cleared() {
        let mut machine = RemoteMachine::new(RemoteMachineEntry {
            label: "Dell".into(),
            connection: "dell-lan".into(),
        });
        // Première détection : à journaliser. Les suivantes, pendant le même blocage : non.
        assert!(machine.mark_link_stalled());
        assert!(!machine.mark_link_stalled());
        assert!(!machine.mark_link_stalled());
        // Le blocage levé, le suivant est de nouveau une première détection.
        machine.link_stalled = false;
        assert!(machine.mark_link_stalled());
    }

    #[test]
    fn the_pool_opens_a_master_beyond_six_long_lived_holders() {
        let mut pool = SlotPool::new(spec());
        let mut held: Vec<Lease> = (0..6).map(|_| pool.long_lived()).collect();
        assert_eq!(pool.slots(), 1);
        assert!(held.iter().all(|lease| slot_of(lease) == 0));

        held.push(pool.long_lived());
        assert_eq!(pool.slots(), 2);
        assert_eq!(slot_of(held.last().unwrap()), 1);

        // Un bail lâché sur le maître 0 libère sa place : le suivant y retourne, sans ouvrir de maître.
        drop(held.remove(0));
        let reused = pool.long_lived();
        assert_eq!(pool.slots(), 2);
        assert_eq!(slot_of(&reused), 0);

        // Cent clones du serveur bref ne comptent jamais comme des baux.
        let clones: Vec<Arc<TmuxServer>> = (0..100).map(|_| pool.brief()).collect();
        drop(held);
        drop(reused);
        let fresh: Vec<Lease> = (0..6).map(|_| pool.long_lived()).collect();
        assert_eq!(pool.slots(), 2);
        assert!(fresh.iter().all(|lease| slot_of(lease) == 0));
        assert_eq!(clones.len(), 100);
    }

    #[test]
    fn every_master_of_the_pool_can_be_closed_by_its_own_control_path() {
        let mut pool = SlotPool::new(spec());
        let _held: Vec<Lease> = (0..7).map(|_| pool.long_lived()).collect();
        let slots: Vec<u8> = pool.master_specs().iter().map(|spec| spec.slot).collect();
        assert_eq!(slots, vec![0, 1]);
    }

    #[test]
    fn the_brief_server_shares_the_first_master_spec_without_being_it() {
        let mut pool = SlotPool::new(spec());
        let lease = pool.long_lived();
        let brief = pool.brief();
        assert!(!Arc::ptr_eq(&brief, &lease.server));
        assert_eq!(brief.remote_spec(), lease.server.remote_spec());
    }

    #[test]
    fn update_facts_keeps_the_pool_and_changes_home_and_shell() {
        let mut pool = SlotPool::new(spec());
        let kept: Vec<Lease> = (0..7).map(|_| pool.long_lived()).collect();
        pool.update_facts("/home/other".into(), "/bin/zsh".into());
        assert_eq!(pool.slots(), 2);
        assert_eq!(pool.brief().remote_spec().unwrap().home, "/home/other");
        // Les baux vivants comptent toujours : le maître 0 est plein, un nouveau bail va sur le maître 1.
        let next = pool.long_lived();
        assert_eq!(slot_of(&next), 1);
        assert_eq!(next.server.remote_spec().unwrap().shell, "/bin/zsh");
        assert_eq!(kept.len(), 7);
    }
}
