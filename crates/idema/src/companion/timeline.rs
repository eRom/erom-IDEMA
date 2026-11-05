//! La chronologie : les 500 dernières transitions qui comptent, gardées en mémoire (perdues au redémarrage d'IdemA).
//! Comptent : l'entrée et la sortie d'« attend » et d'« erreur », et la fin d'une période « en cours » d'au moins 60 s
//! (« a fini »). Les autres allers-retours ne sont pas gardés.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, SystemTime};

use idema_term::activity::ActivityState;
use serde::Serialize;

use super::fleet::{state_label, unix_millis};

/// Le nombre de transitions gardées ; la plus ancienne part la première.
const CAPACITY: usize = 500;

/// Une période « en cours » plus courte que cela n'est pas une fin qui compte.
const MIN_RUN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineEvent {
    /// L'instant de la transition, en millisecondes depuis l'époque Unix.
    pub at: u64,
    pub session: String,
    pub from: &'static str,
    pub to: &'static str,
    /// La fin d'une période « en cours » d'au moins 60 s.
    pub finished: bool,
}

pub struct Timeline {
    started_at: SystemTime,
    events: VecDeque<TimelineEvent>,
    /// Depuis quand chaque session tourne, pour mesurer la période quand elle finit.
    running_since: HashMap<String, SystemTime>,
}

impl Timeline {
    pub fn new(started_at: SystemTime) -> Self {
        Self {
            started_at,
            events: VecDeque::with_capacity(CAPACITY),
            running_since: HashMap::new(),
        }
    }

    /// Note une transition d'état de `session`, ou l'ignore si elle ne compte pas.
    pub fn record(
        &mut self,
        session: &str,
        from: ActivityState,
        to: ActivityState,
        at: SystemTime,
    ) {
        use ActivityState::{Error, Idle, Running, Waiting};
        if from == to {
            return;
        }
        // La période « en cours » qui s'achève : la mesurer ici, avant de relancer l'horloge d'une éventuelle reprise.
        let mut finished = false;
        if from == Running
            && let Some(since) = self.running_since.remove(session)
        {
            finished = to == Idle && at.duration_since(since).is_ok_and(|run| run >= MIN_RUN);
        }
        if to == Running {
            self.running_since.insert(session.to_owned(), at);
        }
        let waiting_or_error = [from, to]
            .iter()
            .any(|state| matches!(state, Waiting | Error));
        if waiting_or_error || finished {
            self.push(session, from, to, at, finished);
        }
    }

    /// Une session cesse d'avoir un état à suivre : sa vue est remplacée ou perdue (retour au repos), ou la session est
    /// fermée. Ce n'est jamais la fin d'une période « en cours » (rien n'a fini). Une attente ou une erreur ouverte est
    /// close (vers le repos), sinon elle resterait ouverte pour toujours ; l'horloge « en cours » est oubliée.
    pub fn reset(&mut self, session: &str, from: ActivityState, at: SystemTime) {
        self.running_since.remove(session);
        if matches!(from, ActivityState::Waiting | ActivityState::Error) {
            self.push(session, from, ActivityState::Idle, at, false);
        }
    }

    fn push(
        &mut self,
        session: &str,
        from: ActivityState,
        to: ActivityState,
        at: SystemTime,
        finished: bool,
    ) {
        if self.events.len() == CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(TimelineEvent {
            at: unix_millis(at),
            session: session.to_owned(),
            from: state_label(from),
            to: state_label(to),
            finished,
        });
    }

    /// Les transitions gardées dont l'instant est au moins `ms`, dans l'ordre chronologique.
    pub fn since(&self, ms: u64) -> Vec<TimelineEvent> {
        let mut kept: Vec<_> = self
            .events
            .iter()
            .filter(|event| event.at >= ms)
            .cloned()
            .collect();
        // Tri stable : deux transitions du même instant gardent leur ordre d'arrivée.
        kept.sort_by_key(|event| event.at);
        kept
    }

    /// L'instant de démarrage d'IdemA, en millisecondes depuis l'époque Unix.
    pub fn started_at(&self) -> u64 {
        unix_millis(self.started_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ActivityState::{Error, Idle, Running, Waiting};
    use std::time::UNIX_EPOCH;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000 + seconds)
    }

    fn millis(seconds: u64) -> u64 {
        1_800_000_000_000 + seconds * 1000
    }

    fn timeline() -> Timeline {
        Timeline::new(at(0))
    }

    #[test]
    fn timeline_keeps_only_what_counts() {
        let mut timeline = timeline();
        // Un va-et-vient de 20 s : ni l'entrée ni la fin ne comptent.
        timeline.record("a", Idle, Running, at(10));
        timeline.record("a", Running, Idle, at(30));
        assert!(timeline.since(0).is_empty());

        // L'attente : entrée et sortie, gardées.
        timeline.record("a", Idle, Running, at(100));
        timeline.record("a", Running, Waiting, at(110));
        timeline.record("a", Waiting, Running, at(300));
        // La fin d'une période de 61 s : gardée, avec `finished`.
        timeline.record("a", Running, Idle, at(361));
        // L'erreur, entrée et sortie.
        timeline.record("b", Running, Error, at(400));
        timeline.record("b", Error, Idle, at(410));

        let kept: Vec<_> = timeline
            .since(0)
            .into_iter()
            .map(|event| (event.session, event.from, event.to, event.finished))
            .collect();
        assert_eq!(
            kept,
            [
                ("a".into(), "running", "waiting", false),
                ("a".into(), "waiting", "running", false),
                ("a".into(), "running", "idle", true),
                ("b".into(), "running", "error", false),
                ("b".into(), "error", "idle", false),
            ]
        );
    }

