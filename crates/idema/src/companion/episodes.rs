//! Les épisodes « attend » et « erreur » et la politique qui décide quand les notifier (spec §3.7). Tout est pur : le
//! temps est celui que l'appelant passe, rien ne dort ni ne lit l'horloge. Le service (`service.rs`) y branche ses
//! événements et agit sur ce que `due` rend.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use idema_term::activity::ActivityState;

/// Au plus une notification par session toutes les 30 s.
pub const COOLDOWN: Duration = Duration::from_secs(30);
/// Renvois d'un épisode dont l'envoi a échoué pour une raison passagère (429, 5xx, délai de curl) : 3 au plus.
pub const MAX_RETRIES: u8 = 3;
/// Une déclaration « je regarde ce pane » tombe 30 s après la dernière.
pub const WATCH_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeKind {
    Waiting,
    Error,
}

impl EpisodeKind {
    /// Le mot du journal (`kind=`) et de la charge utile (`type`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Error => "error",
        }
    }
}

/// L'état d'activité qui fait vivre un épisode de ce type.
pub fn episode_state(kind: EpisodeKind) -> ActivityState {
    match kind {
        EpisodeKind::Waiting => ActivityState::Waiting,
        EpisodeKind::Error => ActivityState::Error,
    }
}

/// Les types d'épisode que l'utilisateur veut voir notifiés (`notify_waiting`, `notify_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub notify_waiting: bool,
    pub notify_error: bool,
}

/// Ce que devient un épisode qui s'ouvre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOutcome {
    /// Rien à faire de plus : l'épisode attend sa notification.
    Pending,
    /// Le pane est regardé sur le téléphone : il faut l'acquitter aussitôt (`AckOrigin::Watched`).
    AutoAck,
}

/// Pourquoi une notification est retenue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    Disabled,
    Present,
    Cooldown,
    NoSubscriber,
}

impl Held {
    /// Le mot du journal (`reason=`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Present => "present",
            Self::Cooldown => "cooldown",
            Self::NoSubscriber => "no_subscriber",
        }
    }
}

/// Un épisode à notifier maintenant. `seq` identifie l'épisode (posé à son ouverture) : un résultat d'envoi qui revient
/// après que l'épisode a été clos, puis rouvert, ne touche pas le nouveau (`delivered`, `abandon`, `retry_later` et
/// `release` comparent `seq`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    pub session: String,
    pub pane: u32,
    pub kind: EpisodeKind,
    pub seq: u64,
}

/// Ce que l'état d'une session change aux épisodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeChange {
    Open(EpisodeKind),
    Close(EpisodeKind),
}

/// Où en est l'envoi d'un épisode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Pas encore notifié : `due` le rend dès que les conditions tiennent.
    Pending,
    /// Un envoi est parti, son résultat n'est pas revenu.
    InFlight,
    /// Notifié, ou abandonné (réponse sans appel, trop d'échecs) : plus jamais envoyé.
    Done,
}

struct Episode {
    /// L'identité de l'épisode : un épisode clos puis rouvert en a une autre.
    seq: u64,
    pane: u32,
    phase: Phase,
    /// Renvois déjà demandés par `retry_later`.
    retries: u8,
    /// La dernière raison dite pour cet épisode : une retenue n'est rapportée qu'une fois, pas à chaque tour.
    last_held: Option<Held>,
    /// L'anti-rafale de la session avant le départ de l'envoi en cours, que `release` rétablit.
    attempt_before: Option<Instant>,
}

#[derive(Default)]
struct Slot {
    waiting: Option<Episode>,
    error: Option<Episode>,
    /// Le dernier envoi tenté pour la session (réussi ou non) : l'anti-rafale part de là.
    last_attempt: Option<Instant>,
}

impl Slot {
    fn episode(&self, kind: EpisodeKind) -> Option<&Episode> {
        match kind {
            EpisodeKind::Waiting => self.waiting.as_ref(),
            EpisodeKind::Error => self.error.as_ref(),
        }
    }

    fn episode_mut(&mut self, kind: EpisodeKind) -> &mut Option<Episode> {
        match kind {
            EpisodeKind::Waiting => &mut self.waiting,
            EpisodeKind::Error => &mut self.error,
        }
    }

    /// L'épisode que ce `Due` désigne, s'il existe encore (même type, même `seq`).
    fn episode_of(&mut self, due: &Due) -> Option<&mut Episode> {
        self.episode_mut(due.kind)
            .as_mut()
            .filter(|episode| episode.seq == due.seq)
    }
}

struct Watch {
    session: String,
    pane: u32,
    declared_at: Instant,
}

impl Watch {
    fn holds(&self, session: &str, pane: u32, now: Instant) -> bool {
        self.session == session && self.pane == pane && now < self.declared_at + WATCH_TTL
    }
}

/// L'état de tous les épisodes, par session.
pub struct Episodes {
    policy: Policy,
    away: bool,
    /// Le pane que chaque appareil dit regarder (une déclaration par appareil : le `null` d'un appareil n'efface pas
    /// celle d'un autre).
    watches: BTreeMap<String, Watch>,
    sessions: BTreeMap<String, Slot>,
    next_seq: u64,
}

const KINDS: [EpisodeKind; 2] = [EpisodeKind::Waiting, EpisodeKind::Error];

