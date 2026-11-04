use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

const LIST_SAMPLE: &str = include_str!("../testdata/skills-list.sample.json");
const SEARCH_SAMPLE: &str = include_str!("../testdata/search-motion-5.json");

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Un dossier jetable sous `std::env::temp_dir()`, unique par appel.
fn temp_project(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "idema-skills-{label}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(project: &Path, relative: &str, text: &str) {
    let path = project.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn skill_md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n")
}

/// Une fausse Corbeille : déplace `path` dans `bin` sous un nom unique et rend la nouvelle adresse.
fn move_into(bin: &Path, path: &Path) -> io::Result<PathBuf> {
    let target = bin.join(format!("{}", COUNTER.fetch_add(1, Ordering::SeqCst)));
    fs::rename(path, &target)?;
    Ok(target)
}

fn find<'a>(skills: &'a [InstalledSkill], dir: &str) -> &'a InstalledSkill {
    skills
        .iter()
        .find(|skill| skill.dir == dir)
        .unwrap_or_else(|| panic!("{dir} missing"))
}

#[test]
fn format_installs_rounds_to_one_decimal() {
    assert_eq!(format_installs(0), "0");
    assert_eq!(format_installs(14), "14");
    assert_eq!(format_installs(999), "999");
    assert_eq!(format_installs(1_000), "1K");
    assert_eq!(format_installs(267_049), "267K");
    assert_eq!(format_installs(916_912), "916,9K");
    assert_eq!(format_installs(559_791), "559,8K");
    assert_eq!(format_installs(999_960), "1M");
    assert_eq!(format_installs(1_234_567), "1,2M");
}

#[test]
fn coverage_text_lists_the_missing_harnesses() {
    assert_eq!(
        coverage_text(&[Harness::Codex, Harness::Antigravity]),
        "Absente pour Codex et Antigravity"
    );
    assert_eq!(
        coverage_text(&[Harness::Claude, Harness::Codex, Harness::Antigravity]),
        "Absente pour Claude, Codex et Antigravity"
    );
    assert_eq!(coverage_text(&[Harness::Grok]), "Absente pour Grok");
}

#[test]
fn search_url_encodes_the_query() {
    assert_eq!(
        search_url("https://skills.sh", "motion graphics"),
        "https://skills.sh/api/search?q=motion+graphics&limit=20"
    );
    assert_eq!(
        search_url("https://skills.sh/", "a&b=c"),
        "https://skills.sh/api/search?q=a%26b%3Dc&limit=20"
    );
}

#[test]
fn add_and_remove_args_match_the_cli() {
    assert_eq!(
        add_args("emilkowalski/skills", Some("review-animations")).unwrap(),
        [
            "skills",
            "add",
            "emilkowalski/skills",
            "--skill",
            "review-animations",
            "--agent",
            "claude-code",
            "grok",
            "codex",
            "--copy",
            "-y",
            "--json"
        ]
    );
    assert_eq!(
        add_args("emilkowalski/skills", None).unwrap(),
        [
            "skills",
            "add",
            "emilkowalski/skills",
            "--agent",
            "claude-code",
            "grok",
            "codex",
            "--copy",
            "-y",
            "--json"
        ]
    );
    assert_eq!(
        remove_args("review-animations").unwrap(),
        ["skills", "remove", "review-animations", "-y"]
    );
    assert_eq!(list_args(), ["skills", "list", "--json"]);
}

#[test]
fn cli_values_cannot_start_with_a_dash() {
    assert!(add_args("-x", None).is_err());
    assert!(add_args("o/r", Some("--all")).is_err());
    assert!(remove_args("--all").is_err());
}

