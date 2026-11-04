use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use super::*;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("idema-mcp-model-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn stdio(script: &str) -> Value {
    json!({"type": "stdio", "command": "bun", "args": [script]})
}

/// Écrit un serveur dans le fichier d'un harnais, par le traducteur de la Tâche 2 (le format est donc valide).
fn seed(project: &Path, harness: Harness, name: &str, core: &Value) {
    let path = file_path(project, harness);
    let original = fs::read_to_string(&path).ok();
    let change = ServerChange::Put {
        name: name.to_string(),
        core: core.clone(),
    };
    let text = write_servers(harness, original.as_deref(), &[change]).unwrap();
    write_raw(project, harness.mcp_file(), &text);
}

fn write_raw(project: &Path, rel: &str, text: &str) {
    let path = project.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn read_raw(project: &Path, rel: &str) -> String {
    fs::read_to_string(project.join(rel)).unwrap()
}

/// Contenu et date de modification : si l'un bouge, le fichier a été réécrit.
fn fingerprint(project: &Path, rel: &str) -> (Vec<u8>, SystemTime) {
    let path = project.join(rel);
    (
        fs::read(&path).unwrap(),
        fs::metadata(&path).unwrap().modified().unwrap(),
    )
}

fn age_file(project: &Path, rel: &str) {
    let file = fs::File::options()
        .write(true)
        .open(project.join(rel))
        .unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
}

fn save(name: &str, core: Value) -> McpOp {
    McpOp::Save {
        name: name.to_string(),
        core,
    }
}

fn row<'a>(servers: &'a ProjectServers, name: &str) -> &'a ServerRow {
    servers
        .rows
        .iter()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("row {name} missing"))
}

fn seed_all(project: &Path, name: &str, core: &Value) {
    for harness in Harness::ALL {
        seed(project, harness, name, core);
    }
}

#[test]
fn a_server_in_one_file_is_incomplete() {
    let dir = temp_dir("incomplete");
    seed(&dir, Harness::Claude, "hello", &stdio("hello.ts"));
    let claude_before = fingerprint(&dir, ".mcp.json");

    let servers = load(&dir);
    assert_eq!(
        row(&servers, "hello").state,
        RowState::Incomplete {
            present: vec![Harness::Claude]
        }
    );
    assert!(servers.unreadable.is_empty());

    let errors = apply(
        &dir,
        &McpOp::Propagate {
            name: "hello".into(),
        },
    );
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(fingerprint(&dir, ".mcp.json"), claude_before);
    let after = load(&dir);
    assert_eq!(row(&after, "hello").state, RowState::Complete);
    for harness in Harness::ALL {
        assert!(
            file_path(&dir, harness).is_file(),
            "{harness:?} not written"
        );
    }
}

#[test]
fn same_name_two_definitions_is_divergent() {
    let dir = temp_dir("divergent");
    for harness in [
        Harness::Codex,
        Harness::Grok,
        Harness::OpenCode,
        Harness::Antigravity,
    ] {
        seed(&dir, harness, "srv", &stdio("server.ts"));
    }
    seed(&dir, Harness::Claude, "srv", &stdio("main.ts"));

    let servers = load(&dir);
    let RowState::Divergent { versions } = &row(&servers, "srv").state else {
        panic!("not divergent: {:?}", row(&servers, "srv").state);
    };
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].harnesses, vec![Harness::Claude]);
    assert_eq!(
        versions[1].harnesses,
        vec![
            Harness::Codex,
            Harness::Grok,
            Harness::OpenCode,
            Harness::Antigravity
        ]
    );

    let errors = apply(&dir, &save("srv", stdio("main.ts")));
    assert!(errors.is_empty(), "{errors:?}");
    let after = load(&dir);
    assert_eq!(row(&after, "srv").state, RowState::Complete);
    assert_eq!(
        normalize(&row(&after, "srv").core),
        normalize(&stdio("main.ts"))
    );
}

