//! Le geste : arrêter une session Claude Code qui tourne hors d'IdemA, puis la reprendre dans le pane choisi.
//!
//! Ce module envoie de vrais signaux à de vrais processus. Une mauvaise identité tue le travail de quelqu'un : chaque
//! signal est précédé d'une lecture fraîche du processus (identité complète avant `SIGTERM`, `ps` avant `SIGKILL`), et
//! un seul `pid` est jamais signalé (pas de groupe, pas de recherche par nom). La logique (`stop_session`) ne touche au système que par `ProcessControl`, pour
//! être testée avec un faux ; `SystemControl` est la version réelle.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::discover::{OutsideSession, is_session_command};
use super::disk::{ps_output, ps_start};
use super::sources::{MAX_SESSION_FILE_BYTES, parse_live_session};
use crate::hud;
use crate::servers::stop::Signal;

/// Bulle : le processus a disparu ou changé entre le menu et le clic.
pub const MESSAGE_GONE: &str = "Cette session n'est plus là, rien n'a été arrêté";
/// Bulle : le pane n'est pas au prompt.
pub const MESSAGE_BUSY: &str = "Un programme tourne déjà, rien n'a été arrêté";
/// Bulle : l'arrêt n'est pas confirmé (la session a résisté à `SIGTERM` puis à `SIGKILL`, ou `ps` n'a pas su répondre) ;
/// la commande est dans le presse-papiers, au cas où la session serait bien partie.
pub const MESSAGE_SURVIVED: &str = "Arrêt non confirmé, rien n'a été tapé. Commande copiée";
/// Bulle : la session est arrêtée mais la commande n'a pas pu être tapée ; elle est dans le presse-papiers.
pub const MESSAGE_COPIED: &str = "Session arrêtée, commande copiée : colle-la au prompt";

/// Ce que l'on sait d'un processus à l'instant de la lecture : le fichier de session (`session_id`, `proc_start`) et
/// `ps` (`started_at`, `command`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    /// L'identifiant de conversation que le fichier de session du `pid` porte.
    pub session_id: String,
    /// `procStart` du fichier de session, tel que Claude l'a écrit.
    pub proc_start: String,
    /// La date de démarrage que `ps` donne aujourd'hui pour ce `pid`.
    pub started_at: String,
    /// La ligne de commande du processus.
    pub command: String,
}

/// Les gestes que `stop_session` demande au système.
pub trait ProcessControl {
    /// L'identité fraîche de `pid`, `None` si le fichier de session ou le processus est introuvable ou illisible.
    fn identity(&self, pid: i32) -> Option<ProcessIdentity>;
    /// Ce que `ps` seul rapporte du `pid`, `(ligne de commande, date de démarrage)`, sans le fichier de session : `claude`
    /// le retire pendant qu'il s'éteint. `None` quand `ps` ne connaît plus le processus (ou n'a pas pu répondre).
    fn running(&self, pid: i32) -> Option<(String, String)>;
    fn is_dir(&self, path: &Path) -> bool;
    /// Envoie le signal à ce seul `pid`.
    fn send(&self, pid: i32, signal: Signal);
    /// Faux seulement quand le processus est parti pour de bon ; au doute, vrai.
    fn is_alive(&self, pid: i32) -> bool;
}

/// Les attentes de l'arrêt : combien de temps laisser à chaque signal, et le rythme de la surveillance.
#[derive(Debug, Clone, Copy)]
pub struct StopTimings {
    pub term_wait: Duration,
    pub kill_wait: Duration,
    pub poll: Duration,
}

/// `SIGTERM` laisse 5 s au plus (mesuré : `claude` part en 1,4 s), `SIGKILL` 1 s, surveillance toutes les 50 ms.
pub const STOP_TIMINGS: StopTimings = StopTimings {
    term_wait: Duration::from_secs(5),
    kill_wait: Duration::from_secs(1),
    poll: Duration::from_millis(50),
};