#[test]
fn visibility_follows_the_harness_table() {
    let seen = |present: &[&str]| -> Vec<(Harness, bool)> { visible_to(present) };
    assert_eq!(
        seen(&[".claude/skills"]),
        vec![
            (Harness::Claude, true),
            (Harness::Codex, false),
            (Harness::Grok, true),
            (Harness::OpenCode, true),
            (Harness::Antigravity, false),
        ]
    );
    assert!(
        seen(&[".claude/skills", ".agents/skills"])
            .iter()
            .all(|(_, visible)| *visible)
    );
    assert_eq!(
        seen(&[".grok/skills"]),
        vec![
            (Harness::Claude, false),
            (Harness::Codex, false),
            (Harness::Grok, true),
            (Harness::OpenCode, false),
            (Harness::Antigravity, false),
        ]
    );
    assert!(seen(&[]).iter().all(|(_, visible)| !*visible));
    // Le dossier commun est lu par tous sauf Claude.
    let agents_only = seen(&[".agents/skills"]);
    for (harness, visible) in &agents_only {
        assert_eq!(*visible, *harness != Harness::Claude, "{harness:?}");
    }
}

#[test]
fn parse_search_sorts_by_installs() {
    let hits = parse_search(SEARCH_SAMPLE).unwrap();
    let installs: Vec<u64> = hits.iter().map(|hit| hit.installs).collect();
    assert_eq!(installs, [1_005_122, 916_856, 559_791, 514_070, 14]);
    let remotion = &hits[2];
    assert_eq!(remotion.skill_id, "remotion-to-hyperframes");
    assert_eq!(remotion.source, "heygen-com/hyperframes");
    assert_eq!(remotion.name, "remotion-to-hyperframes");
    assert!(parse_search(r#"{"query":"x"}"#).is_err());
    assert!(parse_search("not json").is_err());
}

#[test]
fn parse_list_keeps_project_skills_by_folder() {
    let listed = parse_list(LIST_SAMPLE).unwrap();
    let dirs: Vec<&str> = listed.iter().map(|skill| skill.dir.as_str()).collect();
    assert_eq!(dirs, ["hello-demo", "review-animations", "manual-one"]);
    assert_eq!(listed[1].source.as_deref(), Some("emilkowalski/skills"));
    assert_eq!(listed[1].kind.as_deref(), Some("github"));
    assert_eq!(listed[2].source, None);

    let global =
        r#"[{"name":"g","path":"/home/x/.claude/skills/g","scope":"global","source":null}]"#;
    assert!(parse_list(global).unwrap().is_empty());
    let escaping = r#"[{"name":"x","path":"/p/.agents/skills/..","scope":"project"}]"#;
    assert!(parse_list(escaping).unwrap().is_empty());
    assert!(parse_list("{}").is_err());
    assert!(parse_list("nope").is_err());
}

#[test]
fn scan_reads_sources_descriptions_and_copies() {
    let project = temp_project("scan");
    let listed = parse_list(LIST_SAMPLE).unwrap();
    write(
        &project,
        ".agents/skills/hello-demo/SKILL.md",
        &skill_md("hello-demo", "Says hello"),
    );
    write(
        &project,
        ".agents/skills/review-animations/SKILL.md",
        &skill_md("review-animations", "Reviews motion"),
    );
    write(
        &project,
        ".claude/skills/manual-one/SKILL.md",
        &skill_md("manual-one", "Written by hand"),
    );
    // Une skill de GitLab, absente de l'échantillon.
    write(
        &project,
        ".agents/skills/from-gitlab/SKILL.md",
        &skill_md("from-gitlab", "Remote"),
    );
    let mut listed = listed;
    listed.extend(
        parse_list(
            r#"[{"name":"from-gitlab","path":"/p/.agents/skills/from-gitlab","scope":"project","source":"g/r","sourceType":"gitlab"}]"#,
        )
        .unwrap(),
    );

    let skills = scan(&project, &listed);
    let dirs: Vec<&str> = skills.iter().map(|skill| skill.dir.as_str()).collect();
    assert_eq!(
        dirs,
        [
            "from-gitlab",
            "hello-demo",
            "manual-one",
            "review-animations"
        ]
    );

    assert_eq!(
        find(&skills, "review-animations").source,
        SkillSource::Locked {
            source: "emilkowalski/skills".into(),
            kind: "github".into()
        }
    );
    assert!(matches!(
        &find(&skills, "hello-demo").source,
        SkillSource::Locked { kind, .. } if kind == "local"
    ));
    assert!(matches!(
        &find(&skills, "from-gitlab").source,
        SkillSource::Locked { kind, .. } if kind == "gitlab"
    ));
    let manual = find(&skills, "manual-one");
    assert_eq!(manual.source, SkillSource::Handwritten);
    assert_eq!(manual.description, "Written by hand");
    assert_eq!(manual.present, [".claude/skills"]);
    assert_eq!(manual.versions, vec![vec![".claude/skills"]]);

    // Une copie retouchée dans .claude/skills : deux versions.
    write(
        &project,
        ".agents/skills/hello-demo/SKILL.md",
        &skill_md("hello-demo", "Says hello"),
    );
    write(
        &project,
        ".claude/skills/hello-demo/SKILL.md",
        &skill_md("hello-demo", "Says hello"),
    );
    write(&project, ".claude/skills/hello-demo/notes.txt", "tweaked");
    let skills = scan(&project, &listed);
    let hello = find(&skills, "hello-demo");
    assert_eq!(hello.present, [".claude/skills", ".agents/skills"]);
    assert_eq!(hello.versions.len(), 2);

    // Un .DS_Store dans une seule copie ne crée pas de divergence.
    let project = temp_project("scan-dsstore");
    write(
        &project,
        ".claude/skills/foo/SKILL.md",
        &skill_md("foo", "d"),
    );
    write(
        &project,
        ".agents/skills/foo/SKILL.md",
        &skill_md("foo", "d"),
    );
    write(&project, ".agents/skills/foo/.DS_Store", "junk");
    let skills = scan(&project, &[]);
    assert_eq!(
        find(&skills, "foo").versions,
        vec![vec![".claude/skills", ".agents/skills"]]
    );
}

#[test]
fn a_skill_is_its_folder() {
    let project = temp_project("folder");
    write(
        &project,
        ".claude/skills/foo/SKILL.md",
        &skill_md("bar", "Named otherwise"),
    );
    let skills = scan(&project, &[]);
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].dir, "foo");
    assert_eq!(skills[0].name, "bar");
    assert_eq!(skills[0].description, "Named otherwise");

    // Un dossier sans SKILL.md n'est pas une skill.
    fs::create_dir_all(project.join(".claude/skills/empty")).unwrap();
    assert_eq!(scan(&project, &[]).len(), 1);

    let mut calls = 0;
    let mut trash = |_: &Path| -> io::Result<PathBuf> {
        calls += 1;
        Ok(PathBuf::new())
    };
    for bad in ["../x", "", "a/b", "..", ".", "/abs"] {
        assert!(trash_copies(&project, bad, &mut trash).is_err(), "{bad:?}");
        assert!(propagate(&project, bad).is_err(), "{bad:?}");
        assert!(
            keep_version(&project, bad, ".claude/skills", &mut trash).is_err(),
            "{bad:?}"
        );
    }
    assert_eq!(calls, 0);
}

