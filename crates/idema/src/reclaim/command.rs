//! Construction de la commande de reprise tapée dans un pane zsh. Du texte pur, aucun effet de bord.
//! Les noms de session et les dossiers sont du texte libre : le quoting est la frontière de sécurité.

use std::path::Path;

use crate::restore::claude::is_uuid;

/// Mode `--remote-control` d'une session : absent, nu, ou nommé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteControl {
    Off,
    On,
    Named(String),
}

/// Tout ce qu'il faut pour relancer une session dans un pane.
#[derive(Debug, Clone)]
pub struct ResumeSpec<'a> {
    pub cwd: &'a Path,
    pub agent: &'a str,
    pub engine: Option<&'a str>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub remote_control: RemoteControl,
    /// Toujours un UUID déjà validé : le seul mot qui n'est pas quoté.
    pub session_id: &'a str,
}

/// Entoure une valeur de quotes simples pour zsh ; une quote simple interne devient `'\''`.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// `cd <cwd> && <relaunch> --resume <id>`, où `<relaunch>` est `relaunch_command`. Mêmes refus que `relaunch_command`.
pub fn resume_command(spec: &ResumeSpec) -> Option<String> {
    let relaunch = relaunch_command(spec)?;
    Some(format!(
        "cd {} && {relaunch} --resume {}",
        shell_quote(&spec.cwd.to_string_lossy()),
        spec.session_id
    ))
}

/// `idnma run <agent> [--engine ..] [--model ..] [--effort ..] [--remote-control [..]]` : la commande de relance d'une
/// session, sans le `cd` ni le `--resume`. C'est ce que la restauration rejoue (`@idema_resume`) avec sa conversation.
/// Chaque valeur libre passe par `shell_quote`.
///
/// `None` quand la session ne peut pas être reprise sans risque : les quotes arrêtent le shell mais pas l'éditeur de
/// ligne du terminal, qui reçoit les octets bruts (Ctrl-U, Ctrl-C, ESC, retour à la ligne y sont des commandes). Un
/// caractère de contrôle dans `cwd`, l'agent, le moteur, le modèle, l'effort ou le nom remote-control, un `cwd` non
/// absolu, un mot ou un nom remote-control qui commence par `-` (il serait lu comme un flag d'`idnma`) ou un `session_id`
/// qui n'est pas un UUID rendent donc la commande impossible : l'interface grise la session.
pub fn relaunch_command(spec: &ResumeSpec) -> Option<String> {
    let cwd = spec.cwd.to_string_lossy();
    let remote_name = match &spec.remote_control {
        RemoteControl::Named(name) => Some(name.as_str()),
        _ => None,
    };
    let free_texts = [
        Some(cwd.as_ref()),
        Some(spec.agent),
        spec.engine,
        spec.model,
        spec.effort,
        remote_name,
    ];
    let words = [
        Some(spec.agent),
        spec.engine,
        spec.model,
        spec.effort,
        remote_name,
    ];
    if free_texts
        .into_iter()
        .flatten()
        .any(|text| text.chars().any(char::is_control))
        || words
            .into_iter()
            .flatten()
            .any(|word| word.starts_with('-'))
        || !spec.cwd.is_absolute()
        || !is_uuid(spec.session_id)
    {
        return None;
    }
    let mut command = format!("idnma run {}", shell_quote(spec.agent));
    for (flag, value) in [
        ("--engine", spec.engine),
        ("--model", spec.model),
        ("--effort", spec.effort),
    ] {
        if let Some(value) = value {
            command.push_str(&format!(" {flag} {}", shell_quote(value)));
        }
    }
    match &spec.remote_control {
        RemoteControl::Off => {}
        RemoteControl::On => command.push_str(" --remote-control"),
        RemoteControl::Named(name) => {
            command.push_str(&format!(" --remote-control {}", shell_quote(name)));
        }
    }
    Some(command)
}

