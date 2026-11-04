//! Arrêter un serveur sans tuer autre chose (spec P4 §5.4, P4-12, P4-R6) : le choix du sous-arbre et des signaux est pur
//! (`stop_target`, `plan_stop`, testés sur des arbres décrits en données) ; `execute_stop` signale, attend le port, puis
//! insiste. `execute_stop` est synchrone et bloquant (au plus `STOP_GRACE` plus une courte attente) : l'appelant le
//! lance sur un fil d'arrière-plan, jamais sur le fil principal.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};

use idema_tmux::server::ListedServerSession;

use super::registry::{PaneOwner, PaneRoots, RowKind, ServerRow};
use super::scan::{Process, TOOL_TIMEOUT, parse_lsof_listen, parse_ps, run_tool};
use crate::hud;

/// Le délai laissé au serveur pour libérer son port après SIGTERM, avant SIGKILL (P4-12).
pub const STOP_GRACE: Duration = Duration::from_secs(5);
/// Le port est sondé à ce rythme pendant le délai.
const POLL_PERIOD: Duration = Duration::from_millis(100);
/// Après SIGKILL, on laisse ce délai au port pour se libérer avant de conclure, en sortant dès qu'il l'est. Le noyau ferme les
/// sockets d'un processus tué presque aussitôt, mais pas dans une durée qu'on ait mesurée : un serveur chargé (des milliers de
/// connexions, un disque qui swappe) ferme plus tard que 100 ms, et conclure « toujours ouvert » à tort refuserait un arrêt
/// réussi. Une seconde borne l'attente sans la sentir : on ne la paie que quand le port reste tenu.
const KILL_SETTLE: Duration = Duration::from_secs(1);
const KILL_SETTLE_STEP: Duration = Duration::from_millis(20);

/// Ce que l'arrêt vise (spec §5.4), relevé au moment du geste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopTarget {
    /// Un serveur démarré par IdemA : le shell de connexion a fait `exec`, le `pane_pid` est le programme lui-même.
    ServerSession { pane_pid: i32 },
    /// Un serveur sous un pane : le processus en écoute et ses descendants, jamais un ancêtre. `pane_root` est la
    /// racine du pane (shell), ancêtre du serveur, qui n'est jamais visée ni elle ni ce qui la sépare du serveur.
    UnderPane { listener: i32, pane_root: i32 },
    /// Un serveur sans pane ancêtre : lui et ses descendants, jamais son parent.
    Bare { listener: i32 },
}

/// Les processus à signaler. `group` n'est posé que si tous les membres du groupe sont dans `pids`.
/// `commands` garde la ligne de commande de chaque PID au moment du relevé : avant SIGKILL, un PID dont la commande a
/// changé est un PID réutilisé et n'est plus touché.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopPlan {
    pub pids: Vec<i32>,
    pub group: Option<i32>,
    pub commands: HashMap<i32, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// Le port est libre : SIGTERM a suffi (ou il l'était déjà).
    Freed,
    /// Le port n'a été libéré qu'après SIGKILL.
    Killed,
    /// Le port a été repris par un autre processus pendant l'attente : l'ancien n'est pas tué pour un port qu'il ne
    /// tient plus.
    Replaced { by: i32 },
    /// L'arrêt n'a pas abouti : le processus en écoute a changé avant tout signal, le relevé a échoué, ou le port
    /// reste ouvert après SIGKILL. `pids` sont ceux du plan encore en vie.
    Refused { pids: Vec<i32> },
}

/// Les PID à ne jamais signaler : init, le noyau, IdemA lui-même.
fn is_protected(pid: i32) -> bool {
    pid <= 1 || pid == std::process::id() as i32
}

/// Le processus, puis ses parents jusqu'à la racine, dans l'ordre. S'arrête sur un parent inconnu du relevé ou
/// sur une boucle.
fn ancestry(pid: i32, processes: &HashMap<i32, Process>) -> Vec<i32> {
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut current = pid;
    while let Some(process) = processes.get(&current) {
        if !seen.insert(current) {
            break;
        }
        chain.push(current);
        if process.ppid <= 1 {
            break;
        }
        current = process.ppid;
    }
    chain
}

/// Le processus et tous ses descendants du relevé, en largeur, enfants par PID croissant. Les PID protégés en sont
/// exclus.
fn subtree(root: i32, processes: &HashMap<i32, Process>) -> Vec<i32> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for process in processes.values() {
        children.entry(process.ppid).or_default().push(process.pid);
    }
    for list in children.values_mut() {
        list.sort_unstable();
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([root]);
    while let Some(pid) = queue.pop_front() {
        if !processes.contains_key(&pid) || is_protected(pid) || !seen.insert(pid) {
            continue;
        }
        out.push(pid);
        queue.extend(children.get(&pid).into_iter().flatten().copied());
    }
    out
}

/// Les programmes d'agent reconnus par leur nom : les lanceurs de `~/.config/idema/config.toml` (`idnma run <agent>`,
/// dont `agy` et `grok`) et les CLI d'agent courants. Né de la revue de la tâche 5 : un agent qui répond en HTTP
/// (rappel OAuth ou IDE de `claude`, doublure `idnma`) apparaît comme un serveur, et l'arrêter le tuerait avec tout
/// son arbre. `caserne` reste pour les agents qu'il a lancés avant le passage à idnma et qui tournent encore.
const AGENT_PROGRAMS: [&str; 10] = [
    "claude",
    "codex",
    "gemini",
    "opencode",
    "aider",
    "idnma",
    "caserne",
    "cursor-agent",
    "agy",
    "grok",
];
/// Les interpréteurs derrière lesquels le vrai programme est le premier argument (`bun idnma run codex`).
const INTERPRETERS: [&str; 5] = ["bun", "node", "python3", "python", "deno"];

fn basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// Les noms de programme d'une ligne de commande : le nom de base de `argv[0]` et, derrière un interpréteur, celui du
/// premier argument qui n'est ni une option ni `run`/`x`. Jamais une sous-chaîne brute : `node server.js` n'est pas
/// `claude` parce qu'il tourne sous lui.
pub(crate) fn program_names(command: &str) -> Vec<&str> {
    let mut tokens = command.split_whitespace();
    let Some(first) = tokens.next().map(basename) else {
        return Vec::new();
    };
    let mut names = vec![first];
    if INTERPRETERS.contains(&first)
        && let Some(script) =
            tokens.find(|token| !token.starts_with('-') && !matches!(*token, "run" | "x"))
    {
        names.push(basename(script));
    }
    names
}

/// La commande ressemble à l'agent : un programme connu, ou le programme de la commande de lancement de l'agent du pane.
pub(crate) fn looks_like_agent(command: &str, agent_command: Option<&str>) -> bool {
    let agent_program = agent_command.and_then(|launch| {
        program_names(launch)
            .into_iter()
            .rev()
            .find(|name| !INTERPRETERS.contains(name))
    });
    program_names(command)
        .into_iter()
        .any(|name| AGENT_PROGRAMS.contains(&name) || agent_program == Some(name))
}