#[test]
fn propagate_and_keep_version_copy_folders() {
    let project = temp_project("copy");
    write(
        &project,
        ".claude/skills/foo/SKILL.md",
        &skill_md("foo", "d"),
    );
    write(&project, ".claude/skills/foo/refs/extra.md", "extra");

    propagate(&project, "foo").unwrap();
    for root in SKILL_DIRS {
        assert_eq!(
            fs::read_to_string(project.join(root).join("foo/SKILL.md")).unwrap(),
            skill_md("foo", "d"),
            "{root}"
        );
        assert_eq!(
            fs::read_to_string(project.join(root).join("foo/refs/extra.md")).unwrap(),
            "extra",
            "{root}"
        );
    }
    let skills = scan(&project, &[]);
    assert_eq!(find(&skills, "foo").versions.len(), 1);

    // Propager n'écrase pas une copie existante.
    write(&project, ".agents/skills/foo/SKILL.md", "mine");
    propagate(&project, "foo").unwrap();
    assert_eq!(
        fs::read_to_string(project.join(".agents/skills/foo/SKILL.md")).unwrap(),
        "mine"
    );
    assert_eq!(find(&scan(&project, &[]), "foo").versions.len(), 2);

    // Garder la version de .claude remplace les autres, fichiers périmés compris ; l'ancienne copie part à la Corbeille.
    write(&project, ".agents/skills/foo/stale.txt", "stale");
    let bin = temp_project("copy-bin");
    let mut trashed = Vec::new();
    let mut trash = |path: &Path| -> io::Result<PathBuf> {
        let target = move_into(&bin, path)?;
        trashed.push(target.clone());
        Ok(target)
    };
    keep_version(&project, "foo", ".claude/skills", &mut trash).unwrap();
    // La seule copie qui différait, celle de .agents, est dans la fausse Corbeille avec son contenu d'origine.
    assert_eq!(trashed.len(), 1);
    assert_eq!(
        fs::read_to_string(trashed[0].join("SKILL.md")).unwrap(),
        "mine"
    );
    assert_eq!(
        fs::read_to_string(trashed[0].join("stale.txt")).unwrap(),
        "stale"
    );
    for root in SKILL_DIRS {
        assert_eq!(
            fs::read_to_string(project.join(root).join("foo/SKILL.md")).unwrap(),
            skill_md("foo", "d"),
            "{root}"
        );
    }
    assert!(!project.join(".agents/skills/foo/stale.txt").exists());
    assert_eq!(find(&scan(&project, &[]), "foo").versions.len(), 1);

    // Une racine inconnue est refusée, une copie absente aussi.
    let mut refuse = |_: &Path| -> io::Result<PathBuf> { panic!("nothing to trash") };
    assert!(keep_version(&project, "foo", "elsewhere", &mut refuse).is_err());
    assert!(keep_version(&project, "nope", ".claude/skills", &mut refuse).is_err());
}

