//! État d'un pane d'après sa sortie et ses signaux (spec §5.2), et envoi de la commande d'un agent
//! dans un pane neuf (spec §4.2). L'horloge est injectée : tout se teste sans attendre.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::signals::Signal;

/// Un pane qui a écrit depuis moins longtemps est « en cours ».
pub const RUNNING_WINDOW: Duration = Duration::from_secs(2);

/// Silence qui, après une période « en cours », marque la fin d'un tour d'agent.
pub const TURN_SILENCE: Duration = Duration::from_secs(3);

/// Volume minimal d'une rafale pour compter comme activité. Claude Code au repos envoie environ
/// 71 octets par minute (mesure R2 du 14/10) : sans ce seuil, sa pastille clignoterait en « en cours »
/// et chaque envoi finirait en faux « attend ».
pub const ACTIVITY_MIN_BYTES: usize = 256;

/// Après l'attache, la sortie ne compte pas comme activité : un programme qui se redessine parce que son
/// pane change de taille n'a pas commencé de tour.
pub const ATTACH_GRACE: Duration = Duration::from_secs(3);

/// Après une saisie de l'utilisateur, la sortie qui arrive dans cette fenêtre est l'écho de la frappe ou du
/// collage (la boîte de saisie d'un agent se redessine), pas un tour d'agent : l'écho suit la saisie avec un
/// décalage (tmux, agent), et une saisie ou un collage rapide l'accumule derrière la dernière touche.
pub const ECHO_WINDOW: Duration = Duration::from_millis(500);

/// La sortie arrivée à `arrival` est-elle l'écho de la dernière saisie, `last_input` ?
pub fn is_input_echo(last_input: Option<Instant>, arrival: Instant) -> bool {
    last_input.is_some_and(|input| arrival < input + ECHO_WINDOW)
}

/// État d'un pane, d'une session ou d'un projet replié. L'ordre est celui de l'urgence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ActivityState {
    #[default]
    Idle,
    Running,
    Waiting,
    Error,
}

/// Le plus urgent d'un ensemble d'états (spec §5.3) ; repos s'il est vide.
pub fn most_urgent(states: impl IntoIterator<Item = ActivityState>) -> ActivityState {
    states.into_iter().max().unwrap_or_default()
}

/// Le son part à chaque passage en « attend » d'une session non vue (spec §5.4).
pub fn should_chime(previous: ActivityState, next: ActivityState, seen: bool) -> bool {
    !seen && next == ActivityState::Waiting && previous != ActivityState::Waiting
}

#[derive(Debug, Default)]
pub struct PaneActivity {
    /// Octets de la rafale en cours : remis à zéro après `RUNNING_WINDOW` de silence.
    burst_bytes: usize,
    last_output: Option<Instant>,
    progress: bool,
    command_running: bool,
    at_prompt: bool,
    /// Un marqueur OSC 133 est arrivé depuis l'attache : il fait foi sur le prompt et la commande.
    marks_seen: bool,
    /// Repli sans marqueur (agent lancé avant l'attache, shell non intégré) : `pane_current_command`
    /// vaut-il le shell ? Inconnu tant que l'abonnement n'a pas répondu.
    foreground_is_shell: Option<bool>,
    /// Fin de la période qui suit l'attache, où la sortie n'arme aucun tour.
    grace_until: Option<Instant>,
    /// Fin de la fenêtre d'écho d'un rapport de focus : la sortie qui y arrive est peut-être un redessin.
    focus_until: Option<Instant>,
    /// Le pane a été « en cours » depuis le dernier tour constaté.
    turn_active: bool,
    waiting: bool,
    error: bool,
    /// Le tour de l'agent s'est fini, ou il a sonné, pendant que le pane était vu, et rien n'a été saisi dans ce pane
    /// depuis : la question attend une réponse que personne n'a donnée. `rearm_unanswered` la fait passer en « attend »
    /// quand le pane cesse d'être vu parce que Romain s'éloigne (AC-25).
    unanswered_while_seen: bool,
}

impl PaneActivity {
    pub fn new() -> Self {
        Self::default()
    }

    /// Un pane qu'on vient d'attacher : sa sortie des `ATTACH_GRACE` qui suivent ne compte pas.
    pub fn attached(now: Instant) -> Self {
        Self {
            grace_until: Some(now + ATTACH_GRACE),
            ..Self::default()
        }
    }

    /// Idema vient de rapporter un changement de focus au programme (mode 1004) : il peut se redessiner, et ce
    /// redessin n'est pas un tour. Pendant `ECHO_WINDOW`, la sortie n'arme aucun tour ; elle n'en désarme jamais un
    /// non plus (contrairement à `on_input`) : un agent qui écrit au moment où l'utilisateur quitte la fenêtre
    /// doit encore atteindre « attend ».
    pub fn on_focus_report(&mut self, now: Instant) {
        self.focus_until = Some(now + ECHO_WINDOW);
    }

    /// Sortie du pane (octets reçus, même ceux qu'une relecture jette à l'écran).
    pub fn on_output(&mut self, len: usize, now: Instant) {
        let redraw_window = self.focus_until.is_some_and(|until| now < until);
        if redraw_window && !self.turn_active {
            // Peut-être un redessin au focus : compté pour le silence, pas pour la rafale.
            self.last_output = Some(now);
            return;
        }
        if self.grace_until.is_some_and(|until| now < until) {
            return;
        }
        if self
            .last_output
            .is_none_or(|last| now.saturating_duration_since(last) >= RUNNING_WINDOW)
        {
            self.burst_bytes = 0;
        }
        self.burst_bytes = self.burst_bytes.saturating_add(len);
        self.last_output = Some(now);
        if self.burst_bytes >= ACTIVITY_MIN_BYTES {
            self.turn_active = true;
        }
    }