/// Ce que l'arrêt de cette ligne vise, ou `None` quand rien ne peut être arrêté sans toucher le shell d'un pane
/// (le processus en écoute est lui-même la racine d'un pane, ou n'est plus dans le relevé).
///
/// Sous un pane, on ne remonte jamais : l'agent tourne derrière des lanceurs (idnma, caffeinate, npx) que
/// « le plus haut ancêtre sous le pane » emporterait avec lui.
///
/// Refuse aussi (`None`, journal `server stop refused reason=agent`) quand un processus du plan, le processus en
/// écoute ou un descendant, ressemble à l'agent du pane : arrêter un serveur ne doit jamais emporter l'agent.
pub fn stop_target(
    row: &ServerRow,
    processes: &HashMap<i32, Process>,
    roots: &PaneRoots,
    server_sessions: &[ListedServerSession],
) -> Option<StopTarget> {
    let target = find_target(row, processes, roots, server_sessions)?;
    // Une session serveur est le programme configuré par l'utilisateur pour ce serveur : aucun agent n'y est attendu.
    if matches!(target, StopTarget::ServerSession { .. }) {
        return Some(target);
    }
    let agent_command = match target {
        StopTarget::UnderPane { pane_root, .. } => roots
            .panes
            .iter()
            .find(|root| !root.dead && root.pid == pane_root)
            .and_then(|root| root.agent_command.as_deref()),
        _ => None,
    };
    let plan = plan_stop(&target, processes);
    let agents: Vec<i32> = plan
        .pids
        .iter()
        .copied()
        .filter(|pid| {
            processes
                .get(pid)
                .is_some_and(|process| looks_like_agent(&process.command, agent_command))
        })
        .collect();
    if agents.is_empty() {
        return Some(target);
    }
    let list: Vec<String> = agents.iter().map(i32::to_string).collect();
    hud::log_event(&format!(
        "server stop refused reason=agent pids={}",
        list.join(",")
    ));
    None
}

fn find_target(
    row: &ServerRow,
    processes: &HashMap<i32, Process>,
    roots: &PaneRoots,
    server_sessions: &[ListedServerSession],
) -> Option<StopTarget> {
    let listener = row.listener_pid?;
    let chain = ancestry(listener, processes);
    if chain.first() != Some(&listener) || is_protected(listener) {
        return None;
    }
    for (index, &pid) in chain.iter().enumerate() {
        // Une session serveur vivante dont le programme est cet ancêtre (ou le processus en écoute lui-même).
        let session = server_sessions.iter().any(|session| {
            !session.dead
                && session.pane_pid == pid
                && match &row.kind {
                    RowKind::Config(key) => {
                        session.project == key.project && session.entry == key.name
                    }
                    RowKind::Detected { .. } => true,
                }
        });
        if session {
            return Some(StopTarget::ServerSession { pane_pid: pid });
        }
        // Une racine morte n'attribue rien : son PID a pu être réutilisé.
        let Some(root) = roots
            .panes
            .iter()
            .find(|root| !root.dead && root.pid == pid)
        else {
            continue;
        };
        if matches!(root.owner, PaneOwner::ServerSession { .. }) {
            return Some(StopTarget::ServerSession { pane_pid: pid });
        }
        // Le serveur est la racine du pane elle-même : rien dessous à arrêter.
        if index == 0 {
            return None;
        }
        return Some(StopTarget::UnderPane {
            listener,
            pane_root: pid,
        });
    }
    Some(StopTarget::Bare { listener })
}

/// Les PID à signaler pour la cible, et le groupe seulement s'il est tout entier dans le sous-arbre.
pub fn plan_stop(target: &StopTarget, processes: &HashMap<i32, Process>) -> StopPlan {
    let top = match *target {
        StopTarget::ServerSession { pane_pid } => Some(pane_pid),
        StopTarget::Bare { listener } => Some(listener),
        StopTarget::UnderPane {
            listener,
            pane_root,
        } => {
            // Le processus en écoute, à condition que la racine du pane soit bien un de ses ancêtres.
            let chain = ancestry(listener, processes);
            chain
                .iter()
                .skip(1)
                .any(|&pid| pid == pane_root)
                .then_some(listener)
        }
    };
    let pids = top.map_or_else(Vec::new, |top| subtree(top, processes));
    let group = pids
        .first()
        .and_then(|first| processes.get(first))
        .map(|process| process.pgid)
        .filter(|&group| group > 1)
        .filter(|&group| {
            let members: Vec<i32> = processes
                .values()
                .filter(|process| process.pgid == group)
                .map(|process| process.pid)
                .collect();
            !members.is_empty() && members.iter().all(|pid| pids.contains(pid))
        });
    let commands = pids
        .iter()
        .filter_map(|pid| {
            processes
                .get(pid)
                .map(|process| (*pid, process.command.clone()))
        })
        .collect();
    StopPlan {
        pids,
        group,
        commands,
    }
}

/// Le PID qui écoute en TCP sur ce port, d'après `lsof` : `Ok(None)` quand rien n'écoute, `Err` quand `lsof`
/// échoue ou dépasse son délai (on ne sait alors pas).
pub fn listener_of(port: u16) -> Result<Option<i32>, String> {
    let filter = format!("-iTCP:{port}");
    let output = run_tool(
        "lsof",
        &["-nP", &filter, "-sTCP:LISTEN", "-F", "pn"],
        TOOL_TIMEOUT,
        &AtomicBool::new(false),
    )?;
    Ok(parse_lsof_listen(&output)
        .into_iter()
        .find(|listener| listener.port == port)
        .map(|listener| listener.pid))
}

/// Le relevé des processus de `ps`, pour revérifier les commandes juste avant SIGKILL.
pub fn current_processes() -> Option<HashMap<i32, Process>> {
    run_tool(
        "ps",
        &["-axo", "pid=,ppid=,pgid=,command="],
        TOOL_TIMEOUT,
        &AtomicBool::new(false),
    )
    .ok()
    .map(|output| parse_ps(&output))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    Term,
    Kill,
}

impl Signal {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Signal::Term => "TERM",
            Signal::Kill => "KILL",
        }
    }

    pub(crate) fn number(self) -> i32 {
        match self {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        }
    }
}