#[test]
fn extras_and_disabled_do_not_make_a_divergence() {
    let dir = temp_dir("extras");
    seed_all(&dir, "srv", &stdio("server.ts"));
    let codex = read_raw(&dir, ".codex/config.toml");
    write_raw(
        &dir,
        ".codex/config.toml",
        &format!("{codex}startup_timeout_sec = 30\n"),
    );
    let opencode = read_raw(&dir, "opencode.json");
    let mut value: Value = serde_json::from_str(&opencode).unwrap();
    value["mcp"]["servers"]["srv"]["disabled"] = json!(true);
    write_raw(
        &dir,
        "opencode.json",
        &serde_json::to_string_pretty(&value).unwrap(),
    );

    let servers = load(&dir);
    let found = row(&servers, "srv");
    assert_eq!(found.state, RowState::Complete);
    assert!(
        found
            .notes
            .contains(&(Harness::OpenCode, "Désactivé".to_string()))
    );
    // Faits bruts, sans libellé : l'interface les met en phrase.
    assert!(
        found
            .notes
            .contains(&(Harness::Codex, "startup_timeout_sec".to_string())),
        "{:?}",
        found.notes
    );
    assert_eq!(found.notes.len(), 2, "{:?}", found.notes);
}

#[test]
fn an_unreadable_file_is_reported_and_never_written() {
    let dir = temp_dir("unreadable");
    seed(&dir, Harness::Claude, "a", &stdio("a.ts"));
    write_raw(
        &dir,
        ".codex/config.toml",
        "[mcp_servers.a\ncommand = \"bun\"\n",
    );
    let jsonc = "{\n  // commentaire\n  \"mcp\": {\"servers\": {\"b\": {\"type\": \"local\", \"command\": [\"bun\"]}}}\n}\n";
    write_raw(&dir, "opencode.json", jsonc);
    let codex_before = fingerprint(&dir, ".codex/config.toml");
    let opencode_before = fingerprint(&dir, "opencode.json");

    let servers = load(&dir);
    let broken: Vec<Harness> = servers
        .unreadable
        .iter()
        .map(|(harness, _)| *harness)
        .collect();
    assert_eq!(broken, vec![Harness::Codex, Harness::OpenCode]);
    // Un serveur absent d'un fichier illisible n'est pas « incomplet » à cause de lui.
    seed(&dir, Harness::Grok, "a", &stdio("a.ts"));
    seed(&dir, Harness::Antigravity, "a", &stdio("a.ts"));
    assert_eq!(row(&load(&dir), "a").state, RowState::Complete);

    let errors = apply(&dir, &save("a", stdio("changed.ts")));
    let failed: Vec<Harness> = errors.iter().map(|(harness, _)| *harness).collect();
    assert_eq!(failed, vec![Harness::Codex, Harness::OpenCode]);
    assert_eq!(fingerprint(&dir, ".codex/config.toml"), codex_before);
    assert_eq!(fingerprint(&dir, "opencode.json"), opencode_before);
    for harness in [Harness::Claude, Harness::Grok, Harness::Antigravity] {
        let after = read_servers(harness, &read_raw(&dir, harness.mcp_file())).unwrap();
        assert_eq!(
            normalize(&after["a"].core),
            normalize(&stdio("changed.ts")),
            "{harness:?}"
        );
    }
}

#[test]
fn apply_rereads_the_disk() {
    let dir = temp_dir("reread");
    seed(&dir, Harness::Claude, "a", &stdio("a.ts"));
    let _stale = load(&dir);
    // Un agent ajoute `z` à la main après l'affichage du volet.
    let mut value: Value = serde_json::from_str(&read_raw(&dir, ".mcp.json")).unwrap();
    value["mcpServers"]["z"] = json!({"type": "stdio", "command": "zsh"});
    write_raw(
        &dir,
        ".mcp.json",
        &serde_json::to_string_pretty(&value).unwrap(),
    );

    let errors = apply(&dir, &save("a", stdio("b.ts")));
    assert!(errors.is_empty(), "{errors:?}");
    let after = read_servers(Harness::Claude, &read_raw(&dir, ".mcp.json")).unwrap();
    assert!(after.contains_key("z"));
    assert_eq!(normalize(&after["a"].core), normalize(&stdio("b.ts")));
}

#[test]
fn remove_clears_all_five() {
    let dir = temp_dir("remove");
    seed_all(&dir, "gone", &stdio("gone.ts"));
    seed_all(&dir, "stay", &stdio("stay.ts"));

    let errors = apply(
        &dir,
        &McpOp::Remove {
            name: "gone".into(),
        },
    );
    assert!(errors.is_empty(), "{errors:?}");
    let after = load(&dir);
    assert!(after.rows.iter().all(|row| row.name != "gone"));
    assert_eq!(row(&after, "stay").state, RowState::Complete);
}

