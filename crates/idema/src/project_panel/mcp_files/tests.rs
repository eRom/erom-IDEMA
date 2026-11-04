use serde_json::{Value, json};

use super::*;

const USER_CLAUDE: &str = include_str!("../testdata/user.mcp.json");
const USER_OPENCODE: &str = include_str!("../testdata/user.opencode.json");
const USER_CODEX: &str = include_str!("../testdata/user.codex.toml");
const USER_GROK: &str = include_str!("../testdata/user.grok.toml");
const USER_AGY: &str = include_str!("../testdata/user.agents.mcp_config.json");
const OPENCODE_ADD: &str = include_str!("../testdata/opencode-add.sample.json");
const GROK_ADD: &str = include_str!("../testdata/grok-add-config.toml");

fn read_one(harness: Harness, text: &str, name: &str) -> ReadServer {
    let servers = read_servers(harness, text).expect("the sample reads");
    servers
        .get(name)
        .unwrap_or_else(|| panic!("{name} missing"))
        .clone()
}

fn normalized(harness: Harness, text: &str, name: &str) -> Value {
    normalize(&read_one(harness, text, name).core)
}

fn put(name: &str, core: Value) -> ServerChange {
    ServerChange::Put {
        name: name.to_string(),
        core,
    }
}

fn remove(name: &str) -> ServerChange {
    ServerChange::Remove {
        name: name.to_string(),
    }
}

fn stdio_core() -> Value {
    json!({
        "type": "stdio",
        "command": "/usr/bin/tool",
        "args": ["--flag", "two words"],
        "env": {"FOO": "bar"}
    })
}

fn http_core() -> Value {
    json!({
        "type": "http",
        "url": "https://example.invalid/mcp",
        "headers": {"Authorization": "Bearer x"}
    })
}

fn parse_json(text: &str) -> Value {
    serde_json::from_str(text).expect("valid JSON output")
}

fn parse_toml(text: &str) -> toml::Table {
    text.parse().expect("valid TOML output")
}

#[test]
fn each_sample_reads_to_the_same_core() {
    let expected = json!({
        "type": "stdio",
        "command": "/Users/me/.bun/bin/bun",
        "args": ["/Users/me/dev/tmp/skills-sh-tests2/hello-mcp/server.ts"],
        "env": {}
    });
    let samples = [
        (Harness::Claude, USER_CLAUDE),
        (Harness::Codex, USER_CODEX),
        (Harness::Grok, USER_GROK),
        (Harness::OpenCode, USER_OPENCODE),
        (Harness::Antigravity, USER_AGY),
    ];
    for (harness, text) in samples {
        let server = read_one(harness, text, "hello-demo");
        assert_eq!(
            normalize(&server.core),
            expected,
            "{} disagrees",
            harness.name()
        );
        assert!(server.supported, "{} unsupported", harness.name());
        assert!(!server.disabled, "{} disabled", harness.name());
        assert!(
            server.extras.is_empty(),
            "{} extras {:?}",
            harness.name(),
            server.extras
        );
    }
}

#[test]
fn harness_labels_and_files_are_stable() {
    let chips: Vec<_> = Harness::ALL.iter().map(|h| h.chip()).collect();
    assert_eq!(chips, ["CLAUDE", "CODEX", "GROK", "OPENCODE", "AGY"]);
    let names: Vec<_> = Harness::ALL.iter().map(|h| h.name()).collect();
    assert_eq!(
        names,
        ["Claude", "Codex", "Grok", "OpenCode", "Antigravity"]
    );
    let files: Vec<_> = Harness::ALL.iter().map(|h| h.mcp_file()).collect();
    assert_eq!(
        files,
        [
            ".mcp.json",
            ".codex/config.toml",
            ".grok/config.toml",
            "opencode.json",
            ".agents/mcp_config.json"
        ]
    );
}

#[test]
fn normalize_fills_the_defaults() {
    assert_eq!(
        normalize(&json!({"command": "x"})),
        json!({"type": "stdio", "command": "x", "args": [], "env": {}})
    );
    assert_eq!(
        normalize(&json!({"type": "http", "url": "https://a.invalid"})),
        json!({"type": "http", "url": "https://a.invalid", "headers": {}})
    );
}

