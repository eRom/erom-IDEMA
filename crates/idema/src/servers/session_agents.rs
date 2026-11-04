//! Les agents qui tournent vraiment dans chaque session de la barre, lus dans l'arbre des processus de ses panes : le
//! lanceur choisi à la création (`@idema_agent`) ne dit pas ce qu'on a lancé ensuite à la main, ni qu'un agent est
//! sorti. Un agent se reconnaît à la commande d'une entrée du lanceur (`idnma run codex`) dans la ligne d'un processus.

use std::collections::HashMap;
use std::path::Path;

use crate::config::AgentEntry;

use super::registry::{PaneOwner, PaneRoots};
use super::scan::Process;
use super::stop::looks_like_agent;

/// Par session : les noms des entrées du lanceur qui tournent dans ses panes, sans doublon, dans l'ordre des panes puis
/// des processus. Une entrée reconnue arrête la descente : le `codex` que lance `idnma run codex` n'est pas un second
/// agent.
///
/// `idnma run glm` ne reste pas dans l'arbre : il se remplace par `claude` (`execve`), dont la ligne ne nomme plus
/// l'entrée. Un pane où aucune entrée ne se lit mais où un agent tourne garde donc l'entrée de sa commande de
/// lancement (`agent_command` : le dernier `idnma run` du pane, sinon son lanceur).
pub fn session_agents(
    roots: &PaneRoots,
    processes: &HashMap<i32, Process>,
    launchers: &[AgentEntry],
) -> HashMap<String, Vec<String>> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for process in processes.values() {
        children.entry(process.ppid).or_default().push(process.pid);
    }
    for pids in children.values_mut() {
        pids.sort_unstable();
    }
    let mut agents: HashMap<String, Vec<String>> = HashMap::new();
    for root in &roots.panes {
        let PaneOwner::Session { session, .. } = &root.owner else {
            continue;
        };
        if root.dead {
            continue;
        }
        let found = agents.entry(session.clone()).or_default();
        let agent_command = root.agent_command.as_deref();
        let (mut matched_any, mut agent_running) = (false, false);
        let mut stack = vec![root.pid];
        while let Some(pid) = stack.pop() {
            let process = processes.get(&pid);
            if let Some(entry) =
                process.and_then(|process| launcher_of(&process.command, launchers))
            {
                matched_any = true;
                if !found.contains(&entry.name) {
                    found.push(entry.name.clone());
                }
                continue;
            }
            agent_running |=
                process.is_some_and(|process| looks_like_agent(&process.command, agent_command));
            // Pile : les enfants partent dans l'ordre inverse pour être visités dans l'ordre des PID.
            if let Some(pids) = children.get(&pid) {
                stack.extend(pids.iter().rev());
            }
        }
        if !matched_any && agent_running {
            // `agent_command` est la commande d'une entrée, copiée telle quelle à la lecture du pane.
            let launcher = agent_command
                .and_then(|command| launchers.iter().find(|entry| entry.command == command));
            if let Some(entry) = launcher
                && !found.contains(&entry.name)
            {
                found.push(entry.name.clone());
            }
        }
    }
    agents.retain(|_, found| !found.is_empty());
    agents
}

/// L'entrée dont la commande figure dans la ligne du processus : son programme (comparé par nom de fichier des deux
/// côtés : la ligne porte souvent le chemin complet, derrière un interpréteur, `bun /…/idnma run codex`), suivi de
/// ses arguments à l'identique. Un programme sans argument (`claude`) ne se lit qu'en tête de ligne ou juste derrière
/// l'interpréteur, jamais comme un fichier passé à un autre programme (`vim notes/claude`). Entre deux entrées qui
/// conviennent, la plus précise (le plus d'arguments) l'emporte : `idnma run codex --model x` n'est pas Codex.
fn launcher_of<'a>(command: &str, launchers: &'a [AgentEntry]) -> Option<&'a AgentEntry> {
    let argv: Vec<&str> = command.split_whitespace().collect();
    launchers
        .iter()
        .filter_map(|entry| {
            let words: Vec<&str> = entry.command.split_whitespace().collect();
            let (program, args) = words.split_first()?;
            let program = file_name(program)?;
            let found = argv.iter().enumerate().any(|(index, word)| {
                (!args.is_empty() || index == 0 || (index == 1 && is_interpreter(argv[0])))
                    && file_name(word) == Some(program)
                    && argv.get(index + 1..index + 1 + args.len()) == Some(args)
            });
            found.then_some((args.len(), entry))
        })
        // `max_by_key` rend la dernière des égales : on garde la première de la config.
        .fold(
            None,
            |best: Option<(usize, &AgentEntry)>, candidate| match best {
                Some(kept) if kept.0 >= candidate.0 => Some(kept),
                _ => Some(candidate),
            },
        )
        .map(|(_, entry)| entry)
}

