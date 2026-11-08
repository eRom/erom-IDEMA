//! Les agents du `~/.idnma.yml` d'une machine distante : leur clé, leur `label` et leur `avatar`, rien d'autre.

/// Un agent déclaré sous `agents:`.
#[derive(Debug, Clone, PartialEq)]
pub struct IdnmaAgentInfo {
    pub key: String,
    pub label: Option<String>,
    pub avatar: Option<String>,
}

/// Le nom de la clé d'une ligne `clé: valeur`, sans guillemets. `None` pour un élément de liste ou une ligne sans `:`.
fn key_of(body: &str) -> Option<&str> {
    if body.starts_with('-') {
        return None;
    }
    let (key, _) = body.split_once(':')?;
    let key = key.trim().trim_matches(|c| c == '"' || c == '\'');
    (!key.is_empty()).then_some(key)
}

/// La valeur scalaire d'une ligne `clé: valeur`, guillemets retirés. `None` pour une valeur vide (bloc imbriqué),
/// une ancre, un alias ou un commentaire seul.
fn scalar_of(body: &str) -> Option<String> {
    let (_, raw) = body.split_once(':')?;
    let raw = raw.trim();
    let value = match raw.chars().next()? {
        quote @ ('"' | '\'') => {
            let inner = &raw[1..];
            match inner.find(quote) {
                Some(end) => &inner[..end],
                None => inner,
            }
        }
        '#' | '&' | '*' | '|' | '>' => return None,
        _ => raw
            .split_once(" #")
            .map_or(raw, |(before, _)| before.trim_end()),
    };
    (!value.is_empty()).then(|| value.to_string())
}

/// Lit `~/.idnma.yml` sans analyse YAML : on compte le retrait, comme `parse_idnma_agents`. Sous `agents:` (colonne 0),
/// chaque clé au premier retrait est un agent, dans l'ordre du fichier ; ses champs `label` et `avatar` au premier
/// niveau de l'agent sont gardés s'ils sont scalaires. Tout le reste (`presets:`, `env`, ancres, fusions) est ignoré :
/// aucune autre valeur n'est jamais lue ni conservée.
pub fn parse_idnma_agent_list(text: &str) -> Vec<IdnmaAgentInfo> {
    let mut agents: Vec<IdnmaAgentInfo> = Vec::new();
    let mut in_agents = false;
    let mut agent_indent: Option<usize> = None;
    let mut field_indent: Option<usize> = None;

    for line in text.lines() {
        let line = line.trim_end();
        let body = line.trim_start();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        let indent = line.len() - body.len();
        if indent == 0 {
            in_agents = key_of(body) == Some("agents");
            agent_indent = None;
            field_indent = None;
            continue;
        }
        if !in_agents {
            continue;
        }
        let Some(key) = key_of(body) else {
            continue;
        };
        let top = *agent_indent.get_or_insert(indent);
        if indent == top {
            agents.push(IdnmaAgentInfo {
                key: key.to_string(),
                label: None,
                avatar: None,
            });
            field_indent = None;
            continue;
        }
        if indent < top {
            continue;
        }
        let Some(agent) = agents.last_mut() else {
            continue;
        };
        let field = *field_indent.get_or_insert(indent);
        if indent != field {
            continue;
        }
        match key {
            "label" => agent.label = scalar_of(body),
            "avatar" => agent.avatar = scalar_of(body),
            _ => {}
        }
    }
    agents
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(key: &str, label: &str, avatar: &str) -> IdnmaAgentInfo {
        IdnmaAgentInfo {
            key: key.into(),
            label: Some(label.into()),
            avatar: Some(avatar.into()),
        }
    }

    #[test]
    fn idnma_list_keeps_keys_labels_and_avatars_only() {
        let fixture = [
            "presets:",
            "  openrouter: &openrouter",
            "    env:",
            "      ANTHROPIC_BASE_URL: https://openrouter.ai/api",
            "      ANTHROPIC_AUTH_TOKEN: \"IDEMA-FAKE-TOKEN\"",
            "      label: not-an-agent-label",
            "",
            "agents:",
            "  opus:",
            "    label: Opus 5.5",
            "    avatar: claude",
            "    engine: claude",
            "    env:",
            "      label: nested-label-is-ignored",
            "      ANTHROPIC_AUTH_TOKEN: IDEMA-FAKE-TOKEN",
            "  sonnet:",
            "    label: Sonnet 5.5",
            "    avatar: claude",
            "  \"glm\":",
            "    <<: *openrouter",
            "    label: \"GLM 5.3\"",
            "    avatar: 'glm'",
            "    env:",
            "      ANTHROPIC_AUTH_TOKEN: \"IDEMA-FAKE-TOKEN\"",
            "other:",
            "  stray:",
            "    label: not-an-agent",
            "",
        ]
        .join("\r\n");
        let result = parse_idnma_agent_list(&fixture);
        assert_eq!(
            result,
            vec![
                info("opus", "Opus 5.5", "claude"),
                info("sonnet", "Sonnet 5.5", "claude"),
                info("glm", "GLM 5.3", "glm"),
            ]
        );
        assert!(!format!("{result:?}").contains("IDEMA-FAKE-TOKEN"));
    }

    #[test]
    fn idnma_list_leaves_missing_or_non_scalar_fields_empty() {
        let text = "agents:\n  bare:\n    engine: x\n  nested:\n    label:\n      deep: 1\n    avatar: &a pic\n";
        let result = parse_idnma_agent_list(text);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].label, None);
        assert_eq!(result[1].label, None);
        assert_eq!(result[1].avatar, None);
    }

    #[test]
    fn idnma_list_is_empty_without_agents() {
        assert!(parse_idnma_agent_list("presets:\n  a: 1\n").is_empty());
        assert!(parse_idnma_agent_list("").is_empty());
    }
}
