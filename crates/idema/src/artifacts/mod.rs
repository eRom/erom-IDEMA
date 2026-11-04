//! La galerie Artifacts : les artifacts claude.ai de l'utilisateur, lus par l'API de l'app web elle-même. `source` porte la
//! page cachée qui rejoue la requête de l'app (même magasin de cookies que le volet Browser).

pub(crate) mod cards;
pub(crate) mod listing;
pub(crate) mod model;
pub(crate) mod source;
pub(crate) mod thumbnails;
pub(crate) mod view;

/// Pourquoi la liste n'a pas pu être lue. Définie ici, `model::classify` en rend le verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// Redirection vers la connexion, 401 ou 403.
    SignedOut,
    /// Toute autre réponse que 200.
    Http(u16),
    /// Réponse reçue mais illisible.
    Unreadable,
    /// claude.ai injoignable ou page sans capture (le motif sert au journal).
    Unavailable(String),
}

/// La demande d'une liste. `model::query_string` la met en paramètres d'URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    pub search: Option<String>,
}
