//! La recherche dans la page (`⌘F`, spec P4 §4.9) : l'état de la barre et son libellé. La partie pure ici ; la rangée
//! est dessinée par `chrome.rs`, les gestes sont dans `panel.rs`.

use gpui::{Entity, Subscription};
use gpui_component::input::InputState;

/// L'indication du champ de recherche quand il est vide.
pub const PLACEHOLDER: &str = "Chercher dans la page";

/// La barre ouverte : son champ, ce que la dernière recherche a trouvé, et le numéro de la dernière demande (la réponse
/// d'une demande dépassée est jetée).
pub struct FindBar {
    pub field: Entity<InputState>,
    pub status: FindStatus,
    pub request: u64,
    pub _subscription: Subscription,
}

/// Ce que la dernière recherche a trouvé. WebKit ne rend, par `WKFindResult`, que « il y a une occurrence » ou non : ni
/// rang ni total (le « <i> sur <n> » du plan n'a pas de source publique).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindStatus {
    /// Rien n'a été cherché (champ vide, ou recherche en vol).
    Idle,
    Found,
    NotFound,
}

/// Le texte à droite du champ : « Aucun résultat » seulement quand rien n'est trouvé ; une occurrence se montre
/// sélectionnée dans la page.
pub fn status_label(status: FindStatus) -> &'static str {
    match status {
        FindStatus::NotFound => "Aucun résultat",
        FindStatus::Idle | FindStatus::Found => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_search_without_match_says_so() {
        assert_eq!(status_label(FindStatus::NotFound), "Aucun résultat");
        assert_eq!(status_label(FindStatus::Found), "");
        assert_eq!(status_label(FindStatus::Idle), "");
    }
}