/// Comment l'arrêt s'est terminé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Le processus est parti ; le signal est celui qui l'a fait.
    Stopped(Signal),
    /// L'identité ne correspondait plus avant le premier signal : aucun signal n'est parti.
    Changed,
    /// L'arrêt n'est pas confirmé : le processus est encore vivant après `SIGKILL`, ou `ps` n'a pas su dire s'il est
    /// parti (muet, ou délai dépassé). Il a pu mourir sans qu'on le sache.
    Survived,
}

/// Vrai si le processus est la session visée, avant tout signal : même conversation, même fichier, même démarrage que
/// ce que `ps` rapporte, ligne de commande d'une vraie conversation Claude (ni `remote-control`, ni `mcp`, ni
/// `agents`), et dossier de travail encore là. Le dossier est testé avant la lecture de l'identité : le dernier `ps`
/// reste ainsi le plus près du signal.
fn is_the_target(target: &OutsideSession, control: &impl ProcessControl) -> bool {
    if !control.is_dir(&target.cwd) {
        return false;
    }
    let Some(found) = control.identity(target.pid) else {
        return false;
    };
    found.session_id == target.session_id
        && found.proc_start == target.proc_start
        // `ps` et le fichier s'accordent : un PID repris par un autre processus, ou un `claude` relancé sous le même
        // numéro, a une autre date de démarrage (un `SIGKILL` laisse le fichier derrière lui).
        && found.started_at == found.proc_start
        && is_session_command(&found.command)
}

/// Attend la mort du processus, en le sondant à chaque `poll`, au plus `limit`. Vrai s'il est parti.
fn wait_gone(control: &impl ProcessControl, pid: i32, limit: Duration, poll: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if !control.is_alive(pid) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(poll.min(deadline - now));
    }
}

/// Arrête la session visée : `SIGTERM`, attente, puis `SIGKILL` si elle ne part pas. Seul `target.pid` est signalé.
/// Avant `SIGTERM`, l'identité complète est vérifiée (fichier, démarrage, commande, dossier) : sinon `Changed`, et
/// aucun signal n'est parti. Après `SIGTERM`, `ps` seul décide (`running`) : un processus parti, ou un numéro
/// repris par un autre, a quitté la cible (`Stopped(Term)`), et `SIGKILL` ne part que sur le même processus. Bloquant
/// (il dort entre deux lectures) : à lancer hors du fil d'interface.
pub fn stop_session(
    target: &OutsideSession,
    control: &impl ProcessControl,
    timings: &StopTimings,
) -> StopOutcome {
    let pid = target.pid;
    if !is_the_target(target, control) {
        return StopOutcome::Changed;
    }
    control.send(pid, Signal::Term);
    if wait_gone(control, pid, timings.term_wait, timings.poll) {
        return StopOutcome::Stopped(Signal::Term);
    }
    // Le fichier de session et le dossier ne prouvent plus rien : `claude` retire son fichier en s'éteignant. `ps` seul
    // décide, par le démarrage et la ligne de commande.
    match control.running(pid) {
        Some((command, started_at)) => {
            // Un numéro repris par un autre processus : la cible a quitté, et `SIGKILL` ne visera pas l'intrus.
            if started_at != target.proc_start || !is_session_command(&command) {
                return StopOutcome::Stopped(Signal::Term);
            }
        }
        // Parti à l'instant : arrêtée. Mais un `ps` qui n'a pas su répondre ne prouve pas la mort : le processus est
        // alors tenu pour vivant, sans `SIGKILL` à l'aveugle ni « arrêtée ».
        None => {
            return if control.is_alive(pid) {
                StopOutcome::Survived
            } else {
                StopOutcome::Stopped(Signal::Term)
            };
        }
    }
    control.send(pid, Signal::Kill);
    if wait_gone(control, pid, timings.kill_wait, timings.poll) {
        StopOutcome::Stopped(Signal::Kill)
    } else {
        StopOutcome::Survived
    }
}

