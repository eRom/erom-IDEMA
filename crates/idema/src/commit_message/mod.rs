//! Message de commit rédigé par un modèle Gemini (spec B-11). Ce module porte les réglages jusqu'au volet Changes ;
//! `prompt` assemble la requête, `client` la transporte vers Gemini.

use gpui::Global;

use crate::config::CommitMessageConfig;

pub(crate) mod client;
pub(crate) mod prompt;

/// La section `[commit_message]` du `config.toml` valide courant, posée au démarrage et à chaque rechargement réussi.
pub struct CommitMessageSettings(pub Option<CommitMessageConfig>);

impl Global for CommitMessageSettings {}

impl CommitMessageSettings {
    /// La config seulement quand la fonction est allumée : section absente ou `enabled = false`, rien.
    pub fn active(&self) -> Option<&CommitMessageConfig> {
        self.0.as_ref().filter(|section| section.enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;

    fn settings(text: &str) -> CommitMessageSettings {
        CommitMessageSettings(parse_config(text).unwrap().commit_message)
    }

    #[test]
    fn the_feature_is_inactive_without_its_section_or_when_disabled() {
        assert!(settings("").active().is_none());
        assert!(
            settings("[commit_message]\nenabled = false\n")
                .active()
                .is_none()
        );
        // Sans `enabled`, la section existe mais reste éteinte.
        assert!(
            settings("[commit_message]\napi_key = \"fake-key-for-tests\"\n")
                .active()
                .is_none()
        );
    }

    #[test]
    fn the_feature_is_active_when_enabled() {
        let settings = settings("[commit_message]\nenabled = true\n");
        assert!(settings.active().is_some());
    }
}