#[test]
fn opencode_reads_v2_and_legacy() {
    let v2 = read_one(Harness::OpenCode, OPENCODE_ADD, "hello");
    assert_eq!(normalize(&v2.core)["env"], json!({"FOO": "bar"}));
    assert_eq!(
        normalize(&v2.core)["command"],
        json!("/Users/me/.bun/bin/bun")
    );
    assert!(!v2.disabled);

    let legacy_text = r#"{
  "mcp": {
    "legacy": {
      "type": "local",
      "command": ["node", "server.js", "--port", "1"],
      "enabled": false
    }
  }
}"#;
    let legacy = read_one(Harness::OpenCode, legacy_text, "legacy");
    assert!(legacy.disabled);
    assert!(!legacy.extras.iter().any(|key| key == "enabled"));
    assert_eq!(
        normalize(&legacy.core),
        json!({"type": "stdio", "command": "node", "args": ["server.js", "--port", "1"], "env": {}})
    );
}

#[test]
fn remote_keys_translate_per_harness() {
    let core = http_core();

    let opencode = write_servers(Harness::OpenCode, None, &[put("r", core.clone())]).unwrap();
    let entry = &parse_json(&opencode)["mcp"]["servers"]["r"];
    assert_eq!(entry["type"], "remote");
    assert_eq!(entry["url"], "https://example.invalid/mcp");
    assert_eq!(entry["headers"], json!({"Authorization": "Bearer x"}));

    let codex = write_servers(Harness::Codex, None, &[put("r", core.clone())]).unwrap();
    let table = parse_toml(&codex);
    let entry = &table["mcp_servers"]["r"];
    assert_eq!(entry["url"].as_str(), Some("https://example.invalid/mcp"));
    assert_eq!(
        entry["http_headers"]["Authorization"].as_str(),
        Some("Bearer x")
    );
    assert!(entry.get("headers").is_none());

    let grok = write_servers(Harness::Grok, None, &[put("r", core.clone())]).unwrap();
    let table = parse_toml(&grok);
    let entry = &table["mcp_servers"]["r"];
    assert_eq!(entry["url"].as_str(), Some("https://example.invalid/mcp"));
    assert_eq!(entry["headers"]["Authorization"].as_str(), Some("Bearer x"));

    let agy = write_servers(Harness::Antigravity, None, &[put("r", core.clone())]).unwrap();
    let entry = &parse_json(&agy)["mcpServers"]["r"];
    assert_eq!(entry["serverUrl"], "https://example.invalid/mcp");
    assert!(entry.get("url").is_none());
    assert_eq!(entry["headers"], json!({"Authorization": "Bearer x"}));

    let claude = write_servers(Harness::Claude, None, &[put("r", core.clone())]).unwrap();
    assert_eq!(parse_json(&claude)["mcpServers"]["r"]["type"], "http");

    let texts = [
        (Harness::Claude, claude),
        (Harness::Codex, codex),
        (Harness::Grok, grok),
        (Harness::OpenCode, opencode),
        (Harness::Antigravity, agy),
    ];
    for (harness, text) in texts {
        assert_eq!(
            normalized(harness, &text, "r"),
            normalize(&core),
            "{} round trip",
            harness.name()
        );
    }

    let agy_plain_url =
        r#"{"mcpServers": {"r": {"url": "https://example.invalid/mcp", "headers": {"A": "b"}}}}"#;
    assert_eq!(
        normalized(Harness::Antigravity, agy_plain_url, "r"),
        json!({"type": "http", "url": "https://example.invalid/mcp", "headers": {"A": "b"}})
    );
}

#[test]
fn stdio_core_round_trips_in_every_format() {
    for harness in Harness::ALL {
        let text = write_servers(harness, None, &[put("s", stdio_core())]).unwrap();
        assert_eq!(
            normalized(harness, &text, "s"),
            stdio_core(),
            "{}",
            harness.name()
        );
    }
}

#[test]
fn grok_sample_reads_both_transports() {
    assert_eq!(
        normalized(Harness::Grok, GROK_ADD, "st"),
        json!({"type": "stdio", "command": "/Users/me/.bun/bin/bun", "args": ["x.ts"], "env": {"FOO": "bar"}})
    );
    assert_eq!(
        normalized(Harness::Grok, GROK_ADD, "rem"),
        json!({"type": "http", "url": "https://example.invalid/mcp", "headers": {"Authorization": "Bearer x"}})
    );
}

