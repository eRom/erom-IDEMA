//! Les erreurs courantes, expliquées en français ; le message brut reste en ligne de détail (spec jalon 4, §5).

use std::io;

use gpui::{App, PromptButton, PromptLevel, Window};
use idema_tmux::server::ServerError;

use crate::commit_message::client::GenerationError;
use crate::config::{ConfigError, ConfigErrorKind};
use crate::git::Gesture;

/// Ce que l'on dit d'une erreur hors catalogue : le brut suit en détail.
pub(crate) const UNEXPECTED: &str = "Erreur inattendue.";

/// Une erreur expliquée : le texte lisible, et le message brut quand IdemA ne l'a pas écrit lui-même.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Explained {
    pub(crate) text: String,
    pub(crate) detail: Option<String>,
}

/// Messages de `toml` 1.1.8 (sonde du 16/10/2026) : motif contenu dans le brut, puis texte. Cherchés dans l'ordre.
const CONFIG_SYNTAX: &[(&str, &str)] = &[
    (
        "string values must be quoted",
        "une valeur texte doit être entre guillemets",
    ),
    ("unknown field `", "clé inconnue"),
    ("invalid type: ", "valeur du mauvais type"),
    ("duplicate key", "clé en double"),
    ("missing field `", "champ obligatoire manquant"),
];

/// Messages de tmux 3.8 (sondes du 15 et du 16/10/2026) : motif contenu dans le brut, puis texte.
const TMUX: &[(&str, &str)] = &[
    ("no server running", "le serveur tmux est injoignable"),
    ("error connecting to", "le serveur tmux est injoignable"),
    ("can't find session", "session introuvable"),
    ("no such session", "session introuvable"),
    ("duplicate session", "ce nom de session est déjà pris"),
];

fn lookup(table: &[(&str, &'static str)], raw: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(pattern, _)| raw.contains(pattern))
        .map(|(_, text)| *text)
}

/// « une clé » devient « Une clé. » : majuscule et point.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    let first = chars.next().map(|c| c.to_uppercase().collect::<String>());
    format!("{}{}.", first.unwrap_or_default(), chars.as_str())
}

/// Le texte du catalogue tmux pour un message brut, `None` hors catalogue.
pub(crate) fn tmux_text(raw: &str) -> Option<String> {
    lookup(TMUX, raw).map(sentence)
}

pub(crate) fn explain_config(error: &ConfigError) -> Explained {
    let raw = Some(error.to_string());
    match error.kind {
        // Écrit en français par IdemA : rien à traduire (E37).
        ConfigErrorKind::Invalid => Explained {
            text: error.message.clone(),
            detail: None,
        },
        ConfigErrorKind::Read => Explained {
            text: sentence("fichier illisible"),
            detail: raw,
        },
        ConfigErrorKind::Syntax => match lookup(CONFIG_SYNTAX, &error.message) {
            Some(text) => Explained {
                text: match error.line {
                    Some(line) => format!("Ligne {line} : {text}."),
                    None => sentence(text),
                },
                detail: raw,
            },
            None => Explained {
                text: UNEXPECTED.to_string(),
                detail: raw,
            },
        },
    }
}

pub(crate) fn explain_server(error: &ServerError) -> Explained {
    let text = match error {
        // Déjà écrits en français (ou prêts à lire) par IdemA : tels quels.
        ServerError::NotFound | ServerError::Unsupported(_) => {
            return Explained {
                text: error.to_string(),
                detail: None,
            };
        }
        ServerError::ProjectGone(_) => Some(sentence("le dossier du projet n'existe plus")),
        ServerError::Tmux { stderr, .. } => tmux_text(stderr),
        // Refus interne d'une commande sur un tmux distant : le brut va en détail.
        ServerError::Io(_) | ServerError::RemoteForbidden(_) => None,
    };
    Explained {
        text: text.unwrap_or_else(|| UNEXPECTED.to_string()),
        detail: Some(error.to_string()),
    }
}

/// Une alerte à un bouton, pour un refus ou un échec que l'utilisateur doit lire : le texte, et le brut en détail.
pub(crate) fn alert(window: &mut Window, cx: &mut App, message: &str, detail: Option<&str>) {
    // La réponse n'a pas de suite : on laisse tomber le récepteur.
    let _answer = window.prompt(
        PromptLevel::Warning,
        message,
        detail,
        &[PromptButton::ok("OK")],
        cx,
    );
}

/// Une écriture refusée dans un fichier du volet Fichiers (spec P2 §5) : le texte d'après le genre d'erreur du système,
/// le message brut en détail.
pub(crate) fn explain_write(error: &io::Error) -> Explained {
    let text = match error.kind() {
        io::ErrorKind::PermissionDenied => sentence("écriture refusée : droits insuffisants"),
        io::ErrorKind::StorageFull => sentence("écriture impossible : le disque est plein"),
        io::ErrorKind::ReadOnlyFilesystem => {
            sentence("écriture impossible : le volume est en lecture seule")
        }
        _ => UNEXPECTED.to_string(),
    };
    Explained {
        text,
        detail: Some(error.to_string()),
    }
}

/// Ce qui a mal tourné dans une commande Git d'écriture (spec P3 §5), lu dans sa sortie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitFailure {
    /// `index.lock` est pris : un agent travaille.
    Busy,
    /// Push refusé parce que la branche distante a avancé (non-fast-forward).
    PushRejected,
    PushFailed,
    CommitFailed,
    Other,
}

