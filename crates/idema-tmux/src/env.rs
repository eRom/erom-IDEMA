//! Environnement transmis au serveur tmux d'Idema (spec §3.5, B-14).

/// Variables héritées qu'Idema laisse passer. Tout le reste est retiré.
const ALLOWED: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "TMPDIR",
    "LANG",
    "SSH_AUTH_SOCK",
    "__CF_USER_TEXT_ENCODING",
];

/// Construit l'environnement du serveur depuis celui d'Idema.
/// `apple_locale` est la sortie de `defaults read -g AppleLocale` (par exemple « fr_FR »).
pub fn server_env(
    inherited: impl IntoIterator<Item = (String, String)>,
    apple_locale: Option<&str>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = inherited
        .into_iter()
        .filter(|(key, _)| ALLOWED.contains(&key.as_str()) || key.starts_with("LC_"))
        .collect();
    env.push(("COLORTERM".into(), "truecolor".into()));
    if !env.iter().any(|(key, _)| key == "LANG") {
        // « fr_FR@rg=frzzzz » donne « fr_FR.UTF-8 ».
        let locale = apple_locale
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or("en_US");
        let base = locale.split('@').next().unwrap_or(locale);
        env.push(("LANG".into(), format!("{base}.UTF-8")));
    }
    env.sort();
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn drops_claude_zed_and_terminal_variables() {
        let env = server_env(
            vars(&[
                ("HOME", "/Users/r"),
                ("PATH", "/usr/bin"),
                ("CLAUDECODE", "1"),
                ("CLAUDE_CODE_ENTRYPOINT", "cli"),
                ("ZED_TERM", "true"),
                ("TERM_PROGRAM", "zed"),
                ("TMUX", "/tmp/tmux-501/default,1,0"),
                ("LANG", "fr_FR.UTF-8"),
            ]),
            None,
        );
        assert_eq!(
            env,
            vars(&[
                ("COLORTERM", "truecolor"),
                ("HOME", "/Users/r"),
                ("LANG", "fr_FR.UTF-8"),
                ("PATH", "/usr/bin"),
            ])
        );
    }

    #[test]
    fn keeps_locale_categories() {
        let env = server_env(vars(&[("LC_ALL", "fr_FR.UTF-8"), ("LANG", "C")]), None);
        assert!(env.contains(&("LC_ALL".to_string(), "fr_FR.UTF-8".to_string())));
    }

    #[test]
    fn derives_lang_from_apple_locale_when_missing() {
        let env = server_env(vars(&[("HOME", "/Users/r")]), Some("fr_FR@rg=frzzzz\n"));
        assert!(env.contains(&("LANG".to_string(), "fr_FR.UTF-8".to_string())));
    }

    #[test]
    fn falls_back_to_en_us_without_any_locale() {
        let env = server_env(Vec::new(), None);
        assert!(env.contains(&("LANG".to_string(), "en_US.UTF-8".to_string())));
    }
}
