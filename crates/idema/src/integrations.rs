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

/// Pose (ou remplace) la commande et l'action sous `home`. Rien d'autre n'est touché.
pub fn install(home: &Path) -> io::Result<Installed> {
    let bin = home.join(".local/bin");
    fs::create_dir_all(&bin)?;
    let cli = bin.join("idema");
    fs::write(&cli, CLI)?;
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o755))?;

    let workflow = home.join("Library/Services").join(WORKFLOW);
    let contents = workflow.join("Contents");
    fs::create_dir_all(&contents)?;
    fs::write(contents.join("Info.plist"), WORKFLOW_INFO)?;
    fs::write(contents.join("document.wflow"), WORKFLOW_DOCUMENT)?;
    Ok(Installed { cli, workflow })
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
            "usage: idema <folder>\n"
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
}