/// Classe l'échec d'une commande Git. Le verrou prime sur le geste ; le refus d'un push se lit sur la sortie
/// `--porcelain` (stdout), où Git écrit `[rejected]`.
pub(crate) fn git_failure(gesture: Gesture, stderr: &str, stdout: &str) -> GitFailure {
    if stderr.contains("index.lock") {
        return GitFailure::Busy;
    }
    match gesture {
        Gesture::Push if stdout.contains("[rejected]") => GitFailure::PushRejected,
        Gesture::Push => GitFailure::PushFailed,
        Gesture::Commit => GitFailure::CommitFailed,
        _ => GitFailure::Other,
    }
}

/// Le texte de la bulle pour un échec Git, la sortie brute de Git en détail.
pub(crate) fn explain_git(failure: GitFailure, raw: &str) -> Explained {
    let text = match failure {
        GitFailure::Busy => "Git est occupé, réessaie dans un instant.",
        GitFailure::PushRejected => "La branche distante a avancé : à régler avec Claude.",
        GitFailure::PushFailed => "Push impossible.",
        GitFailure::CommitFailed => "Le commit a échoué.",
        GitFailure::Other => UNEXPECTED,
    };
    Explained {
        text: text.to_string(),
        detail: Some(raw.trim().to_string()).filter(|raw| !raw.is_empty()),
    }
}