#[test]
fn writing_keeps_the_rest_of_the_file() {
    let opencode = r#"{
  "$schema": "https://opencode.ai/config.json",
  "model": "anthropic/x",
  "mcp": {
    "servers": {
      "a": {
        "type": "local",
        "command": ["a"]
      }
    }
  },
  "theme": "dark"
}
"#;
    let written =
        write_servers(Harness::OpenCode, Some(opencode), &[put("b", stdio_core())]).unwrap();
    let parsed = parse_json(&written);
    let keys: Vec<_> = parsed.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["$schema", "model", "mcp", "theme"]);
    assert_eq!(parsed["model"], "anthropic/x");
    let servers: Vec<_> = parsed["mcp"]["servers"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(servers, ["a", "b"]);
    assert!(written.ends_with("}\n"));
    assert!(written.contains("\n  \"model\""));

    let codex = r#"# réglages de l'utilisateur
model = "gpt-x"

[projects."/x"]
trust_level = "trusted"

[mcp_servers.a]
command = "old"
args = ["one"]
startup_timeout_sec = 20
"#;
    let new_core = json!({"type": "stdio", "command": "old", "args": ["two", "three"]});
    let written = write_servers(Harness::Codex, Some(codex), &[put("a", new_core)]).unwrap();
    assert!(written.contains("# réglages de l'utilisateur"));
    assert!(written.contains("[projects.\"/x\"]"));
    assert!(written.contains("trust_level = \"trusted\""));
    assert!(written.contains("startup_timeout_sec = 20"));
    let table = parse_toml(&written);
    assert_eq!(
        table["mcp_servers"]["a"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>(),
        ["two", "three"]
    );
    let read = read_one(Harness::Codex, &written, "a");
    assert_eq!(read.extras, ["startup_timeout_sec"]);
}

#[test]
fn put_on_an_existing_entry_keeps_extras_and_the_disabled_flag() {
    let claude =
        r#"{"mcpServers": {"a": {"type": "stdio", "cwd": "/w", "command": "x", "args": ["1"]}}}"#;
    let written = write_servers(
        Harness::Claude,
        Some(claude),
        &[put(
            "a",
            json!({"type": "http", "url": "https://u.invalid"}),
        )],
    )
    .unwrap();
    let entry = &parse_json(&written)["mcpServers"]["a"];
    assert_eq!(entry["cwd"], "/w");
    assert_eq!(entry["url"], "https://u.invalid");
    assert!(entry.get("command").is_none());
    assert!(entry.get("args").is_none());

    let agy =
        r#"{"mcpServers": {"a": {"command": "x", "disabled": true, "oauth": {"clientId": "i"}}}}"#;
    let written = write_servers(
        Harness::Antigravity,
        Some(agy),
        &[put("a", json!({"command": "y"}))],
    )
    .unwrap();
    let read = read_one(Harness::Antigravity, &written, "a");
    assert!(read.disabled);
    assert_eq!(read.extras, ["oauth"]);
    assert_eq!(normalize(&read.core)["command"], "y");

    let grok = "[mcp_servers.a]\ncommand = \"x\"\nenabled = false\n";
    let written = write_servers(
        Harness::Grok,
        Some(grok),
        &[put("a", json!({"command": "y", "env": {"K": "v"}}))],
    )
    .unwrap();
    let read = read_one(Harness::Grok, &written, "a");
    assert!(read.disabled);
    assert_eq!(normalize(&read.core)["env"], json!({"K": "v"}));
}

#[test]
fn legacy_opencode_entry_moves_to_v2_on_put() {
    let legacy = r#"{
  "mcp": {
    "legacy": {
      "type": "local",
      "command": ["node", "old.js"],
      "enabled": false,
      "timeout": 5000,
      "cwd": "/w"
    }
  }
}
"#;
    let before = read_one(Harness::OpenCode, legacy, "legacy");
    assert!(before.extras.iter().any(|key| key == "timeout"));

    let core = json!({"type": "stdio", "command": "node", "args": ["new.js"]});
    let written = write_servers(Harness::OpenCode, Some(legacy), &[put("legacy", core)]).unwrap();
    let parsed = parse_json(&written);
    assert!(parsed["mcp"].get("legacy").is_none());
    let entry = &parsed["mcp"]["servers"]["legacy"];
    assert_eq!(entry["disabled"], true);
    assert!(entry.get("enabled").is_none());
    assert!(entry.get("timeout").is_none());
    assert_eq!(entry["cwd"], "/w");
    assert_eq!(entry["command"], json!(["node", "new.js"]));

    let after = read_one(Harness::OpenCode, &written, "legacy");
    assert!(after.disabled);
    assert!(!after.extras.iter().any(|key| key == "timeout"));
    assert_eq!(after.extras, ["cwd"]);
}