    /// Le pane vient de changer de taille : le programme se redessine seul, ce n'est pas un tour. Repose la même
    /// grâce que l'attache, remet la rafale à zéro et désarme le tour (I-2, redessin au changement de taille).
    pub fn on_resize(&mut self, now: Instant) {
        self.restart_grace(now);
    }

    fn restart_grace(&mut self, now: Instant) {
        self.grace_until = Some(now + ATTACH_GRACE);
        self.burst_bytes = 0;
        self.turn_active = false;
    }

    /// Idema vient de taper la commande d'un agent : le rendu de démarrage (animation, écran d'accueil) dure
    /// plus ou moins longtemps selon l'agent et n'est pas un tour. Repose la grâce à partir de ce moment, remet
    /// la rafale à zéro et désarme le tour, comme `on_resize`. Ce n'est pas une saisie de l'utilisateur : pas de
    /// fenêtre d'écho (`on_input` n'est pas appelé).
    pub fn on_launch(&mut self, now: Instant) {
        self.restart_grace(now);
    }

    /// L'utilisateur a saisi quelque chose pour ce pane (touches, collage, texte validé de l'IME) : l'écho de la
    /// frappe n'est pas un tour d'agent. Remet la rafale à zéro et désarme le tour (I-2, écho dans la boîte de saisie).
    pub fn on_input(&mut self) {
        self.burst_bytes = 0;
        self.turn_active = false;
        // La saisie répond à la question : rien à réarmer au départ de Romain.
        self.unanswered_while_seen = false;
    }

    /// Un signal du détecteur. Tant que la session est vue, aucun drapeau ne se pose.
    pub fn on_signal(&mut self, signal: Signal, seen: bool) {
        match signal {
            Signal::Notify => {
                self.waiting |= !seen;
                self.unanswered_while_seen |= seen;
            }
            Signal::Progress(active) => {
                self.progress = active;
                self.turn_active |= active;
            }
            Signal::PromptStart | Signal::PromptEnd => {
                self.marks_seen = true;
                self.at_prompt = true;
                self.command_running = false;
                // Une progression que le programme n'a pas refermée ne survit pas à son retour au prompt.
                self.progress = false;
            }
            Signal::CommandStart => {
                self.marks_seen = true;
                self.at_prompt = false;
                self.command_running = true;
                // Une rafale d'avant la commande ne doit pas armer le tour de la commande qui démarre.
                self.turn_active = false;
                // Une commande qui démarre n'est pas une question en suspens.
                self.unanswered_while_seen = false;
            }
            Signal::CommandEnd(code) => {
                self.marks_seen = true;
                self.command_running = false;
                self.progress = false;
                self.turn_active = false;
                // 130 : interruption par Ctrl-C, qui n'est pas une erreur.
                let failed = code.is_some_and(|code| code != 0 && code != 130);
                self.error |= failed && !seen;
            }
        }
    }

    /// Valeur de l'abonnement `pane_current_command` : le shell est-il au premier plan ?
    pub fn set_foreground_is_shell(&mut self, is_shell: bool) {
        self.foreground_is_shell = Some(is_shell);
    }

    /// Pas régulier : constate la fin d'un tour. `bracketed_paste` est le mode 2004 du programme.
    pub fn tick(&mut self, now: Instant, bracketed_paste: bool, seen: bool) {
        let silent = self
            .last_output
            .is_some_and(|last| now.saturating_duration_since(last) >= TURN_SILENCE);
        if self.turn_active && self.command_running() && bracketed_paste && !self.progress && silent
        {
            self.turn_active = false;
            self.waiting |= !seen;
            self.unanswered_while_seen |= seen;
        }
    }

    pub fn state(&self, now: Instant) -> ActivityState {
        if self.error {
            ActivityState::Error
        } else if self.waiting {
            ActivityState::Waiting
        } else if self.progress || self.recently_active(now) {
            ActivityState::Running
        } else {
            ActivityState::Idle
        }
    }

    /// La session est vue : les drapeaux « erreur » et « attend » retombent.
    pub fn mark_seen(&mut self) {
        self.waiting = false;
        self.error = false;
    }

    /// Romain s'éloigne (le compagnon passe en absent) et le pane cesse d'être vu : si l'agent a fini son tour ou sonné
    /// pendant qu'il était vu, sans saisie depuis, la question se réarme en « attend » pour qu'elle parte en
    /// notification. Rend vrai si l'état « attend » vient d'être posé. À appeler avant de publier l'état du pane devenu
    /// non vu : ce n'est pas elle qui publie.
    pub fn rearm_unanswered(&mut self) -> bool {
        let rearm = std::mem::take(&mut self.unanswered_while_seen);
        let changed = rearm && !self.waiting;
        self.waiting |= rearm;
        changed
    }

    /// Dernier marqueur reçu : 133;A ou 133;B. Sans marqueur depuis l'attache, le shell au premier plan.
    pub fn at_prompt(&self) -> bool {
        if self.marks_seen {
            self.at_prompt
        } else {
            self.foreground_is_shell == Some(true)
        }
    }