/// Les programmes qui lancent un script donné en premier argument (`node /…/claude`) : derrière eux, ce script est le
/// programme de la ligne.
fn is_interpreter(word: &str) -> bool {
    matches!(
        file_name(word),
        Some(
            "node"
                | "bun"
                | "deno"
                | "python"
                | "python3"
                | "ruby"
                | "perl"
                | "sh"
                | "bash"
                | "zsh"
        )
    )
}

fn file_name(word: &str) -> Option<&str> {
    Path::new(word).file_name()?.to_str()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::servers::registry::PaneRoot;

    fn launcher(name: &str, command: &str) -> AgentEntry {
        AgentEntry {
            name: name.into(),
            harness: None,
            command: command.into(),
            default: false,
            icon: None,
        }
    }

    fn launchers() -> Vec<AgentEntry> {
        vec![
            launcher("Claude", "idnma run claude"),
            launcher("Codex", "idnma run codex"),
            launcher("GLM", "idnma run glm"),
            launcher("Grok", "idnma run grok"),
        ]
    }

    fn processes(list: &[(i32, i32, &str)]) -> HashMap<i32, Process> {
        list.iter()
            .map(|&(pid, ppid, command)| {
                (
                    pid,
                    Process {
                        pid,
                        ppid,
                        pgid: pid,
                        command: command.into(),
                    },
                )
            })
            .collect()
    }

    fn session_pane(pid: i32, session: &str) -> PaneRoot {
        PaneRoot {
            pid,
            owner: PaneOwner::Session {
                session: session.into(),
                display_name: session.into(),
                project: PathBuf::from("/p/alpha"),
            },
            agent_command: None,
            dead: false,
        }
    }

    #[test]
    fn two_agents_in_two_panes_give_both_in_pane_order() {
        let roots = PaneRoots {
            panes: vec![session_pane(10, "s1"), session_pane(20, "s1")],
        };
        let tree = processes(&[
            (10, 1, "-zsh"),
            (11, 10, "bun /Users/me/.bun/bin/idnma run grok"),
            (12, 11, "/opt/homebrew/bin/grok --model grok-4.7"),
            (20, 1, "-zsh"),
            (21, 20, "bun /Users/me/.bun/bin/idnma run codex"),
        ]);
        let agents = session_agents(&roots, &tree, &launchers());
        assert_eq!(agents["s1"], ["Grok", "Codex"]);
    }

    #[test]
    fn an_agent_that_took_the_place_of_its_launcher_keeps_the_entry_of_its_pane() {
        // `idnma run glm` s'est remplacé par `claude` : la ligne ne nomme plus l'entrée, `agent_command` la donne.
        let mut glm = session_pane(10, "s1");
        glm.agent_command = Some("idnma run glm".into());
        let mut resumed = session_pane(20, "s1");
        resumed.agent_command = Some("idnma run claude".into());
        let roots = PaneRoots {
            panes: vec![glm, resumed],
        };
        let tree = processes(&[
            (10, 1, "-zsh"),
            (
                11,
                10,
                "claude --model glm-5.3-flash:cloud[1m] --effort max --name glm-hera-ab12",
            ),
            (12, 11, "zsh -c npm test"),
            (20, 1, "-zsh"),
            (
                21,
                20,
                "/Users/me/.local/bin/claude --model opus --continue",
            ),
        ]);
        assert_eq!(
            session_agents(&roots, &tree, &launchers())["s1"],
            ["GLM", "Claude"]
        );
    }

    #[test]
    fn a_pane_whose_agent_has_left_or_whose_entry_is_gone_shows_nothing() {
        // Agent sorti : le shell seul, ou un programme qui n'est pas un agent, sous le pane lancé « GLM ».
        let mut exited = session_pane(10, "s1");
        exited.agent_command = Some("idnma run glm".into());
        // Entrée retirée du lanceur depuis la création du pane.
        let mut removed = session_pane(20, "s2");
        removed.agent_command = Some("idnma run kimi".into());
        let roots = PaneRoots {
            panes: vec![exited, removed],
        };
        let tree = processes(&[
            (10, 1, "-zsh"),
            (11, 10, "vim notes.md"),
            (20, 1, "-zsh"),
            (21, 20, "claude --model kimi-k3:cloud[1m]"),
        ]);
        assert!(session_agents(&roots, &tree, &launchers()).is_empty());
    }

    #[test]
    fn the_same_agent_twice_counts_once() {
        let roots = PaneRoots {
            panes: vec![session_pane(10, "s1"), session_pane(20, "s1")],
        };
        let tree = processes(&[
            (10, 1, "-zsh"),
            (11, 10, "idnma run codex"),
            (20, 1, "-zsh"),
            (21, 20, "bun /x/idnma run codex"),
        ]);
        assert_eq!(session_agents(&roots, &tree, &launchers())["s1"], ["Codex"]);
    }

    #[test]
    fn what_runs_wins_over_the_launcher_chosen_at_creation() {
        // Lancée « GLM » (`agent_command`), la session fait tourner Codex : c'est Codex.
        let mut pane = session_pane(10, "s1");
        pane.agent_command = Some("idnma run glm".into());
        let roots = PaneRoots { panes: vec![pane] };
        let tree = processes(&[
            (10, 1, "-zsh"),
            (11, 10, "bun /x/idnma run codex"),
            (12, 11, "codex --model gpt-6-luna"),
        ]);
        assert_eq!(session_agents(&roots, &tree, &launchers())["s1"], ["Codex"]);
    }

    #[test]
    fn a_shell_alone_or_a_dead_pane_has_no_agent() {
        let mut dead = session_pane(20, "s2");
        dead.dead = true;
        let roots = PaneRoots {
            panes: vec![session_pane(10, "s1"), dead],
        };
        let tree = processes(&[
            (10, 1, "-zsh"),
            (11, 10, "vim notes.md"),
            (20, 1, "idnma run codex"),
        ]);
        let agents = session_agents(&roots, &tree, &launchers());
        assert!(!agents.contains_key("s1"));
        assert!(!agents.contains_key("s2"));
    }

    #[test]
    fn a_launcher_written_with_its_path_still_matches() {
        let roots = PaneRoots {
            panes: vec![session_pane(10, "s1")],
        };
        let tree = processes(&[(10, 1, "-zsh"), (11, 10, "bun /x/idnma run codex")]);
        let launchers = [launcher("Codex", "~/.bun/bin/idnma run codex")];
        assert_eq!(session_agents(&roots, &tree, &launchers)["s1"], ["Codex"]);
    }

    #[test]
    fn the_most_precise_launcher_wins() {
        let roots = PaneRoots {
            panes: vec![session_pane(10, "s1")],
        };
        let tree = processes(&[
            (10, 1, "-zsh"),
            (11, 10, "bun /x/idnma run codex --engine fast"),
        ]);
        let launchers = [
            launcher("Codex", "idnma run codex"),
            launcher("Codex rapide", "idnma run codex --engine fast"),
        ];
        assert_eq!(
            session_agents(&roots, &tree, &launchers)["s1"],
            ["Codex rapide"]
        );
    }

    #[test]
    fn a_bare_program_is_never_a_file_given_to_another_one() {
        let roots = PaneRoots {
            panes: vec![session_pane(10, "s1"), session_pane(20, "s2")],
        };
        let tree = processes(&[
            (10, 1, "-zsh"),
            (11, 10, "vim /tmp/notes/claude"),
            (20, 1, "-zsh"),
            (21, 20, "node /usr/local/bin/claude --resume"),
        ]);
        let launchers = [launcher("Claude", "claude")];
        let agents = session_agents(&roots, &tree, &launchers);
        assert!(!agents.contains_key("s1"));
        // Derrière son interpréteur, le programme est bien là.
        assert_eq!(agents["s2"], ["Claude"]);
    }

    #[test]
    fn a_command_matches_on_whole_words_only() {
        let roots = PaneRoots {
            panes: vec![session_pane(10, "s1")],
        };
        // `idnma run codex-dev` ni `notidnma run codex` ne sont l'entrée Codex.
        let tree = processes(&[
            (10, 1, "-zsh"),
            (11, 10, "idnma run codex-dev"),
            (12, 10, "notidnma run codex"),
        ]);
        assert!(session_agents(&roots, &tree, &launchers()).is_empty());
    }
}
