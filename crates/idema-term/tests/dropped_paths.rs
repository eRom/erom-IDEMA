//! Les chemins déposés sur un pane (spec §6.3, AC-24), relus par un vrai zsh : l'échappement rend chaque chemin
//! tel quel, un seul mot par chemin, sans expansion.

use std::process::Command;

use idema_term::input::dropped_paths_text;

#[test]
fn zsh_reads_every_escaped_path_back_unchanged() {
    let paths = [
        "/tmp/idema test.txt",
        "/Users/r/Café (2).md",
        "/x/$HOME*'\"`!&;|<>[]{}?#~^=%,:@+",
        "/x/a\\b",
        "/x/日本語 ✓.txt",
        "/x/=start",
    ];
    let text = dropped_paths_text(&paths).unwrap();
    // `print -rl` écrit un argument par ligne, sans interpréter les antislashs ; `-f` : sans fichiers de démarrage.
    let output = Command::new("/bin/zsh")
        .args([
            "-f",
            "-c",
            &format!("setopt extended_glob; print -rl -- {text}"),
        ])
        .output()
        .expect("zsh runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let words: Vec<String> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(words, paths);
}