#[test]
fn legacy_object_timeout_survives_the_move() {
    let legacy =
        r#"{"mcp": {"x": {"type": "local", "command": ["c"], "timeout": {"startup": 9}}}}"#;
    let written = write_servers(
        Harness::OpenCode,
        Some(legacy),
        &[put("x", json!({"command": "c"}))],
    )
    .unwrap();
    let entry = &parse_json(&written)["mcp"]["servers"]["x"];
    assert_eq!(entry["timeout"], json!({"startup": 9}));
}

#[test]
fn disabled_flags_are_read_everywhere() {
    let opencode_v2 =
        r#"{"mcp": {"servers": {"a": {"type": "local", "command": ["x"], "disabled": true}}}}"#;
    let opencode_legacy =
        r#"{"mcp": {"a": {"type": "local", "command": ["x"], "enabled": false}}}"#;
    let agy = r#"{"mcpServers": {"a": {"command": "x", "disabled": true}}}"#;
    let grok = "[mcp_servers.a]\ncommand = \"x\"\nenabled = false\n";
    let codex = "[mcp_servers.a]\ncommand = \"x\"\nenabled = false\n";
    let cases = [
        (Harness::OpenCode, opencode_v2),
        (Harness::OpenCode, opencode_legacy),
        (Harness::Antigravity, agy),
        (Harness::Grok, grok),
        (Harness::Codex, codex),
    ];
    for (harness, text) in cases {
        let server = read_one(harness, text, "a");
        assert!(server.disabled, "{} not disabled", harness.name());
        assert!(
            server.extras.is_empty(),
            "{} extras {:?}",
            harness.name(),
            server.extras
        );
    }

    let enabled_true = "[mcp_servers.a]\ncommand = \"x\"\nenabled = true\n";
    assert!(!read_one(Harness::Grok, enabled_true, "a").disabled);
}

#[test]
fn a_missing_file_is_created_in_each_format() {
    for harness in Harness::ALL {
        let text = write_servers(harness, None, &[put("created", stdio_core())]).unwrap();
        let servers = read_servers(harness, &text).unwrap();
        assert_eq!(servers.len(), 1, "{}", harness.name());
        assert!(servers.contains_key("created"));
        assert!(text.ends_with('\n'), "{} ends with newline", harness.name());
    }
    let opencode = write_servers(Harness::OpenCode, None, &[put("created", stdio_core())]).unwrap();
    assert!(
        opencode.starts_with("{\n  \"$schema\": \"https://opencode.ai/config.json\""),
        "{opencode}"
    );
    let toml_text = write_servers(Harness::Codex, None, &[put("created", stdio_core())]).unwrap();
    assert!(!toml_text.contains("[mcp_servers]"), "{toml_text}");
    assert!(toml_text.contains("[mcp_servers.created]"));
}

#[test]
fn remove_drops_only_that_server() {
    let originals = [
        (Harness::Claude, USER_CLAUDE),
        (Harness::Codex, USER_CODEX),
        (Harness::Grok, USER_GROK),
        (Harness::OpenCode, USER_OPENCODE),
        (Harness::Antigravity, USER_AGY),
    ];
    for (harness, original) in originals {
        let with_two =
            write_servers(harness, Some(original), &[put("other", http_core())]).unwrap();
        let after = write_servers(harness, Some(&with_two), &[remove("hello-demo")]).unwrap();
        let servers = read_servers(harness, &after).unwrap();
        assert_eq!(
            servers.keys().cloned().collect::<Vec<_>>(),
            ["other"],
            "{}",
            harness.name()
        );
    }
    let toml_with_comment = "# garde-moi\nmodel = \"m\"\n\n[mcp_servers.a]\ncommand = \"x\"\n\n[mcp_servers.b]\ncommand = \"y\"\n";
    let after = write_servers(Harness::Codex, Some(toml_with_comment), &[remove("a")]).unwrap();
    assert!(after.contains("# garde-moi"));
    assert!(!after.contains("command = \"x\""));
    assert!(after.contains("command = \"y\""));

    let legacy = r#"{"mcp": {"old": {"type": "local", "command": ["x"]}, "servers": {"keep": {"type": "local", "command": ["y"]}}}}"#;
    let after = write_servers(Harness::OpenCode, Some(legacy), &[remove("old")]).unwrap();
    let servers = read_servers(Harness::OpenCode, &after).unwrap();
    assert_eq!(servers.keys().cloned().collect::<Vec<_>>(), ["keep"]);
}

