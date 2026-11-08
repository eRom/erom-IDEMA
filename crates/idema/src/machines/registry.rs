//! Le registre des machines distantes et le pool de maîtres ssh de chacune.

use std::sync::Arc;

use idema_tmux::remote::RemoteSpec;
use idema_tmux::server::TmuxServer;

use super::probe::{MachineFacts, RemoteFailure};
use crate::config::RemoteMachineEntry;

/// Canaux longs (un par volet de terminal ou client de contrôle) portés par un maître avant d'en ouvrir un autre :
/// sshd limite à 10 sessions par connexion (`MaxSessions`), on en garde 6 pour les longs et le reste pour les brefs.
const LONG_LIVED_PER_MASTER: usize = 6;

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
    token: Arc<()>,
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
            token: Arc::clone(&master.token),
        }
    }

    /// Le serveur des appels brefs : même spec que le maître 0 (donc même `ControlPath`), mais un `TmuxServer` à lui,
    /// pour que ses clones ne comptent jamais comme des baux.
    pub fn brief(&self) -> Arc<TmuxServer> {
        Arc::clone(&self.brief)
    }

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
    pub facts: Option<MachineFacts>,
    /// Le nombre d'essais de connexion depuis le dernier succès (indice de `retry_delay`).
    pub attempt: u32,
    pub pool: Option<SlotPool>,
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
