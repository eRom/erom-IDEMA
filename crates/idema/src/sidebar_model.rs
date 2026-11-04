//! Le modèle de la barre, sans GPUI : ordre des sessions, navigation au clavier, noms par défaut, pastilles
//! des projets repliés et badge du Dock (spec §4.1, §4.2, §5.3, §5.4, §7.3).

use std::path::{Path, PathBuf};

use idema_term::activity::ActivityState;

/// Une session telle que la barre la montre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// Nom tmux, `idema-<8 hex>` : l'identifiant stable.
    pub name: String,
    pub project: PathBuf,
    pub display_name: String,
    pub created: u128,
    pub state: ActivityState,
    /// Session d'idéation (« Bouillon de neurones ») : elle vit dans la section du haut, hors de tout projet.
    pub ideation: bool,
}

/// Ordre de la barre dans un projet : `@idema_created`, puis le nom tmux pour départager.
pub fn session_order(a: &SessionSummary, b: &SessionSummary) -> std::cmp::Ordering {
    (a.created, &a.name).cmp(&(b.created, &b.name))
}

pub fn sort_sessions(sessions: &mut [SessionSummary]) {
    sessions.sort_by(session_order);
}

/// Les sessions qu'on parcourt au clavier : les bouillons d'abord si leur section est affichée (activée dans la
/// config et dépliée), puis celles des projets dépliés, dans l'ordre de la barre.
pub fn navigation_order<'a>(
    projects: &[PathBuf],
    is_expanded: impl Fn(&Path) -> bool,
    ideation_shown: bool,
    sessions: &'a [SessionSummary],
) -> Vec<&'a str> {
    let ideation_sessions = sessions
        .iter()
        .filter(move |session| ideation_shown && session.ideation)
        .map(|session| session.name.as_str());
    let in_projects = projects
        .iter()
        .filter(|project| is_expanded(project))
        .flat_map(|project| {
            sessions
                .iter()
                .filter(move |session| !session.ideation && &session.project == project)
                .map(|session| session.name.as_str())
        });
    ideation_sessions.chain(in_projects).collect()
}

/// `⌘1` à `⌘9` : `index` part de 0.
pub fn nth_session<'a>(order: &[&'a str], index: usize) -> Option<&'a str> {
    order.get(index).copied()
}

/// `⌃⇥` et `⌃⇧⇥` : la suivante ou la précédente, en boucle. Sans sélection listée, la première ou la dernière.
pub fn cycle<'a>(order: &[&'a str], current: Option<&str>, forward: bool) -> Option<&'a str> {
    if order.is_empty() {
        return None;
    }
    let position = current.and_then(|name| order.iter().position(|candidate| *candidate == name));
    let index = match (position, forward) {
        (Some(at), true) => (at + 1) % order.len(),
        (Some(at), false) => (at + order.len() - 1) % order.len(),
        (None, true) => 0,
        (None, false) => order.len() - 1,
    };
    Some(order[index])
}

/// La session à sélectionner quand `closing` disparaît : la suivante, sinon la précédente.
pub fn neighbor_after_close<'a>(order: &[&'a str], closing: &str) -> Option<&'a str> {
    let at = order.iter().position(|name| *name == closing)?;
    order
        .get(at + 1)
        .or_else(|| at.checked_sub(1).and_then(|before| order.get(before)))
        .copied()
}

/// Le projet que l'écran vide affiche et que `⌘T` cible : le dernier geste gagne. `project_wins` dit que le
/// projet sélectionné a été choisi après la session (clic sur sa ligne, ajout) : il passe alors devant elle.
/// Sinon c'est le projet de la session sélectionnée, puis le projet sélectionné, puis le premier de la barre.
/// `None` sans aucun projet : `⌘T` ouvre alors le dialogue des projets (spec §4.2), sauf devant un bouillon
/// sélectionné, seule session qui naît hors d'un projet.
pub fn target_project(
    selected_session: Option<PathBuf>,
    selected_project: Option<PathBuf>,
    project_wins: bool,
    first_project: Option<PathBuf>,
) -> Option<PathBuf> {
    if project_wins && selected_project.is_some() {
        return selected_project;
    }
    selected_session.or(selected_project).or(first_project)
}