#[test]
fn a_divergent_row_refuses_propagate() {
    let dir = temp_dir("refuse");
    seed(&dir, Harness::Claude, "srv", &stdio("one.ts"));
    seed(&dir, Harness::Codex, "srv", &stdio("two.ts"));
    // Divergent ET absent de trois fichiers : la divergence l'emporte.
    assert!(matches!(
        row(&load(&dir), "srv").state,
        RowState::Divergent { .. }
    ));
    let claude_before = fingerprint(&dir, ".mcp.json");
    let codex_before = fingerprint(&dir, ".codex/config.toml");

    let errors = apply(&dir, &McpOp::Propagate { name: "srv".into() });
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(fingerprint(&dir, ".mcp.json"), claude_before);
    assert_eq!(fingerprint(&dir, ".codex/config.toml"), codex_before);
    assert!(!file_path(&dir, Harness::Grok).exists());
    assert!(!file_path(&dir, Harness::OpenCode).exists());
    assert!(!file_path(&dir, Harness::Antigravity).exists());
}

#[test]
fn files_without_change_are_not_rewritten() {
    let dir = temp_dir("untouched");
    seed(&dir, Harness::Claude, "hello", &stdio("hello.ts"));
    seed(&dir, Harness::Codex, "hello", &stdio("hello.ts"));
    // Un fichier à la mise en forme propre : réécrit par le traducteur, il serait reformaté.
    write_raw(
        &dir,
        ".mcp.json",
        "{\"mcpServers\":{\"hello\":{\"type\":\"stdio\",\"command\":\"bun\",\"args\":[\"hello.ts\"]}}}",
    );
    age_file(&dir, ".mcp.json");
    age_file(&dir, ".codex/config.toml");
    let claude_before = fingerprint(&dir, ".mcp.json");
    let codex_before = fingerprint(&dir, ".codex/config.toml");

    let errors = apply(
        &dir,
        &McpOp::Propagate {
            name: "hello".into(),
        },
    );
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(fingerprint(&dir, ".mcp.json"), claude_before);
    assert_eq!(fingerprint(&dir, ".codex/config.toml"), codex_before);
    assert!(file_path(&dir, Harness::Grok).is_file());

    // Un enregistrement identique à ce que le fichier porte déjà ne le réécrit pas non plus.
    let grok_before = fingerprint(&dir, ".grok/config.toml");
    let errors = apply(&dir, &save("hello", stdio("hello.ts")));
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(fingerprint(&dir, ".mcp.json"), claude_before);
    assert_eq!(fingerprint(&dir, ".grok/config.toml"), grok_before);
}

#[test]
fn opencode_jsonc_is_reported_not_shadowed() {
    let dir = temp_dir("jsonc");
    seed(&dir, Harness::Claude, "hello", &stdio("hello.ts"));
    write_raw(
        &dir,
        "opencode.jsonc",
        "{\n  // commentaire\n  \"mcp\": {}\n}\n",
    );

    let servers = load(&dir);
    assert_eq!(
        servers.unreadable,
        vec![(
            Harness::OpenCode,
            "opencode.jsonc : format non géré".to_string()
        )]
    );
    let errors = apply(
        &dir,
        &McpOp::Propagate {
            name: "hello".into(),
        },
    );
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, Harness::OpenCode);
    assert!(!dir.join("opencode.json").exists());
    assert_eq!(
        read_raw(&dir, "opencode.jsonc"),
        "{\n  // commentaire\n  \"mcp\": {}\n}\n"
    );
    assert!(file_path(&dir, Harness::Codex).is_file());
}

#[test]
fn toml_errors_do_not_echo_the_file() {
    let dir = temp_dir("secret");
    write_raw(
        &dir,
        ".codex/config.toml",
        "[mcp_servers.a]\ncommand = \"bun\"\nTOKEN = \"sk-secret\n",
    );
    seed(&dir, Harness::Claude, "a", &stdio("a.ts"));

    let servers = load(&dir);
    assert_eq!(servers.unreadable.len(), 1);
    assert!(
        !servers.unreadable[0].1.contains("sk-secret"),
        "{}",
        servers.unreadable[0].1
    );
    let errors = apply(&dir, &McpOp::Propagate { name: "a".into() });
    assert!(
        errors
            .iter()
            .all(|(_, message)| !message.contains("sk-secret")),
        "{errors:?}"
    );
}