#[test]
fn keep_version_never_deletes_a_copy_the_trash_refused() {
    let project = temp_project("keep-refused");
    write(&project, ".claude/skills/foo/SKILL.md", "kept");
    write(
        &project,
        ".agents/skills/foo/SKILL.md",
        "precious local edits",
    );
    let mut failing =
        |_: &Path| -> io::Result<PathBuf> { Err(io::Error::other("trash unavailable")) };
    assert!(keep_version(&project, "foo", ".claude/skills", &mut failing).is_err());
    assert_eq!(
        fs::read_to_string(project.join(".agents/skills/foo/SKILL.md")).unwrap(),
        "precious local edits"
    );
    // Le dossier de préparation ne reste pas derrière.
    assert!(!project.join(".agents/skills/.foo.idema-tmp").exists());
}

#[cfg(unix)]
#[test]
fn keep_version_skips_a_copy_that_resolves_to_the_source() {
    let project = temp_project("keep-link");
    write(&project, ".claude/skills/foo/SKILL.md", "source");
    fs::create_dir_all(project.join(".agents/skills")).unwrap();
    std::os::unix::fs::symlink(
        project.join(".claude/skills/foo"),
        project.join(".agents/skills/foo"),
    )
    .unwrap();
    write(&project, ".grok/skills/foo/SKILL.md", "other");

    let bin = temp_project("keep-link-bin");
    let mut trashed = Vec::new();
    let mut trash = |path: &Path| -> io::Result<PathBuf> {
        trashed.push(path.to_path_buf());
        move_into(&bin, path)
    };
    keep_version(&project, "foo", ".claude/skills", &mut trash).unwrap();
    // Seule la copie de .grok est remplacée ; le lien n'est ni mis à la Corbeille ni cassé.
    assert_eq!(trashed, [project.join(".grok/skills/foo")]);
    assert_eq!(
        fs::read_to_string(project.join(".claude/skills/foo/SKILL.md")).unwrap(),
        "source"
    );
    assert_eq!(
        fs::read_to_string(project.join(".agents/skills/foo/SKILL.md")).unwrap(),
        "source"
    );
    assert_eq!(
        fs::read_to_string(project.join(".grok/skills/foo/SKILL.md")).unwrap(),
        "source"
    );
}

