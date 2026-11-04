//! La section « Ramener dans ce pane » du menu d'un pane : les sessions qui tournent hors d'IdemA, un sous-menu par projet.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::assets::logo_path;
use crate::config::AgentEntry;
use crate::project::display_name;
use crate::reclaim::discover::OutsideSession;
use crate::servers::model::idnma_entry;

/// Une session à proposer : `enabled` dit si on peut la ramener ici, `logo` celui de son agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimItem {
    pub session: OutsideSession,
    pub enabled: bool,
    pub logo: Option<&'static str>,
}

/// Les sessions d'un même projet : un sous-menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimGroup {
    pub label: String,
    pub root: PathBuf,
    pub items: Vec<ReclaimItem>,
}

/// Vrai si la ligne de commande de l'entrée demande `--model <model>` (ou `--model=<model>`), guillemets ôtés.
fn command_asks_model(command: &str, model: &str) -> bool {
    let unquote = |word: &str| word.trim_matches(|c| c == '\'' || c == '"').to_string();
    let mut words = command.split_whitespace();
    while let Some(word) = words.next() {
        if word == "--model" {
            return words.next().is_some_and(|value| unquote(value) == model);
        }
        if let Some(value) = word.strip_prefix("--model=") {
            return unquote(value) == model;
        }
    }
    false
}

/// L'entrée du lanceur qui correspond à une session : celle de son agent (`idnma run <agent>`) dont la commande porte le
/// modèle de la session, la plus courte si plusieurs. Modèle absent ou sans entrée : le choix de `idnma_entry`.
pub fn agent_entry<'a>(
    agent: &str,
    model: Option<&str>,
    agents: &'a [AgentEntry],
) -> Option<&'a AgentEntry> {
    let by_model = model.and_then(|model| {
        agents
            .iter()
            .filter(|candidate| command_asks_model(&candidate.command, model))
            // `idnma_entry` rend le candidat lui-même quand il lance bien cet agent : c'est le test d'appartenance.
            .filter(|candidate| {
                idnma_entry(agent, Some(candidate), agents)
                    .is_some_and(|found| std::ptr::eq(found, *candidate))
            })
            .min_by_key(|candidate| candidate.command.split_whitespace().count())
    });
    by_model.or_else(|| idnma_entry(agent, None, agents))
}