impl Episodes {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            away: false,
            watches: BTreeMap::new(),
            sessions: BTreeMap::new(),
            next_seq: 0,
        }
    }

    /// Les réglages de notification ont changé (config relue à chaud) : les épisodes ouverts sont réévalués au prochain
    /// `due`.
    pub fn set_policy(&mut self, policy: Policy) {
        self.policy = policy;
    }

    fn enabled(&self, kind: EpisodeKind) -> bool {
        match kind {
            EpisodeKind::Waiting => self.policy.notify_waiting,
            EpisodeKind::Error => self.policy.notify_error,
        }
    }

    /// Jusqu'à quand un appareil au moins déclare regarder ce pane (le plus tard de ses déclarations), s'il en est un.
    fn watched_until(&self, session: &str, pane: u32, now: Instant) -> Option<Instant> {
        self.watches
            .values()
            .filter(|watch| watch.holds(session, pane, now))
            .map(|watch| watch.declared_at + WATCH_TTL)
            .max()
    }

    /// Un épisode s'ouvre. Un pane regardé sur un téléphone est acquitté aussitôt (`AutoAck`) et ne devient pas un
    /// épisode à notifier ; sinon l'épisode attend ses conditions. Rouvrir un épisode déjà ouvert de ce type ne fait
    /// rien (il n'est notifié qu'une fois).
    pub fn open(
        &mut self,
        session: &str,
        pane: u32,
        kind: EpisodeKind,
        now: Instant,
    ) -> OpenOutcome {
        if self.watched_until(session, pane, now).is_some() {
            return OpenOutcome::AutoAck;
        }
        let seq = self.next_seq;
        let slot = self.sessions.entry(session.to_owned()).or_default();
        let episode = slot.episode_mut(kind);
        if episode.is_none() {
            self.next_seq += 1;
            *episode = Some(Episode {
                seq,
                pane,
                phase: Phase::Pending,
                retries: 0,
                last_held: None,
                attempt_before: None,
            });
        }
        OpenOutcome::Pending
    }

    /// L'épisode est fini (l'état a quitté « attend » ou « erreur », acquitté ou session disparue). L'anti-rafale de la
    /// session, lui, survit à la clôture.
    pub fn close(&mut self, session: &str, kind: EpisodeKind) {
        if let Some(slot) = self.sessions.get_mut(session) {
            *slot.episode_mut(kind) = None;
        }
    }

    /// Romain est absent (vrai) ou présent. Rien ne part ici : `due` décide au prochain tour.
    pub fn set_away(&mut self, away: bool) {
        self.away = away;
    }

    /// Une saisie ou un acquittement venus du téléphone ont touché cette session : l'anti-rafale repart de zéro.
    pub fn phone_interaction(&mut self, session: &str) {
        if let Some(slot) = self.sessions.get_mut(session) {
            slot.last_attempt = None;
        }
    }

    /// L'appareil `device` déclare regarder ce pane. `AutoAck` quand ce pane a déjà un épisode ouvert : il est à acquitter
    /// aussi.
    pub fn watch(&mut self, device: &str, session: &str, pane: u32, now: Instant) -> OpenOutcome {
        self.watches
            .retain(|_, watch| now < watch.declared_at + WATCH_TTL);
        self.watches.insert(
            device.to_owned(),
            Watch {
                session: session.to_owned(),
                pane,
                declared_at: now,
            },
        );
        let open_there = self.sessions.get(session).is_some_and(|slot| {
            KINDS.iter().any(|kind| {
                slot.episode(*kind)
                    .is_some_and(|episode| episode.pane == pane)
            })
        });
        if open_there {
            OpenOutcome::AutoAck
        } else {
            OpenOutcome::Pending
        }
    }

    /// L'appareil `device` ne regarde plus rien (la page est passée en arrière-plan). Les autres appareils gardent la leur.
    pub fn unwatch(&mut self, device: &str) {
        self.watches.remove(device);
    }

    /// Les épisodes à notifier maintenant, et ceux que leur raison retient (chacune rapportée une fois, quand elle
    /// change). `subscribers` : le nombre d'appareils abonnés. Les conditions se lisent dans l'ordre de la spec :
    /// type activé, Romain absent, pane non regardé, anti-rafale, au moins un appareil. Un épisode rendu est « en vol » :
    /// il ne repart que si `retry_later` ou `release` le remet en attente.
    pub fn due(&mut self, now: Instant, subscribers: usize) -> (Vec<Due>, Vec<(Due, Held)>) {
        let mut ready = Vec::new();
        let mut held = Vec::new();
        let (policy, away) = (self.policy, self.away);
        let enabled = |kind: EpisodeKind| match kind {
            EpisodeKind::Waiting => policy.notify_waiting,
            EpisodeKind::Error => policy.notify_error,
        };
        let watches = &self.watches;
        for (session, slot) in &mut self.sessions {
            for kind in KINDS {
                let before = slot.last_attempt;
                let cooling = before.is_some_and(|attempt| now < attempt + COOLDOWN);
                let Some(episode) = slot.episode_mut(kind).as_mut() else {
                    continue;
                };
                if episode.phase != Phase::Pending {
                    continue;
                }
                let verdict = if !enabled(kind) {
                    Some(Held::Disabled)
                } else if !away {
                    Some(Held::Present)
                } else if watches
                    .values()
                    .any(|watch| watch.holds(session, episode.pane, now))
                {
                    // Regardé : l'acquittement est en route, il n'y a rien à dire ni à envoyer.
                    episode.last_held = None;
                    continue;
                } else if cooling {
                    Some(Held::Cooldown)
                } else if subscribers == 0 {
                    Some(Held::NoSubscriber)
                } else {
                    None
                };
                let entry = Due {
                    session: session.clone(),
                    pane: episode.pane,
                    kind,
                    seq: episode.seq,
                };
                match verdict {
                    Some(reason) => {
                        if episode.last_held != Some(reason) {
                            episode.last_held = Some(reason);
                            held.push((entry, reason));
                        }
                    }
                    None => {
                        episode.phase = Phase::InFlight;
                        episode.last_held = None;
                        episode.attempt_before = before;
                        // La tentative compte dès son départ : un second épisode de la session, dans ce même tour,
                        // est déjà dans l'anti-rafale.
                        slot.last_attempt = Some(now);
                        ready.push(entry);
                    }
                }
            }
        }
        // Une session sans épisode et hors de son anti-rafale n'a plus rien à retenir.
        self.sessions.retain(|_, slot| {
            slot.waiting.is_some()
                || slot.error.is_some()
                || slot
                    .last_attempt
                    .is_some_and(|attempt| now < attempt + COOLDOWN)
        });
        (ready, held)
    }

    /// Le service push a pris la notification : l'épisode est notifié, et l'anti-rafale part de maintenant. Sans effet si
    /// l'épisode a été clos entre-temps (et rouvert : un autre `seq`).
    pub fn delivered(&mut self, due: &Due, now: Instant) {
        let Some(slot) = self.sessions.get_mut(&due.session) else {
            return;
        };
        if let Some(episode) = slot.episode_of(due) {
            episode.phase = Phase::Done;
            slot.last_attempt = Some(now);
        }
    }

    /// L'envoi a reçu une réponse sans appel (ni succès ni raison de réessayer) : l'épisode ne sera plus envoyé.
    pub fn abandon(&mut self, due: &Due) {
        if let Some(episode) = self
            .sessions
            .get_mut(&due.session)
            .and_then(|slot| slot.episode_of(due))
        {
            episode.phase = Phase::Done;
        }
    }

    /// L'envoi n'a pu joindre personne (plus aucun appareil abonné au moment d'envoyer) : l'épisode reste en attente,
    /// sans renvoi compté et sans anti-rafale consommée.
    pub fn release(&mut self, due: &Due) {
        let Some(slot) = self.sessions.get_mut(&due.session) else {
            return;
        };
        if let Some(episode) = slot.episode_of(due) {
            episode.phase = Phase::Pending;
            episode.last_held = None;
            let before = episode.attempt_before.take();
            slot.last_attempt = before;
        }
    }

    /// L'envoi a échoué pour une raison passagère. Rend vrai quand l'épisode sera renvoyé à la fin de l'anti-rafale ;
    /// faux quand il est abandonné (plus de `MAX_RETRIES` renvois) ou n'existe plus (clos entre-temps, ou rouvert : un
    /// autre `seq` que celui de l'envoi qui revient).
    pub fn retry_later(&mut self, due: &Due, now: Instant) -> bool {
        let Some(slot) = self.sessions.get_mut(&due.session) else {
            return false;
        };
        if slot.episode_of(due).is_none() {
            return false;
        }
        slot.last_attempt = Some(now);
        let Some(episode) = slot.episode_of(due) else {
            return false;
        };
        episode.retries += 1;
        if episode.retries > MAX_RETRIES {
            episode.phase = Phase::Done;
            return false;
        }
        episode.phase = Phase::Pending;
        true
    }

    /// Le prochain instant où `due` pourrait rendre quelque chose sans qu'un événement arrive : la fin d'une
    /// anti-rafale qui retient un épisode, ou la chute de la dernière déclaration de pane regardé qui en retient un.
    pub fn next_wakeup(&self, now: Instant) -> Option<Instant> {
        let mut earliest: Option<Instant> = None;
        let mut consider = |instant: Instant| {
            if instant > now {
                earliest = Some(earliest.map_or(instant, |held| held.min(instant)));
            }
        };
        for (session, slot) in &self.sessions {
            for kind in KINDS {
                let Some(episode) = slot.episode(kind) else {
                    continue;
                };
                if episode.phase != Phase::Pending || !self.enabled(kind) || !self.away {
                    continue;
                }
                if let Some(until) = self.watched_until(session, episode.pane, now) {
                    consider(until);
                } else if let Some(attempt) = slot.last_attempt {
                    consider(attempt + COOLDOWN);
                }
            }
        }
        earliest
    }
}