#[test]
fn a_listed_skill_with_no_copy_in_the_three_folders_is_left_out() {
    let project = temp_project("no-copy");
    write(
        &project,
        ".cursor/skills/elsewhere/SKILL.md",
        &skill_md("elsewhere", "d"),
    );
    let listed = parse_list(
        r#"[{"name":"elsewhere","path":"/p/.cursor/skills/elsewhere","scope":"project","source":"o/r","sourceType":"github"}]"#,
    )
    .unwrap();
    assert_eq!(listed.len(), 1);
    assert!(scan(&project, &listed).is_empty());
}

#[test]
fn trash_copies_calls_the_injected_function_once_per_copy() {
    let project = temp_project("trash");
    let bin = temp_project("trash-bin");
    write(&project, ".claude/skills/foo/SKILL.md", "a");
    write(&project, ".agents/skills/foo/SKILL.md", "a");
    write(&project, ".grok/skills/other/SKILL.md", "o");

    let mut seen = Vec::new();
    let mut trash = |path: &Path| -> io::Result<PathBuf> {
        seen.push(path.to_path_buf());
        let target = bin.join(format!("{}", seen.len()));
        fs::rename(path, &target)?;
        Ok(target)
    };
    trash_copies(&project, "foo", &mut trash).unwrap();
    assert_eq!(
        seen,
        [
            project.join(".claude/skills/foo"),
            project.join(".agents/skills/foo")
        ]
    );
    assert!(!project.join(".claude/skills/foo").exists());
    assert!(project.join(".grok/skills/other/SKILL.md").exists());
}

#[test]
fn a_path_query_starts_like_a_path() {
    for yes in ["/x", "~/x", "./x", "../x"] {
        assert!(path_query(yes), "{yes}");
    }
    for no in ["motion", "x/y", "", "~x"] {
        assert!(!path_query(no), "{no}");
    }
    let project = Path::new("/work/proj");
    let home = Path::new("/home/me");
    assert_eq!(resolve_path("~/x", project, home), Path::new("/home/me/x"));
    assert_eq!(
        resolve_path("./x", project, home),
        Path::new("/work/proj/x")
    );
    assert_eq!(
        resolve_path("../x", project, home),
        Path::new("/work/proj/../x")
    );
    assert_eq!(resolve_path("/x", project, home), Path::new("/x"));
}

#[test]
fn only_the_latest_search_is_accepted() {
    let mut queue = SearchQueue::default();
    let a = queue.next();
    let b = queue.next();
    assert!(!queue.accept(a));
    assert!(queue.accept(b));
}

#[test]
fn target_dir_for_ports_the_cli_sanitize_name() {
    let cases = [
        ("review-animations", "review-animations"),
        ("Review Animations", "review-animations"),
        ("a   b", "a-b"),
        ("My_Skill.v2", "my_skill.v2"),
        ("a/b", "a-b"),
        ("../etc", "etc"),
        ("  --Foo--  ", "foo"),
        ("...x...", "x"),
        ("Été", "t"),
        ("x\u{1F600}y", "x-y"),
        // Le signe Kelvin se met en minuscule en `k`, comme en JavaScript.
        ("\u{212A}it", "kit"),
        ("", "unnamed-skill"),
        ("***", "unnamed-skill"),
        ("日本語", "unnamed-skill"),
    ];
    for (name, expected) in cases {
        assert_eq!(target_dir_for(name), expected, "{name:?}");
    }
    assert_eq!(target_dir_for(&"a".repeat(300)).len(), 255);
}

fn installed_skill(dir: &str, source: SkillSource) -> InstalledSkill {
    InstalledSkill {
        dir: dir.to_string(),
        name: dir.to_string(),
        source,
        description: String::new(),
        present: vec![".claude/skills"],
        versions: vec![vec![".claude/skills"]],
    }
}