/// Le processus existe encore (un refus de permission compte comme existant).
fn is_alive(pid: i32) -> bool {
    // SAFETY : le signal 0 ne fait que tester l'existence du processus.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// L'échec d'un signal va au journal, sauf ESRCH (déjà parti).
fn log_signal_failure(signal: Signal, target: &str, errno: Option<i32>) {
    if errno != Some(libc::ESRCH) {
        hud::log_event(&format!(
            "server signal failed sig={} {target} errno={}",
            signal.name(),
            errno.unwrap_or(0)
        ));
    }
}

/// Signale les PID, et le groupe quand le plan en désigne un (SIGTERM seulement). ESRCH (déjà parti) n'est pas une
/// erreur. Écrit la ligne `server signal` au journal avant d'envoyer.
fn send(signal: Signal, pids: &[i32], group: Option<i32>) {
    if pids.is_empty() {
        return;
    }
    let list: Vec<String> = pids.iter().map(i32::to_string).collect();
    let group_part = group.map_or_else(String::new, |group| format!(" group={group}"));
    hud::log_event(&format!(
        "server signal sig={} pids={}{group_part}",
        signal.name(),
        list.join(",")
    ));
    // SAFETY : `kill` et `killpg` n'ont d'effet que sur les PID du plan, bornés par `plan_stop` au sous-arbre du
    // serveur ; `group` n'est posé que si tout le groupe est dans ce sous-arbre.
    unsafe {
        if let Some(group) = group
            && libc::killpg(group, signal.number()) != 0
        {
            // L'errno est lu avant tout formatage, qui peut l'écraser.
            let errno = std::io::Error::last_os_error().raw_os_error();
            log_signal_failure(signal, &format!("group={group}"), errno);
        }
        for &pid in pids {
            if libc::kill(pid, signal.number()) != 0 {
                let errno = std::io::Error::last_os_error().raw_os_error();
                log_signal_failure(signal, &format!("pid={pid}"), errno);
            }
        }
    }
}

fn refuse(reason: &str, pids: &[i32]) -> StopOutcome {
    let list: Vec<String> = pids.iter().map(i32::to_string).collect();
    hud::log_event(&format!(
        "server stop refused reason={reason} pids={}",
        list.join(",")
    ));
    StopOutcome::Refused {
        pids: pids.to_vec(),
    }
}

/// Le port est-il toujours tenu par le serveur qu'on arrête ? Le processus en écoute que `lsof` rend est `listener`, ou un
/// autre PID du plan : un serveur qui partage sa socket avec ses enfants (uvicorn `--reload`, werkzeug, gunicorn) est
/// rendu tantôt par le père, tantôt par un enfant, et l'enfant reste à nous tant que le port est ouvert. Seul un PID
/// hors du plan est un autre processus.
fn holds_port(plan: &StopPlan, listener: i32, current: i32) -> bool {
    current == listener || plan.pids.contains(&current)
}

/// Les PID du plan qui vivent encore.
fn alive_of(plan: &StopPlan) -> Vec<i32> {
    plan.pids
        .iter()
        .copied()
        .filter(|&pid| is_alive(pid))
        .collect()
}

/// Le plan réduit à ce que le relevé du moment confirme : les PID encore là sous leur commande d'origine, et le groupe
/// seulement si tous ses membres d'aujourd'hui sont de ces PID. Un PID réutilisé par un autre programme (un agent dont
/// le groupe porte l'ancien numéro) sort du plan, et le groupe avec lui : `killpg` ne touche jamais un processus que le
/// plan ne nomme pas.
fn revalidate(plan: &StopPlan, now: &HashMap<i32, Process>) -> StopPlan {
    let pids: Vec<i32> = plan
        .pids
        .iter()
        .copied()
        .filter(|pid| {
            matches!(
                (now.get(pid), plan.commands.get(pid)),
                (Some(current), Some(before)) if &current.command == before
            )
        })
        .collect();
    let group = plan.group.filter(|&group| {
        let members: Vec<i32> = now
            .values()
            .filter(|process| process.pgid == group)
            .map(|process| process.pid)
            .collect();
        !members.is_empty() && members.iter().all(|pid| pids.contains(pid))
    });
    StopPlan {
        pids,
        group,
        commands: plan.commands.clone(),
    }
}

/// Arrête la session serveur d'IdemA dont aucun port ne répond (un démarrage en cours ou en échec, P4-R7) : même
/// déroulé que `execute_stop`, mais la sortie du serveur ne se lit pas sur un port, elle se lit sur le plan. SIGTERM au
/// sous-arbre du pane de la session, attente (`grace`) que tous les PID du plan soient partis, puis SIGKILL à ceux qui
/// vivent encore et portent toujours leur commande du relevé (un PID réutilisé n'est pas touché). SIGKILL ne passe jamais
/// par le groupe. Un plan vide (le processus est déjà parti) est un arrêt réussi du côté des processus : l'appelant
/// vérifie que la session a bien disparu. Bloquant : hors du fil principal.
///
/// Le plan vient d'un relevé qui peut dater (le dernier relevé réussi est gardé quand un relevé échoue) : `processes_now`
/// est relu juste avant SIGTERM, et le plan est réduit à ce que ce relevé confirme (voir `revalidate`). Un relevé qui
/// échoue refuse l'arrêt sans rien signaler.
pub fn execute_session_stop(
    plan: &StopPlan,
    grace: Duration,
    processes_now: impl Fn() -> Option<HashMap<i32, Process>>,
) -> StopOutcome {
    if plan.pids.is_empty() {
        return StopOutcome::Freed;
    }
    let Some(before_term) = processes_now() else {
        return refuse("scan-failed", &alive_of(plan));
    };
    let plan = &revalidate(plan, &before_term);
    if plan.pids.is_empty() {
        // Plus rien du plan n'est là sous sa commande d'origine : le serveur est parti, ses PID sont peut-être ceux d'autres.
        return StopOutcome::Freed;
    }
    send(Signal::Term, &plan.pids, plan.group);
    let deadline = Instant::now() + grace;
    loop {
        if alive_of(plan).is_empty() {
            return StopOutcome::Freed;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep(POLL_PERIOD.min(deadline - now));
    }
    let Some(now_processes) = processes_now() else {
        return refuse("scan-failed", &alive_of(plan));
    };
    let to_kill: Vec<i32> = plan
        .pids
        .iter()
        .copied()
        .filter(|pid| {
            matches!(
                (now_processes.get(pid), plan.commands.get(pid)),
                (Some(now), Some(before)) if &now.command == before
            )
        })
        .collect();
    send(Signal::Kill, &to_kill, None);
    let settle_deadline = Instant::now() + KILL_SETTLE;
    loop {
        if alive_of(plan).is_empty() {
            return StopOutcome::Killed;
        }
        if Instant::now() >= settle_deadline {
            break;
        }
        thread::sleep(KILL_SETTLE_STEP);
    }
    refuse("still-alive-after-kill", &alive_of(plan))
}

/// SIGTERM au sous-arbre, attente que le port se libère (`grace`, sondé toutes les 100 ms), puis SIGKILL à ceux qui
/// vivent encore si le port est toujours tenu par `listener`. Bloquant : à lancer hors du fil principal.
///
/// Le processus en écoute est relu juste avant le premier signal et avant SIGKILL : si ce n'est plus `listener`
/// (le port a été repris par un autre processus), on ne signale plus rien (Review Focus 2). Un relevé qui échoue
/// avant tout signal refuse l'arrêt ; pendant l'attente, il est ignoré, et à l'échéance sans confirmation que le
/// port est encore tenu par `listener`, SIGKILL n'est pas envoyé. Juste avant SIGKILL, `processes_now` relit `ps` :
/// un PID dont la commande a changé n'est pas touché. SIGKILL ne passe jamais par le groupe.
pub fn execute_stop(
    plan: &StopPlan,
    port: u16,
    listener: i32,
    grace: Duration,
    listener_of: impl Fn(u16) -> Result<Option<i32>, String>,
    processes_now: impl Fn() -> Option<HashMap<i32, Process>>,
) -> StopOutcome {
    match listener_of(port) {
        Ok(Some(current)) if holds_port(plan, listener, current) => {}
        // Plus rien n'écoute : le serveur est déjà parti, il n'y a rien à signaler.
        Ok(None) => return StopOutcome::Freed,
        Ok(Some(_)) => {
            hud::log_event("server stop skipped reason=pid-changed");
            return StopOutcome::Refused {
                pids: plan.pids.clone(),
            };
        }
        Err(_) => return refuse("scan-failed", &plan.pids),
    }
    send(Signal::Term, &plan.pids, plan.group);

    let deadline = Instant::now() + grace;
    let mut held;
    loop {
        match listener_of(port) {
            Ok(Some(current)) if holds_port(plan, listener, current) => held = true,
            Ok(None) => return StopOutcome::Freed,
            Ok(Some(other)) => {
                hud::log_event(&format!("server stop replaced by={other}"));
                return StopOutcome::Replaced { by: other };
            }
            // Un relevé raté ne dit rien du port : on continue d'attendre.
            Err(_) => held = false,
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep(POLL_PERIOD.min(deadline - now));
    }
    if !held {
        let alive: Vec<i32> = plan
            .pids
            .iter()
            .copied()
            .filter(|&pid| is_alive(pid))
            .collect();
        return refuse("scan-failed", &alive);
    }

    // Les PID du plan encore en vie et toujours le même programme (un PID réutilisé aurait une autre commande).
    let Some(now_processes) = processes_now() else {
        let alive: Vec<i32> = plan
            .pids
            .iter()
            .copied()
            .filter(|&pid| is_alive(pid))
            .collect();
        return refuse("scan-failed", &alive);
    };
    let to_kill: Vec<i32> = plan
        .pids
        .iter()
        .copied()
        .filter(|pid| {
            matches!(
                (now_processes.get(pid), plan.commands.get(pid)),
                (Some(now), Some(before)) if &now.command == before
            )
        })
        .collect();
    send(Signal::Kill, &to_kill, None);

    let settle_deadline = Instant::now() + KILL_SETTLE;
    loop {
        match listener_of(port) {
            Ok(Some(current)) if holds_port(plan, listener, current) => {}
            Ok(_) => return StopOutcome::Killed,
            Err(_) => {}
        }
        if Instant::now() >= settle_deadline {
            break;
        }
        thread::sleep(KILL_SETTLE_STEP);
    }
    let alive: Vec<i32> = plan
        .pids
        .iter()
        .copied()
        .filter(|&pid| is_alive(pid))
        .collect();
    refuse("still-open-after-kill", &alive)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Instant;

    use super::super::registry::{Origin, PaneOwner, PaneRoot, RowKind, RowState, ServerKey};
    use super::super::scan::Process;
    use super::*;

    // ---- arbres décrits en données ----

    fn tree(entries: &[(i32, i32, i32, &str)]) -> HashMap<i32, Process> {
        entries
            .iter()
            .map(|&(pid, ppid, pgid, command)| {
                (
                    pid,
                    Process {
                        pid,
                        ppid,
                        pgid,
                        command: command.to_string(),
                    },
                )
            })
            .collect()
    }

    fn row_with(kind: RowKind, listener: Option<i32>) -> ServerRow {
        ServerRow {
            kind,
            project: PathBuf::from("/work/alpha"),
            name: "web".into(),
            command: "node server.js".into(),
            port: Some(3000),
            url: "http://localhost:3000".into(),
            state: RowState::Stopped,
            origin: Some(Origin::Project),
            listener_pid: listener,
            output: None,
            session_alive: false,
            extra_ports: Vec::new(),
            failure: None,
        }
    }

    fn detected_row(listener: i32) -> ServerRow {
        row_with(
            RowKind::Detected {
                pid: listener,
                port: 3000,
            },
            Some(listener),
        )
    }

    fn key() -> ServerKey {
        ServerKey {
            project: PathBuf::from("/work/alpha"),
            name: "web".into(),
        }
    }

    fn pane(pid: i32) -> PaneRoot {
        PaneRoot {
            pid,
            owner: PaneOwner::Session {
                session: "alpha-1".into(),
                display_name: "alpha".into(),
                project: PathBuf::from("/work/alpha"),
            },
            agent_command: None,
            dead: false,
        }
    }

    /// Un pane dont l'agent a été lancé par `idnma run codex`.
    fn agent_pane(pid: i32) -> PaneRoot {
        PaneRoot {
            agent_command: Some("idnma run codex".into()),
            ..pane(pid)
        }
    }

    fn roots(panes: Vec<PaneRoot>) -> PaneRoots {
        PaneRoots { panes }
    }

    fn server_session(pane_pid: i32, dead: bool) -> ListedServerSession {
        ListedServerSession {
            name: "idema-server-1".into(),
            project: PathBuf::from("/work/alpha"),
            entry: "web".into(),
            pane: idema_tmux::protocol::PaneId(7),
            pane_pid,
            dead,
        }
    }

    // Le vrai arbre d'un agent sur la machine de l'utilisateur : la doublure d'idnma est entre le shell du pane et l'agent.
    // zsh(100, pane) -> bun idnma(105) -> codex(110) -> zsh -c(115) -> npm(120) -> node(130, écoute)
    fn idnma_tree() -> HashMap<i32, Process> {
        tree(&[
            (1, 0, 1, "launchd"),
            (100, 1, 100, "zsh"),
            (105, 100, 105, "bun /Users/me/.bun/bin/idnma run codex"),
            (110, 105, 105, "codex"),
            (115, 110, 115, "zsh -c npm run dev"),
            (120, 115, 115, "npm run dev"),
            (130, 120, 115, "node server.js"),
        ])
    }

    fn pids_for(
        processes: &HashMap<i32, Process>,
        listener: i32,
        pane_roots: &PaneRoots,
    ) -> Vec<i32> {
        let target = stop_target(&detected_row(listener), processes, pane_roots, &[])
            .expect("a target is found");
        plan_stop(&target, processes).pids
    }

    #[test]
    fn a_pane_server_stops_below_the_shell_and_the_agent() {
        // Derrière un lanceur d'agent (idnma) : le node seul, ni npm ni codex.
        let processes = idnma_tree();
        assert_eq!(
            pids_for(&processes, 130, &roots(vec![pane(100)])),
            vec![130]
        );

        // Un npm lancé à la main dans le shell : le node seul, npm sort de lui-même.
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (100, 1, 100, "zsh"),
            (120, 100, 120, "npm run dev"),
            (130, 120, 120, "node server.js"),
        ]);
        assert_eq!(
            pids_for(&processes, 130, &roots(vec![pane(100)])),
            vec![130]
        );

        // L'agent lance le node lui-même.
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (100, 1, 100, "zsh"),
            (110, 100, 110, "claude"),
            (130, 110, 130, "node server.js"),
        ]);
        assert_eq!(
            pids_for(&processes, 130, &roots(vec![pane(100)])),
            vec![130]
        );

        // Agent annoncé mais parti : le serveur est l'enfant direct du shell, ce n'est pas la racine du pane.
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (100, 1, 100, "zsh"),
            (140, 100, 140, "python3 -m http.server 8811"),
        ]);
        assert_eq!(
            pids_for(&processes, 140, &roots(vec![pane(100)])),
            vec![140]
        );
    }

    #[test]
    fn a_pane_server_takes_its_descendants_and_nothing_above() {
        let mut processes = idnma_tree();
        processes.extend(tree(&[(131, 130, 115, "node worker.js")]));
        let pids = pids_for(&processes, 130, &roots(vec![pane(100)]));
        assert_eq!(pids, vec![130, 131]);
        for spared in [100, 105, 110, 115, 120] {
            assert!(!pids.contains(&spared), "{spared} must be spared");
        }
    }

    #[test]
    fn a_listener_that_is_the_pane_root_has_no_target() {
        // Le shell du pane écoute : rien à arrêter sans le tuer.
        let processes = tree(&[(100, 1, 100, "zsh"), (110, 100, 110, "claude")]);
        let panes = roots(vec![pane(100)]);
        assert_eq!(
            stop_target(&detected_row(100), &processes, &panes, &[]),
            None
        );
    }

    #[test]
    fn an_under_pane_target_needs_the_root_to_be_an_ancestor() {
        let processes = idnma_tree();
        let stray = plan_stop(
            &StopTarget::UnderPane {
                listener: 130,
                pane_root: 999,
            },
            &processes,
        );
        assert!(stray.pids.is_empty());
        let itself = plan_stop(
            &StopTarget::UnderPane {
                listener: 130,
                pane_root: 130,
            },
            &processes,
        );
        assert!(itself.pids.is_empty());
    }

    #[test]
    fn a_plan_keeps_the_command_of_each_pid() {
        let processes = idnma_tree();
        let plan = plan_stop(&StopTarget::Bare { listener: 130 }, &processes);
        assert_eq!(
            plan.commands.get(&130).map(String::as_str),
            Some("node server.js")
        );
    }

    #[test]
    fn a_dead_pane_root_does_not_claim_a_process() {
        // Le PID du shell d'un pane mort a pu être réutilisé : il n'est plus un ancêtre protégé, ni un ancêtre.
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (100, 1, 100, "something"),
            (130, 100, 130, "node server.js"),
        ]);
        let mut dead_pane = pane(100);
        dead_pane.dead = true;
        let target = stop_target(&detected_row(130), &processes, &roots(vec![dead_pane]), &[]);
        assert_eq!(target, Some(StopTarget::Bare { listener: 130 }));
    }

    #[test]
    fn a_server_started_by_idema_stops_as_its_session_command() {
        // La session serveur : le shell de connexion a fait `exec`, le `pane_pid` est le programme.
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (300, 1, 300, "npm run dev"),
            (310, 300, 300, "node server.js"),
        ]);
        let row = row_with(RowKind::Config(key()), Some(310));
        let target = stop_target(
            &row,
            &processes,
            &roots(vec![]),
            &[server_session(300, false)],
        );
        assert_eq!(target, Some(StopTarget::ServerSession { pane_pid: 300 }));
        let plan = plan_stop(&target.unwrap(), &processes);
        assert_eq!(plan.pids, vec![300, 310]);
        assert_eq!(plan.group, Some(300));
    }

    #[test]
    fn a_server_session_of_another_entry_or_a_dead_one_is_not_this_target() {
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (300, 1, 300, "node"),
            (310, 300, 300, "node"),
        ]);
        let row = row_with(RowKind::Config(key()), Some(310));
        let mut other = server_session(300, false);
        other.entry = "api".into();
        let dead = server_session(300, true);
        for sessions in [vec![other], vec![dead]] {
            let target = stop_target(&row, &processes, &roots(vec![]), &sessions);
            assert_eq!(target, Some(StopTarget::Bare { listener: 310 }));
        }
    }

    #[test]
    fn a_server_pane_root_is_a_server_session_target() {
        // La racine d'un pane de session serveur connue par le registre, sans la liste tmux.
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (300, 1, 300, "node"),
            (310, 300, 300, "node"),
        ]);
        let root = PaneRoot {
            pid: 300,
            owner: PaneOwner::ServerSession { key: key() },
            agent_command: None,
            dead: false,
        };
        let target = stop_target(&detected_row(310), &processes, &roots(vec![root]), &[]);
        assert_eq!(target, Some(StopTarget::ServerSession { pane_pid: 300 }));
    }

    #[test]
    fn a_row_without_a_live_listener_has_no_target() {
        let processes = idnma_tree();
        let none = row_with(RowKind::Config(key()), None);
        assert_eq!(stop_target(&none, &processes, &roots(vec![]), &[]), None);
        let unknown = detected_row(999);
        assert_eq!(stop_target(&unknown, &processes, &roots(vec![]), &[]), None);
    }

    #[test]
    fn a_launcher_sharing_its_group_is_never_killed_by_group() {
        // Le lanceur (sh, 200) et le serveur (python, 300) partagent le groupe 200 : le lanceur est hors du sous-arbre.
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (100, 1, 100, "zsh"),
            (200, 100, 200, "sh launcher.sh"),
            (300, 200, 200, "python3 -m http.server"),
        ]);
        let target = stop_target(&detected_row(300), &processes, &roots(vec![]), &[]).unwrap();
        let plan = plan_stop(&target, &processes);
        assert_eq!(plan.pids, vec![300]);
        assert_eq!(plan.group, None);
    }

    #[test]
    fn a_bare_server_spares_its_parent() {
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (200, 1, 200, "supervisor"),
            (300, 200, 300, "node server.js"),
            (310, 300, 300, "node worker.js"),
        ]);
        let target = stop_target(&detected_row(300), &processes, &roots(vec![]), &[]).unwrap();
        assert_eq!(target, StopTarget::Bare { listener: 300 });
        let plan = plan_stop(&target, &processes);
        assert_eq!(plan.pids, vec![300, 310]);
        assert!(!plan.pids.contains(&200));
    }

    #[test]
    fn an_isolated_group_is_signalled_as_a_group() {
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (100, 1, 100, "zsh"),
            (300, 100, 300, "node server.js"),
            (310, 300, 300, "node worker.js"),
        ]);
        let target = stop_target(&detected_row(300), &processes, &roots(vec![]), &[]).unwrap();
        let plan = plan_stop(&target, &processes);
        assert_eq!(plan.pids, vec![300, 310]);
        assert_eq!(plan.group, Some(300));
    }

    #[test]
    fn a_group_with_a_stranger_member_is_not_signalled_as_a_group() {
        // Un processus étranger au sous-arbre dans le groupe du serveur : pas de killpg.
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (300, 1, 300, "node server.js"),
            (900, 1, 300, "unrelated"),
        ]);
        let plan = plan_stop(&StopTarget::Bare { listener: 300 }, &processes);
        assert_eq!(plan.pids, vec![300]);
        assert_eq!(plan.group, None);
    }

    #[test]
    fn init_and_ourselves_are_never_in_a_plan() {
        let me = std::process::id() as i32;
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (300, 1, 300, "node server.js"),
            (me, 300, 300, "idema"),
        ]);
        let plan = plan_stop(&StopTarget::Bare { listener: 300 }, &processes);
        assert_eq!(plan.pids, vec![300]);
        assert_eq!(plan.group, None);
        let init = plan_stop(&StopTarget::Bare { listener: 1 }, &processes);
        assert!(init.pids.is_empty());
    }

    #[test]
    fn a_cyclic_or_broken_tree_does_not_loop() {
        let processes = tree(&[(200, 300, 200, "a"), (300, 200, 300, "b")]);
        let plan = plan_stop(&StopTarget::Bare { listener: 300 }, &processes);
        assert_eq!(plan.pids, vec![300, 200]);
        let broken = plan_stop(
            &StopTarget::UnderPane {
                listener: 300,
                pane_root: 100,
            },
            &processes,
        );
        assert!(broken.pids.is_empty());
    }

    #[test]
    fn an_agent_that_listens_is_never_stopped() {
        let processes = idnma_tree();
        let panes = roots(vec![agent_pane(100)]);
        // L'agent lui-même écoute (rappel OAuth, pont IDE).
        assert_eq!(
            stop_target(&detected_row(110), &processes, &panes, &[]),
            None
        );
        // Son lanceur écoute.
        assert_eq!(
            stop_target(&detected_row(105), &processes, &panes, &[]),
            None
        );
        // Un serveur lancé par l'agent reste arrêtable : le node seul.
        let target = stop_target(&detected_row(130), &processes, &panes, &[]).unwrap();
        assert_eq!(plan_stop(&target, &processes).pids, vec![130]);
    }

    #[test]
    fn the_agent_is_recognised_without_knowing_its_launch_command() {
        // `agent_command` encore inconnu (None) : les noms de programme connus suffisent.
        let processes = idnma_tree();
        let panes = roots(vec![pane(100)]);
        assert_eq!(
            stop_target(&detected_row(110), &processes, &panes, &[]),
            None
        );
        assert_eq!(
            stop_target(&detected_row(105), &processes, &panes, &[]),
            None
        );
        // Même sans pane (serveur nu).
        assert_eq!(
            stop_target(&detected_row(110), &processes, &roots(vec![]), &[]),
            None
        );
    }

    #[test]
    fn a_custom_agent_launcher_is_recognised_by_its_command() {
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (100, 1, 100, "zsh"),
            (105, 100, 105, "bun /opt/tools/myagent run x"),
            (130, 105, 105, "node server.js"),
        ]);
        let custom = PaneRoot {
            agent_command: Some("myagent run x".into()),
            ..pane(100)
        };
        assert_eq!(
            stop_target(&detected_row(105), &processes, &roots(vec![custom]), &[]),
            None
        );
        // Sans sa commande de lancement, ce lanceur inconnu ne se distingue pas d'un serveur.
        assert!(
            stop_target(&detected_row(105), &processes, &roots(vec![pane(100)]), &[]).is_some()
        );
    }

    #[test]
    fn a_descendant_that_is_the_agent_refuses_the_whole_stop() {
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (100, 1, 100, "zsh"),
            (130, 100, 130, "node server.js"),
            (131, 130, 130, "claude --resume"),
        ]);
        assert_eq!(
            stop_target(&detected_row(130), &processes, &roots(vec![pane(100)]), &[]),
            None
        );
    }

    #[test]
    fn a_server_is_not_an_agent_because_of_a_substring() {
        // Le nom de base du programme ou du script compte, pas une sous-chaîne de la ligne de commande.
        let processes = tree(&[
            (1, 0, 1, "launchd"),
            (100, 1, 100, "zsh"),
            (
                130,
                100,
                130,
                "node /work/idnma-app/server.js --claude-mode",
            ),
            (140, 100, 140, "python3 -m http.server 8811"),
        ]);
        let panes = roots(vec![agent_pane(100)]);
        let target = stop_target(&detected_row(130), &processes, &panes, &[]).unwrap();
        assert_eq!(plan_stop(&target, &processes).pids, vec![130]);
        // Un pane sans agent : le serveur Python est arrêtable.
        let plain = roots(vec![pane(100)]);
        let target = stop_target(&detected_row(140), &processes, &plain, &[]).unwrap();
        assert_eq!(plan_stop(&target, &processes).pids, vec![140]);
    }

    #[test]
    fn program_names_follow_the_interpreter() {
        assert!(looks_like_agent("/usr/local/bin/claude --resume", None));
        assert!(looks_like_agent("node /usr/local/bin/codex", None));
        assert!(looks_like_agent("python3 -m aider", None));
        assert!(looks_like_agent(
            "bun /Users/me/.bun/bin/idnma run codex",
            None
        ));
        assert!(!looks_like_agent("node server.js", None));
        assert!(!looks_like_agent("python3 -m http.server 8811", None));
        assert!(!looks_like_agent("bun run dev", None));
        // Un interpréteur seul comme commande de lancement ne désigne pas tous les `node`.
        assert!(!looks_like_agent("node server.js", Some("node")));
    }

    // ---- vrais processus jetables ----

    /// Un enfant du test, tué et récolté à la chute du garde même quand le test échoue.
    struct Spawned {
        pid: i32,
        reaped: Arc<AtomicBool>,
        group: bool,
    }

    impl Spawned {
        fn alive(&self) -> bool {
            !self.reaped.load(Ordering::SeqCst) && is_alive(self.pid)
        }
    }

    impl Drop for Spawned {
        fn drop(&mut self) {
            if !self.reaped.load(Ordering::SeqCst) {
                // SAFETY : le PID (ou son groupe, quand l'enfant en est le chef) est celui d'un enfant du test, pas encore récolté.
                unsafe {
                    if self.group {
                        libc::killpg(self.pid, libc::SIGKILL);
                    }
                    libc::kill(self.pid, libc::SIGKILL);
                }
            }
        }
    }

    fn adopt(mut child: Child, group: bool) -> Spawned {
        let pid = child.id() as i32;
        let reaped = Arc::new(AtomicBool::new(false));
        let flag = reaped.clone();
        // Récolte l'enfant dès qu'il sort : un zombie répondrait encore au signal 0.
        thread::spawn(move || {
            let _ = child.wait();
            flag.store(true, Ordering::SeqCst);
        });
        Spawned { pid, reaped, group }
    }

    fn free_port() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    fn port_open(port: u16) -> bool {
        TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            Duration::from_millis(200),
        )
        .is_ok()
    }

    fn wait_for_port(port: u16) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !port_open(port) {
            assert!(Instant::now() < deadline, "the test server never listened");
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Attend qu'un PID qui n'est pas un enfant du test disparaisse.
    fn wait_until_gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !is_alive(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Le PID qui écoute, d'après `connect` : pas de `lsof` ici, la fonction réelle a son propre test.
    fn probe(port: u16, pid: i32) -> Result<Option<i32>, String> {
        Ok(port_open(port).then_some(pid))
    }

    /// `python3 -m http.server` qui honore SIGTERM.
    fn spawn_polite(port: u16) -> Spawned {
        let child = Command::new("python3")
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let server = adopt(child, false);
        wait_for_port(port);
        server
    }

    /// Un `sh` qui ignore SIGTERM (`trap '' TERM`) puis devient le serveur : l'ignorance survit à `exec`.
    fn spawn_stubborn(port: u16) -> Spawned {
        let script = format!("trap '' TERM; exec python3 -m http.server {port} --bind 127.0.0.1");
        let child = Command::new("sh")
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let server = adopt(child, false);
        wait_for_port(port);
        server
    }

    /// Un serveur qui ferme son port sur SIGTERM mais continue de vivre : SIGKILL n'a alors aucune raison d'être.
    fn spawn_port_closer(port: u16) -> Spawned {
        let script = format!(
            "import socket, signal, time\n\
             s = socket.socket()\n\
             s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n\
             s.bind(('127.0.0.1', {port}))\n\
             s.listen(8)\n\
             signal.signal(signal.SIGTERM, lambda *a: s.close())\n\
             while True:\n\
             \x20   time.sleep(0.05)\n"
        );
        let child = Command::new("python3")
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let server = adopt(child, false);
        wait_for_port(port);
        server
    }

    /// Le plan de l'arrêt d'un seul processus, relevé sur le vrai `ps` comme le fera l'appelant.
    fn plan_of(pid: i32) -> StopPlan {
        let processes = current_processes().expect("ps works");
        plan_stop(&StopTarget::Bare { listener: pid }, &processes)
    }

    fn stop(plan: &StopPlan, port: u16, pid: i32, grace: Duration) -> StopOutcome {
        execute_stop(
            plan,
            port,
            pid,
            grace,
            |port| probe(port, pid),
            current_processes,
        )
    }

    #[test]
    fn a_polite_server_is_freed_by_sigterm() {
        let port = free_port();
        let server = spawn_polite(port);
        let started = Instant::now();
        let outcome = stop(
            &plan_of(server.pid),
            port,
            server.pid,
            Duration::from_secs(5),
        );
        assert_eq!(outcome, StopOutcome::Freed);
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(!port_open(port));
    }

    #[test]
    fn execute_stop_sends_kill_only_if_the_port_is_still_open() {
        // Un serveur têtu : SIGTERM ignoré, le port reste ouvert, SIGKILL le ferme après le délai.
        let port = free_port();
        let stubborn = spawn_stubborn(port);
        let grace = Duration::from_millis(1200);
        let started = Instant::now();
        let outcome = stop(&plan_of(stubborn.pid), port, stubborn.pid, grace);
        assert_eq!(outcome, StopOutcome::Killed);
        assert!(
            started.elapsed() >= grace,
            "SIGKILL arrived before the grace"
        );
        assert!(!port_open(port));

        // Un serveur qui ferme son port sur SIGTERM sans mourir : le port est libre, pas de SIGKILL, il vit encore.
        let port = free_port();
        let closer = spawn_port_closer(port);
        let outcome = stop(&plan_of(closer.pid), port, closer.pid, grace);
        assert_eq!(outcome, StopOutcome::Freed);
        assert!(!port_open(port));
        assert!(
            closer.alive(),
            "SIGKILL was sent although the port was free"
        );
    }

    #[test]
    fn a_changed_listener_is_not_stopped() {
        // Le port est repris par un autre PID avant le signal : rien n'est envoyé.
        let port = free_port();
        let server = spawn_polite(port);
        let outcome = execute_stop(
            &plan_of(server.pid),
            port,
            server.pid,
            Duration::from_millis(500),
            |port| probe(port, server.pid + 100_000),
            current_processes,
        );
        assert_eq!(
            outcome,
            StopOutcome::Refused {
                pids: vec![server.pid]
            }
        );
        thread::sleep(Duration::from_millis(300));
        assert!(server.alive() && port_open(port), "a signal was sent");
    }

    #[test]
    fn a_listener_replaced_during_the_grace_is_not_killed() {
        // Le serveur têtu garde SIGTERM ignoré ; un autre PID prend le port pendant l'attente : pas de SIGKILL.
        let port = free_port();
        let stubborn = spawn_stubborn(port);
        let calls = Cell::new(0);
        let outcome = execute_stop(
            &plan_of(stubborn.pid),
            port,
            stubborn.pid,
            Duration::from_secs(3),
            |port| {
                calls.set(calls.get() + 1);
                if calls.get() <= 2 {
                    probe(port, stubborn.pid)
                } else {
                    Ok(Some(stubborn.pid + 100_000))
                }
            },
            current_processes,
        );
        assert_eq!(
            outcome,
            StopOutcome::Replaced {
                by: stubborn.pid + 100_000
            }
        );
        assert!(
            stubborn.alive(),
            "the old process was killed for a port it no longer holds"
        );
    }

    #[test]
    fn a_port_already_free_is_freed_without_a_signal() {
        let port = free_port();
        let server = spawn_polite(port);
        let outcome = execute_stop(
            &plan_of(server.pid),
            port,
            server.pid,
            Duration::from_millis(500),
            |_| Ok(None),
            current_processes,
        );
        assert_eq!(outcome, StopOutcome::Freed);
        assert!(server.alive());
    }

    #[test]
    fn a_failed_scan_before_the_first_signal_refuses_the_stop() {
        let port = free_port();
        let server = spawn_polite(port);
        let outcome = execute_stop(
            &plan_of(server.pid),
            port,
            server.pid,
            Duration::from_millis(500),
            |_| Err("lsof timed out".to_string()),
            current_processes,
        );
        assert_eq!(
            outcome,
            StopOutcome::Refused {
                pids: vec![server.pid]
            }
        );
        thread::sleep(Duration::from_millis(300));
        assert!(server.alive() && port_open(port), "a signal was sent");
    }

    #[test]
    fn a_failed_scan_during_the_grace_does_not_end_the_wait() {
        // Deux relevés ratés après SIGTERM : on continue d'attendre, puis le port libre est constaté.
        let port = free_port();
        let server = spawn_polite(port);
        let calls = Cell::new(0);
        let outcome = execute_stop(
            &plan_of(server.pid),
            port,
            server.pid,
            Duration::from_secs(5),
            |port| {
                calls.set(calls.get() + 1);
                match calls.get() {
                    1 => probe(port, server.pid),
                    2 | 3 => Err("lsof failed".to_string()),
                    _ => probe(port, server.pid),
                }
            },
            current_processes,
        );
        assert_eq!(outcome, StopOutcome::Freed);
        assert!(calls.get() >= 4, "the wait stopped on a failed scan");
    }

    #[test]
    fn no_kill_is_sent_when_the_port_cannot_be_confirmed_at_the_deadline() {
        let port = free_port();
        let stubborn = spawn_stubborn(port);
        let calls = Cell::new(0);
        let outcome = execute_stop(
            &plan_of(stubborn.pid),
            port,
            stubborn.pid,
            Duration::from_millis(400),
            |port| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    probe(port, stubborn.pid)
                } else {
                    Err("lsof failed".to_string())
                }
            },
            current_processes,
        );
        assert_eq!(
            outcome,
            StopOutcome::Refused {
                pids: vec![stubborn.pid]
            }
        );
        assert!(
            stubborn.alive() && port_open(port),
            "SIGKILL was sent blind"
        );
    }

    #[test]
    fn a_pid_whose_command_changed_is_not_killed() {
        // Entre le relevé du plan et SIGKILL, le PID porte une autre commande : c'est un PID réutilisé.
        let port = free_port();
        let stubborn = spawn_stubborn(port);
        let plan = plan_of(stubborn.pid);
        let outcome = execute_stop(
            &plan,
            port,
            stubborn.pid,
            Duration::from_millis(300),
            |port| probe(port, stubborn.pid),
            || {
                let mut processes = current_processes()?;
                processes.get_mut(&stubborn.pid)?.command = "something else".into();
                Some(processes)
            },
        );
        assert_eq!(
            outcome,
            StopOutcome::Refused {
                pids: vec![stubborn.pid]
            }
        );
        assert!(
            stubborn.alive() && port_open(port),
            "SIGKILL hit a reused PID"
        );
    }

    #[test]
    fn a_stop_leaves_the_launcher_sharing_the_group_alive() {
        // Le lanceur (sh) est chef de son groupe et le serveur en est un membre : l'arrêt du serveur épargne le lanceur.
        let port = free_port();
        let script =
            format!("python3 -m http.server {port} --bind 127.0.0.1 & while :; do sleep 1; done");
        let child = Command::new("sh")
            .args(["-c", &script])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let launcher = adopt(child, true);
        wait_for_port(port);

        let processes = current_processes().expect("ps works");
        let server_pid = processes
            .values()
            .find(|process| process.ppid == launcher.pid && process.command.contains("http.server"))
            .expect("the server is a child of the launcher")
            .pid;

        let plan = plan_stop(
            &StopTarget::Bare {
                listener: server_pid,
            },
            &processes,
        );
        assert_eq!(plan.pids, vec![server_pid]);
        assert_eq!(plan.group, None, "the launcher shares the group");

        let outcome = stop(&plan, port, server_pid, Duration::from_secs(5));
        assert_eq!(outcome, StopOutcome::Freed);
        assert!(launcher.alive(), "the launcher died with its server");
    }

    #[test]
    fn an_isolated_group_dies_as_a_group() {
        // Un serveur et son enfant, seuls dans leur groupe (`process_group(0)`) : le plan désigne le groupe, et les
        // deux meurent.
        let port = free_port();
        let script = format!("sleep 300 & exec python3 -m http.server {port} --bind 127.0.0.1");
        let child = Command::new("sh")
            .args(["-c", &script])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let server = adopt(child, true);
        wait_for_port(port);

        let processes = current_processes().expect("ps works");
        let sleeper = processes
            .values()
            .find(|process| process.ppid == server.pid && process.command.contains("sleep"))
            .expect("the server has a sleeping child")
            .pid;
        let plan = plan_stop(
            &StopTarget::Bare {
                listener: server.pid,
            },
            &processes,
        );
        assert_eq!(plan.pids, vec![server.pid, sleeper]);
        assert_eq!(plan.group, Some(server.pid));

        let outcome = stop(&plan, port, server.pid, Duration::from_secs(5));
        assert_eq!(outcome, StopOutcome::Freed);
        assert!(wait_until_gone(server.pid), "the server survived");
        assert!(wait_until_gone(sleeper), "the server's child survived");
    }

    /// Un plan à la main sur de vrais PID : leurs commandes sont celles du `ps` d'à l'instant.
    fn plan_over(pids: &[i32]) -> StopPlan {
        let processes = current_processes().expect("ps works");
        StopPlan {
            pids: pids.to_vec(),
            group: None,
            commands: pids
                .iter()
                .filter_map(|pid| {
                    processes
                        .get(pid)
                        .map(|process| (*pid, process.command.clone()))
                })
                .collect(),
        }
    }

    fn spawn_sleeper() -> Spawned {
        let child = Command::new("sleep")
            .arg("300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        adopt(child, false)
    }

    #[test]
    fn a_child_of_the_plan_reported_as_the_listener_still_holds_the_port() {
        // Une socket partagée avec un enfant (uvicorn --reload, werkzeug, gunicorn) : `lsof` rend l'enfant, qui est du plan.
        // L'arrêt n'est pas « remplacé » : le port est tenu, SIGKILL part et le libère.
        let port = free_port();
        let stubborn = spawn_stubborn(port);
        let sibling = spawn_sleeper();
        let plan = plan_over(&[stubborn.pid, sibling.pid]);
        let calls = Cell::new(0);
        let outcome = execute_stop(
            &plan,
            port,
            stubborn.pid,
            Duration::from_millis(400),
            |port| {
                calls.set(calls.get() + 1);
                if !stubborn.alive() {
                    return Ok(None);
                }
                // Le père ouvre le port, mais `lsof` nomme l'enfant dès que le signal est parti.
                let reported = if calls.get() == 1 {
                    stubborn.pid
                } else {
                    sibling.pid
                };
                Ok(port_open(port).then_some(reported))
            },
            current_processes,
        );
        assert_eq!(outcome, StopOutcome::Killed);
        assert!(wait_until_gone(stubborn.pid), "the server survived");
    }

    #[test]
    fn a_port_closed_late_after_sigkill_is_still_a_killed_server() {
        // Le noyau ferme les sockets du processus tué après un délai : 400 ms ici, au-delà de l'ancien filet de 100 ms.
        let port = free_port();
        let stubborn = spawn_stubborn(port);
        let gone_since: Cell<Option<Instant>> = Cell::new(None);
        let outcome = execute_stop(
            &plan_of(stubborn.pid),
            port,
            stubborn.pid,
            Duration::from_millis(300),
            |port| {
                if stubborn.alive() {
                    return probe(port, stubborn.pid);
                }
                let since = gone_since.get().unwrap_or_else(|| {
                    gone_since.set(Some(Instant::now()));
                    Instant::now()
                });
                Ok((since.elapsed() < Duration::from_millis(400)).then_some(stubborn.pid))
            },
            current_processes,
        );
        assert_eq!(outcome, StopOutcome::Killed);
    }

    #[test]
    fn a_session_without_a_port_is_stopped_through_its_plan_alone() {
        // Le pane de la session est un `sh` chef de son groupe, avec un enfant : le plan est le sous-arbre, et le groupe
        // entier est dans le plan.
        let child = Command::new("sh")
            .args(["-c", "sleep 300 & wait"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pane = adopt(child, true);
        let deadline = Instant::now() + Duration::from_secs(5);
        let (plan, sleeper) = loop {
            let processes = current_processes().expect("ps works");
            let plan = plan_stop(
                &StopTarget::ServerSession { pane_pid: pane.pid },
                &processes,
            );
            if plan.pids.len() == 2 {
                let sleeper = plan.pids[1];
                break (plan, sleeper);
            }
            assert!(Instant::now() < deadline, "the pane never forked its child");
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(plan.group, Some(pane.pid));
        let outcome = execute_session_stop(&plan, Duration::from_secs(5), current_processes);
        assert_eq!(outcome, StopOutcome::Freed);
        assert!(wait_until_gone(pane.pid), "the pane process survived");
        assert!(wait_until_gone(sleeper), "the pane's child survived");
    }

    #[test]
    fn a_session_whose_program_ignores_sigterm_is_killed_after_the_grace() {
        let child = Command::new("sh")
            .args(["-c", "trap '' TERM; while :; do sleep 1; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pane = adopt(child, false);
        thread::sleep(Duration::from_millis(300));
        let plan = plan_stop(
            &StopTarget::ServerSession { pane_pid: pane.pid },
            &current_processes().expect("ps works"),
        );
        let grace = Duration::from_millis(600);
        let started = Instant::now();
        let outcome = execute_session_stop(&plan, grace, current_processes);
        assert_eq!(outcome, StopOutcome::Killed);
        assert!(
            started.elapsed() >= grace,
            "SIGKILL arrived before the grace"
        );
        assert!(wait_until_gone(pane.pid), "the pane process survived");
    }

    #[test]
    fn a_session_pid_whose_command_changed_is_not_killed() {
        let child = Command::new("sh")
            .args(["-c", "trap '' TERM; while :; do sleep 1; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pane = adopt(child, false);
        thread::sleep(Duration::from_millis(300));
        let plan = plan_over(&[pane.pid]);
        // Le premier relevé (avant SIGTERM) confirme le plan ; le PID est réutilisé pendant le délai.
        let scans = Cell::new(0);
        let outcome = execute_session_stop(&plan, Duration::from_millis(300), || {
            scans.set(scans.get() + 1);
            let mut processes = current_processes()?;
            if scans.get() > 1 {
                processes.get_mut(&pane.pid)?.command = "something else".into();
            }
            Some(processes)
        });
        assert_eq!(
            outcome,
            StopOutcome::Refused {
                pids: vec![pane.pid]
            }
        );
        assert!(pane.alive(), "SIGKILL hit a reused PID");
    }

    #[test]
    fn a_session_stop_whose_scan_fails_before_the_first_signal_signals_nothing() {
        // Un plan non vide et exact (un `sh` chef de groupe et son enfant) : sans relevé pour le confirmer, ni SIGTERM ni
        // `killpg` ne partent, sur le plan périmé non plus.
        let child = Command::new("sh")
            .args(["-c", "sleep 300 & wait"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pane = adopt(child, true);
        let deadline = Instant::now() + Duration::from_secs(5);
        let plan = loop {
            let plan = plan_stop(
                &StopTarget::ServerSession { pane_pid: pane.pid },
                &current_processes().expect("ps works"),
            );
            if plan.pids.len() == 2 {
                break plan;
            }
            assert!(Instant::now() < deadline, "the pane never forked its child");
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(plan.group, Some(pane.pid));
        let sleeper = plan.pids[1];
        let outcome = execute_session_stop(&plan, Duration::from_millis(300), || None);
        assert_eq!(
            outcome,
            StopOutcome::Refused {
                pids: plan.pids.clone()
            }
        );
        thread::sleep(Duration::from_millis(300));
        let leader_alive = pane.alive();
        let sleeper_alive = is_alive(sleeper);
        // La garde ne tue que le chef et son groupe : si le test échoue, l'enfant ne reste pas derrière.
        // SAFETY : `sleeper` est l'enfant `sleep` de ce test.
        unsafe { libc::kill(sleeper, libc::SIGKILL) };
        assert!(leader_alive, "the pane process received a signal");
        assert!(sleeper_alive, "the pane's child received a signal");
    }

    #[test]
    fn a_session_stop_whose_snapshot_is_stale_signals_nothing_of_a_reused_pgid() {
        // Le relevé du plan date : le pane d'alors (pid et groupe 7xx) est parti, et son numéro est celui d'un arbre
        // d'agent (un `sh` chef de groupe avec son enfant). Aucun signal, `killpg` compris, ne doit l'atteindre.
        let agent = Command::new("sh")
            .args(["-c", "sleep 300 & wait"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let agent = adopt(agent, true);
        let deadline = Instant::now() + Duration::from_secs(5);
        let sleeper = loop {
            let processes = current_processes().expect("ps works");
            if let Some(child) = processes.values().find(|process| process.ppid == agent.pid) {
                break child.pid;
            }
            assert!(Instant::now() < deadline, "the agent never forked");
            thread::sleep(Duration::from_millis(50));
        };
        let plan = StopPlan {
            pids: vec![agent.pid],
            group: Some(agent.pid),
            commands: HashMap::from([(agent.pid, "node vite".to_string())]),
        };
        let outcome = execute_session_stop(&plan, Duration::from_millis(300), current_processes);
        assert_eq!(outcome, StopOutcome::Freed);
        thread::sleep(Duration::from_millis(300));
        assert!(agent.alive(), "the reused pid received a signal");
        assert!(is_alive(sleeper), "the reused group received a signal");
    }

    #[test]
    fn a_session_stop_drops_the_group_when_a_member_is_outside_the_plan() {
        // Le chef du groupe est dans le plan, son enfant non : SIGTERM va au chef seul, jamais au groupe.
        let leader = Command::new("sh")
            .args(["-c", "sleep 300 & wait"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let leader = adopt(leader, true);
        let deadline = Instant::now() + Duration::from_secs(5);
        let sleeper = loop {
            let processes = current_processes().expect("ps works");
            if let Some(child) = processes
                .values()
                .find(|process| process.ppid == leader.pid)
            {
                break child.pid;
            }
            assert!(Instant::now() < deadline, "the leader never forked");
            thread::sleep(Duration::from_millis(50));
        };
        let plan = StopPlan {
            group: Some(leader.pid),
            ..plan_over(&[leader.pid])
        };
        let outcome = execute_session_stop(&plan, Duration::from_secs(5), current_processes);
        assert_eq!(outcome, StopOutcome::Freed);
        let leader_gone = wait_until_gone(leader.pid);
        let sleeper_alive = is_alive(sleeper);
        // Le chef parti, la garde ne tue plus le groupe : l'enfant orphelin est à nous de le tuer.
        // SAFETY : `sleeper` est l'enfant `sleep` de ce test.
        unsafe { libc::kill(sleeper, libc::SIGKILL) };
        assert!(leader_gone, "the plan's own process survived");
        assert!(sleeper_alive, "the group received SIGTERM");
    }

    #[test]
    fn a_session_whose_process_is_already_gone_stops_without_a_signal() {
        let plan = StopPlan {
            pids: Vec::new(),
            group: None,
            commands: HashMap::new(),
        };
        assert_eq!(
            execute_session_stop(&plan, Duration::from_millis(100), || None),
            StopOutcome::Freed
        );
    }

    #[test]
    fn listener_of_finds_the_process_listening_on_a_port() {
        let port = free_port();
        assert_eq!(listener_of(port), Ok(None));
        let server = spawn_polite(port);
        assert_eq!(listener_of(port), Ok(Some(server.pid)));
    }
}