    /// Entre 133;C et 133;D. Sans marqueur depuis l'attache, un autre programme que le shell au premier plan.
    pub fn command_running(&self) -> bool {
        if self.marks_seen {
            self.command_running
        } else {
            self.foreground_is_shell == Some(false)
        }
    }

    fn recently_active(&self, now: Instant) -> bool {
        self.burst_bytes >= ACTIVITY_MIN_BYTES
            && self
                .last_output
                .is_some_and(|last| now.saturating_duration_since(last) < RUNNING_WINDOW)
    }
}

/// Attente de l'OSC 133;B (ZLE a la ligne) avant d'accepter l'OSC 133;A seul, et avant le repli sur le calme.
pub const PROMPT_WAIT: Duration = Duration::from_secs(5);

/// Repli sans OSC 133;A : la sortie du pane doit être stable depuis ce délai.
pub const QUIET_BEFORE_SEND: Duration = Duration::from_millis(500);

/// Au-delà, rien n'est envoyé et le pane reste au shell.
pub const LAUNCH_GIVE_UP: Duration = Duration::from_secs(30);

/// Ce que l'interface doit faire de la commande en attente.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchStep {
    Wait,
    Send,
    GiveUp,
}

/// Envoi de la commande d'un agent dans un pane neuf, jamais à l'aveugle (spec §4.2, étape 4).
#[derive(Debug)]
pub struct LaunchGate {
    /// Le début de l'attente : la création de la porte, ou la fin du délai d'une restauration (`new` avec un instant
    /// futur). Avant lui, rien ne part, même un prompt déjà vu ; l'abandon compte depuis lui.
    started: Instant,
    /// OSC 133;A vu : le prompt se prépare, mais ZLE n'a pas forcément encore la main sur le tty.
    prompt_started: bool,
    /// OSC 133;B vu : ZLE a la ligne, la saisie ne sera pas écho deux fois.
    prompt_ended: bool,
    last_output: Option<Instant>,
}

impl LaunchGate {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            prompt_started: false,
            prompt_ended: false,
            last_output: None,
        }
    }

    pub fn on_signal(&mut self, signal: Signal) {
        match signal {
            Signal::PromptStart => self.prompt_started = true,
            Signal::PromptEnd => self.prompt_ended = true,
            _ => {}
        }
    }

    pub fn on_output(&mut self, now: Instant) {
        self.last_output = Some(now);
    }

    /// `shell_in_foreground` : `pane_current_command` vaut le shell, s'il est déjà connu.
    pub fn poll(&self, now: Instant, shell_in_foreground: Option<bool>) -> LaunchStep {
        // Le lancement n'est pas encore dû : les lancements d'une restauration s'étalent.
        if now < self.started {
            return LaunchStep::Wait;
        }
        let elapsed = now.saturating_duration_since(self.started);
        // L'abandon passe avant tout envoi : un prompt vu après le délai ne ressuscite pas la commande.
        if elapsed >= LAUNCH_GIVE_UP {
            return LaunchStep::GiveUp;
        }
        if self.prompt_ended {
            return LaunchStep::Send;
        }
        if elapsed < PROMPT_WAIT {
            return LaunchStep::Wait;
        }
        // Repli d'un shell dont le crochet de ligne a été remplacé : A seul suffit après l'attente.
        if self.prompt_started {
            return LaunchStep::Send;
        }
        let quiet_since = self.last_output.unwrap_or(self.started);
        let quiet = now.saturating_duration_since(quiet_since) >= QUIET_BEFORE_SEND;
        if quiet && shell_in_foreground == Some(true) {
            LaunchStep::Send
        } else {
            LaunchStep::Wait
        }
    }
}

/// Le rythme commun des lancements d'une restauration : l'instant avant lequel aucun d'eux ne part. Les portes de
/// plusieurs panes peuvent être prêtes au même tick ; ce rythme les sert une à une.
#[derive(Debug, Clone)]
pub struct LaunchPacer(Rc<Cell<Instant>>);

impl LaunchPacer {
    /// Un rythme dont le premier envoi est permis dès `now`.
    pub fn new(now: Instant) -> Self {
        Self(Rc::new(Cell::new(now)))
    }
}

/// Ce que la porte d'un pane a le droit de faire à `now` : un `Send` n'est accordé que si le rythme commun le permet,
/// et il repousse alors le suivant de `spacing`. Sans rythme (une session neuve), la porte décide seule. L'attente et
/// l'abandon passent tels quels : l'abandon ne consomme rien.
pub fn paced_step(
    step: LaunchStep,
    pacer: Option<&LaunchPacer>,
    now: Instant,
    spacing: Duration,
) -> LaunchStep {
    let (LaunchStep::Send, Some(LaunchPacer(next))) = (step, pacer) else {
        return step;
    };
    if now < next.get() {
        return LaunchStep::Wait;
    }
    next.set(now + spacing);
    LaunchStep::Send
}

/// Octet que le terminal envoie pour Ctrl-C.
const CTRL_C: u8 = 0x03;

/// Ce que devient une saisie reçue pendant qu'un lancement d'agent attend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeldStep {
    /// La saisie est retenue : elle partira derrière la commande de l'agent.
    Held,
    /// Ctrl-C : le lancement est abandonné, ces octets (la saisie retenue puis ce Ctrl-C) vont au pane tels quels.
    Abandon(Vec<u8>),
}