fn hit(name: &str, skill_id: &str) -> SearchHit {
    SearchHit {
        name: name.to_string(),
        source: "owner/repo".to_string(),
        skill_id: skill_id.to_string(),
        installs: 1,
    }
}

fn dirs_of(found: &[DiskCollision]) -> Vec<&str> {
    found
        .iter()
        .map(|collision| collision.dir.as_str())
        .collect()
}

#[test]
fn a_hit_collides_with_a_handwritten_skill_of_its_target_folder() {
    let project = temp_project("collide-hit");
    write(&project, ".claude/skills/review-animations/SKILL.md", "a");
    write(&project, ".claude/skills/other/SKILL.md", "b");
    let known = scan(&project, &[]);
    let targets = hit_target_dirs(&hit("Review Animations", "review-animations"));
    // Le nom assaini et l'identifiant donnent le même dossier : une seule cible.
    assert_eq!(targets, ["review-animations"]);
    let found = disk_collisions(&project, &targets, &known);
    assert_eq!(dirs_of(&found), ["review-animations"]);
    assert_eq!(
        found[0].origin,
        CollisionOrigin::Skill(SkillSource::Handwritten)
    );

    // Un dossier différent ne collisionne pas.
    assert!(
        disk_collisions(&project, &hit_target_dirs(&hit("Motion", "motion")), &known).is_empty()
    );

    // Les deux cibles comptent quand le nom assaini et l'identifiant diffèrent.
    let both = hit_target_dirs(&hit("Fancy Name", "fancy-id"));
    assert_eq!(both, ["fancy-name", "fancy-id"]);
    write(&project, ".grok/skills/fancy-id/SKILL.md", "c");
    assert_eq!(
        dirs_of(&disk_collisions(&project, &both, &known)),
        ["fancy-id"]
    );
}

#[test]
fn a_collision_is_found_whatever_the_case_of_the_folder() {
    // Le disque de macOS confond les casses : `Review-Animations` est écrasé par `review-animations`.
    let project = temp_project("collide-case");
    write(&project, ".claude/skills/Review-Animations/SKILL.md", "a");
    let found = disk_collisions(
        &project,
        &hit_target_dirs(&hit("review-animations", "review-animations")),
        &[],
    );
    assert_eq!(dirs_of(&found), ["Review-Animations"]);
}

#[test]
fn an_entry_created_after_the_listing_is_detected() {
    let project = temp_project("collide-late");
    write(&project, ".claude/skills/older/SKILL.md", "a");
    // La liste du projet date d'avant `fresh` : elle ne la connaît pas, le disque si.
    let stale_listing = scan(&project, &[]);
    assert!(stale_listing.iter().all(|skill| skill.dir != "fresh"));
    write(&project, ".agents/skills/fresh/SKILL.md", "b");

    let targets = ["fresh".to_string()];
    let found = disk_collisions(&project, &targets, &stale_listing);
    assert_eq!(dirs_of(&found), ["fresh"]);
    assert_eq!(
        found[0].origin,
        CollisionOrigin::Skill(SkillSource::Handwritten)
    );
    // Et une entrée absente de la liste et du disque n'est rien.
    assert!(disk_collisions(&project, &["absent".to_string()], &stale_listing).is_empty());
}

#[test]
fn a_folder_without_a_skill_file_is_detected() {
    let project = temp_project("collide-bare");
    write(&project, ".grok/skills/notes/readme.txt", "no skill here");
    // Le CLI vide aussi ce dossier : il est détecté, et dit ce qu'il est.
    let found = disk_collisions(&project, &["notes".to_string()], &[]);
    assert_eq!(dirs_of(&found), ["notes"]);
    assert_eq!(found[0].origin, CollisionOrigin::Folder);
    // Une entrée qui est un fichier, ou un lien cassé, serait vidée tout autant.
    write(&project, ".claude/skills/loose", "a file");
    fs::create_dir_all(project.join(".agents/skills")).unwrap();
    std::os::unix::fs::symlink(
        project.join("nowhere"),
        project.join(".agents/skills/ghost"),
    )
    .unwrap();
    assert_eq!(
        dirs_of(&disk_collisions(&project, &["loose".to_string()], &[])),
        ["loose"]
    );
    assert_eq!(
        dirs_of(&disk_collisions(&project, &["ghost".to_string()], &[])),
        ["ghost"]
    );
    // Un autre nom ne collisionne pas.
    assert!(disk_collisions(&project, &["other".to_string()], &[]).is_empty());
}

