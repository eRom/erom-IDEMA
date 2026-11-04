//! L'état de la liste de la galerie, sans fenêtre : les éléments lus, où en est la demande, et la règle qui décide quelle
//! réponse a le droit de s'afficher. La vue ne fait que l'appeler : tout ce qui décide ici se teste directement (AC-5).

use super::ApiError;
use super::model::{Artifact, RequestTracker, parse_page};

/// Où en est la liste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Aucune réponse encore (ou après un échec, pendant la nouvelle demande) : les squelettes.
    Loading,
    /// La liste est lue. Un rechargement garde cet état : les éléments restent à l'écran jusqu'à la réponse.
    Loaded,
    SignedOut,
    Failed(ApiError),
}

/// Ce que `Listing::apply` a fait d'une réponse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Une demande plus récente est partie depuis : la réponse est jetée, l'état n'a pas bougé.
    Ignored,
    /// La liste lue, avec le nombre d'éléments gardés.
    Loaded(usize),
    SignedOut,
    Failed(ApiError),
}

#[derive(Debug)]
pub struct Listing {
    /// La dernière liste lue, dans l'ordre de l'API, tous genres confondus : le filtre se joue à l'affichage.
    pub items: Vec<Artifact>,
    pub status: Status,
    tracker: RequestTracker,
}

impl Default for Listing {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            status: Status::Loading,
            tracker: RequestTracker::default(),
        }
    }
}

impl Listing {
    /// Ouvre une demande : elle périme les précédentes. Les éléments déjà lus restent à l'écran ; hors d'une liste lue
    /// (premier chargement, ou après un échec), les squelettes reviennent.
    pub fn begin(&mut self) -> u64 {
        if self.status != Status::Loaded {
            self.status = Status::Loading;
        }
        self.tracker.begin()
    }

    /// Applique la réponse de la demande `generation`, si c'est encore la dernière. Une réponse périmée, bonne ou
    /// mauvaise, ne touche à rien.
    pub fn apply(&mut self, generation: u64, result: Result<String, ApiError>) -> Applied {
        if !self.tracker.is_current(generation) {
            return Applied::Ignored;
        }
        match result.and_then(|body| parse_page(&body)) {
            Ok(items) => {
                let count = items.len();
                self.items = items;
                self.status = Status::Loaded;
                Applied::Loaded(count)
            }
            Err(ApiError::SignedOut) => {
                self.status = Status::SignedOut;
                Applied::SignedOut
            }
            Err(error) => {
                self.status = Status::Failed(error.clone());
                Applied::Failed(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(titles: &[&str]) -> String {
        let items: Vec<String> = titles
            .iter()
            .enumerate()
            .map(|(index, title)| {
                format!(r#"{{"id":"{index}","title":"{title}","open_url":"https://claude.ai/artifact/{index}"}}"#)
            })
            .collect();
        format!(r#"{{"artifacts":[{}]}}"#, items.join(","))
    }

    fn titles(listing: &Listing) -> Vec<&str> {
        listing
            .items
            .iter()
            .map(|item| item.title.as_str())
            .collect()
    }

    #[test]
    fn an_older_response_arriving_last_does_not_replace_the_newer_one() {
        // AC-5 : « a » part, puis « ab ». La réponse de « ab » arrive la première, celle de « a » en retard.
        let mut listing = Listing::default();
        let older = listing.begin();
        let newer = listing.begin();
        assert_eq!(
            listing.apply(newer, Ok(body(&["résultat de ab"]))),
            Applied::Loaded(1)
        );
        assert_eq!(
            listing.apply(older, Ok(body(&["résultat de a", "autre"]))),
            Applied::Ignored
        );
        assert_eq!(titles(&listing), ["résultat de ab"]);
        assert_eq!(listing.status, Status::Loaded);
    }

    #[test]
    fn an_older_response_arriving_first_is_dropped_and_the_newer_one_still_shows() {
        let mut listing = Listing::default();
        let older = listing.begin();
        let newer = listing.begin();
        assert_eq!(
            listing.apply(older, Ok(body(&["périmé"]))),
            Applied::Ignored
        );
        // Rien n'est encore affiché : la demande récente attend toujours sa réponse.
        assert_eq!(listing.status, Status::Loading);
        assert!(listing.items.is_empty());
        listing.apply(newer, Ok(body(&["récent"])));
        assert_eq!(titles(&listing), ["récent"]);
    }

    #[test]
    fn a_stale_failure_does_not_replace_a_good_newer_list() {
        let mut listing = Listing::default();
        let older = listing.begin();
        let newer = listing.begin();
        listing.apply(newer, Ok(body(&["bon"])));
        assert_eq!(
            listing.apply(older, Err(ApiError::Http(404))),
            Applied::Ignored
        );
        assert_eq!(listing.status, Status::Loaded);
        assert_eq!(titles(&listing), ["bon"]);
    }

    #[test]
    fn a_failure_then_a_reload_shows_the_skeletons_until_the_answer() {
        let mut listing = Listing::default();
        let first = listing.begin();
        assert_eq!(
            listing.apply(first, Err(ApiError::Http(404))),
            Applied::Failed(ApiError::Http(404))
        );
        assert_eq!(listing.status, Status::Failed(ApiError::Http(404)));
        let retry = listing.begin();
        assert_eq!(listing.status, Status::Loading);
        listing.apply(retry, Ok(body(&["ok"])));
        assert_eq!(titles(&listing), ["ok"]);
    }

    #[test]
    fn a_reload_keeps_the_items_on_screen_until_its_answer() {
        let mut listing = Listing::default();
        let first = listing.begin();
        listing.apply(first, Ok(body(&["a", "b"])));
        let _ = listing.begin();
        assert_eq!(listing.status, Status::Loaded);
        assert_eq!(titles(&listing), ["a", "b"]);
    }

    #[test]
    fn signed_out_and_unreadable_answers_set_their_status() {
        let mut listing = Listing::default();
        let signed_out = listing.begin();
        assert_eq!(
            listing.apply(signed_out, Err(ApiError::SignedOut)),
            Applied::SignedOut
        );
        assert_eq!(listing.status, Status::SignedOut);
        let unreadable = listing.begin();
        assert_eq!(
            listing.apply(unreadable, Ok("<html>".to_owned())),
            Applied::Failed(ApiError::Unreadable)
        );
    }
}