/// La saisie de l'utilisateur retenue entre `⌘T` et l'envoi de la commande de l'agent (m1) : sans elle, elle
/// partirait au shell et la commande de l'agent s'ajouterait derrière (« fix the bugcc⏎ »).
#[derive(Debug, Default)]
pub struct HeldInput {
    bytes: Vec<u8>,
}

impl HeldInput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retient `input` en gardant l'ordre ; un Ctrl-C seul abandonne le lancement.
    pub fn push(&mut self, input: &[u8]) -> HeldStep {
        if input == [CTRL_C] {
            let mut all = std::mem::take(&mut self.bytes);
            all.extend_from_slice(input);
            return HeldStep::Abandon(all);
        }
        self.bytes.extend_from_slice(input);
        HeldStep::Held
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Rend la saisie retenue et vide la file : à `Send` elle part derrière la commande, à `GiveUp` telle quelle.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Une rafale d'agent : 50 octets toutes les 100 ms pendant `duration`.
    fn burst(activity: &mut PaneActivity, from: Instant, duration: Duration) -> Instant {
        let mut at = from;
        while at < from + duration {
            activity.on_output(50, at);
            at += ms(100);
        }
        at
    }

    #[test]
    fn urgency_orders_error_waiting_running_idle() {
        use ActivityState::*;
        assert_eq!(most_urgent([Idle, Running, Waiting]), Waiting);
        assert_eq!(most_urgent([Waiting, Error, Running]), Error);
        assert_eq!(most_urgent([]), Idle);
    }

    #[test]
    fn a_bell_while_unseen_waits_until_seen() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::Notify, false);
        assert_eq!(activity.state(t0), ActivityState::Waiting);
        activity.mark_seen();
        assert_eq!(activity.state(t0), ActivityState::Idle);
    }

    #[test]
    fn a_bell_while_seen_is_rearmed_when_the_user_leaves() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::Notify, true);
        assert_eq!(activity.state(t0), ActivityState::Idle);
        assert!(activity.rearm_unanswered());
        assert_eq!(activity.state(t0), ActivityState::Waiting);
        // Une seule fois : la mémoire est consommée, et revoir le pane fait retomber l'attente comme d'habitude.
        activity.mark_seen();
        assert!(!activity.rearm_unanswered());
        assert_eq!(activity.state(t0), ActivityState::Idle);
    }

    #[test]
    fn a_turn_that_ended_while_seen_is_rearmed_when_the_user_leaves() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, true);
        let end = burst(&mut activity, t0, ms(2000));
        activity.tick(end + ms(3100), true, true);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Idle);
        assert!(activity.rearm_unanswered());
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Waiting);
    }

    #[test]
    fn an_input_since_the_question_cancels_the_rearming() {
        let t0 = Instant::now();
        let mut bell = PaneActivity::new();
        bell.on_signal(Signal::Notify, true);
        bell.on_input();
        assert!(!bell.rearm_unanswered());
        assert_eq!(bell.state(t0), ActivityState::Idle);

        let mut turn = PaneActivity::new();
        turn.on_signal(Signal::CommandStart, true);
        let end = burst(&mut turn, t0, ms(2000));
        turn.tick(end + ms(3100), true, true);
        turn.on_input();
        assert!(!turn.rearm_unanswered());
        assert_eq!(turn.state(end + ms(3100)), ActivityState::Idle);
    }

    #[test]
    fn a_question_asked_after_the_last_input_is_still_rearmed() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_input();
        activity.on_signal(Signal::Notify, true);
        assert!(activity.rearm_unanswered());
        assert_eq!(activity.state(t0), ActivityState::Waiting);
    }

    #[test]
    fn nothing_to_rearm_without_a_question_or_when_already_waiting() {
        let t0 = Instant::now();
        let mut idle = PaneActivity::new();
        assert!(!idle.rearm_unanswered());
        assert_eq!(idle.state(t0), ActivityState::Idle);
        // Sonné hors vue : déjà « attend », rien ne change et rien n'est mémorisé.
        let mut waiting = PaneActivity::new();
        waiting.on_signal(Signal::Notify, false);
        assert!(!waiting.rearm_unanswered());
        waiting.mark_seen();
        assert_eq!(waiting.state(t0), ActivityState::Idle);
    }

    #[test]
    fn a_new_command_forgets_the_question_of_the_previous_one() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::Notify, true);
        activity.on_signal(Signal::CommandStart, true);
        assert!(!activity.rearm_unanswered());
        assert_eq!(activity.state(t0), ActivityState::Idle);
    }

    #[test]
    fn nothing_flags_a_seen_session() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::Notify, true);
        activity.on_signal(Signal::CommandEnd(Some(3)), true);
        assert_eq!(activity.state(t0), ActivityState::Idle);
    }

    #[test]
    fn a_failed_command_is_an_error_but_ctrl_c_and_success_are_not() {
        let t0 = Instant::now();
        for (code, expected) in [
            (Some(3), ActivityState::Error),
            (Some(127), ActivityState::Error),
            (Some(130), ActivityState::Idle),
            (Some(0), ActivityState::Idle),
            (None, ActivityState::Idle),
        ] {
            let mut activity = PaneActivity::new();
            activity.on_signal(Signal::CommandStart, false);
            activity.on_signal(Signal::CommandEnd(code), false);
            assert_eq!(activity.state(t0), expected, "code {code:?}");
        }
    }

    #[test]
    fn error_outranks_waiting() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::Notify, false);
        activity.on_signal(Signal::CommandEnd(Some(1)), false);
        assert_eq!(activity.state(t0), ActivityState::Error);
    }

    #[test]
    fn sustained_output_runs_then_falls_idle_after_the_window() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        let end = burst(&mut activity, t0, ms(1000));
        assert_eq!(activity.state(end), ActivityState::Running);
        assert_eq!(activity.state(end + ms(2100)), ActivityState::Idle);
    }

    #[test]
    fn a_small_idle_redraw_is_not_activity() {
        // La statusline de Claude Code au repos : 71 octets isolés (R2).
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        activity.on_output(71, t0);
        assert_eq!(activity.state(t0), ActivityState::Idle);
        activity.tick(t0 + ms(3500), true, false);
        assert_eq!(activity.state(t0 + ms(3500)), ActivityState::Idle);
    }

    #[test]
    fn end_of_turn_with_bracketed_paste_waits() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        let end = burst(&mut activity, t0, ms(2000));
        activity.tick(end + ms(2500), true, false);
        assert_ne!(activity.state(end + ms(2500)), ActivityState::Waiting);
        activity.tick(end + ms(3100), true, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Waiting);
    }

    #[test]
    fn end_of_turn_without_bracketed_paste_stays_idle() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        let end = burst(&mut activity, t0, ms(2000));
        activity.tick(end + ms(3100), false, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Idle);
    }

    #[test]
    fn end_of_turn_needs_a_running_command() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        let end = burst(&mut activity, t0, ms(2000));
        activity.tick(end + ms(3100), true, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Idle);
    }

    #[test]
    fn an_agent_running_before_the_attach_ends_its_turn_from_the_foreground_command() {
        // Idema relancé pendant que `cc` tourne : aucun 133;C n'arrivera, l'abonnement dit « bun ».
        let t0 = Instant::now();
        let mut activity = PaneActivity::attached(t0);
        activity.set_foreground_is_shell(false);
        assert!(activity.command_running() && !activity.at_prompt());
        let end = burst(&mut activity, t0 + ATTACH_GRACE, ms(2000));
        activity.tick(end + ms(3100), true, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Waiting);
    }

    #[test]
    fn a_shell_idle_before_the_attach_is_at_its_prompt() {
        let mut activity = PaneActivity::attached(Instant::now());
        assert!(
            !activity.at_prompt(),
            "unknown until the subscription answers"
        );
        activity.set_foreground_is_shell(true);
        assert!(activity.at_prompt() && !activity.command_running());
    }

    #[test]
    fn marks_take_over_from_the_foreground_command() {
        let mut activity = PaneActivity::new();
        activity.set_foreground_is_shell(false);
        activity.on_signal(Signal::PromptStart, false);
        assert!(activity.at_prompt() && !activity.command_running());
    }

    #[test]
    fn output_during_the_attach_grace_arms_no_turn() {
        // Un agent qui se redessine parce que son pane change de taille à l'attache.
        let t0 = Instant::now();
        let mut activity = PaneActivity::attached(t0);
        activity.set_foreground_is_shell(false);
        let end = burst(&mut activity, t0, ms(2500));
        assert_eq!(activity.state(end), ActivityState::Idle);
        activity.tick(end + ms(3100), true, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Idle);
    }

    #[test]
    fn a_turn_is_reported_once() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        let end = burst(&mut activity, t0, ms(2000));
        activity.tick(end + ms(3100), true, false);
        activity.mark_seen();
        activity.tick(end + ms(6000), true, false);
        assert_eq!(activity.state(end + ms(6000)), ActivityState::Idle);
    }

    #[test]
    fn a_turn_that_ends_while_seen_is_consumed() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        let end = burst(&mut activity, t0, ms(2000));
        activity.tick(end + ms(3100), true, true);
        activity.tick(end + ms(4000), true, false);
        assert_eq!(activity.state(end + ms(4000)), ActivityState::Idle);
    }

    #[test]
    fn progress_runs_and_holds_the_turn_open() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        activity.on_signal(Signal::Progress(true), false);
        activity.tick(t0 + ms(10_000), true, false);
        assert_eq!(activity.state(t0 + ms(10_000)), ActivityState::Running);
        activity.on_signal(Signal::Progress(false), false);
        activity.on_output(10, t0 + ms(10_000));
        activity.tick(t0 + ms(13_100), true, false);
        assert_eq!(activity.state(t0 + ms(13_100)), ActivityState::Waiting);
    }

    #[test]
    fn an_unclosed_progress_ends_with_its_command() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        activity.on_signal(Signal::Progress(true), false);
        activity.on_signal(Signal::CommandEnd(Some(0)), false);
        assert_eq!(activity.state(t0 + ms(5000)), ActivityState::Idle);
        activity.on_signal(Signal::Progress(true), false);
        activity.on_signal(Signal::PromptStart, false);
        assert_eq!(activity.state(t0 + ms(5000)), ActivityState::Idle);
    }

    #[test]
    fn prompt_and_command_marks_track_the_shell() {
        let mut activity = PaneActivity::new();
        assert!(!activity.at_prompt());
        activity.on_signal(Signal::PromptStart, false);
        assert!(activity.at_prompt() && !activity.command_running());
        activity.on_signal(Signal::CommandStart, false);
        assert!(!activity.at_prompt() && activity.command_running());
        activity.on_signal(Signal::CommandEnd(Some(0)), false);
        activity.on_signal(Signal::PromptStart, false);
        assert!(activity.at_prompt() && !activity.command_running());
    }

    #[test]
    fn chime_on_each_passage_to_waiting_of_an_unseen_session() {
        use ActivityState::*;
        assert!(should_chime(Idle, Waiting, false));
        assert!(should_chime(Running, Waiting, false));
        assert!(!should_chime(Waiting, Waiting, false));
        assert!(!should_chime(Idle, Waiting, true));
        assert!(!should_chime(Idle, Error, false));
    }

    #[test]
    fn launch_sends_as_soon_as_zle_owns_the_line() {
        let t0 = Instant::now();
        let mut gate = LaunchGate::new(t0);
        assert_eq!(gate.poll(t0 + ms(800), None), LaunchStep::Wait);
        gate.on_signal(Signal::PromptStart);
        assert_eq!(gate.poll(t0 + ms(850), None), LaunchStep::Wait);
        gate.on_signal(Signal::PromptEnd);
        assert_eq!(gate.poll(t0 + ms(900), None), LaunchStep::Send);
    }

    #[test]
    fn launch_with_only_the_prompt_start_waits_five_seconds() {
        let t0 = Instant::now();
        let mut gate = LaunchGate::new(t0);
        gate.on_signal(Signal::PromptStart);
        assert_eq!(gate.poll(t0 + ms(4_999), None), LaunchStep::Wait);
        assert_eq!(gate.poll(t0 + PROMPT_WAIT, None), LaunchStep::Send);
    }

    #[test]
    fn launch_gives_up_even_when_the_prompt_shows_after_the_deadline() {
        let t0 = Instant::now();
        let mut gate = LaunchGate::new(t0);
        gate.on_signal(Signal::PromptStart);
        gate.on_signal(Signal::PromptEnd);
        assert_eq!(
            gate.poll(t0 + LAUNCH_GIVE_UP, Some(true)),
            LaunchStep::GiveUp
        );
        assert_eq!(
            gate.poll(t0 + LAUNCH_GIVE_UP + ms(900), Some(true)),
            LaunchStep::GiveUp
        );
    }

    #[test]
    fn launch_without_prompt_mark_waits_five_seconds_then_needs_quiet_and_the_shell() {
        let t0 = Instant::now();
        let mut gate = LaunchGate::new(t0);
        gate.on_output(t0 + ms(4900));
        assert_eq!(gate.poll(t0 + ms(4950), Some(true)), LaunchStep::Wait);
        assert_eq!(gate.poll(t0 + ms(5200), Some(true)), LaunchStep::Wait);
        assert_eq!(gate.poll(t0 + ms(5400), None), LaunchStep::Wait);
        assert_eq!(gate.poll(t0 + ms(5400), Some(false)), LaunchStep::Wait);
        assert_eq!(gate.poll(t0 + ms(5400), Some(true)), LaunchStep::Send);
    }

    #[test]
    fn a_delayed_launch_waits_for_its_start_even_with_the_prompt_ready() {
        let t0 = Instant::now();
        let start = t0 + ms(900);
        let mut gate = LaunchGate::new(start);
        gate.on_signal(Signal::PromptStart);
        gate.on_signal(Signal::PromptEnd);
        assert_eq!(gate.poll(t0, Some(true)), LaunchStep::Wait);
        assert_eq!(gate.poll(start - ms(1), Some(true)), LaunchStep::Wait);
        assert_eq!(gate.poll(start, Some(true)), LaunchStep::Send);
    }

    #[test]
    fn a_delayed_launch_gives_up_thirty_seconds_after_its_start() {
        let t0 = Instant::now();
        let start = t0 + ms(900);
        let gate = LaunchGate::new(start);
        assert_eq!(
            gate.poll(start + LAUNCH_GIVE_UP - ms(1), Some(false)),
            LaunchStep::Wait
        );
        assert_eq!(
            gate.poll(start + LAUNCH_GIVE_UP, Some(false)),
            LaunchStep::GiveUp
        );
    }

    #[test]
    fn two_ready_gates_sharing_a_pacer_send_one_after_the_other() {
        let t0 = Instant::now();
        let spacing = ms(300);
        let pacer = LaunchPacer::new(t0);
        let other = pacer.clone();
        // Deux portes prêtes au même instant : une seule part.
        let first = paced_step(LaunchStep::Send, Some(&pacer), t0, spacing);
        let second = paced_step(LaunchStep::Send, Some(&other), t0, spacing);
        assert_eq!((first, second), (LaunchStep::Send, LaunchStep::Wait));
        // La seconde attend l'écart plein, et pas une milliseconde de moins.
        assert_eq!(
            paced_step(LaunchStep::Send, Some(&other), t0 + ms(299), spacing),
            LaunchStep::Wait
        );
        assert_eq!(
            paced_step(LaunchStep::Send, Some(&other), t0 + ms(300), spacing),
            LaunchStep::Send
        );
        // Une troisième porte attend derrière la seconde.
        assert_eq!(
            paced_step(LaunchStep::Send, Some(&pacer), t0 + ms(450), spacing),
            LaunchStep::Wait
        );
    }

    #[test]
    fn without_a_pacer_every_ready_gate_sends() {
        let t0 = Instant::now();
        for _ in 0..2 {
            assert_eq!(
                paced_step(LaunchStep::Send, None, t0, ms(300)),
                LaunchStep::Send
            );
        }
    }

    #[test]
    fn a_wait_or_a_give_up_never_spends_the_pacer() {
        let t0 = Instant::now();
        let pacer = LaunchPacer::new(t0);
        assert_eq!(
            paced_step(LaunchStep::Wait, Some(&pacer), t0, ms(300)),
            LaunchStep::Wait
        );
        assert_eq!(
            paced_step(LaunchStep::GiveUp, Some(&pacer), t0, ms(300)),
            LaunchStep::GiveUp
        );
        assert_eq!(
            paced_step(LaunchStep::Send, Some(&pacer), t0, ms(300)),
            LaunchStep::Send
        );
    }

    #[test]
    fn launch_gives_up_instead_of_typing_blind() {
        let t0 = Instant::now();
        let gate = LaunchGate::new(t0);
        assert_eq!(gate.poll(t0 + ms(30_000), Some(false)), LaunchStep::GiveUp);
    }

    /// Une frappe de `len` octets suivie de son écho, à `at`.
    fn typed_and_echoed(activity: &mut PaneActivity, at: Instant, len: usize) {
        activity.on_input();
        activity.on_output(len, at + ms(5));
    }

    #[test]
    fn a_redraw_after_a_resize_is_not_a_turn() {
        // Contre-exemple de la revue : 4 096 octets de redessin bien après l'attache, puis départ.
        let t0 = Instant::now();
        let mut activity = PaneActivity::attached(t0);
        activity.set_foreground_is_shell(false);
        let resized = t0 + ms(60_000);
        activity.on_resize(resized);
        activity.on_output(4096, resized + ms(100));
        activity.tick(resized + ms(3200), true, false);
        assert_eq!(activity.state(resized + ms(3200)), ActivityState::Idle);
    }

    /// Rejoue le démarrage de Codex mesuré le 15/10 : attache à t0, commande tapée à t0 + 1,07 s, impulsions de
    /// 14 octets jusqu'à t0 + 3,7 s, puis deux de 43 octets (t0 + 4,29 s et t0 + 4,87 s), soit 296 octets après
    /// la fin de la grâce d'attache. `launch` dit si Idema repose la grâce quand il tape la commande.
    fn replay_agent_startup(t0: Instant, launch: bool) -> PaneActivity {
        let mut activity = PaneActivity::attached(t0);
        activity.set_foreground_is_shell(false);
        if launch {
            activity.on_launch(t0 + ms(1070));
        }
        let mut at = t0 + ms(1100);
        while at < t0 + ms(3000) {
            activity.on_output(14, at);
            at += ms(100);
        }
        let mut at = t0 + ms(3000);
        while at < t0 + ms(3750) {
            activity.on_output(14, at);
            at += ms(50);
        }
        activity.on_output(43, t0 + ms(4290));
        activity.on_output(43, t0 + ms(4870));
        activity
    }

    #[test]
    fn the_startup_render_after_a_launch_is_not_a_turn() {
        let t0 = Instant::now();
        let mut activity = replay_agent_startup(t0, true);
        let quiet = t0 + ms(4870) + ms(4000);
        activity.tick(quiet, true, false);
        assert_eq!(activity.state(quiet), ActivityState::Idle);
        assert!(!should_chime(
            ActivityState::Idle,
            activity.state(quiet),
            false
        ));
    }

    #[test]
    fn without_a_launch_grace_the_same_startup_render_ends_as_a_turn() {
        // Avant le correctif : la grâce d'attache seule laisse passer la fin de l'animation de démarrage.
        let t0 = Instant::now();
        let mut activity = replay_agent_startup(t0, false);
        let quiet = t0 + ms(4870) + ms(4000);
        activity.tick(quiet, true, false);
        assert_eq!(activity.state(quiet), ActivityState::Waiting);
    }

    #[test]
    fn a_real_turn_after_the_launch_grace_still_waits() {
        let t0 = Instant::now();
        let mut activity = replay_agent_startup(t0, true);
        let turn_start = t0 + ms(1070) + ATTACH_GRACE + ms(2000);
        let end = burst(&mut activity, turn_start, ms(2000));
        activity.tick(end + TURN_SILENCE + ms(100), true, false);
        assert_eq!(
            activity.state(end + TURN_SILENCE + ms(100)),
            ActivityState::Waiting
        );
    }

    #[test]
    fn a_launch_disarms_a_turn_already_armed() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        let end = burst(&mut activity, t0, ms(2000));
        activity.on_launch(end);
        activity.tick(end + ms(3100), true, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Idle);
    }

    #[test]
    fn a_resize_disarms_a_turn_already_armed() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        let end = burst(&mut activity, t0, ms(2000));
        activity.on_resize(end);
        activity.tick(end + ms(3100), true, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Idle);
    }

    #[test]
    fn typing_echo_in_the_agent_input_box_is_not_a_turn() {
        // Vingt frappes de 30 octets d'écho en 3 s, puis départ.
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        for key in 0..20 {
            typed_and_echoed(&mut activity, t0 + ms(150 * key), 30);
        }
        let end = t0 + ms(3000);
        assert_ne!(activity.state(end), ActivityState::Running);
        activity.tick(end + ms(3100), true, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Idle);
    }

    #[test]
    fn an_isolated_small_redraw_stays_idle() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        activity.on_output(71, t0);
        activity.tick(t0 + ms(3100), true, false);
        assert_eq!(activity.state(t0 + ms(3100)), ActivityState::Idle);
    }

    #[test]
    fn a_real_turn_after_the_user_pressed_enter_still_waits() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        typed_and_echoed(&mut activity, t0, 30);
        activity.on_input(); // Entrée
        let end = burst(&mut activity, t0 + ms(200), ms(2000));
        activity.tick(end + ms(3100), true, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Waiting);
    }

    #[test]
    fn a_burst_before_a_command_start_does_not_arm_that_commands_turn() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        let end = burst(&mut activity, t0, ms(2000));
        activity.on_signal(Signal::CommandStart, false);
        activity.tick(end + ms(3100), true, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Idle);
    }

    #[test]
    fn a_burst_before_a_command_end_does_not_leave_the_turn_armed() {
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        let end = burst(&mut activity, t0, ms(2000));
        activity.on_signal(Signal::CommandEnd(Some(0)), false);
        // Le programme suivant démarre sans nouvelle rafale : le tour ne doit pas survivre.
        activity.on_signal(Signal::PromptStart, false);
        activity.on_signal(Signal::CommandStart, false);
        activity.tick(end + ms(3100), true, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Idle);
    }

    #[test]
    fn held_input_keeps_its_order_and_flushes_once() {
        let mut held = HeldInput::new();
        assert!(held.is_empty());
        assert_eq!(held.push(b"fix "), HeldStep::Held);
        assert_eq!(held.push(b"the bug"), HeldStep::Held);
        assert_eq!(held.len(), 11);
        assert_eq!(held.take(), b"fix the bug");
        assert!(held.is_empty());
        assert!(held.take().is_empty());
    }

    #[test]
    fn ctrl_c_abandons_the_launch_and_passes_everything_to_the_pane() {
        let mut held = HeldInput::new();
        held.push(b"ab");
        assert_eq!(held.push(&[0x03]), HeldStep::Abandon(b"ab\x03".to_vec()));
        assert!(held.is_empty());
    }

    #[test]
    fn a_ctrl_c_inside_a_longer_chunk_is_just_held() {
        let mut held = HeldInput::new();
        assert_eq!(held.push(b"a\x03b"), HeldStep::Held);
        assert_eq!(held.take(), b"a\x03b");
    }

    #[test]
    fn output_just_after_an_input_is_its_echo() {
        let t0 = Instant::now();
        assert!(is_input_echo(Some(t0), t0 + ms(30)));
        assert!(is_input_echo(Some(t0), t0 + ms(499)));
        // Arrivée antérieure à la dernière touche : l'écho d'une touche précédente.
        assert!(is_input_echo(Some(t0 + ms(100)), t0 + ms(50)));
        assert!(!is_input_echo(Some(t0), t0 + ECHO_WINDOW));
        assert!(!is_input_echo(None, t0));
    }

    #[test]
    fn a_pasted_text_echo_after_the_last_key_is_not_a_turn() {
        // Collage ou frappe instantanée : l'écho (880 octets) arrive après la dernière saisie. Le glue de la vue
        // le reconnaît (`is_input_echo`) et rejoue `on_input` : le tour ne s'arme pas.
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        activity.on_input();
        let last_input = Some(t0);
        for key in 0..20 {
            let arrival = t0 + ms(10 + 5 * key);
            activity.on_output(44, arrival);
            if is_input_echo(last_input, arrival) {
                activity.on_input();
            }
        }
        activity.tick(t0 + ms(3200), true, false);
        assert_eq!(activity.state(t0 + ms(3200)), ActivityState::Idle);
    }

    #[test]
    fn output_right_after_a_focus_report_does_not_end_an_active_turn() {
        // ⌘-tab hors d'Idema pendant que l'agent écrit : le rapport de focus (mode 1004) et la sortie qui le suit
        // de près ne doivent pas désarmer le tour, sinon « attend » manque quand l'utilisateur est parti.
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        let end = burst(&mut activity, t0, ms(2000));
        activity.on_focus_report(end);
        // L'agent continue 400 ms, dans la fenêtre d'écho du rapport, puis se tait.
        let end = burst(&mut activity, end + ms(50), ms(400));
        activity.tick(end + ms(3100), true, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Waiting);
    }

    #[test]
    fn a_redraw_after_a_focus_report_does_not_arm_a_turn() {
        // Le programme se redessine au focus (800 octets, dans la fenêtre d'écho du rapport) : pas un tour.
        let t0 = Instant::now();
        let mut activity = PaneActivity::new();
        activity.on_signal(Signal::CommandStart, false);
        activity.on_focus_report(t0);
        for step in 0..8 {
            activity.on_output(100, t0 + ms(20 + 10 * step));
        }
        activity.tick(t0 + ms(3500), true, false);
        assert_eq!(activity.state(t0 + ms(3500)), ActivityState::Idle);
        // La sortie qui suit la fenêtre arme un tour comme d'habitude.
        let end = burst(&mut activity, t0 + ms(1000), ms(2000));
        activity.tick(end + ms(3100), true, false);
        assert_eq!(activity.state(end + ms(3100)), ActivityState::Waiting);
    }
}