#[test]
fn a_project_path_with_space_apostrophe_and_accent_works() {
    let base = temp_dir("odd-path");
    let dir = base.join("l'été x y");
    fs::create_dir_all(&dir).unwrap();
    seed(&dir, Harness::Claude, "hello", &stdio("hello.ts"));
    let errors = apply(
        &dir,
        &McpOp::Propagate {
            name: "hello".into(),
        },
    );
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(row(&load(&dir), "hello").state, RowState::Complete);
}

#[test]
fn a_missing_project_is_an_error_and_is_not_recreated() {
    let base = temp_dir("gone-project");
    let dir = base.join("vanished");
    let errors = apply(&dir, &save("a", stdio("a.ts")));
    assert_eq!(errors.len(), 1);
    assert!(!dir.exists());
}

#[test]
fn the_home_folder_is_never_written_as_a_project() {
    let home = temp_dir("home-as-project");
    seed(&home, Harness::Claude, "a", &stdio("a.ts"));
    let before = (
        read_raw(&home, ".mcp.json"),
        home.join(".codex").exists(),
        home.join(".grok").exists(),
    );
    // Le dossier personnel, atteint par un lien symbolique : la forme canonique le reconnaît.
    let link = temp_dir("home-link").join("link");
    std::os::unix::fs::symlink(&home, &link).unwrap();
    for project in [&home, &link] {
        for op in [
            save("b", stdio("b.ts")),
            McpOp::Propagate { name: "a".into() },
            McpOp::Remove { name: "a".into() },
        ] {
            let errors = apply_in(project, &op, Some(&home));
            assert_eq!(
                errors,
                vec![(Harness::Claude, HOME_IS_NOT_A_PROJECT.to_string())],
                "{op:?}"
            );
        }
    }
    assert_eq!(
        (
            read_raw(&home, ".mcp.json"),
            home.join(".codex").exists(),
            home.join(".grok").exists()
        ),
        before
    );
    // Un dossier sous le dossier personnel est un projet.
    let project = home.join("work");
    fs::create_dir_all(&project).unwrap();
    assert!(apply_in(&project, &save("b", stdio("b.ts")), Some(&home)).is_empty());
    assert!(project.join(".codex/config.toml").exists());
    // Sans dossier personnel connu, rien n'est refusé.
    assert!(apply_in(&project, &save("c", stdio("c.ts")), None).is_empty());
}

#[test]
fn an_unsupported_server_is_never_copied_elsewhere() {
    let dir = temp_dir("sse");
    write_raw(
        &dir,
        ".mcp.json",
        "{\"mcpServers\":{\"live\":{\"type\":\"sse\",\"url\":\"https://x.test/sse\"}}}\n",
    );
    let servers = load(&dir);
    assert!(!row(&servers, "live").supported);
    let before = fingerprint(&dir, ".mcp.json");

    let propagate = apply(
        &dir,
        &McpOp::Propagate {
            name: "live".into(),
        },
    );
    assert_eq!(propagate.len(), 1);
    assert_eq!(fingerprint(&dir, ".mcp.json"), before);
    // Enregistrer ne le recopie pas non plus : il ne va que dans le fichier qui le porte.
    let saved = apply(
        &dir,
        &save("live", json!({"type": "http", "url": "https://x.test/mcp"})),
    );
    assert!(saved.is_empty(), "{saved:?}");
    for harness in [
        Harness::Codex,
        Harness::Grok,
        Harness::OpenCode,
        Harness::Antigravity,
    ] {
        assert!(
            !file_path(&dir, harness).exists(),
            "{harness:?} was created"
        );
    }
}