#[test]
fn removing_from_a_missing_file_creates_nothing() {
    for harness in Harness::ALL {
        let text = write_servers(harness, None, &[remove("ghost")]).unwrap();
        assert!(read_servers(harness, &text).unwrap().is_empty());
    }
}

#[test]
fn toml_keys_that_need_quotes_round_trip() {
    for harness in [Harness::Codex, Harness::Grok] {
        let mut text = write_servers(
            harness,
            None,
            &[put("my.server", stdio_core()), put("a b", http_core())],
        )
        .unwrap();
        let servers = read_servers(harness, &text).unwrap();
        assert_eq!(
            servers.keys().cloned().collect::<Vec<_>>(),
            ["a b", "my.server"],
            "{}",
            harness.name()
        );
        assert_eq!(normalize(&servers["my.server"].core), stdio_core());
        let table = parse_toml(&text);
        assert!(
            table["mcp_servers"]
                .as_table()
                .unwrap()
                .contains_key("my.server")
        );

        // Réécriture à l'identique d'un nom déjà présent : la clé reste citée.
        text = write_servers(harness, Some(&text), &[put("my.server", stdio_core())]).unwrap();
        assert_eq!(
            parse_toml(&text)["mcp_servers"].as_table().unwrap().len(),
            2
        );
    }
}

#[test]
fn json_names_outside_the_safe_charset_are_kept() {
    let text = r#"{"mcpServers": {"my.server é": {"command": "x"}}}"#;
    let written = write_servers(
        Harness::Claude,
        Some(text),
        &[put("my.server é", json!({"command": "y"}))],
    )
    .unwrap();
    assert_eq!(
        normalized(Harness::Claude, &written, "my.server é")["command"],
        "y"
    );
}

#[test]
fn an_sse_server_is_read_but_not_supported() {
    let text = r#"{"mcpServers": {"s": {"type": "sse", "url": "https://s.invalid/sse"}}}"#;
    let server = read_one(Harness::Claude, text, "s");
    assert!(!server.supported);
    assert_eq!(server.core["type"], "sse");

    let error = write_servers(
        Harness::Claude,
        Some(text),
        &[put(
            "s",
            json!({"type": "sse", "url": "https://s.invalid/sse"}),
        )],
    )
    .unwrap_err();
    assert!(!error.is_empty());
}

#[test]
fn put_rejects_malformed_cores() {
    for core in [
        json!({"type": "stdio"}),
        json!({"command": "x", "args": "not-an-array"}),
        json!({"command": "x", "env": {"A": 1}}),
        json!({"type": "http"}),
        json!({"type": "http", "url": "u", "headers": ["x"]}),
        json!("text"),
    ] {
        for harness in Harness::ALL {
            assert!(
                write_servers(harness, None, &[put("n", core.clone())]).is_err(),
                "{} accepted {core}",
                harness.name()
            );
        }
    }
}

#[test]
fn unreadable_files_report_without_leaking_content() {
    let secret = "sk-very-secret-value";
    let broken_toml = format!("[mcp_servers.a]\nenv = {{ KEY = \"{secret}\"\ncommand = ");
    let error = read_servers(Harness::Codex, &broken_toml).unwrap_err();
    assert!(!error.contains(secret), "{error}");
    assert!(error.contains("ligne"), "{error}");
    let error = write_servers(
        Harness::Codex,
        Some(&broken_toml),
        &[put("a", stdio_core())],
    )
    .unwrap_err();
    assert!(!error.contains(secret), "{error}");

    let jsonc =
        format!("{{\n  // commentaire\n  \"mcp\": {{\"servers\": {{}}, \"k\": \"{secret}\"}}\n}}");
    let error = read_servers(Harness::OpenCode, &jsonc).unwrap_err();
    assert!(!error.contains(secret), "{error}");
    assert!(write_servers(Harness::OpenCode, Some(&jsonc), &[put("a", stdio_core())]).is_err());
}

#[test]
fn files_without_a_mcp_section_read_as_empty() {
    assert!(read_servers(Harness::Claude, "{}").unwrap().is_empty());
    assert!(
        read_servers(Harness::OpenCode, "{\"model\": \"x\"}")
            .unwrap()
            .is_empty()
    );
    assert!(
        read_servers(Harness::Codex, "model = \"x\"\n")
            .unwrap()
            .is_empty()
    );
    assert!(read_servers(Harness::Grok, "").unwrap().is_empty());
    assert!(read_servers(Harness::Antigravity, "").unwrap().is_empty());
}