    #[test]
    fn a_run_is_finished_from_sixty_seconds() {
        let mut timeline = timeline();
        timeline.record("a", Idle, Running, at(0));
        timeline.record("a", Running, Idle, at(59));
        assert!(timeline.since(0).is_empty(), "59 s is too short");
        timeline.record("a", Idle, Running, at(100));
        timeline.record("a", Running, Idle, at(160));
        let events = timeline.since(0);
        assert_eq!(events.len(), 1, "exactly 60 s counts");
        assert!(events[0].finished);
        assert_eq!(events[0].at, millis(160));
    }

    #[test]
    fn a_run_interrupted_by_a_wait_restarts_its_clock() {
        let mut timeline = timeline();
        timeline.record("a", Idle, Running, at(0));
        timeline.record("a", Running, Waiting, at(30));
        timeline.record("a", Waiting, Running, at(100));
        // 20 s depuis la reprise : pas une période de 60 s, même si la session tourne depuis 120 s en tout.
        timeline.record("a", Running, Idle, at(120));
        let finished = timeline.since(0).into_iter().filter(|e| e.finished).count();
        assert_eq!(finished, 0);
    }

    #[test]
    fn a_run_with_an_unknown_start_is_not_a_finish() {
        let mut timeline = timeline();
        // IdemA a démarré pendant que la session tournait : on ne sait pas depuis quand.
        timeline.record("a", Running, Idle, at(500));
        assert!(timeline.since(0).is_empty());
    }

    #[test]
    fn a_reset_closes_an_open_wait_but_is_never_a_finished_run() {
        let mut timeline = timeline();
        timeline.record("a", Idle, Running, at(0));
        timeline.record("a", Running, Waiting, at(100));
        // La vue de la session est remplacée pendant l'attente : l'attente est close, sans « a fini ».
        timeline.reset("a", Waiting, at(200));
        // Une période « en cours » de 5 minutes interrompue de même ne compte pas comme une fin.
        timeline.record("b", Idle, Running, at(0));
        timeline.reset("b", Running, at(300));
        let kept: Vec<_> = timeline
            .since(0)
            .into_iter()
            .map(|event| (event.session, event.from, event.to, event.finished))
            .collect();
        assert_eq!(
            kept,
            [
                ("a".into(), "running", "waiting", false),
                ("a".into(), "waiting", "idle", false),
            ]
        );
    }

    #[test]
    fn a_reset_forgets_the_running_clock() {
        let mut timeline = timeline();
        timeline.record("a", Idle, Running, at(0));
        timeline.reset("a", Running, at(10));
        // La vue suivante part de zéro : sans nouveau départ « en cours », une sortie 500 s plus tard n'est pas une fin.
        timeline.record("a", Running, Idle, at(500));
        assert!(timeline.since(0).is_empty());
        assert!(
            timeline.running_since.is_empty(),
            "nothing is kept for the session"
        );
    }

    #[test]
    fn timeline_holds_at_most_500() {
        let mut timeline = timeline();
        for round in 0..600u64 {
            timeline.record("a", Running, Waiting, at(round * 2));
            timeline.record("a", Waiting, Running, at(round * 2 + 1));
        }
        let kept = timeline.since(0);
        assert_eq!(kept.len(), 500);
        // Les plus anciennes sont parties : la première gardée est la 701e des 1200.
        assert_eq!(kept[0].at, millis(700));
        assert_eq!(kept.last().unwrap().at, millis(1199));
    }

    #[test]
    fn timeline_since_filters_and_orders() {
        let mut timeline = timeline();
        // Arrivées dans le désordre (deux sessions, deux horloges de remontée) : la sortie est chronologique.
        timeline.record("b", Running, Waiting, at(300));
        timeline.record("a", Running, Waiting, at(100));
        timeline.record("a", Waiting, Idle, at(200));
        let all: Vec<_> = timeline.since(0).into_iter().map(|e| e.at).collect();
        assert_eq!(all, [millis(100), millis(200), millis(300)]);
        let later: Vec<_> = timeline
            .since(millis(200))
            .into_iter()
            .map(|e| e.at)
            .collect();
        assert_eq!(later, [millis(200), millis(300)], "inclusive lower bound");
        assert!(timeline.since(millis(301)).is_empty());
    }

    #[test]
    fn the_start_of_idema_is_reported() {
        assert_eq!(timeline().started_at(), millis(0));
    }

    #[test]
    fn events_serialize_with_their_field_names() {
        let mut timeline = timeline();
        timeline.record("a", Running, Waiting, at(5));
        let json = serde_json::to_value(timeline.since(0)).unwrap();
        assert_eq!(
            json[0],
            serde_json::json!({
                "at": millis(5), "session": "a", "from": "running", "to": "waiting", "finished": false
            })
        );
    }
}