/// La suite du geste une fois `stop_session` rendu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextStep {
    /// Taper la commande de reprise dans le pane.
    Type,
    /// Dire pourquoi rien ne se passe.
    Tell(&'static str),
}

pub fn after_stop(outcome: StopOutcome) -> NextStep {
    match outcome {
        StopOutcome::Stopped(_) => NextStep::Type,
        StopOutcome::Changed => NextStep::Tell(MESSAGE_GONE),
        StopOutcome::Survived => NextStep::Tell(MESSAGE_SURVIVED),
    }
}

/// Le geste peut-il démarrer ? `Ok(commande)` : la commande de reprise à taper, seule façon d'obtenir le feu vert (une
/// session sans commande n'a rien à reprendre, le geste ne part donc jamais sans elle). `Err(None)` : ignoré sans bulle
/// (déjà un geste en cours, ou rien à taper) ; `Err(Some(texte))` : refusé avec une bulle.
pub fn may_start(
    reclaiming: Option<i32>,
    at_prompt: bool,
    resume: Option<&str>,
) -> Result<&str, Option<&'static str>> {
    if reclaiming.is_some() {
        return Err(None);
    }
    if !at_prompt {
        return Err(Some(MESSAGE_BUSY));
    }
    resume.ok_or(None)
}

/// Le contrôle réel : `ps` pour l'identité et la vie, `libc::kill` pour les signaux.
pub struct SystemControl {
    pub sessions_dir: PathBuf,
}

impl ProcessControl for SystemControl {
    /// Le fichier de session est relu par le lecteur strict (le nom du fichier doit dire la vérité sur `pid`), puis
    /// `ps` donne la ligne de commande et la date de démarrage, en `TZ=UTC LC_ALL=C` pour être comparable à `procStart`.
    /// Ni la ligne de commande ni l'environnement ne sont journalisés.
    fn identity(&self, pid: i32) -> Option<ProcessIdentity> {
        if pid <= 0 {
            return None;
        }
        let file = self.sessions_dir.join(format!("{pid}.json"));
        // `symlink_metadata` ne suit pas les liens : un lien vers `/dev/zero` ou un FIFO ne bloquerait pas la lecture.
        let meta = std::fs::symlink_metadata(&file).ok()?;
        if !meta.is_file() || meta.len() > MAX_SESSION_FILE_BYTES {
            return None;
        }
        let live = parse_live_session(&std::fs::read_to_string(file).ok()?, Some(pid))?;
        let command = ps_output(&["-o", "command=", "-p", &pid.to_string()], &[], pid)?;
        let command = command.trim();
        if command.is_empty() {
            return None;
        }
        Some(ProcessIdentity {
            session_id: live.session_id,
            proc_start: live.proc_start,
            started_at: ps_start(pid)?,
            command: command.to_string(),
        })
    }

    /// La ligne de commande et le démarrage selon `ps`, rien d'autre. Vide ou `ps` muet : `None`.
    fn running(&self, pid: i32) -> Option<(String, String)> {
        let command = ps_output(&["-o", "command=", "-p", &pid.to_string()], &[], pid)?;
        let command = command.trim();
        if command.is_empty() {
            return None;
        }
        Some((command.to_string(), ps_start(pid)?))
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    /// Un seul `pid`, jamais un groupe : un `pid` qui n'est pas strictement positif viserait un groupe ou tous les
    /// processus, il est refusé ici. ESRCH (déjà parti) n'est pas une erreur ; le reste va au journal.
    fn send(&self, pid: i32, signal: Signal) {
        if pid <= 0 {
            return;
        }
        // SAFETY : `kill` n'agit que sur `pid`, strictement positif, relu juste avant par `stop_session`.
        let result = unsafe { libc::kill(pid, signal.number()) };
        if result != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error();
            if errno != Some(libc::ESRCH) {
                hud::log_event(&format!(
                    "reclaim signal failed pid={pid} signal={} errno={}",
                    signal.name(),
                    errno.unwrap_or(0)
                ));
            }
        }
    }