#[test]
fn unknown_toml_keys_are_listed_as_extras() {
    let text = "[mcp_servers.f]\nurl = \"https://f.invalid\"\nbearer_token_env_var = \"T\"\n\n[mcp_servers.f.http_headers]\nX = \"y\"\n";
    let server = read_one(Harness::Codex, text, "f");
    assert_eq!(server.extras, ["bearer_token_env_var"]);
    assert_eq!(
        normalize(&server.core),
        json!({"type": "http", "url": "https://f.invalid", "headers": {"X": "y"}})
    );
}

#[test]
fn toml_edits_keep_comments_and_separate_new_tables() {
    let codex = "# réglages\nmodel = \"gpt\"\n\n[mcp_servers.a]\ncommand = \"old\" # note\n# avant args\nargs = [\"one\"]\nstartup_timeout_sec = 20\n\n[mcp_servers.a.env]\nK = \"v\"\n";
    let written = write_servers(
        Harness::Codex,
        Some(codex),
        &[put("a", stdio_core()), put("b", http_core())],
    )
    .unwrap();
    let expected = "# réglages\nmodel = \"gpt\"\n\n[mcp_servers.a]\ncommand = \"/usr/bin/tool\" # note\n# avant args\nargs = [\"--flag\", \"two words\"]\nstartup_timeout_sec = 20\n\n[mcp_servers.a.env]\nFOO = \"bar\"\n\n[mcp_servers.b]\nurl = \"https://example.invalid/mcp\"\n\n[mcp_servers.b.http_headers]\nAuthorization = \"Bearer x\"\n";
    assert_eq!(written, expected);

    // Fichier sans retour final : la table neuve ne se colle pas à la dernière ligne.
    let written = write_servers(
        Harness::Grok,
        Some("model = \"x\""),
        &[put("s", stdio_core())],
    )
    .unwrap();
    assert!(
        parse_toml(&written).contains_key("mcp_servers"),
        "{written}"
    );
    assert!(
        written.starts_with("model = \"x\"\n\n[mcp_servers.s]\n"),
        "{written}"
    );
}

#[test]
fn an_opencode_server_switching_type_drops_the_keys_of_the_other_type() {
    let local = r#"{"mcp":{"servers":{"s":{"type":"local","command":["bun","a.ts"],"cwd":"tools","environment":{"K":"v"},"timeout":{"startup":9}}}}}"#;
    let remote_core = json!({"type": "http", "url": "https://example.com/mcp"});
    let written = write_servers(Harness::OpenCode, Some(local), &[put("s", remote_core)]).unwrap();
    let entry = &parse_json(&written)["mcp"]["servers"]["s"];
    assert_eq!(entry["type"], "remote");
    assert_eq!(entry["url"], "https://example.com/mcp");
    // Les clés du type quitté partent, celles que les deux types portent restent.
    for gone in ["cwd", "command", "environment"] {
        assert!(entry.get(gone).is_none(), "{gone} survived: {entry}");
    }
    assert_eq!(entry["timeout"]["startup"], 9);

    let remote = r#"{"mcp":{"servers":{"s":{"type":"remote","url":"https://example.com/mcp","oauth":{"clientId":"x"},"headers":{"A":"b"}}}}}"#;
    // Enregistré en stdio : `oauth` et `headers` partent.
    let written =
        write_servers(Harness::OpenCode, Some(remote), &[put("s", stdio_core())]).unwrap();
    let entry = &parse_json(&written)["mcp"]["servers"]["s"];
    assert_eq!(entry["type"], "local");
    for gone in ["oauth", "headers", "url"] {
        assert!(entry.get(gone).is_none(), "{gone} survived: {entry}");
    }
    // Un enregistrement qui ne change pas de type garde ses clés propres.
    let same = json!({"type": "http", "url": "https://example.com/other"});
    let written = write_servers(Harness::OpenCode, Some(remote), &[put("s", same)]).unwrap();
    let entry = &parse_json(&written)["mcp"]["servers"]["s"];
    assert_eq!(entry["oauth"]["clientId"], "x");
    let local_same =
        write_servers(Harness::OpenCode, Some(local), &[put("s", stdio_core())]).unwrap();
    assert_eq!(
        parse_json(&local_same)["mcp"]["servers"]["s"]["cwd"],
        "tools"
    );
}