#[test]
fn a_symlinked_config_stays_a_symlink_and_keeps_its_mode() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = temp_dir("symlink");
    let real = dir.join("real.json");
    fs::write(&real, "{\"mcpServers\":{}}\n").unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&real, dir.join(".mcp.json")).unwrap();

    let errors = apply(&dir, &save("a", stdio("a.ts")));
    assert!(errors.is_empty(), "{errors:?}");
    assert!(
        fs::symlink_metadata(dir.join(".mcp.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fs::read_to_string(&real).unwrap().contains("a.ts"));
    for harness in [
        Harness::Codex,
        Harness::Grok,
        Harness::OpenCode,
        Harness::Antigravity,
    ] {
        assert!(
            file_path(&dir, harness).is_file(),
            "{harness:?} not created"
        );
    }
    assert_eq!(
        fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn save_creates_missing_files() {
    let dir = temp_dir("save-creates");
    seed(&dir, Harness::Claude, "hello-demo", &stdio("hello.ts"));
    assert!(matches!(
        row(&load(&dir), "hello-demo").state,
        RowState::Incomplete { .. }
    ));

    let errors = apply(&dir, &save("hello-demo", stdio("hello.ts")));
    assert!(errors.is_empty(), "{errors:?}");
    for harness in Harness::ALL {
        assert!(
            file_path(&dir, harness).is_file(),
            "{harness:?} not created"
        );
    }
    assert_eq!(row(&load(&dir), "hello-demo").state, RowState::Complete);
}

#[test]
fn keep_this_one_creates_the_files_the_divergent_row_lacks() {
    let dir = temp_dir("keep-creates");
    seed(&dir, Harness::Claude, "srv", &stdio("one.ts"));
    seed(&dir, Harness::Codex, "srv", &stdio("two.ts"));

    let errors = apply(&dir, &save("srv", stdio("two.ts")));
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(row(&load(&dir), "srv").state, RowState::Complete);
}

#[test]
fn a_new_file_is_not_created_for_an_unsupported_server() {
    let dir = temp_dir("sse-missing");
    write_raw(
        &dir,
        ".mcp.json",
        "{\"mcpServers\":{\"live\":{\"type\":\"sse\",\"url\":\"https://x.test/sse\"}}}\n",
    );
    let errors = apply(
        &dir,
        &save("live", json!({"type": "http", "url": "https://x.test/mcp"})),
    );
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!file_path(&dir, Harness::Codex).exists());
}

#[test]
fn a_vanished_project_is_not_recreated_by_the_parent_dir_guard() {
    // `write_atomic` ne crée un dossier que pour `.codex/`, `.grok/`, `.agents/` : jamais le projet.
    let base = temp_dir("vanish");
    let project = base.join("gone");
    let result = write_atomic(&project.join(".mcp.json"), "{}", false);
    assert!(result.is_err());
    assert!(!project.exists());
    let nested = write_atomic(&project.join(".codex/config.toml"), "", true);
    assert!(nested.is_err());
    assert!(!project.exists());
}

#[test]
fn a_new_file_is_never_world_writable_and_an_existing_mode_is_kept() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("modes");
    write_atomic(&dir.join("fresh.json"), "{}", false).unwrap();
    assert_eq!(
        fs::metadata(dir.join("fresh.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o022,
        0
    );
    let secret = dir.join("secret.json");
    fs::write(&secret, "{}").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    write_atomic(&secret, "{\"a\":1}", false).unwrap();
    assert_eq!(
        fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn a_broken_symlink_is_reported_and_kept() {
    use std::os::unix::fs::symlink;
    let dir = temp_dir("broken-link");
    let link = dir.join(".mcp.json");
    symlink(dir.join("missing-target.json"), &link).unwrap();

    let servers = load(&dir);
    assert_eq!(servers.unreadable.len(), 1);
    assert_eq!(servers.unreadable[0].0, Harness::Claude);
    let errors = apply(&dir, &save("a", stdio("a.ts")));
    assert!(
        errors
            .iter()
            .any(|(harness, _)| *harness == Harness::Claude)
    );
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!dir.join("missing-target.json").exists());
    // Les autres fichiers, eux, sont écrits.
    assert!(file_path(&dir, Harness::Codex).is_file());
}

// Porte de la Tâche 11 : les cinq vrais fichiers écrits par `apply`, dans le projet jetable de `IDEMA_HOST_PROJECT`
// (voir `host_gate`). `#[ignore]` : la porte lance les vrais harnais dessus ensuite.

/// Le serveur de test de la porte, sous le dossier personnel : un outil `say_hello`, nom MCP `hello-demo`.
fn host_paths() -> (String, String) {
    let home = std::env::var("HOME").expect("HOME must be set");
    (
        format!("{home}/.bun/bin/bun"),
        format!("{home}/dev/tmp/skills-sh-tests2/hello-mcp/server.ts"),
    )
}

#[test]
#[ignore = "host gate: writes the five MCP files of IDEMA_HOST_PROJECT"]
fn host_writes_the_five_files() {
    let project = crate::project_panel::host_gate::host_project();
    let (host_bun, host_server) = host_paths();
    assert!(
        Path::new(&host_server).is_file(),
        "the test MCP server is missing"
    );
    let core = json!({"type": "stdio", "command": host_bun, "args": [host_server]});
    let errors = apply(
        &project,
        &McpOp::Save {
            name: "hello-demo".to_string(),
            core: core.clone(),
        },
    );
    assert!(errors.is_empty(), "{errors:?}");

    for harness in Harness::ALL {
        let path = file_path(&project, harness);
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} unreadable: {error}", harness.mcp_file()));
        let servers = read_servers(harness, &text).unwrap();
        let server = servers
            .get("hello-demo")
            .unwrap_or_else(|| panic!("{} lacks hello-demo", harness.mcp_file()));
        assert_eq!(normalize(&server.core), normalize(&core));
        assert!(!server.disabled, "{} is disabled", harness.mcp_file());
    }
    let loaded = load(&project);
    assert!(loaded.unreadable.is_empty(), "{:?}", loaded.unreadable);
    let row = loaded
        .rows
        .iter()
        .find(|row| row.name == "hello-demo")
        .expect("the row exists");
    assert_eq!(row.state, RowState::Complete);
}

#[test]
fn a_config_folder_linked_to_a_global_config_is_never_written() {
    use std::os::unix::fs::symlink;
    let home = temp_dir("global-home");
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(home.join(".codex/config.toml"), "model = \"x\"\n").unwrap();
    let project = home.join("work/app");
    fs::create_dir_all(&project).unwrap();
    // Le `.codex` du projet est le `~/.codex` de la machine.
    symlink(home.join(".codex"), project.join(".codex")).unwrap();

    let errors = apply_in(&project, &save("srv", stdio("a.ts")), Some(&home));

    assert_eq!(
        errors,
        vec![(
            Harness::Codex,
            ".codex/config.toml pointe vers une configuration globale : rien n'a été écrit."
                .to_string()
        )]
    );
    assert_eq!(
        fs::read_to_string(home.join(".codex/config.toml")).unwrap(),
        "model = \"x\"\n"
    );
    // Les autres fichiers du projet, eux, sont écrits.
    assert!(project.join(".mcp.json").exists());
    assert!(project.join(".grok/config.toml").exists());
}

#[test]
fn a_file_symlink_to_dotfiles_is_still_written() {
    use std::os::unix::fs::symlink;
    let home = temp_dir("dotfiles-home");
    fs::create_dir_all(home.join("dotfiles")).unwrap();
    fs::write(home.join("dotfiles/mcp.json"), "{}\n").unwrap();
    let project = home.join("work/app");
    fs::create_dir_all(&project).unwrap();
    symlink(home.join("dotfiles/mcp.json"), project.join(".mcp.json")).unwrap();

    let errors = apply_in(&project, &save("srv", stdio("a.ts")), Some(&home));

    assert!(errors.is_empty(), "{errors:?}");
    assert!(
        fs::symlink_metadata(project.join(".mcp.json"))
            .unwrap()
            .is_symlink()
    );
    assert!(
        fs::read_to_string(home.join("dotfiles/mcp.json"))
            .unwrap()
            .contains("srv")
    );
}

#[test]
fn an_os_failure_reads_in_french_in_the_banners() {
    use std::os::unix::fs::PermissionsExt;
    // Lecture : un `.mcp.json` qui est un dossier.
    let dir = temp_dir("french-read");
    fs::create_dir_all(dir.join(".mcp.json")).unwrap();
    let servers = load(&dir);
    let reason = servers
        .unreadable
        .iter()
        .find_map(|(harness, reason)| (*harness == Harness::Claude).then_some(reason.clone()))
        .expect("the directory is reported as unreadable");
    assert_eq!(reason, ".mcp.json : lecture impossible (c'est un dossier)");

    // Écriture : un projet fermé en écriture refuse le fichier temporaire.
    let locked = temp_dir("french-write");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    let errors = apply(&locked, &save("a", stdio("a.ts")));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let claude = errors
        .iter()
        .find_map(|(harness, reason)| (*harness == Harness::Claude).then_some(reason.clone()))
        .expect("the write is refused");
    assert_eq!(claude, ".mcp.json : écriture impossible (accès refusé)");
}