/// Le texte de la bulle pour un échec de génération du message de commit (spec B-11 §4.7), le brut en détail.
pub(crate) fn explain_generation(error: GenerationError, raw: &str, model: &str) -> Explained {
    let text = match error {
        GenerationError::KeyRejected => "Clé Gemini refusée.".to_string(),
        GenerationError::ModelUnavailable => format!("Modèle {model} indisponible."),
        GenerationError::QuotaExceeded => "Quota Gemini épuisé, réessaie plus tard.".to_string(),
        GenerationError::Unreachable => "Gemini injoignable.".to_string(),
        GenerationError::CurlMissing => "curl est introuvable.".to_string(),
        GenerationError::Failed => "La génération du message a échoué.".to_string(),
    };
    Explained {
        text,
        detail: Some(raw.trim().to_string()).filter(|raw| !raw.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use idema_tmux::server::ServerError;

    use super::*;
    use crate::config::{load_config, parse_config};

    /// Le message de `toml` pour un texte de config, tel que l'analyseur le rend.
    fn explained(text: &str) -> Explained {
        explain_config(&parse_config(text).unwrap_err())
    }

    #[test]
    fn every_config_error_of_the_catalogue_reads_in_french() {
        let cases = [
            (
                "terminal_font_family = Hack\n",
                "Ligne 1 : une valeur texte doit être entre guillemets.",
                "string values must be quoted",
            ),
            (
                "ui_font_size = 20\nterminal_size = 16\n",
                "Ligne 2 : clé inconnue.",
                "unknown field `terminal_size`",
            ),
            (
                "terminal_font_size = \"16\"\n",
                "Ligne 1 : valeur du mauvais type.",
                "invalid type: string \"16\"",
            ),
            (
                "ui_font_size = 20\nui_font_size = 21\n",
                "Ligne 2 : clé en double.",
                "duplicate key",
            ),
            (
                "[[agents]]\nname = \"A\"\n",
                "Ligne 1 : champ obligatoire manquant.",
                "missing field `command`",
            ),
        ];
        for (config, text, toml_message) in cases {
            let explained = explained(config);
            assert_eq!(explained.text, text, "{config:?}");
            let detail = explained
                .detail
                .expect("a parser error keeps its raw message");
            assert!(detail.contains(toml_message), "{detail:?}");
        }
    }

    #[test]
    fn a_read_failure_says_the_file_is_unreadable() {
        let missing = std::env::temp_dir().join("idema-errors-no-such-dir/config.toml");
        let explained = explain_config(&load_config(&missing).unwrap_err());
        assert_eq!(explained.text, "Fichier illisible.");
        assert!(!explained.detail.unwrap().is_empty());
    }

    #[test]
    fn idemas_own_validation_messages_pass_through() {
        let explained = explained("ui_font_size = 99\n");
        assert_eq!(explained.text, "ui_font_size doit être entre 10 et 40");
        assert_eq!(explained.detail, None);
    }

    #[test]
    fn an_unknown_parser_message_is_an_unexpected_error() {
        let explained = explained("terminal_font_family = \"Hack\n");
        assert_eq!(explained.text, UNEXPECTED);
        assert!(explained.detail.unwrap().contains("invalid basic string"));
    }

    fn tmux(stderr: &str) -> ServerError {
        ServerError::Tmux {
            args: "probe".into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn every_tmux_error_of_the_catalogue_reads_in_french() {
        // Messages réels de tmux 3.8, sonde du 16/10/2026 (et du 15/10 pour « no server running »).
        let cases = [
            (
                "error connecting to /private/tmp/tmux-501/idema-probe (No such file or directory)",
                "Le serveur tmux est injoignable.",
            ),
            (
                "no server running on /private/tmp/tmux-501/idema-probe",
                "Le serveur tmux est injoignable.",
            ),
            ("can't find session: idema-0000dead", "Session introuvable."),
            ("no such session: =idema-0000dead:", "Session introuvable."),
            (
                "duplicate session: base",
                "Ce nom de session est déjà pris.",
            ),
        ];
        for (stderr, text) in cases {
            let error = tmux(stderr);
            let explained = explain_server(&error);
            assert_eq!(explained.text, text, "{stderr}");
            assert_eq!(explained.detail, Some(error.to_string()));
            assert_eq!(tmux_text(stderr).as_deref(), Some(text));
        }
        let unknown = explain_server(&tmux("unknown command: frobnicate"));
        assert_eq!(unknown.text, UNEXPECTED);
        assert!(
            unknown
                .detail
                .unwrap()
                .contains("unknown command: frobnicate")
        );
        assert_eq!(tmux_text("unknown command: frobnicate"), None);

        let gone = ServerError::ProjectGone("/nonexistent/project".into());
        let explained = explain_server(&gone);
        assert_eq!(explained.text, "Le dossier du projet n'existe plus.");
        assert_eq!(
            explained.detail.as_deref(),
            Some("no such directory: /nonexistent/project")
        );
    }

    #[test]
    fn a_refused_write_reads_in_french_with_the_raw_message() {
        let cases = [
            (
                io::ErrorKind::PermissionDenied,
                "Écriture refusée : droits insuffisants.",
            ),
            (
                io::ErrorKind::StorageFull,
                "Écriture impossible : le disque est plein.",
            ),
            (
                io::ErrorKind::ReadOnlyFilesystem,
                "Écriture impossible : le volume est en lecture seule.",
            ),
            (io::ErrorKind::TimedOut, UNEXPECTED),
        ];
        for (kind, text) in cases {
            let error = io::Error::from(kind);
            let explained = explain_write(&error);
            assert_eq!(explained.text, text, "{kind:?}");
            assert_eq!(explained.detail, Some(error.to_string()));
        }
    }

    #[test]
    fn idemas_own_tmux_messages_pass_through_without_detail() {
        let missing = explain_server(&ServerError::NotFound);
        assert_eq!(
            (missing.text.as_str(), missing.detail),
            ("tmux introuvable", None)
        );
        let old = explain_server(&ServerError::Unsupported("tmux 3.5".into()));
        assert_eq!(old.text, "tmux 3.8 requis, trouvé : tmux 3.5");
        assert_eq!(old.detail, None);
    }

    #[test]
    fn git_failure_reads_the_lock_and_the_rejection() {
        let lock = "fatal: Unable to create '/p/.git/index.lock': File exists.";
        for gesture in [
            Gesture::Stage,
            Gesture::Unstage,
            Gesture::Commit,
            Gesture::Push,
        ] {
            assert_eq!(
                git_failure(gesture, lock, ""),
                GitFailure::Busy,
                "{gesture:?}"
            );
        }
        let rejected = " !\trefs/heads/main:refs/heads/main\t[rejected] (fetch first)\n";
        assert_eq!(
            git_failure(Gesture::Push, "error: failed to push", rejected),
            GitFailure::PushRejected
        );
        assert_eq!(
            git_failure(Gesture::Push, "fatal: Authentication failed", ""),
            GitFailure::PushFailed
        );
        assert_eq!(
            git_failure(Gesture::Commit, "hook declined", ""),
            GitFailure::CommitFailed
        );
        assert_eq!(git_failure(Gesture::Stage, "boom", ""), GitFailure::Other);
    }

    #[test]
    fn every_git_failure_reads_in_french_with_the_raw_output_as_detail() {
        let cases = [
            (
                GitFailure::Busy,
                "Git est occupé, réessaie dans un instant.",
            ),
            (
                GitFailure::PushRejected,
                "La branche distante a avancé : à régler avec Claude.",
            ),
            (GitFailure::PushFailed, "Push impossible."),
            (GitFailure::CommitFailed, "Le commit a échoué."),
            (GitFailure::Other, UNEXPECTED),
        ];
        for (failure, text) in cases {
            let explained = explain_git(failure, "raw output");
            assert_eq!(explained.text, text, "{failure:?}");
            assert_eq!(explained.detail.as_deref(), Some("raw output"));
        }
        assert_eq!(explain_git(GitFailure::Busy, "  \n").detail, None);
    }

    #[test]
    fn explain_generation_uses_the_catalogue_texts() {
        let cases = [
            (GenerationError::KeyRejected, "Clé Gemini refusée."),
            (
                GenerationError::ModelUnavailable,
                "Modèle gemini-test indisponible.",
            ),
            (
                GenerationError::QuotaExceeded,
                "Quota Gemini épuisé, réessaie plus tard.",
            ),
            (GenerationError::Unreachable, "Gemini injoignable."),
            (GenerationError::CurlMissing, "curl est introuvable."),
            (
                GenerationError::Failed,
                "La génération du message a échoué.",
            ),
        ];
        for (error, text) in cases {
            let explained = explain_generation(error, " raw detail \n", "gemini-test");
            assert_eq!(explained.text, text, "{error:?}");
            assert_eq!(explained.detail.as_deref(), Some("raw detail"));
        }
        assert_eq!(
            explain_generation(GenerationError::Failed, "  ", "m").detail,
            None
        );
    }
}