#[test]
fn a_known_skill_keeps_the_origin_the_listing_gave() {
    let project = temp_project("collide-known");
    write(&project, ".claude/skills/motion/SKILL.md", "a");
    let known = [installed_skill(
        "motion",
        SkillSource::Locked {
            source: "someone/else".to_string(),
            kind: "github".to_string(),
        },
    )];
    let found = disk_collisions(&project, &hit_target_dirs(&hit("motion", "motion")), &known);
    assert_eq!(
        found[0].origin,
        CollisionOrigin::Skill(SkillSource::Locked {
            source: "someone/else".to_string(),
            kind: "github".to_string(),
        })
    );
    // Le dossier sur trois chemins ne fait qu'une entrée.
    write(&project, ".grok/skills/motion/SKILL.md", "a");
    write(&project, ".agents/skills/motion/SKILL.md", "a");
    assert_eq!(
        disk_collisions(&project, &["motion".to_string()], &known).len(),
        1
    );
}

#[test]
fn a_folder_collides_for_each_skill_it_would_overwrite() {
    let source = temp_project("collide-source");
    write(&source, "one/SKILL.md", &skill_md("Review Animations", "d"));
    write(&source, "nested/two/SKILL.md", &skill_md("fresh-one", "d"));
    // Sans `description`, le CLI ignore la skill : elle n'écrase rien.
    write(&source, "three/SKILL.md", "---\nname: ignored\n---\n");
    // Un dossier que le CLI ne parcourt pas.
    write(
        &source,
        "node_modules/x/SKILL.md",
        &skill_md("vendored", "d"),
    );

    let mut targets = folder_target_dirs(&source);
    targets.sort();
    assert_eq!(targets, ["fresh-one", "review-animations"]);

    let project = temp_project("collide-source-project");
    write(&project, ".claude/skills/review-animations/SKILL.md", "a");
    write(&project, ".claude/skills/unrelated/SKILL.md", "b");
    write(&project, ".claude/skills/ignored/SKILL.md", "c");
    let found = disk_collisions(&project, &targets, &[]);
    assert_eq!(dirs_of(&found), ["review-animations"]);
}

#[test]
fn a_folder_that_is_itself_a_skill_is_one_skill() {
    let source = temp_project("collide-root");
    write(&source, "SKILL.md", &skill_md("Root Skill", "d"));
    write(&source, "inner/SKILL.md", &skill_md("inner", "d"));
    assert_eq!(folder_target_dirs(&source), ["root-skill"]);
    assert!(folder_target_dirs(&source.join("missing")).is_empty());
}

#[test]
fn divergent_copies_are_trashed_as_copies_before_a_cli_removal() {
    let project = temp_project("divergent-trash");
    let bin = temp_project("divergent-trash-bin");
    write(&project, ".claude/skills/foo/SKILL.md", "original");
    write(&project, ".grok/skills/foo/SKILL.md", "original");
    write(&project, ".agents/skills/foo/SKILL.md", "edited by hand");
    let skill = find(&scan(&project, &[]), "foo").clone();
    assert_eq!(skill.versions.len(), 2);

    let mut seen = Vec::new();
    let mut trash = |path: &Path| -> io::Result<PathBuf> {
        seen.push(path.file_name().unwrap().to_string_lossy().into_owned());
        move_into(&bin, path)
    };
    trash_divergent_copies(&project, &skill, &mut trash).unwrap();

    // Une copie par version : les deux copies identiques ne font qu'une entrée.
    assert_eq!(seen, ["foo (claude)", "foo (agents)"]);
    let mut kept: Vec<String> = fs::read_dir(&bin)
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path().join("SKILL.md")).unwrap())
        .collect();
    kept.sort();
    assert_eq!(kept, ["edited by hand", "original"]);
    // Les dossiers du projet n'ont pas bougé : le retrait du CLI vient après.
    assert_eq!(
        fs::read_to_string(project.join(".agents/skills/foo/SKILL.md")).unwrap(),
        "edited by hand"
    );
    assert!(project.join(".claude/skills/foo/SKILL.md").is_file());
    assert!(project.join(".grok/skills/foo/SKILL.md").is_file());
}

