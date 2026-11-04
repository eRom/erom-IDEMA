//! Support des tests `host_` de la porte de la Tâche 11 : ils tournent sur le Mac de l'utilisateur, avec le vrai CLI `skills`
//! et les vrais harnais, et n'écrivent que dans un projet jetable désigné par `IDEMA_HOST_PROJECT`. Ils sont tous
//! `#[ignore]` : `cargo test -p idema` reste vert sans cet environnement, et `host-project-panel.sh` les lance avec
//! `--ignored host_`.

use std::path::{Path, PathBuf};

/// Le projet jetable de la porte. Refuse (panique) quand la variable manque, quand le dossier n'existe pas, quand il
/// est le dossier personnel, ou quand il n'est pas strictement sous le dossier temporaire du système (le dossier
/// temporaire lui-même est partagé : la porte n'y écrit pas).
pub(crate) fn host_project() -> PathBuf {
    let raw = std::env::var_os("IDEMA_HOST_PROJECT")
        .expect("IDEMA_HOST_PROJECT must name a throwaway project (run tests/acceptance/host-project-panel.sh)");
    let project =
        std::fs::canonicalize(&raw).expect("IDEMA_HOST_PROJECT must be an existing directory");
    assert!(project.is_dir(), "IDEMA_HOST_PROJECT must be a directory");
    let home = std::env::var_os("HOME")
        .and_then(|home| std::fs::canonicalize(home).ok())
        .expect("HOME must be set");
    assert_ne!(project, home, "refusing to run on $HOME");
    assert!(
        under_temp(&project),
        "refusing to run outside $TMPDIR or /private/var/folders: {}",
        project.display()
    );
    project
}

/// Vrai quand `path` (canonique) est strictement sous le dossier temporaire du système ou sous `/private/var/folders`.
fn under_temp(path: &Path) -> bool {
    let temp = std::fs::canonicalize(std::env::temp_dir()).ok();
    // Sur macOS le dossier temporaire est lui-même sous `/private/var/folders` : on l'écarte d'abord.
    if temp.as_deref() == Some(path) {
        return false;
    }
    strictly_under(path, Path::new("/private/var/folders"))
        || temp.is_some_and(|temp| strictly_under(path, &temp))
}

/// `path` est dans `root`, et n'est pas `root` lui-même.
fn strictly_under(path: &Path, root: &Path) -> bool {
    path != root && path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_temp_root_itself_is_not_a_throwaway_project() {
        let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(!under_temp(&temp));
        assert!(!under_temp(Path::new("/private/var/folders")));
        assert!(under_temp(&temp.join("idema-host-project")));
        assert!(under_temp(Path::new("/private/var/folders/ab/cd/T/x")));
        assert!(!under_temp(Path::new("/Users/someone/project")));
    }
}
