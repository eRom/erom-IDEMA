//! Les portes d'entrée vers IdemA hors de l'app, posées par le menu « IdemA » : la commande `idema <dossier>` dans
//! `~/.local/bin`, et l'action « Ouvrir dans IdemA » du Finder (clic droit sur un dossier) dans `~/Library/Services`.
//! Les deux appellent `open -b com.erom.idema`. Les fichiers sont dans le binaire : l'app ne dépend pas du dépôt.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const CLI: &str = include_str!("../../../packaging/cli/idema");
const WORKFLOW: &str = "Ouvrir dans IdemA.workflow";
const WORKFLOW_INFO: &str =
    include_str!("../../../packaging/finder/Ouvrir dans IdemA.workflow/Contents/Info.plist");
const WORKFLOW_DOCUMENT: &str =
    include_str!("../../../packaging/finder/Ouvrir dans IdemA.workflow/Contents/document.wflow");

/// Ce qui a été posé, pour le dire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub cli: PathBuf,
    pub workflow: PathBuf,
}

/// Le repère de version de `packaging/cli/idema`, remplacé à la pose par celle du binaire.
const VERSION_MARK: &str = "@IDEMA_VERSION@";
/// Ce qui signe une commande `idema` posée par IdemA : un fichier du même nom écrit par quelqu'un d'autre n'est pas
/// réécrit.
const CLI_SIGNATURE: &str = "open -b com.erom.idema";

/// La commande telle qu'elle est posée, avec la version de ce binaire.
fn cli_script() -> String {
    CLI.replace(VERSION_MARK, env!("CARGO_PKG_VERSION"))
}

fn cli_path(home: &Path) -> PathBuf {
    home.join(".local/bin/idema")
}

/// Pose (ou remplace) la commande et l'action sous `home`. Rien d'autre n'est touché.
pub fn install(home: &Path) -> io::Result<Installed> {
    let cli = cli_path(home);
    fs::create_dir_all(home.join(".local/bin"))?;
    fs::write(&cli, cli_script())?;
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o755))?;

    let workflow = home.join("Library/Services").join(WORKFLOW);
    let contents = workflow.join("Contents");
    fs::create_dir_all(&contents)?;
    fs::write(contents.join("Info.plist"), WORKFLOW_INFO)?;
    fs::write(contents.join("document.wflow"), WORKFLOW_DOCUMENT)?;
    Ok(Installed { cli, workflow })
}

/// Au lancement : une commande `idema` déjà posée par IdemA, mais d'une autre version (ou d'un autre contenu), est
/// réécrite avec celle de ce binaire. Absente ou écrite par quelqu'un d'autre, elle n'est pas touchée. Vrai si elle a
/// été réécrite.
pub fn refresh_cli(home: &Path) -> io::Result<bool> {
    let cli = cli_path(home);
    let current = match fs::read_to_string(&cli) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let wanted = cli_script();
    if current == wanted || !current.contains(CLI_SIGNATURE) {
        return Ok(false);
    }
    // `fs::write` garde les droits du fichier existant : la commande reste exécutable.
    fs::write(&cli, wanted)?;
    Ok(true)
}

/// Le Finder relit la liste des services sans attendre une nouvelle session. Un échec ne défait rien : l'action
/// apparaît alors à la prochaine ouverture de session.
pub fn refresh_services() -> io::Result<()> {
    let status = Command::new("/System/Library/CoreServices/pbs")
        .arg("-update")
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("pbs -update: {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_writes_an_executable_cli_and_the_finder_action_and_can_run_again() {
        let home = std::env::temp_dir().join(format!("idema-integrations-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();

        let installed = install(&home).unwrap();
        // Une seconde pose remplace sans erreur (mise à jour après une nouvelle version d'IdemA).
        assert_eq!(install(&home).unwrap(), installed);

        assert_eq!(installed.cli, home.join(".local/bin/idema"));
        let mode = fs::metadata(&installed.cli).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        // La commande posée tourne : sans argument, elle répond par son usage (code 64).
        let output = Command::new(&installed.cli).output().unwrap();
        assert_eq!(output.status.code(), Some(64));
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "usage: idema <folder> | idema --version\n"
        );
        // `--version` dit la version du binaire qui l'a posée, la ligne du menu Aide.
        let output = Command::new(&installed.cli)
            .arg("--version")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{}\n", crate::menus::version_line())
        );

        let contents = installed.workflow.join("Contents");
        for plist in ["Info.plist", "document.wflow"] {
            let lint = Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(contents.join(plist))
                .output()
                .unwrap();
            assert!(lint.status.success(), "{plist}");
        }
        let _ = fs::remove_dir_all(&home);
    }

    fn scratch_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("idema-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        home
    }

    #[test]
    fn a_cli_left_by_an_older_version_is_rewritten_and_stays_executable() {
        let home = scratch_home("refresh-old");
        let cli = home.join(".local/bin/idema");
        fs::write(&cli, CLI.replace(VERSION_MARK, "0.0.1")).unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(refresh_cli(&home).unwrap());
        assert_eq!(fs::read_to_string(&cli).unwrap(), cli_script());
        let mode = fs::metadata(&cli).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        // À jour, la commande n'est plus réécrite.
        assert!(!refresh_cli(&home).unwrap());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_missing_or_foreign_cli_is_left_alone() {
        let home = scratch_home("refresh-foreign");
        assert!(!refresh_cli(&home).unwrap());
        assert!(!home.join(".local/bin/idema").exists());

        let foreign = "#!/bin/sh\necho mine\n";
        fs::write(home.join(".local/bin/idema"), foreign).unwrap();
        assert!(!refresh_cli(&home).unwrap());
        assert_eq!(
            fs::read_to_string(home.join(".local/bin/idema")).unwrap(),
            foreign
        );
        let _ = fs::remove_dir_all(&home);
    }
}