#[test]
fn identical_copies_send_one_copy_to_the_trash_before_a_cli_removal() {
    let project = temp_project("identical-trash");
    let bin = temp_project("identical-trash-bin");
    write(&project, ".claude/skills/foo/SKILL.md", "same");
    write(&project, ".agents/skills/foo/SKILL.md", "same");
    let skill = find(&scan(&project, &[]), "foo").clone();
    assert_eq!(skill.versions.len(), 1);
    let mut trash = |path: &Path| move_into(&bin, path);
    trash_divergent_copies(&project, &skill, &mut trash).unwrap();
    // Une seule copie : les deux dossiers portent la même version.
    let kept: Vec<String> = fs::read_dir(&bin)
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path().join("SKILL.md")).unwrap())
        .collect();
    assert_eq!(kept, ["same"]);
    // Les dossiers du projet n'ont pas bougé : le retrait du CLI vient après.
    assert!(project.join(".claude/skills/foo/SKILL.md").is_file());
    assert!(project.join(".agents/skills/foo/SKILL.md").is_file());
}

#[test]
fn a_copy_edited_after_the_listing_is_staged_too() {
    let project = temp_project("stale-trash");
    let bin = temp_project("stale-trash-bin");
    write(&project, ".claude/skills/foo/SKILL.md", "same");
    write(&project, ".agents/skills/foo/SKILL.md", "same");
    // La liste du volet est lue quand les copies sont identiques…
    let skill = find(&scan(&project, &[]), "foo").clone();
    assert_eq!(skill.versions.len(), 1);
    // … puis une copie est retouchée avant le clic sur « Retirer ».
    write(
        &project,
        ".agents/skills/foo/SKILL.md",
        "edited after the listing",
    );
    let mut trash = |path: &Path| move_into(&bin, path);
    trash_divergent_copies(&project, &skill, &mut trash).unwrap();
    let mut kept: Vec<String> = fs::read_dir(&bin)
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path().join("SKILL.md")).unwrap())
        .collect();
    kept.sort();
    assert_eq!(kept, ["edited after the listing", "same"]);
}

#[test]
fn a_single_copy_is_staged_too() {
    let project = temp_project("single-trash");
    let bin = temp_project("single-trash-bin");
    write(&project, ".claude/skills/foo/SKILL.md", "only");
    let skill = find(&scan(&project, &[]), "foo").clone();
    let mut trash = |path: &Path| move_into(&bin, path);
    trash_divergent_copies(&project, &skill, &mut trash).unwrap();
    assert_eq!(fs::read_dir(&bin).unwrap().count(), 1);
}

#[test]
fn a_refused_trash_stops_before_the_cli_removal_and_leaves_no_staging_folder() {
    let project = temp_project("refused-trash");
    write(&project, ".claude/skills/foo/SKILL.md", "a");
    write(&project, ".agents/skills/foo/SKILL.md", "b");
    let skill = find(&scan(&project, &[]), "foo").clone();
    let mut staged = None;
    let mut trash = |path: &Path| -> io::Result<PathBuf> {
        staged = Some(path.to_path_buf());
        Err(io::Error::other("refused"))
    };
    assert!(trash_divergent_copies(&project, &skill, &mut trash).is_err());
    let staged = staged.expect("the trash was asked once");
    assert!(!staged.exists());
    assert!(!staged.parent().unwrap().exists());
    assert!(project.join(".claude/skills/foo/SKILL.md").is_file());
    assert!(project.join(".agents/skills/foo/SKILL.md").is_file());
}