    /// Mort : `ps` ne connaît plus le `pid`, ou le montre zombie (`Z`, mort et pas encore récolté). Un `ps` qui échoue
    /// ou dépasse son délai ne dit rien : le processus est tenu pour vivant, côté prudent.
    fn is_alive(&self, pid: i32) -> bool {
        let Some(stat) = ps_output(&["-o", "stat=", "-p", &pid.to_string()], &[], pid) else {
            return true;
        };
        let stat = stat.trim();
        !(stat.is_empty() || stat.starts_with('Z'))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::process::{Child, Command, Stdio};

    use super::*;

    const ID: &str = "bbbb2222-cccc-4ddd-8eee-ffff00001111";
    const START: &str = "Mon Nov  2 10:00:00 2026";
    const CLAUDE: &str = "/usr/local/bin/claude --resume bbbb2222-cccc-4ddd-8eee-ffff00001111";

    fn target() -> OutsideSession {
        OutsideSession {
            pid: 4242,
            session_id: ID.to_string(),
            proc_start: START.to_string(),
            name: "demo".to_string(),
            cwd: PathBuf::from("/a/x"),
            root: PathBuf::from("/a/x"),
            agent: "claude".to_string(),
            model: None,
            resume: Some("cd '/a/x' && idnma run 'claude' --resume x".to_string()),
            relaunch: Some("idnma run 'claude'".to_string()),
        }
    }

    fn identity() -> ProcessIdentity {
        ProcessIdentity {
            session_id: ID.to_string(),
            proc_start: START.to_string(),
            started_at: START.to_string(),
            command: CLAUDE.to_string(),
        }
    }

    /// Un faux système : il journalise les signaux, meurt au signal voulu et peut changer d'identité au premier.
    struct FakeControl {
        identity: RefCell<Option<ProcessIdentity>>,
        after_first_signal: RefCell<Option<Option<ProcessIdentity>>>,
        dir: bool,
        dies_on: Option<Signal>,
        alive: Cell<bool>,
        signals: RefCell<Vec<(i32, Signal)>>,
        /// Ce que `ps` seul rapporte : `(commande, démarrage)`, `None` quand le processus n'y est plus.
        running: RefCell<Option<(String, String)>>,
        /// Vrai : le processus meurt à l'instant où `running` est interrogé (il finissait de s'éteindre).
        dies_when_asked: bool,
    }

    impl FakeControl {
        fn new(identity: Option<ProcessIdentity>, dies_on: Option<Signal>) -> Self {
            Self {
                identity: RefCell::new(identity),
                after_first_signal: RefCell::new(None),
                dir: true,
                dies_on,
                alive: Cell::new(true),
                signals: RefCell::new(Vec::new()),
                running: RefCell::new(Some((CLAUDE.to_string(), START.to_string()))),
                dies_when_asked: false,
            }
        }

        fn sent(&self) -> Vec<Signal> {
            self.signals.borrow().iter().map(|(_, s)| *s).collect()
        }
    }

    impl ProcessControl for FakeControl {
        fn identity(&self, _: i32) -> Option<ProcessIdentity> {
            self.identity.borrow().clone()
        }
        fn running(&self, _: i32) -> Option<(String, String)> {
            if self.dies_when_asked {
                self.alive.set(false);
            }
            self.running.borrow().clone()
        }
        fn is_dir(&self, _: &Path) -> bool {
            self.dir
        }
        fn send(&self, pid: i32, signal: Signal) {
            self.signals.borrow_mut().push((pid, signal));
            if self.dies_on == Some(signal) {
                self.alive.set(false);
            }
            if let Some(next) = self.after_first_signal.borrow_mut().take() {
                *self.identity.borrow_mut() = next;
            }
        }
        fn is_alive(&self, _: i32) -> bool {
            self.alive.get()
        }
    }

    const FAST: StopTimings = StopTimings {
        term_wait: Duration::from_millis(1),
        kill_wait: Duration::from_millis(1),
        poll: Duration::from_millis(1),
    };

    fn run(control: &FakeControl) -> StopOutcome {
        stop_session(&target(), control, &FAST)
    }

    #[test]
    fn a_session_that_obeys_term_gets_no_kill() {
        let control = FakeControl::new(Some(identity()), Some(Signal::Term));
        assert_eq!(run(&control), StopOutcome::Stopped(Signal::Term));
        assert_eq!(control.sent(), [Signal::Term]);
    }

    #[test]
    fn a_session_that_ignores_term_gets_a_kill() {
        let control = FakeControl::new(Some(identity()), Some(Signal::Kill));
        assert_eq!(run(&control), StopOutcome::Stopped(Signal::Kill));
        assert_eq!(control.sent(), [Signal::Term, Signal::Kill]);
    }

    #[test]
    fn a_session_that_survives_kill_is_reported() {
        let control = FakeControl::new(Some(identity()), None);
        assert_eq!(run(&control), StopOutcome::Survived);
        assert_eq!(control.sent(), [Signal::Term, Signal::Kill]);
    }

    #[test]
    fn only_the_target_pid_is_signalled() {
        let control = FakeControl::new(Some(identity()), Some(Signal::Kill));
        run(&control);
        assert_eq!(
            *control.signals.borrow(),
            [(4242, Signal::Term), (4242, Signal::Kill)]
        );
    }

    fn assert_untouched(identity: Option<ProcessIdentity>) {
        let control = FakeControl::new(identity, Some(Signal::Term));
        assert_eq!(run(&control), StopOutcome::Changed);
        assert!(control.sent().is_empty());
    }

    #[test]
    fn a_reused_pid_gets_no_signal() {
        let mut other = identity();
        other.command = "sleep 100".to_string();
        assert_untouched(Some(other));
    }

    #[test]
    fn a_restarted_process_gets_no_signal() {
        let mut other = identity();
        other.started_at = "Mon Nov  2 11:30:00 2026".to_string();
        assert_untouched(Some(other));
    }

    #[test]
    fn the_dispatch_server_is_never_signalled() {
        let mut other = identity();
        other.command = "/x/claude remote-control --name HQ".to_string();
        assert_untouched(Some(other));
    }

    #[test]
    fn a_changed_session_file_gets_no_signal() {
        let mut other_id = identity();
        other_id.session_id = "aaaaaaaa-ad58-491e-9f7e-30011ccd1eb3".to_string();
        assert_untouched(Some(other_id));
        // Le fichier a un autre `procStart` que la cible, même si `ps` et le fichier s'accordent.
        let mut other_start = identity();
        other_start.proc_start = "Mon Nov  2 11:30:00 2026".to_string();
        other_start.started_at = other_start.proc_start.clone();
        assert_untouched(Some(other_start));
    }

    #[test]
    fn a_session_already_gone_gets_no_signal() {
        assert_untouched(None);
    }

    #[test]
    fn a_missing_folder_gets_no_signal() {
        let mut control = FakeControl::new(Some(identity()), Some(Signal::Term));
        control.dir = false;
        assert_eq!(run(&control), StopOutcome::Changed);
        assert!(control.sent().is_empty());
    }

    #[test]
    fn a_reused_pid_after_term_gets_no_kill() {
        let control = FakeControl::new(Some(identity()), Some(Signal::Kill));
        // Le processus visé est parti, un autre a repris le numéro : `SIGKILL` le tuerait.
        *control.running.borrow_mut() =
            Some((CLAUDE.to_string(), "Mon Nov  2 11:30:00 2026".to_string()));
        assert_eq!(run(&control), StopOutcome::Stopped(Signal::Term));
        assert_eq!(control.sent(), [Signal::Term]);
    }

    #[test]
    fn a_pid_taken_by_another_program_after_term_gets_no_kill() {
        let control = FakeControl::new(Some(identity()), Some(Signal::Kill));
        *control.running.borrow_mut() = Some(("sleep 100".to_string(), START.to_string()));
        assert_eq!(run(&control), StopOutcome::Stopped(Signal::Term));
        assert_eq!(control.sent(), [Signal::Term]);
    }

    #[test]
    fn a_session_that_removed_its_file_still_gets_a_kill() {
        // `claude` retire son fichier de session pendant qu'il s'éteint, puis s'attarde : `ps` le voit toujours.
        let control = FakeControl::new(Some(identity()), Some(Signal::Kill));
        *control.after_first_signal.borrow_mut() = Some(None);
        assert_eq!(run(&control), StopOutcome::Stopped(Signal::Kill));
        assert_eq!(control.sent(), [Signal::Term, Signal::Kill]);
    }

    #[test]
    fn a_session_gone_after_term_is_stopped() {
        let mut control = FakeControl::new(Some(identity()), None);
        *control.running.borrow_mut() = None;
        control.dies_when_asked = true;
        assert_eq!(run(&control), StopOutcome::Stopped(Signal::Term));
        assert_eq!(control.sent(), [Signal::Term]);
    }

    #[test]
    fn a_ps_that_cannot_tell_after_term_is_not_taken_for_a_death() {
        // `running` ne rend rien mais le processus est tenu pour vivant : ni `SIGKILL` à l'aveugle, ni « arrêtée ».
        let control = FakeControl::new(Some(identity()), None);
        *control.running.borrow_mut() = None;
        assert_eq!(run(&control), StopOutcome::Survived);
        assert_eq!(control.sent(), [Signal::Term]);
    }

    #[test]
    fn a_stopped_session_leads_to_typing() {
        assert_eq!(
            after_stop(StopOutcome::Stopped(Signal::Term)),
            NextStep::Type
        );
        assert_eq!(
            after_stop(StopOutcome::Stopped(Signal::Kill)),
            NextStep::Type
        );
    }

    #[test]
    fn a_changed_or_surviving_session_is_told_apart() {
        assert_eq!(
            after_stop(StopOutcome::Changed),
            NextStep::Tell("Cette session n'est plus là, rien n'a été arrêté")
        );
        assert_eq!(
            after_stop(StopOutcome::Survived),
            NextStep::Tell("Arrêt non confirmé, rien n'a été tapé. Commande copiée")
        );
    }

    #[test]
    fn the_gesture_starts_only_in_a_clean_state() {
        let resume = Some("cd x && idnma run claude");
        assert_eq!(may_start(Some(1), true, resume), Err(None));
        assert_eq!(
            may_start(None, false, resume),
            Err(Some("Un programme tourne déjà, rien n'a été arrêté"))
        );
        assert_eq!(may_start(None, true, None), Err(None));
        assert_eq!(
            may_start(None, true, resume),
            Ok("cd x && idnma run claude")
        );
    }

    #[test]
    fn a_running_gesture_is_ignored_even_away_from_the_prompt() {
        assert_eq!(may_start(Some(1), false, None), Err(None));
    }

    /// Un dossier temporaire propre au test, retiré à la fin.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "idema-test-{}-reclaim-stop-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("test directory");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Un `sleep 300` enfant du test : tué à la chute s'il a survécu, son `pid` est noté au `spawn`.
    struct Sleeper(Child);

    impl Sleeper {
        fn spawn() -> Self {
            let child = Command::new("sleep")
                .arg("300")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("sleep starts");
            Sleeper(child)
        }

        /// Un processus `perl` qui ignore `SIGTERM`, prêt quand il a écrit une ligne : sans cette attente, le signal
        /// pourrait arriver avant que le processus l'ignore. Seul `SIGKILL` l'arrête.
        fn ignoring_term() -> Self {
            use std::io::{BufRead, BufReader};
            let mut child = Command::new("perl")
                .args([
                    "-e",
                    "$SIG{TERM}='IGNORE'; $|=1; print qq(ready\\n); sleep 300",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("perl starts");
            let mut line = String::new();
            BufReader::new(child.stdout.take().expect("piped stdout"))
                .read_line(&mut line)
                .expect("perl answers");
            assert_eq!(line.trim(), "ready");
            Sleeper(child)
        }

        fn pid(&self) -> i32 {
            self.0.id() as i32
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            // Le handle de l'enfant : un seul `pid`, celui du `spawn`.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Le faux fichier de session du `sleep`, au `procStart` que `ps` donne.
    fn session_file(scratch: &Scratch, pid: i32) -> (PathBuf, OutsideSession) {
        let sessions = scratch.0.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let start = ps_start(pid).expect("ps finds the child");
        let cwd = scratch.0.clone();
        let file = format!(
            r#"{{"pid":{pid},"sessionId":"{ID}","cwd":{cwd:?},"kind":"interactive","entrypoint":"cli","procStart":{start:?}}}"#
        );
        std::fs::write(sessions.join(format!("{pid}.json")), file).unwrap();
        let mut session = target();
        session.pid = pid;
        session.proc_start = start;
        session.cwd = cwd;
        (sessions, session)
    }

    /// Le contrôle réel, sauf la ligne de commande : le `sleep` du test joue un `claude`.
    struct PretendClaude(SystemControl);

    impl ProcessControl for PretendClaude {
        fn identity(&self, pid: i32) -> Option<ProcessIdentity> {
            let mut real = self.0.identity(pid)?;
            real.command = CLAUDE.to_string();
            Some(real)
        }
        fn running(&self, pid: i32) -> Option<(String, String)> {
            let (_, started_at) = self.0.running(pid)?;
            Some((CLAUDE.to_string(), started_at))
        }
        fn is_dir(&self, path: &Path) -> bool {
            self.0.is_dir(path)
        }
        fn send(&self, pid: i32, signal: Signal) {
            self.0.send(pid, signal);
        }
        fn is_alive(&self, pid: i32) -> bool {
            self.0.is_alive(pid)
        }
    }

    #[test]
    fn system_control_kills_a_real_child_that_ignores_term() {
        let scratch = Scratch::new("kill");
        let mut stubborn = Sleeper::ignoring_term();
        let pid = stubborn.pid();
        let (sessions_dir, session) = session_file(&scratch, pid);
        // Le fichier de session part pendant l'extinction : seul `ps` reste pour reconnaître le processus.
        let file = sessions_dir.join(format!("{pid}.json"));
        let control = PretendClaude(SystemControl { sessions_dir });
        let timings = StopTimings {
            term_wait: Duration::from_millis(300),
            kill_wait: Duration::from_secs(3),
            poll: Duration::from_millis(20),
        };
        assert!(control.is_alive(pid));
        let stopping = std::thread::scope(|scope| {
            let removal = scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                let _ = std::fs::remove_file(&file);
            });
            let outcome = stop_session(&session, &control, &timings);
            removal.join().unwrap();
            outcome
        });
        assert_eq!(stopping, StopOutcome::Stopped(Signal::Kill));
        assert!(!control.is_alive(pid));
        assert!(!stubborn.0.wait().expect("child is reaped").success());
    }

    #[test]
    fn system_running_reads_ps_only() {
        let scratch = Scratch::new("running");
        let sleeper = Sleeper::spawn();
        let pid = sleeper.pid();
        let (sessions_dir, session) = session_file(&scratch, pid);
        // Sans fichier de session, `running` répond encore : il ne lit que `ps`.
        let _ = std::fs::remove_file(sessions_dir.join(format!("{pid}.json")));
        let control = SystemControl { sessions_dir };
        let (command, started_at) = control.running(pid).expect("ps knows the child");
        assert!(command.starts_with("sleep"), "{command}");
        assert_eq!(started_at, session.proc_start);
        let mut child = Command::new("true").spawn().expect("true starts");
        let gone = child.id() as i32;
        child.wait().expect("child is reaped");
        assert_eq!(control.running(gone), None);
    }

    #[test]
    fn a_session_file_that_is_a_link_is_not_followed() {
        let scratch = Scratch::new("link");
        let sleeper = Sleeper::spawn();
        let pid = sleeper.pid();
        let (sessions_dir, _) = session_file(&scratch, pid);
        let real = sessions_dir.join(format!("{pid}.json"));
        let elsewhere = scratch.0.join("elsewhere.json");
        std::fs::rename(&real, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &real).unwrap();
        let control = SystemControl { sessions_dir };
        assert_eq!(control.identity(pid), None);
    }

    #[test]
    fn system_identity_reads_the_file_and_ps() {
        let scratch = Scratch::new("identity");
        let sleeper = Sleeper::spawn();
        let (sessions_dir, session) = session_file(&scratch, sleeper.pid());
        let control = SystemControl { sessions_dir };
        let found = control.identity(sleeper.pid()).expect("identity is read");
        assert_eq!(found.session_id, ID);
        assert_eq!(found.proc_start, session.proc_start);
        assert_eq!(found.started_at, session.proc_start);
        assert!(found.command.starts_with("sleep"), "{}", found.command);
        // Un `pid` sans fichier de session n'a pas d'identité.
        assert_eq!(control.identity(i32::MAX), None);
        assert!(control.is_alive(sleeper.pid()));
    }

    #[test]
    fn system_control_stops_a_real_child() {
        let scratch = Scratch::new("stop");
        let mut sleeper = Sleeper::spawn();
        let pid = sleeper.pid();
        let (sessions_dir, session) = session_file(&scratch, pid);
        let control = PretendClaude(SystemControl { sessions_dir });
        let timings = StopTimings {
            term_wait: Duration::from_secs(5),
            kill_wait: Duration::from_secs(1),
            poll: Duration::from_millis(20),
        };
        // L'enfant n'est pas récolté pendant l'attente : un zombie compte pour mort.
        assert_eq!(
            stop_session(&session, &control, &timings),
            StopOutcome::Stopped(Signal::Term)
        );
        assert!(!control.is_alive(pid));
        let status = sleeper.0.wait().expect("child is reaped");
        assert!(!status.success());
    }

    #[test]
    fn a_real_child_with_another_start_is_left_alone() {
        let scratch = Scratch::new("restart");
        let sleeper = Sleeper::spawn();
        let pid = sleeper.pid();
        let (sessions_dir, mut session) = session_file(&scratch, pid);
        session.proc_start = "Mon Nov  2 10:00:00 2026".to_string();
        let control = PretendClaude(SystemControl { sessions_dir });
        assert_eq!(
            stop_session(&session, &control, &FAST),
            StopOutcome::Changed
        );
        assert!(control.is_alive(pid));
    }

    #[test]
    fn a_pid_that_is_gone_is_dead() {
        let control = SystemControl {
            sessions_dir: PathBuf::from("/nonexistent"),
        };
        // Un enfant du test, récolté : son `pid` est celui d'un processus disparu.
        let mut child = Command::new("true").spawn().expect("true starts");
        let pid = child.id() as i32;
        child.wait().expect("child is reaped");
        assert!(!control.is_alive(pid));
    }

    #[test]
    fn a_ps_that_cannot_answer_counts_as_alive() {
        let control = SystemControl {
            sessions_dir: PathBuf::from("/nonexistent"),
        };
        // `ps` refuse ce numéro avec un message : on ne sait pas, donc on ne conclut pas à la mort.
        assert!(control.is_alive(i32::MAX));
    }
}