#[cfg(test)]
impl Episodes {
    /// L'épisode ouvert de ce type, tel que `due` l'aurait rendu (pour rejouer un résultat d'envoi dans un test).
    fn current_due(&self, session: &str, kind: EpisodeKind) -> Option<Due> {
        let episode = self.sessions.get(session)?.episode(kind)?;
        Some(Due {
            session: session.to_owned(),
            pane: episode.pane,
            kind,
            seq: episode.seq,
        })
    }
}

/// Plus aucun pane de la session n'attend : l'épisode « attend » qu'un pane caché avait ouvert (ou que l'état agrégé
/// masquait derrière une erreur) est fini. `panes` : (identifiant, actif, état).
pub fn no_pane_waiting(panes: &[(u32, bool, ActivityState)]) -> bool {
    !panes
        .iter()
        .any(|(_, _, state)| *state == ActivityState::Waiting)
}

/// Ce que le passage de `previous` à `current` change aux épisodes de la session : les clôtures d'abord, puis les
/// ouvertures. `chimes` : IdemA sonnerait (`should_chime`, que le son soit coupé ou non) ; `alerting` : la session
/// compte pour le badge.
pub fn episode_changes(
    previous: ActivityState,
    current: ActivityState,
    chimes: bool,
    alerting: bool,
) -> Vec<EpisodeChange> {
    let mut changes = Vec::new();
    if previous == ActivityState::Waiting && current != ActivityState::Waiting {
        changes.push(EpisodeChange::Close(EpisodeKind::Waiting));
    }
    if previous == ActivityState::Error && current != ActivityState::Error {
        changes.push(EpisodeChange::Close(EpisodeKind::Error));
    }
    if alerting && chimes {
        changes.push(EpisodeChange::Open(EpisodeKind::Waiting));
    }
    if alerting && current == ActivityState::Error && previous != ActivityState::Error {
        changes.push(EpisodeChange::Open(EpisodeKind::Error));
    }
    changes
}