/// Vrai quand la ligne d'une session porte le surlignage de sélection : elle est la session affichée et aucun projet
/// ne l'emporte (un seul surlignage dans la barre). Un bouillon est toujours surligné quand il est affiché : aucune
/// ligne de projet ne porte alors le style de sélection (`is_targeted`), même si `project_wins` reste vrai pour
/// garder le contexte de `⌘O` et de l'écran vide.
pub fn session_row_highlighted(is_selected: bool, project_wins: bool, is_ideation: bool) -> bool {
    is_selected && (is_ideation || !project_wins)
}

/// Nom par défaut d'une nouvelle session : le nom de l'agent, suffixé d'un compteur s'il est déjà pris dans
/// le projet (« Claude 2 »).
pub fn default_session_name(base: &str, taken: &[&str]) -> String {
    if !taken.contains(&base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|candidate| !taken.contains(&candidate.as_str()))
        .expect("an unused counter exists")
}

/// Pastille d'un projet replié : sa session la plus urgente, anneau de repos compris (spec §5.3) ;
/// rien s'il n'a pas de session.
pub fn collapsed_project_dot(
    states: impl IntoIterator<Item = ActivityState>,
) -> Option<ActivityState> {
    states.into_iter().max()
}

/// Une session compte pour le badge du Dock et le son d'attente, sauf un bouillon quand la section `[ideation]` est
/// absente : il n'est alors plus visible.
pub fn is_alerting_session(is_ideation: bool, ideation_enabled: bool) -> bool {
    !is_ideation || ideation_enabled
}

/// Badge du Dock : le nombre de sessions en attente ou en erreur (spec §5.4).
pub fn badge_count(states: impl IntoIterator<Item = ActivityState>) -> usize {
    states
        .into_iter()
        .filter(|state| matches!(state, ActivityState::Waiting | ActivityState::Error))
        .count()
}

/// Le badge du Dock sur les sessions données : un bouillon masqué (section `[ideation]` absente) n'y compte pas.
pub fn dock_badge_count<'a>(
    sessions: impl IntoIterator<Item = &'a SessionSummary>,
    ideation_enabled: bool,
) -> usize {
    badge_count(
        sessions
            .into_iter()
            .filter(|session| is_alerting_session(session.ideation, ideation_enabled))
            .map(|session| session.state),
    )
}

