//! Les clés de session. Une session locale garde son nom tmux tel quel ; une session distante est qualifiée par sa
//! machine : `<connexion><séparateur><nom tmux>`. Le séparateur est coupé au premier caractère : la connexion n'en
//! contient jamais (elle refuse les caractères de contrôle), le nom tmux peut en contenir un.

/// La clé réservée de Localhost.
pub const LOCAL_MACHINE: &str = "local";

/// Le séparateur d'une clé distante : U+001F (séparateur d'unité), un caractère de contrôle qu'une connexion valide
/// ne contient pas.
pub const KEY_SEPARATOR: char = '\u{1f}';

/// La clé d'une session de la machine `connection` dont le nom tmux est `tmux_name`.
pub fn remote_session_key(connection: &str, tmux_name: &str) -> String {
    format!("{connection}{KEY_SEPARATOR}{tmux_name}")
}

/// Où vit la session d'une clé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTarget<'a> {
    /// Une session de ce Mac : la clé est son nom tmux.
    Local(&'a str),
    /// Une session d'une machine distante.
    Remote { connection: &'a str, tmux: &'a str },
}

/// Découpe une clé au premier séparateur : sans séparateur, c'est une session locale.
pub fn split_session_key(key: &str) -> SessionTarget<'_> {
    match key.split_once(KEY_SEPARATOR) {
        Some((connection, tmux)) => SessionTarget::Remote { connection, tmux },
        None => SessionTarget::Local(key),
    }
}

/// La machine d'une clé : `LOCAL_MACHINE` ou la connexion.
pub fn machine_of(key: &str) -> &str {
    match split_session_key(key) {
        SessionTarget::Local(_) => LOCAL_MACHINE,
        SessionTarget::Remote { connection, .. } => connection,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_hostile_tmux_names() {
        for name in ["testvpc", "mon audit", "l'audit", "a\u{1f}b"] {
            let key = remote_session_key("caserne-claude", name);
            assert_eq!(
                split_session_key(&key),
                SessionTarget::Remote {
                    connection: "caserne-claude",
                    tmux: name
                }
            );
            assert_eq!(machine_of(&key), "caserne-claude");
        }
        assert_eq!(
            split_session_key("idema-0a1b2c3d"),
            SessionTarget::Local("idema-0a1b2c3d")
        );
        assert_eq!(machine_of("idema-0a1b2c3d"), LOCAL_MACHINE);
        assert_eq!(LOCAL_MACHINE, "local");
    }
}