/// Le pane d'un épisode de session : le plus urgent de ses panes, c'est-à-dire un pane dans l'état de l'épisode (l'actif
/// de préférence), à défaut le pane le plus urgent (l'événement a pu devancer le relevé des panes), à défaut l'actif.
/// `panes` : (identifiant, actif, état).
pub fn urgent_pane(panes: &[(u32, bool, ActivityState)], kind: EpisodeKind) -> Option<u32> {
    let wanted = episode_state(kind);
    let in_state = || panes.iter().filter(|(_, _, state)| *state == wanted);
    in_state()
        .find(|(_, active, _)| *active)
        .or_else(|| in_state().next())
        .or_else(|| {
            panes
                .iter()
                .max_by_key(|(_, active, state)| (*state, *active))
        })
        .map(|(id, _, _)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAITING: EpisodeKind = EpisodeKind::Waiting;
    const ERROR: EpisodeKind = EpisodeKind::Error;
    const BOTH: Policy = Policy {
        notify_waiting: true,
        notify_error: true,
    };

    fn at(base: Instant, seconds: u64) -> Instant {
        base + Duration::from_secs(seconds)
    }

    /// Un `Due` sans identité d'épisode (`seq` à zéro) : `plain_due` ramène ce que `due` rend à cette forme, pour que les
    /// assertions comparent la session, le pane et le type.
    fn due(session: &str, pane: u32, kind: EpisodeKind) -> Due {
        Due {
            session: session.into(),
            pane,
            kind,
            seq: 0,
        }
    }

    fn plain(due: Due) -> Due {
        Due { seq: 0, ..due }
    }

    fn plain_due(
        episodes: &mut Episodes,
        now: Instant,
        subscribers: usize,
    ) -> (Vec<Due>, Vec<(Due, Held)>) {
        let (ready, held) = episodes.due(now, subscribers);
        (
            ready.into_iter().map(plain).collect(),
            held.into_iter()
                .map(|(due, why)| (plain(due), why))
                .collect(),
        )
    }

    /// Les sessions notifiées à cet instant, sans les retenues.
    fn sent(episodes: &mut Episodes, now: Instant, subscribers: usize) -> Vec<Due> {
        plain_due(episodes, now, subscribers).0
    }

    /// Le résultat d'envoi de l'épisode ouvert de ce type.
    fn deliver(episodes: &mut Episodes, session: &str, kind: EpisodeKind, now: Instant) {
        let due = episodes
            .current_due(session, kind)
            .expect("an open episode");
        episodes.delivered(&due, now);
    }

    fn retry(episodes: &mut Episodes, session: &str, kind: EpisodeKind, now: Instant) -> bool {
        match episodes.current_due(session, kind) {
            Some(due) => episodes.retry_later(&due, now),
            None => false,
        }
    }

    fn give_up(episodes: &mut Episodes, session: &str, kind: EpisodeKind) {
        let due = episodes
            .current_due(session, kind)
            .expect("an open episode");
        episodes.abandon(&due);
    }

    #[test]
    fn present_holds_then_away_sends_without_a_new_transition() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        assert_eq!(episodes.open("s", 4, WAITING, t0), OpenOutcome::Pending);

        // Romain est présent : retenu, avec sa raison, une seule fois (pas une ligne à chaque tour).
        let (sent_now, held) = plain_due(&mut episodes, t0, 1);
        assert!(sent_now.is_empty());
        assert_eq!(held, vec![(due("s", 4, WAITING), Held::Present)]);
        let (sent_now, held) = plain_due(&mut episodes, at(t0, 1), 1);
        assert!(
            sent_now.is_empty() && held.is_empty(),
            "held once, not at every turn"
        );

        // Il devient absent : l'épisode, resté ouvert, part sans nouvelle transition.
        episodes.set_away(true);
        assert_eq!(
            sent(&mut episodes, at(t0, 2), 1),
            vec![due("s", 4, WAITING)]
        );
    }

    #[test]
    fn each_episode_is_sent_once() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.open("s", 4, WAITING, t0);
        assert_eq!(sent(&mut episodes, t0, 1).len(), 1);
        // Tant que l'envoi est en cours, ni la boucle ni un autre événement ne le redemandent.
        assert!(sent(&mut episodes, at(t0, 1), 1).is_empty());
        deliver(&mut episodes, "s", WAITING, at(t0, 1));
        // Une fois notifié, ni le temps qui passe, ni l'ouverture répétée du même épisode ne le renvoient.
        episodes.open("s", 4, WAITING, at(t0, 2));
        episodes.set_away(false);
        episodes.set_away(true);
        for seconds in [2, 31, 120, 3600] {
            assert!(
                sent(&mut episodes, at(t0, seconds), 1).is_empty(),
                "{seconds}"
            );
        }
        assert_eq!(episodes.next_wakeup(at(t0, 3)), None);
        // L'épisode clos, puis un nouveau : c'est un autre épisode, qui repart (après l'anti-rafale).
        episodes.close("s", WAITING);
        episodes.open("s", 4, WAITING, at(t0, 100));
        assert_eq!(sent(&mut episodes, at(t0, 100), 1).len(), 1);
    }

    #[test]
    fn a_second_wait_within_30s_is_deferred_not_dropped() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.open("s", 4, WAITING, t0);
        assert_eq!(sent(&mut episodes, t0, 1).len(), 1);
        deliver(&mut episodes, "s", WAITING, t0);

        // La session repart puis réattend à +15 s : différée, avec son réveil à +30 s.
        episodes.close("s", WAITING);
        episodes.open("s", 4, WAITING, at(t0, 15));
        let (now_sent, held) = plain_due(&mut episodes, at(t0, 15), 1);
        assert!(now_sent.is_empty());
        assert_eq!(held, vec![(due("s", 4, WAITING), Held::Cooldown)]);
        assert_eq!(episodes.next_wakeup(at(t0, 15)), Some(at(t0, 30)));
        assert!(sent(&mut episodes, at(t0, 29), 1).is_empty());

        // L'attente tient à la fin du délai : elle part.
        assert_eq!(
            sent(&mut episodes, at(t0, 30), 1),
            vec![due("s", 4, WAITING)]
        );
    }

    #[test]
    fn a_deferred_wait_that_ended_is_never_sent() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.open("s", 4, WAITING, t0);
        sent(&mut episodes, t0, 1);
        deliver(&mut episodes, "s", WAITING, t0);
        episodes.close("s", WAITING);
        episodes.open("s", 4, WAITING, at(t0, 15));
        assert!(sent(&mut episodes, at(t0, 16), 1).is_empty());

        // Acquittée (ou reprise) avant la fin du délai : rien ne part, et plus rien ne réveille la boucle.
        episodes.close("s", WAITING);
        assert!(sent(&mut episodes, at(t0, 30), 1).is_empty());
        assert!(sent(&mut episodes, at(t0, 90), 1).is_empty());
        assert_eq!(episodes.next_wakeup(at(t0, 31)), None);
    }

    #[test]
    fn the_cooldown_is_per_session_and_shared_by_both_kinds() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.open("a", 1, WAITING, t0);
        episodes.open("b", 2, WAITING, t0);
        assert_eq!(
            sent(&mut episodes, t0, 1).len(),
            2,
            "two sessions, two sends"
        );
        deliver(&mut episodes, "a", WAITING, t0);
        deliver(&mut episodes, "b", WAITING, t0);

        // Une erreur de la session « a » à +5 s attend la fin du délai de « a » ; « b » n'est pas concernée.
        episodes.open("a", 1, ERROR, at(t0, 5));
        episodes.close("b", WAITING);
        episodes.open("b", 2, ERROR, at(t0, 5));
        let (now_sent, held) = plain_due(&mut episodes, at(t0, 5), 1);
        assert!(now_sent.is_empty());
        assert_eq!(
            held.len(),
            2,
            "both are inside their own cooldown: {held:?}"
        );
        assert_eq!(sent(&mut episodes, at(t0, 30), 1).len(), 2);
    }

    #[test]
    fn a_phone_reply_resets_the_cooldown() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.open("s", 4, WAITING, t0);
        sent(&mut episodes, t0, 1);
        deliver(&mut episodes, "s", WAITING, t0);

        // Romain répond depuis le téléphone ; l'agent repose une question 15 s plus tard : elle vibre tout de suite.
        episodes.phone_interaction("s");
        episodes.close("s", WAITING);
        episodes.open("s", 4, WAITING, at(t0, 15));
        assert_eq!(
            sent(&mut episodes, at(t0, 15), 1),
            vec![due("s", 4, WAITING)]
        );

        // Même chose quand la réponse arrive alors que la seconde attente est déjà retenue par le délai.
        deliver(&mut episodes, "s", WAITING, at(t0, 15));
        episodes.close("s", WAITING);
        episodes.open("s", 4, WAITING, at(t0, 20));
        assert!(sent(&mut episodes, at(t0, 20), 1).is_empty());
        episodes.phone_interaction("s");
        assert_eq!(
            sent(&mut episodes, at(t0, 21), 1),
            vec![due("s", 4, WAITING)]
        );
    }

    #[test]
    fn a_watched_pane_is_auto_acknowledged() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.watch("phone", "s", 4, t0);
        // Le pane regardé qui se met à attendre (ou à échouer) est acquitté aussitôt, et n'est pas notifié.
        assert_eq!(
            episodes.open("s", 4, WAITING, at(t0, 5)),
            OpenOutcome::AutoAck
        );
        assert_eq!(
            episodes.open("s", 4, ERROR, at(t0, 6)),
            OpenOutcome::AutoAck
        );
        assert!(sent(&mut episodes, at(t0, 6), 1).is_empty());
        // Un autre pane, ou une autre session, suit la règle normale.
        assert_eq!(
            episodes.open("s", 5, WAITING, at(t0, 6)),
            OpenOutcome::Pending
        );
        assert_eq!(
            episodes.open("t", 4, WAITING, at(t0, 6)),
            OpenOutcome::Pending
        );
        assert_eq!(sent(&mut episodes, at(t0, 6), 1).len(), 2);

        // Une déclaration qui arrive alors qu'une attente de ce pane est déjà ouverte la fait acquitter aussi.
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.open("s", 4, WAITING, t0);
        episodes.set_away(false);
        assert_eq!(
            episodes.watch("phone", "s", 4, at(t0, 1)),
            OpenOutcome::AutoAck
        );
        assert_eq!(
            episodes.watch("phone", "s", 9, at(t0, 2)),
            OpenOutcome::Pending
        );

        // Déclarer ne retient pas non plus une attente d'un autre pane déjà partie, et `unwatch` rend la règle normale.
        let mut episodes = Episodes::new(BOTH);
        episodes.watch("phone", "s", 4, t0);
        episodes.unwatch("phone");
        assert_eq!(
            episodes.open("s", 4, WAITING, at(t0, 1)),
            OpenOutcome::Pending
        );
    }

    #[test]
    fn a_pending_episode_on_a_watched_pane_waits_for_the_watch_to_fall() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.open("s", 4, WAITING, t0);
        episodes.watch("phone", "s", 4, at(t0, 1));
        // Regardé : ni envoyé ni compté comme retenu (l'acquittement est en route), et le réveil est à la chute.
        let (now_sent, held) = plain_due(&mut episodes, at(t0, 2), 1);
        assert!(now_sent.is_empty() && held.is_empty());
        assert_eq!(episodes.next_wakeup(at(t0, 2)), Some(at(t0, 31)));
        // Plus de déclaration pendant 30 s : la règle normale reprend.
        assert_eq!(
            sent(&mut episodes, at(t0, 31), 1),
            vec![due("s", 4, WAITING)]
        );
    }

    #[test]
    fn watch_expires_after_30s() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.watch("phone", "s", 4, t0);
        assert_eq!(
            episodes.open("s", 4, WAITING, at(t0, 29)),
            OpenOutcome::AutoAck
        );
        episodes.close("s", WAITING);
        // 30 s après la dernière déclaration, elle ne vaut plus.
        assert_eq!(
            episodes.open("s", 4, WAITING, at(t0, 30)),
            OpenOutcome::Pending
        );
        episodes.close("s", WAITING);
        // Une nouvelle déclaration repart de zéro.
        episodes.watch("phone", "s", 4, at(t0, 40));
        assert_eq!(
            episodes.open("s", 4, WAITING, at(t0, 69)),
            OpenOutcome::AutoAck
        );
    }

    #[test]
    fn disabled_kinds_never_send() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(Policy {
            notify_waiting: false,
            notify_error: true,
        });
        episodes.set_away(true);
        episodes.open("s", 4, WAITING, t0);
        let (now_sent, held) = plain_due(&mut episodes, t0, 1);
        assert!(now_sent.is_empty());
        assert_eq!(held, vec![(due("s", 4, WAITING), Held::Disabled)]);
        // L'erreur de la même session part (l'autre type n'a rien à y voir).
        episodes.open("s", 4, ERROR, t0);
        assert_eq!(sent(&mut episodes, at(t0, 1), 1), vec![due("s", 4, ERROR)]);
        assert!(sent(&mut episodes, at(t0, 100), 1).is_empty());

        // Le réglage suit la config à chaud : réactivé, l'épisode encore ouvert part.
        episodes.set_policy(BOTH);
        assert_eq!(
            sent(&mut episodes, at(t0, 101), 1),
            vec![due("s", 4, WAITING)]
        );

        let mut episodes = Episodes::new(Policy {
            notify_waiting: true,
            notify_error: false,
        });
        episodes.set_away(true);
        episodes.open("s", 4, ERROR, t0);
        let (now_sent, held) = plain_due(&mut episodes, t0, 1);
        assert!(now_sent.is_empty());
        assert_eq!(held, vec![(due("s", 4, ERROR), Held::Disabled)]);
    }

    #[test]
    fn no_subscriber_holds() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.open("s", 4, WAITING, t0);
        let (now_sent, held) = plain_due(&mut episodes, t0, 0);
        assert!(now_sent.is_empty());
        assert_eq!(held, vec![(due("s", 4, WAITING), Held::NoSubscriber)]);
        // Un appareil s'abonne : l'épisode, resté en attente, part.
        assert_eq!(
            sent(&mut episodes, at(t0, 5), 1),
            vec![due("s", 4, WAITING)]
        );
    }

    #[test]
    fn the_conditions_are_checked_in_the_documented_order() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(Policy {
            notify_waiting: false,
            notify_error: true,
        });
        // Type éteint, Romain présent, aucun appareil : la première raison de la liste est celle qui est dite.
        episodes.open("s", 4, WAITING, t0);
        assert_eq!(plain_due(&mut episodes, t0, 0).1[0].1, Held::Disabled);
        let mut episodes = Episodes::new(BOTH);
        episodes.open("s", 4, WAITING, t0);
        assert_eq!(plain_due(&mut episodes, t0, 0).1[0].1, Held::Present);
        episodes.set_away(true);
        assert_eq!(plain_due(&mut episodes, t0, 0).1[0].1, Held::NoSubscriber);
    }

    #[test]
    fn retry_gives_up_after_three() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.open("s", 4, WAITING, t0);

        // Premier envoi, puis trois renvois, chacun à la fin de l'anti-rafale ; le quatrième échec abandonne.
        assert_eq!(sent(&mut episodes, t0, 1).len(), 1);
        assert!(retry(&mut episodes, "s", WAITING, t0));
        for round in 1..=3u64 {
            let now = at(t0, 30 * round);
            assert!(sent(&mut episodes, at(t0, 30 * round - 1), 1).is_empty());
            assert_eq!(episodes.next_wakeup(at(t0, 30 * round - 1)), Some(now));
            assert_eq!(
                sent(&mut episodes, now, 1),
                vec![due("s", 4, WAITING)],
                "resend {round}"
            );
            let retried = retry(&mut episodes, "s", WAITING, now);
            assert_eq!(retried, round < 3, "after resend {round}");
        }
        for seconds in [120, 150, 600] {
            assert!(
                sent(&mut episodes, at(t0, seconds), 1).is_empty(),
                "{seconds}"
            );
        }
        assert_eq!(episodes.next_wakeup(at(t0, 121)), None);
    }

    #[test]
    fn a_closed_episode_is_not_retried() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.open("s", 4, WAITING, t0);
        let attempt = episodes.due(t0, 1).0.remove(0);
        assert!(episodes.retry_later(&attempt, t0));
        episodes.close("s", WAITING);
        assert!(sent(&mut episodes, at(t0, 30), 1).is_empty());
        // Un résultat qui arrive pour un épisode clos entre-temps ne ressuscite rien.
        episodes.delivered(&attempt, at(t0, 31));
        assert!(!episodes.retry_later(&attempt, at(t0, 31)));
        assert!(sent(&mut episodes, at(t0, 90), 1).is_empty());
    }

    #[test]
    fn an_abandoned_episode_is_not_sent_again() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.open("s", 4, WAITING, t0);
        sent(&mut episodes, t0, 1);
        give_up(&mut episodes, "s", WAITING);
        assert!(sent(&mut episodes, at(t0, 60), 1).is_empty());
        assert_eq!(episodes.next_wakeup(at(t0, 60)), None);
    }

    #[test]
    fn state_changes_open_and_close_episodes() {
        use ActivityState::{Error, Idle, Running, Waiting};
        use EpisodeChange::{Close, Open};
        // Le son aurait retenti (should_chime) : un épisode « attend », quel que soit le réglage du son.
        assert_eq!(
            episode_changes(Running, Waiting, true, true),
            vec![Open(EpisodeKind::Waiting)]
        );
        // Session vue ou non comptée : pas de son, donc pas d'épisode.
        assert!(episode_changes(Running, Waiting, false, true).is_empty());
        assert!(episode_changes(Running, Waiting, false, false).is_empty());
        // L'erreur compte pour toute session qui compte au badge, vue ou non.
        assert_eq!(
            episode_changes(Running, Error, false, true),
            vec![Open(EpisodeKind::Error)]
        );
        assert!(episode_changes(Running, Error, false, false).is_empty());
        // L'état qui quitte « attend » ou « erreur » clôt l'épisode.
        assert_eq!(
            episode_changes(Waiting, Idle, false, true),
            vec![Close(EpisodeKind::Waiting)]
        );
        assert_eq!(
            episode_changes(Error, Running, false, true),
            vec![Close(EpisodeKind::Error)]
        );
        // De l'un à l'autre : la clôture d'abord, puis l'ouverture.
        assert_eq!(
            episode_changes(Waiting, Error, false, true),
            vec![Close(EpisodeKind::Waiting), Open(EpisodeKind::Error)]
        );
        assert_eq!(
            episode_changes(Error, Waiting, true, true),
            vec![Close(EpisodeKind::Error), Open(EpisodeKind::Waiting)]
        );
        // Un état qui ne bouge pas, ou qui n'a rien d'urgent, ne change rien.
        assert!(episode_changes(Idle, Running, false, true).is_empty());
        assert!(episode_changes(Waiting, Waiting, false, true).is_empty());
    }

    #[test]
    fn the_episode_pane_is_the_most_urgent_of_the_session() {
        use ActivityState::{Error, Idle, Running, Waiting};
        let panes = [
            (1, true, Running),
            (2, false, Waiting),
            (3, false, Error),
            (4, false, Waiting),
        ];
        assert_eq!(urgent_pane(&panes, EpisodeKind::Waiting), Some(2));
        assert_eq!(urgent_pane(&panes, EpisodeKind::Error), Some(3));
        // L'actif d'abord quand il est dans l'état de l'épisode.
        let active = [(1, false, Waiting), (2, true, Waiting)];
        assert_eq!(urgent_pane(&active, EpisodeKind::Waiting), Some(2));
        // Aucun pane dans l'état voulu (l'événement a devancé le relevé) : le plus urgent, puis l'actif.
        let late = [(1, true, Idle), (2, false, Running)];
        assert_eq!(urgent_pane(&late, EpisodeKind::Waiting), Some(2));
        assert_eq!(urgent_pane(&[], EpisodeKind::Waiting), None);
    }

    /// Un envoi en vol pour l'épisode `s`/Waiting, puis cet épisode clos et rouvert (retenu par l'anti-rafale) : le
    /// résultat tardif de l'ancien envoi.
    fn reopened_behind_a_stale_send() -> (Episodes, Instant, Due) {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.open("s", 4, WAITING, t0);
        let stale = episodes.due(t0, 1).0.remove(0);
        episodes.close("s", WAITING);
        episodes.open("s", 4, WAITING, at(t0, 2));
        let (now_sent, held) = plain_due(&mut episodes, at(t0, 2), 1);
        assert!(now_sent.is_empty());
        assert_eq!(held, vec![(due("s", 4, WAITING), Held::Cooldown)]);
        (episodes, t0, stale)
    }

    #[test]
    fn a_late_delivery_does_not_close_the_reopened_episode() {
        let (mut episodes, t0, stale) = reopened_behind_a_stale_send();
        episodes.delivered(&stale, at(t0, 3));
        assert_eq!(
            sent(&mut episodes, at(t0, 30), 1),
            vec![due("s", 4, WAITING)],
            "the second wait is not silently dropped"
        );
    }

    #[test]
    fn a_late_abandon_does_not_drop_the_reopened_episode() {
        let (mut episodes, t0, stale) = reopened_behind_a_stale_send();
        episodes.abandon(&stale);
        assert_eq!(
            sent(&mut episodes, at(t0, 30), 1),
            vec![due("s", 4, WAITING)]
        );
    }

    #[test]
    fn a_late_retry_does_not_consume_the_retries_of_the_reopened_episode() {
        let (mut episodes, t0, stale) = reopened_behind_a_stale_send();
        for round in 0..=u64::from(MAX_RETRIES) + 2 {
            assert!(
                !episodes.retry_later(&stale, at(t0, 3 + round)),
                "a stale failure is nobody's retry"
            );
        }
        // Les renvois du nouvel épisode sont intacts : trois renvois possibles, comme au premier jour.
        let mut now = at(t0, 30);
        let mut fresh = episodes.due(now, 1).0;
        assert_eq!(fresh.len(), 1);
        for round in 1..=3 {
            assert!(episodes.retry_later(&fresh[0], now), "retry {round}");
            now = at(now, 30);
            fresh = episodes.due(now, 1).0;
            assert_eq!(fresh.len(), 1, "resend {round}");
        }
        assert!(
            !episodes.retry_later(&fresh[0], now),
            "the fourth failure gives up"
        );
    }

    #[test]
    fn a_send_that_reached_nobody_keeps_the_episode_and_its_cooldown() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        // Une notification partie à t0 pour une autre attente : l'anti-rafale court jusqu'à +30 s.
        episodes.open("s", 4, ERROR, t0);
        let first = episodes.due(t0, 1).0.remove(0);
        episodes.delivered(&first, t0);
        episodes.close("s", ERROR);
        episodes.open("s", 4, WAITING, at(t0, 31));
        let attempt = episodes.due(at(t0, 31), 1).0.remove(0);
        // Plus aucun appareil au moment d'envoyer : l'épisode reste en attente, retenu faute d'appareil.
        episodes.release(&attempt);
        let (now_sent, held) = plain_due(&mut episodes, at(t0, 32), 0);
        assert!(now_sent.is_empty());
        assert_eq!(held, vec![(due("s", 4, WAITING), Held::NoSubscriber)]);
        // Un appareil s'abonne : il part, sans renvoi consommé.
        assert_eq!(
            sent(&mut episodes, at(t0, 33), 1),
            vec![due("s", 4, WAITING)]
        );
    }

    #[test]
    fn a_device_clears_only_its_own_watch() {
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.watch("phone", "s", 4, t0);
        episodes.watch("tablet", "s", 4, at(t0, 5));
        // Le téléphone ne regarde plus rien : la tablette regarde toujours ce pane.
        episodes.unwatch("phone");
        assert_eq!(
            episodes.open("s", 4, WAITING, at(t0, 6)),
            OpenOutcome::AutoAck
        );
        episodes.unwatch("tablet");
        assert_eq!(
            episodes.open("s", 4, WAITING, at(t0, 7)),
            OpenOutcome::Pending
        );
        // Chaque déclaration tombe à son heure : la tablette tient jusqu'à +35 s, même après celle du téléphone.
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        episodes.watch("phone", "s", 4, t0);
        episodes.watch("tablet", "s", 4, at(t0, 10));
        episodes.close("s", WAITING);
        assert_eq!(
            episodes.open("s", 4, WAITING, at(t0, 31)),
            OpenOutcome::AutoAck,
            "the tablet declared at +10 s"
        );
        assert_eq!(
            episodes.open("s", 4, WAITING, at(t0, 40)),
            OpenOutcome::Pending
        );
    }

    /// Un épisode « attend » ouvert par un pane caché alors que l'état agrégé est « erreur » ne se clôt pas avec l'état de
    /// la session : c'est la fin de l'attente de tout pane (`no_pane_waiting`) qui le clôt. Resté ouvert, il avalerait
    /// l'attente suivante.
    #[test]
    fn a_hidden_pane_episode_closed_by_its_pane_does_not_swallow_the_next_wait() {
        use ActivityState::{Error, Idle, Running, Waiting};
        let t0 = Instant::now();
        let mut episodes = Episodes::new(BOTH);
        episodes.set_away(true);
        // La session est en erreur ; un de ses panes cachés attend.
        episodes.open("s", 4, WAITING, t0);
        let hidden = episodes.due(t0, 1).0.remove(0);
        episodes.delivered(&hidden, t0);
        let panes = [(4, false, Waiting), (5, true, Error)];
        assert!(!no_pane_waiting(&panes));
        // Le pane caché cesse d'attendre, l'erreur reste : plus rien n'attend.
        let panes = [(4, false, Idle), (5, true, Error)];
        assert!(no_pane_waiting(&panes));
        episodes.close("s", WAITING);
        assert!(no_pane_waiting(&[(1, true, Running)]) && no_pane_waiting(&[]));
        // Plus tard, une vraie attente : un nouvel épisode, qui part.
        episodes.open("s", 5, WAITING, at(t0, 60));
        assert_eq!(
            sent(&mut episodes, at(t0, 60), 1),
            vec![due("s", 5, WAITING)]
        );
        // Sans cette clôture, `open` aurait gardé l'épisode terminé et rien ne serait parti.
        let mut swallowed = Episodes::new(BOTH);
        swallowed.set_away(true);
        swallowed.open("s", 4, WAITING, t0);
        let hidden = swallowed.due(t0, 1).0.remove(0);
        swallowed.delivered(&hidden, t0);
        swallowed.open("s", 5, WAITING, at(t0, 60));
        assert!(sent(&mut swallowed, at(t0, 60), 1).is_empty());
    }
}