/// Valeur d'un flag dans une ligne de commande (`--model opus` ou `--model=opus`). `None` si le flag est absent, s'il
/// est le dernier mot, ou si le mot suivant est lui-même un flag.
pub fn flag_value<'a>(command: &'a str, flag: &str) -> Option<&'a str> {
    let mut words = command.split_whitespace();
    while let Some(word) = words.next() {
        if word == flag {
            return words.next().filter(|next| !next.starts_with('-'));
        }
        if let Some(value) = word
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return Some(value).filter(|value| !value.is_empty());
        }
    }
    None
}

/// Mode `--remote-control` d'une ligne `ps` : le mot suivant en est le nom s'il ne commence pas par `-`.
pub fn remote_control_of(command: &str) -> RemoteControl {
    const FLAG: &str = "--remote-control";
    let mut words = command.split_whitespace();
    while let Some(word) = words.next() {
        if word == FLAG {
            return match words.next().filter(|next| !next.starts_with('-')) {
                Some(name) => RemoteControl::Named(name.to_string()),
                None => RemoteControl::On,
            };
        }
        if let Some(name) = word
            .strip_prefix(FLAG)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return if name.is_empty() {
                RemoteControl::On
            } else {
                RemoteControl::Named(name.to_string())
            };
        }
    }
    RemoteControl::Off
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const ID: &str = "e0dfc540-e15f-4a04-8d36-36059c2501aa";
    const PS_FLORA: &str = "claude --model opus --name claude-flora --session-id e0dfc540-e15f-4a04-8d36-36059c2501aa --effort medium --dangerously-skip-permissions --remote-control";
    const PS_IDEATION: &str = "claude --setting-sources project,local --model fable --effort max --name erom-ideation-2 --remote-control erom-ideation-2 --resume 62e4b16e-fadf-4b5c-bd1c-703d84c7441c";
    const PS_DEMO: &str = "/Users/me/.local/bin/claude --dangerously-skip-permissions --remote-control demo-poc-ceres -n demo-poc-ceres";

    fn spec(cwd: &Path) -> ResumeSpec<'_> {
        ResumeSpec {
            cwd,
            agent: "glm",
            engine: None,
            model: None,
            effort: None,
            remote_control: RemoteControl::Off,
            session_id: ID,
        }
    }

    /// Exécute un script dans un `zsh -f` dont le dossier courant est un dossier temporaire jetable : une régression du
    /// quoting ne peut ainsi créer aucun fichier dans le dépôt. Rend la sortie standard.
    fn run_zsh(label: &str, script: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("idema-command-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let output = Command::new("zsh")
            .arg("-f")
            .arg("-c")
            .arg(script)
            .current_dir(&dir)
            .output()
            .expect("zsh must be available");
        std::fs::remove_dir_all(&dir).expect("temp dir removal");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn builds_the_full_command() {
        let command = resume_command(&ResumeSpec {
            model: Some("glm-5.3-flash:cloud[1m]"),
            effort: Some("max"),
            ..spec(Path::new("/p/a b"))
        });
        assert_eq!(
            command,
            Some(format!(
                "cd '/p/a b' && idnma run 'glm' --model 'glm-5.3-flash:cloud[1m]' --effort 'max' --resume {ID}"
            ))
        );
    }

    #[test]
    fn the_relaunch_command_has_neither_the_cd_nor_the_resume() {
        let full = ResumeSpec {
            engine: Some("gflash"),
            model: Some("haiku"),
            effort: Some("medium"),
            remote_control: RemoteControl::Named("my session".into()),
            ..spec(Path::new("/p/a b"))
        };
        assert_eq!(
            relaunch_command(&full).as_deref(),
            Some(
                "idnma run 'glm' --engine 'gflash' --model 'haiku' --effort 'medium' --remote-control 'my session'"
            )
        );
        let bare = ResumeSpec {
            remote_control: RemoteControl::On,
            model: Some("haiku"),
            effort: Some("medium"),
            ..spec(Path::new("/p"))
        };
        assert_eq!(
            relaunch_command(&bare).as_deref(),
            Some("idnma run 'glm' --model 'haiku' --effort 'medium' --remote-control")
        );
    }

    #[test]
    fn the_resume_command_is_the_relaunch_between_a_cd_and_a_resume() {
        let full = ResumeSpec {
            model: Some("haiku"),
            effort: Some("medium"),
            remote_control: RemoteControl::On,
            ..spec(Path::new("/p/a b"))
        };
        let relaunch = relaunch_command(&full).expect("a plain spec is relaunchable");
        assert_eq!(
            resume_command(&full),
            Some(format!("cd '/p/a b' && {relaunch} --resume {ID}"))
        );
    }

    #[test]
    fn a_refused_spec_has_no_relaunch_command() {
        assert_eq!(relaunch_command(&spec(Path::new("p/a"))), None);
        let refused = ResumeSpec {
            model: Some("-x"),
            ..spec(Path::new("/p"))
        };
        assert_eq!(relaunch_command(&refused), None);
        let control = ResumeSpec {
            effort: Some("a\nb"),
            ..spec(Path::new("/p"))
        };
        assert_eq!(relaunch_command(&control), None);
        let not_a_uuid = ResumeSpec {
            session_id: "nope",
            ..spec(Path::new("/p"))
        };
        assert_eq!(relaunch_command(&not_a_uuid), None);
    }

    #[test]
    fn engine_comes_right_after_the_agent() {
        let command = resume_command(&ResumeSpec {
            engine: Some("gflash"),
            model: Some("m"),
            ..spec(Path::new("/p"))
        });
        assert_eq!(
            command,
            Some(format!(
                "cd '/p' && idnma run 'glm' --engine 'gflash' --model 'm' --resume {ID}"
            ))
        );
    }

    #[test]
    fn bare_remote_control_has_no_value() {
        let command = resume_command(&ResumeSpec {
            remote_control: RemoteControl::On,
            ..spec(Path::new("/p"))
        });
        assert_eq!(
            command,
            Some(format!(
                "cd '/p' && idnma run 'glm' --remote-control --resume {ID}"
            ))
        );
    }

    #[test]
    fn named_remote_control_is_quoted_and_precedes_resume() {
        let command = resume_command(&ResumeSpec {
            effort: Some("max"),
            remote_control: RemoteControl::Named("x".into()),
            ..spec(Path::new("/p"))
        });
        assert_eq!(
            command,
            Some(format!(
                "cd '/p' && idnma run 'glm' --effort 'max' --remote-control 'x' --resume {ID}"
            ))
        );
    }

    #[test]
    fn a_cwd_with_an_apostrophe_stays_one_word() {
        let raw = "/tmp/l'été; touch x";
        let stdout = run_zsh("cwd", &format!("printf %s\\\\n {}", shell_quote(raw)));
        assert_eq!(stdout, format!("{raw}\n"));
    }

    #[test]
    fn a_hostile_name_stays_one_word_in_the_command() {
        let name = "a'; echo pwned #";
        let command = resume_command(&ResumeSpec {
            remote_control: RemoteControl::Named(name.into()),
            ..spec(Path::new("/p"))
        })
        .expect("a quote is not a control character");
        let segment = command
            .split(" --remote-control ")
            .nth(1)
            .and_then(|rest| rest.strip_suffix(&format!(" --resume {ID}")))
            .expect("name segment");
        let stdout = run_zsh("name", &format!("printf '%s\\n' {segment}"));
        assert_eq!(stdout, format!("{name}\n"));
    }

    #[test]
    fn a_control_character_refuses_the_command() {
        for control in ['\x15', '\x03', '\x1b', '\n', '\r'] {
            let bad = format!("a{control}; echo pwned; #");
            let bad_cwd = std::path::PathBuf::from(format!("/p/{bad}"));
            let refused = [
                ResumeSpec {
                    cwd: &bad_cwd,
                    ..spec(Path::new("/p"))
                },
                ResumeSpec {
                    agent: &bad,
                    ..spec(Path::new("/p"))
                },
                ResumeSpec {
                    engine: Some(&bad),
                    ..spec(Path::new("/p"))
                },
                ResumeSpec {
                    model: Some(&bad),
                    ..spec(Path::new("/p"))
                },
                ResumeSpec {
                    effort: Some(&bad),
                    ..spec(Path::new("/p"))
                },
                ResumeSpec {
                    remote_control: RemoteControl::Named(bad.clone()),
                    ..spec(Path::new("/p"))
                },
            ];
            for (index, spec) in refused.iter().enumerate() {
                assert_eq!(
                    resume_command(spec),
                    None,
                    "field {index} with {control:?} must refuse the command"
                );
            }
        }
    }

    #[test]
    fn a_leading_dash_refuses_the_command() {
        let base = Path::new("/p");
        let refused = [
            ResumeSpec {
                agent: "-x",
                ..spec(base)
            },
            ResumeSpec {
                engine: Some("-x"),
                ..spec(base)
            },
            ResumeSpec {
                model: Some("-x"),
                ..spec(base)
            },
            ResumeSpec {
                effort: Some("--x"),
                ..spec(base)
            },
            // `--remote-control -x` : `claude` refuserait le nom après l'arrêt de la session.
            ResumeSpec {
                remote_control: RemoteControl::Named("-x".into()),
                ..spec(base)
            },
        ];
        for (index, spec) in refused.iter().enumerate() {
            assert_eq!(resume_command(spec), None, "field {index}");
        }
    }

    #[test]
    fn a_relative_cwd_refuses_the_command() {
        assert_eq!(resume_command(&spec(Path::new("p/a"))), None);
        assert_eq!(resume_command(&spec(Path::new(""))), None);
    }

    #[test]
    fn a_session_id_that_is_not_a_uuid_refuses_the_command() {
        for id in [
            "",
            "abc",
            "$(id)",
            "e0dfc540-e15f-4a04-8d36-36059c2501aa; x",
        ] {
            let refused = ResumeSpec {
                session_id: id,
                ..spec(Path::new("/p"))
            };
            assert_eq!(resume_command(&refused), None, "{id:?}");
        }
    }

    #[test]
    fn flag_value_reads_both_spellings() {
        assert_eq!(flag_value(PS_FLORA, "--model"), Some("opus"));
        assert_eq!(flag_value(PS_IDEATION, "--effort"), Some("max"));
        assert_eq!(
            flag_value("claude --model=opus --x", "--model"),
            Some("opus")
        );
    }

    #[test]
    fn flag_value_is_none_without_a_value() {
        assert_eq!(flag_value(PS_FLORA, "--missing"), None);
        assert_eq!(flag_value(PS_FLORA, "--remote-control"), None);
        assert_eq!(flag_value("claude --model --effort max", "--model"), None);
        assert_eq!(flag_value("claude --model=", "--model"), None);
    }

    #[test]
    fn remote_control_is_read_from_real_ps_lines() {
        assert_eq!(remote_control_of(PS_FLORA), RemoteControl::On);
        assert_eq!(
            remote_control_of(PS_IDEATION),
            RemoteControl::Named("erom-ideation-2".into())
        );
        assert_eq!(
            remote_control_of(PS_DEMO),
            RemoteControl::Named("demo-poc-ceres".into())
        );
        assert_eq!(
            remote_control_of("claude --remote-control=x --resume y"),
            RemoteControl::Named("x".into())
        );
        assert_eq!(remote_control_of("claude --model opus"), RemoteControl::Off);
        assert_eq!(
            remote_control_of("claude --remote-control --resume y"),
            RemoteControl::On
        );
    }
}