/// Deux libellés sans égard à la casse, puis à la casse même pour que l'ordre reste total.
fn by_name(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

/// Les sessions hors d'IdemA par projet (groupées par racine : deux projets de même nom restent deux groupes), groupes
/// et sessions triés par nom. Une session ne se ramène que dans son projet, au prompt, et si IdemA sait la relancer ;
/// les autres restent visibles, grisées.
pub fn reclaim_groups(
    sessions: &[OutsideSession],
    current_root: &Path,
    at_prompt: bool,
    agents: &[AgentEntry],
) -> Vec<ReclaimGroup> {
    let mut groups: Vec<ReclaimGroup> = Vec::new();
    for session in sessions {
        let enabled = at_prompt && session.root == current_root && session.resume.is_some();
        let logo = agent_entry(&session.agent, session.model.as_deref(), agents)
            .and_then(|entry| logo_path(&entry.icon_name()))
            .or_else(|| logo_path(&session.agent));
        let item = ReclaimItem {
            session: session.clone(),
            enabled,
            logo,
        };
        match groups.iter_mut().find(|group| group.root == session.root) {
            Some(group) => group.items.push(item),
            None => groups.push(ReclaimGroup {
                label: display_name(&session.root),
                root: session.root.clone(),
                items: vec![item],
            }),
        }
    }
    for group in &mut groups {
        group.items.sort_by(|a, b| {
            by_name(&a.session.name, &b.session.name)
                .then_with(|| a.session.session_id.cmp(&b.session.session_id))
        });
    }
    groups.sort_by(|a, b| by_name(&a.label, &b.label).then_with(|| a.root.cmp(&b.root)));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentEntry;

    fn session(id: &str, name: &str, root: &str) -> OutsideSession {
        OutsideSession {
            pid: 100,
            session_id: id.to_string(),
            proc_start: "Mon Nov  2 10:00:00 2026".to_string(),
            name: name.to_string(),
            cwd: PathBuf::from(root),
            root: PathBuf::from(root),
            agent: "claude".to_string(),
            model: None,
            resume: Some(format!("cd '{root}' && idnma run 'claude' --resume {id}")),
            relaunch: Some("idnma run 'claude'".to_string()),
        }
    }

    fn entry(name: &str, command: &str) -> AgentEntry {
        AgentEntry {
            name: name.to_string(),
            harness: None,
            command: command.to_string(),
            default: false,
            icon: None,
        }
    }

    fn groups(sessions: &[OutsideSession], current: &str, at_prompt: bool) -> Vec<ReclaimGroup> {
        reclaim_groups(sessions, Path::new(current), at_prompt, &[])
    }

    #[test]
    fn no_session_means_no_group() {
        assert!(groups(&[], "/a/x", true).is_empty());
    }

    #[test]
    fn groups_and_sessions_are_sorted_by_name() {
        let sessions = [
            session("1", "zeta", "/p/Beta"),
            session("2", "Alpha", "/p/Beta"),
            session("3", "mid", "/p/alpha"),
            session("4", "beta", "/p/Beta"),
        ];
        let found = groups(&sessions, "/p/Beta", true);
        let labels: Vec<&str> = found.iter().map(|group| group.label.as_str()).collect();
        assert_eq!(labels, ["alpha", "Beta"]);
        let names: Vec<&str> = found[1]
            .items
            .iter()
            .map(|item| item.session.name.as_str())
            .collect();
        assert_eq!(names, ["Alpha", "beta", "zeta"]);
    }

    #[test]
    fn two_projects_with_one_base_name_stay_apart() {
        let sessions = [session("1", "a", "/a/x"), session("2", "b", "/b/x")];
        let found = groups(&sessions, "/a/x", true);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].root, Path::new("/a/x"));
        assert_eq!(found[1].root, Path::new("/b/x"));
    }

    #[test]
    fn a_session_of_another_project_is_disabled() {
        let found = groups(&[session("1", "a", "/b/x")], "/a/x", true);
        assert!(!found[0].items[0].enabled);
    }

    #[test]
    fn a_session_of_this_project_is_enabled() {
        let found = groups(&[session("1", "a", "/a/x")], "/a/x", true);
        assert!(found[0].items[0].enabled);
    }

    #[test]
    fn nothing_is_enabled_away_from_the_prompt() {
        let sessions = [session("1", "a", "/a/x"), session("2", "b", "/b/x")];
        let found = groups(&sessions, "/a/x", false);
        assert!(
            found
                .iter()
                .flat_map(|group| &group.items)
                .all(|item| !item.enabled)
        );
    }

    #[test]
    fn a_session_without_a_resume_is_disabled_in_its_project() {
        let mut orphan = session("1", "a", "/a/x");
        orphan.resume = None;
        let found = groups(&[orphan], "/a/x", true);
        assert!(!found[0].items[0].enabled);
    }

    #[test]
    fn two_sessions_with_one_name_keep_their_own_id() {
        let sessions = [
            session("id-1", "claude-flora", "/a/x"),
            session("id-2", "claude-flora", "/a/x"),
        ];
        let found = groups(&sessions, "/a/x", true);
        assert_eq!(found.len(), 1);
        let mut ids: Vec<&str> = found[0]
            .items
            .iter()
            .map(|item| item.session.session_id.as_str())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, ["id-1", "id-2"]);
    }

    #[test]
    fn the_model_picks_the_entry() {
        let agents = [
            entry("Opus", "idnma run claude --model opus"),
            entry("Sonnet", "idnma run claude --model sonnet"),
        ];
        assert_eq!(
            agent_entry("claude", Some("sonnet"), &agents).map(|e| e.name.as_str()),
            Some("Sonnet")
        );
        assert_eq!(
            agent_entry("claude", Some("opus"), &agents).map(|e| e.name.as_str()),
            Some("Opus")
        );
        // Modèle inconnu : le repli est `idnma_entry` (ici, les deux ont la même longueur, le premier gagne).
        let fallback = agent_entry("claude", Some("haiku"), &agents);
        assert_eq!(
            fallback.map(|e| e.name.as_str()),
            idnma_entry("claude", None, &agents).map(|e| e.name.as_str())
        );
        assert!(fallback.is_some());
        assert!(agent_entry("glm", Some("sonnet"), &agents).is_none());
    }

    #[test]
    fn the_logo_falls_back_on_the_agent_key() {
        let mut one = session("1", "a", "/a/x");
        one.agent = "claude".to_string();
        let found = groups(&[one], "/a/x", true);
        assert_eq!(found[0].items[0].logo, crate::assets::logo_path("claude"));
        assert!(found[0].items[0].logo.is_some());
    }

    #[test]
    fn the_logo_comes_from_the_matching_entry_first() {
        let mut named = entry("Sonnet", "idnma run claude --model sonnet");
        named.icon = Some("gemini".to_string());
        let found = reclaim_groups(
            &[session("1", "a", "/a/x")],
            Path::new("/a/x"),
            true,
            &[named],
        );
        assert_eq!(found[0].items[0].logo, crate::assets::logo_path("gemini"));
    }
}
