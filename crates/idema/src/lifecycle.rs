//! Les décisions pures de la vie des sessions : que faire après la chute d'un client de contrôle (spec jalon 4, §5.1).

use std::path::PathBuf;
use std::time::Duration;

use idema_tmux::server::ListedSession;

/// Combien d'essais de reconnexion avant « Session injoignable ».
pub(crate) const REATTACH_ATTEMPTS: u8 = 3;

/// Un client qui a vécu au moins ce temps a réussi son essai : la série d'échecs repart de zéro (écart E33). Sans
/// cette durée, « réussi à la première réponse de tmux » ne laisserait jamais la boucle d'essais arriver au bout.
pub(crate) const REATTACH_STABLE: Duration = Duration::from_secs(2);

/// Ce que l'espace de travail fait d'une session dont le client est tombé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterClose {
    /// La session n'existe plus (ou le serveur est mort) : l'entrée part de la barre.
    Remove,
    /// Un nouvel essai, le `attempt`-ième de la série.
    Retry { attempt: u8 },
    /// Les essais sont épuisés : « Session injoignable ».
    Unreachable,
}

/// La suite d'une chute. `attempts` est le compte de la série en cours ; `alive_for` la durée de vie du client
/// tombé, `None` pour une attache qui n'a pas abouti (elle compte comme un essai raté).
pub(crate) fn after_close(exists: bool, attempts: u8, alive_for: Option<Duration>) -> AfterClose {
    if !exists {
        return AfterClose::Remove;
    }
    let streak = match alive_for {
        Some(lived) if lived >= REATTACH_STABLE => 0,
        _ => attempts,
    };
    if streak < REATTACH_ATTEMPTS {
        AfterClose::Retry {
            attempt: streak + 1,
        }
    } else {
        AfterClose::Unreachable
    }
}

/// Ce qu'une relecture de `list-sessions` change dans la barre (spec jalon 4, §5.2).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionSync {
    /// Les sessions listées que la barre ne connaît pas encore : elles ont été créées hors d'IdemA.
    pub(crate) added: Vec<String>,
    /// Les entrées qui ont disparu de la liste : elles ont été tuées hors d'IdemA.
    pub(crate) gone: Vec<String>,
}

/// Compare une relecture aux entrées. `known` : les entrées d'aujourd'hui ; `snapshot` : celles du moment où la
/// relecture est partie ; `pending` : les créations d'IdemA encore en vol (projet, nom affiché).
///
/// Une session que `new-session` vient de créer pour IdemA apparaît dans la liste (et déclenche
/// `%sessions-changed`) avant que l'entrée existe : son couple (projet, nom affiché) la désigne, la relecture ne
/// l'ajoute pas. À l'inverse, une relecture partie avant une création ne la connaît pas : seul `snapshot` peut
/// désigner une entrée disparue.
pub(crate) fn session_sync(
    known: &[String],
    snapshot: &[String],
    pending: &[(PathBuf, String)],
    listed: &[ListedSession],
) -> SessionSync {
    let added = listed
        .iter()
        .filter(|session| !known.contains(&session.name))
        .filter(|session| {
            !pending.iter().any(|(project, name)| {
                project.as_os_str() == session.project.as_str() && *name == session.display_name
            })
        })
        .map(|session| session.name.clone())
        .collect();
    let gone = snapshot
        .iter()
        .filter(|name| !listed.iter().any(|session| &session.name == *name))
        .cloned()
        .collect();
    SessionSync { added, gone }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use std::path::PathBuf;

    use idema_tmux::server::ListedSession;

    use super::{AfterClose, REATTACH_ATTEMPTS, REATTACH_STABLE, after_close, session_sync};

    #[test]
    fn a_session_gone_is_removed_whatever_the_count() {
        for attempts in 0..=REATTACH_ATTEMPTS {
            for alive_for in [
                None,
                Some(Duration::from_millis(100)),
                Some(REATTACH_STABLE),
            ] {
                assert_eq!(after_close(false, attempts, alive_for), AfterClose::Remove);
            }
        }
    }

    #[test]
    fn three_quick_failures_make_a_session_unreachable() {
        let quick = Some(Duration::from_millis(100));
        for attempts in 0..REATTACH_ATTEMPTS {
            assert_eq!(
                after_close(true, attempts, quick),
                AfterClose::Retry {
                    attempt: attempts + 1
                }
            );
        }
        assert_eq!(
            after_close(true, REATTACH_ATTEMPTS, quick),
            AfterClose::Unreachable
        );
    }

    #[test]
    fn a_client_that_lived_two_seconds_starts_a_new_streak() {
        assert_eq!(
            after_close(true, REATTACH_ATTEMPTS, Some(REATTACH_STABLE)),
            AfterClose::Retry { attempt: 1 }
        );
    }

    #[test]
    fn a_failed_attach_counts_as_an_attempt() {
        assert_eq!(after_close(true, 2, None), AfterClose::Retry { attempt: 3 });
        assert_eq!(after_close(true, 3, None), AfterClose::Unreachable);
    }

    fn listed(name: &str, project: &str, display_name: &str) -> ListedSession {
        ListedSession {
            name: name.into(),
            project: project.into(),
            created: 1,
            layout: String::new(),
            ideation: false,
            remote_name: String::new(),
            display_name: display_name.into(),
        }
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_new_outside_session_is_added_once() {
        let listing = [
            listed("idema-a", "/p", "Shell"),
            listed("idema-b", "/p", "Other"),
        ];
        let known = names(&["idema-a"]);
        let sync = session_sync(&known, &known, &[], &listing);
        assert_eq!(sync.added, names(&["idema-b"]));
        // Une fois connue, elle n'est plus ajoutée.
        let known = names(&["idema-a", "idema-b"]);
        assert!(session_sync(&known, &known, &[], &listing).added.is_empty());
    }

    #[test]
    fn a_session_being_created_by_idema_is_not_added_by_the_listing() {
        let listing = [listed("idema-a", "/p", "Shell")];
        let pending = [(PathBuf::from("/p"), "Shell".to_owned())];
        let sync = session_sync(&[], &[], &pending, &listing);
        assert!(sync.added.is_empty());
        // Un autre projet, ou un autre nom, n'est pas une création en cours.
        let elsewhere = [(PathBuf::from("/q"), "Shell".to_owned())];
        assert_eq!(
            session_sync(&[], &[], &elsewhere, &listing).added,
            names(&["idema-a"])
        );
        let renamed = [(PathBuf::from("/p"), "Claude".to_owned())];
        assert_eq!(
            session_sync(&[], &[], &renamed, &listing).added,
            names(&["idema-a"])
        );
    }

    #[test]
    fn a_listing_that_predates_a_creation_never_removes_it() {
        // La relecture est partie avant la création : `snapshot` ne contient pas la session neuve.
        let known = names(&["idema-old", "idema-new"]);
        let snapshot = names(&["idema-old"]);
        let listing = [listed("idema-old", "/p", "Shell")];
        let sync = session_sync(&known, &snapshot, &[], &listing);
        assert!(sync.gone.is_empty());
        assert!(sync.added.is_empty());
    }

    #[test]
    fn a_session_killed_outside_is_gone() {
        let snapshot = names(&["idema-a", "idema-b"]);
        let listing = [listed("idema-a", "/p", "Shell")];
        let sync = session_sync(&snapshot, &snapshot, &[], &listing);
        assert_eq!(sync.gone, names(&["idema-b"]));
        assert!(sync.added.is_empty());
    }
}