/// Les bouillons figurent dans l'ordre de navigation quand la section existe (`[ideation]` présente) et qu'elle
/// est dépliée.
pub fn ideation_shown(enabled: bool, collapsed: bool) -> bool {
    enabled && !collapsed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str, project: &str, created: u128) -> SessionSummary {
        SessionSummary {
            name: name.into(),
            project: PathBuf::from(project),
            display_name: name.into(),
            created,
            state: ActivityState::Idle,
            ideation: false,
        }
    }

    fn ideation_session(name: &str, created: u128) -> SessionSummary {
        SessionSummary {
            ideation: true,
            ..session(name, "/Users/Shared/erom-ideation", created)
        }
    }

    #[test]
    fn the_target_project_prefers_the_selected_session_then_the_selected_project_then_the_first() {
        let path = |name: &str| Some(PathBuf::from(name));
        assert_eq!(
            target_project(path("/a"), path("/b"), false, path("/c")),
            path("/a")
        );
        assert_eq!(
            target_project(None, path("/b"), false, path("/c")),
            path("/b")
        );
        assert_eq!(target_project(None, None, false, path("/c")), path("/c"));
    }

    #[test]
    fn a_project_picked_after_the_session_wins_over_it() {
        let path = |name: &str| Some(PathBuf::from(name));
        assert_eq!(
            target_project(path("/a"), path("/b"), true, path("/c")),
            path("/b")
        );
        // Sans session affichée, le projet choisi l'emporte aussi sur le premier.
        assert_eq!(
            target_project(None, path("/b"), true, path("/c")),
            path("/b")
        );
    }

    #[test]
    fn a_session_picked_after_the_project_takes_the_priority_back() {
        let path = |name: &str| Some(PathBuf::from(name));
        // Choisir une session pose le projet sélectionné sur le sien et rend la main à la session.
        assert_eq!(
            target_project(path("/a"), path("/a"), false, path("/c")),
            path("/a")
        );
    }

    #[test]
    fn a_removed_project_is_no_longer_targeted() {
        let path = |name: &str| Some(PathBuf::from(name));
        // Le projet retiré n'est plus sélectionné : le drapeau seul ne vise rien, la session ou le premier reprennent.
        assert_eq!(
            target_project(path("/a"), None, true, path("/c")),
            path("/a")
        );
        assert_eq!(target_project(None, None, true, path("/c")), path("/c"));
    }

    #[test]
    fn the_target_project_is_none_without_any_project() {
        assert_eq!(target_project(None, None, true, None), None);
        assert_eq!(target_project(None, None, false, None), None);
    }

    #[test]
    fn a_session_row_is_highlighted_only_when_displayed_and_no_project_wins() {
        assert!(session_row_highlighted(true, false, false));
        assert!(!session_row_highlighted(true, true, false));
        assert!(!session_row_highlighted(false, false, false));
        assert!(!session_row_highlighted(false, true, false));
    }

    #[test]
    fn a_displayed_ideation_row_is_highlighted_even_when_a_project_was_clicked_last() {
        assert!(session_row_highlighted(true, true, true));
        assert!(session_row_highlighted(true, false, true));
        assert!(!session_row_highlighted(false, true, true));
    }

    #[test]
    fn sessions_sort_by_creation_then_name() {
        let mut sessions = vec![
            session("b", "/p", 5),
            session("c", "/p", 1),
            session("a", "/p", 5),
        ];
        sort_sessions(&mut sessions);
        let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["c", "a", "b"]);
    }

    #[test]
    fn navigation_skips_collapsed_projects_and_follows_project_order() {
        let projects = vec![
            PathBuf::from("/b"),
            PathBuf::from("/a"),
            PathBuf::from("/c"),
        ];
        let sessions = vec![
            session("a1", "/a", 1),
            session("b1", "/b", 2),
            session("c1", "/c", 3),
            session("b2", "/b", 4),
        ];
        let order = navigation_order(&projects, |p| p != Path::new("/c"), true, &sessions);
        assert_eq!(order, ["b1", "b2", "a1"]);
        assert_eq!(nth_session(&order, 1), Some("b2"));
        assert_eq!(nth_session(&order, 8), None);
    }

    #[test]
    fn ideation_sessions_come_first_in_navigation() {
        let projects = vec![PathBuf::from("/p")];
        let sessions = vec![session("p1", "/p", 1), ideation_session("b1", 2)];
        let order = navigation_order(&projects, |_| true, true, &sessions);
        assert_eq!(order, ["b1", "p1"]);
    }

    #[test]
    fn a_collapsed_section_leaves_keyboard_navigation() {
        let projects = vec![PathBuf::from("/p")];
        let sessions = vec![session("p1", "/p", 1), ideation_session("b1", 2)];
        let order = navigation_order(&projects, |_| true, false, &sessions);
        assert_eq!(order, ["p1"]);
    }

    #[test]
    fn ideation_sessions_are_in_navigation_only_when_enabled_and_expanded() {
        let projects = vec![PathBuf::from("/p")];
        let sessions = vec![ideation_session("b1", 1), session("p1", "/p", 2)];
        let order_for = |enabled, collapsed| {
            navigation_order(
                &projects,
                |_| true,
                ideation_shown(enabled, collapsed),
                &sessions,
            )
        };
        assert_eq!(order_for(true, false), ["b1", "p1"]);
        assert_eq!(order_for(true, true), ["p1"]);
        assert_eq!(order_for(false, false), ["p1"]);
        assert_eq!(order_for(false, true), ["p1"]);
    }

    #[test]
    fn a_hidden_ideation_session_does_not_alert() {
        assert!(is_alerting_session(false, false));
        assert!(is_alerting_session(true, true));
        assert!(!is_alerting_session(true, false));
        let sessions = [
            session("p1", "/p", 1),
            SessionSummary {
                state: ActivityState::Waiting,
                ..ideation_session("b1", 2)
            },
        ];
        assert_eq!(dock_badge_count(&sessions, true), 1);
        assert_eq!(dock_badge_count(&sessions, false), 0);
    }

    #[test]
    fn the_project_loop_skips_ideation_sessions_even_in_the_ideation_folder() {
        let projects = vec![
            PathBuf::from("/p"),
            PathBuf::from("/Users/Shared/erom-ideation"),
        ];
        let sessions = vec![ideation_session("b1", 1)];
        let order = navigation_order(&projects, |_| true, true, &sessions);
        assert_eq!(order, ["b1"]);
        let order = navigation_order(&projects, |_| true, false, &sessions);
        assert!(order.is_empty());
    }

    #[test]
    fn closing_the_last_ideation_session_hands_over_to_the_first_project_session() {
        let projects = vec![PathBuf::from("/p")];
        let sessions = vec![session("p1", "/p", 1), ideation_session("b1", 2)];
        let order = navigation_order(&projects, |_| true, true, &sessions);
        assert_eq!(neighbor_after_close(&order, "b1"), Some("p1"));
    }

    #[test]
    fn cycling_wraps_around_both_ways() {
        let order = ["s1", "s2", "s3"];
        assert_eq!(cycle(&order, Some("s3"), true), Some("s1"));
        assert_eq!(cycle(&order, Some("s1"), false), Some("s3"));
        assert_eq!(cycle(&order, Some("s2"), true), Some("s3"));
        assert_eq!(cycle(&order, None, true), Some("s1"));
        assert_eq!(cycle(&order, Some("hidden"), false), Some("s3"));
        assert_eq!(cycle(&[], Some("s1"), true), None);
    }

    #[test]
    fn closing_selects_the_next_session_or_the_previous_one() {
        let order = ["s1", "s2", "s3"];
        assert_eq!(neighbor_after_close(&order, "s2"), Some("s3"));
        assert_eq!(neighbor_after_close(&order, "s3"), Some("s2"));
        assert_eq!(neighbor_after_close(&["s1"], "s1"), None);
    }

    #[test]
    fn default_names_count_up_from_two() {
        assert_eq!(default_session_name("Claude", &[]), "Claude");
        assert_eq!(default_session_name("Claude", &["Claude"]), "Claude 2");
        assert_eq!(
            default_session_name("Claude", &["Claude", "Claude 2"]),
            "Claude 3"
        );
        assert_eq!(default_session_name("Claude", &["Claude 2"]), "Claude");
        assert_eq!(
            default_session_name("Claude", &["Claude", "Claude 3"]),
            "Claude 2"
        );
    }

    #[test]
    fn a_collapsed_project_shows_its_most_urgent_session() {
        use ActivityState::*;
        assert_eq!(collapsed_project_dot([Idle, Running]), Some(Running));
        assert_eq!(collapsed_project_dot([Waiting, Error, Idle]), Some(Error));
        assert_eq!(collapsed_project_dot([Idle]), Some(Idle));
        assert_eq!(collapsed_project_dot([]), None);
    }

    #[test]
    fn the_badge_counts_waiting_and_failed_sessions() {
        use ActivityState::*;
        assert_eq!(badge_count([Idle, Waiting, Running, Error, Waiting]), 3);
        assert_eq!(badge_count([Idle, Running]), 0);
    }
}
