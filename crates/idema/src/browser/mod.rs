//! Le navigateur embarqué (P4) : adresses, onglets, réglages. Les modules ici n'ouvrent aucune fenêtre.

use gpui::Global;

use crate::config::{Config, Favorite};

pub(crate) mod address;
pub(crate) mod favicons;
pub(crate) mod load_error;
pub(crate) mod panel;
pub(crate) mod tabs;
pub(crate) mod webkit;

/// Les favoris du `config.toml` valide courant, posés au démarrage et à chaque rechargement réussi. Vides sans
/// section `[browser]`.
pub struct BrowserSettings(pub Vec<Favorite>);

impl Global for BrowserSettings {}

impl BrowserSettings {
    pub fn from_config(config: &Config) -> Self {
        Self(
            config
                .browser
                .as_ref()
                .map(|browser| browser.favorites.clone())
                .unwrap_or_default(),
        )
    }
}
