//! Les téléchargements hors de WebKit : où un fichier se pose pendant qu'il arrive, et comment il rejoint `~/Downloads`.
//! Rien ici ne parle à AppKit. `~/Downloads` est protégé par TCC : WebKit n'en reçoit jamais le chemin (son
//! `SandboxExtension::createHandle` bloque le fil principal tant que l'utilisateur n'a pas répondu), et IdemA n'y touche
//! que depuis un fil à part, une fois le fichier complet.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::{safe_file_name, unique_destination};

/// Ce que dit l'onglet quand macOS refuse l'écriture dans `~/Downloads`.
pub(super) const PERMISSION_DENIED_MESSAGE: &str = "accès au dossier Téléchargements refusé (Réglages Système › Confidentialité et sécurité › Fichiers et dossiers)";

/// Le dossier de dépôt des téléchargements en cours : `~/Library/Caches/com.erom.idema/downloads`. Un cache de
/// l'utilisateur, hors des dossiers que TCC protège.
pub(super) fn staging_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join("Library/Caches")
            .join(super::BUNDLE_ID)
            .join("downloads")
    })
}

/// Où un téléchargement se pose pendant qu'il arrive : `<racine>/<uuid>/<nom sûr>`. Un dossier par téléchargement : deux
/// téléchargements du même nom ne se rencontrent pas, et le nettoyage d'un échec retire le dossier entier.
pub(super) fn staging_path(root: &Path, uuid: &str, suggested: &str) -> PathBuf {
    root.join(uuid).join(safe_file_name(suggested))
}

/// Le texte d'un échec de déplacement : le refus de macOS a sa phrase, le reste dit l'erreur du système.
pub(super) fn failure_message(error: &io::Error) -> String {
    if error.kind() == ErrorKind::PermissionDenied {
        PERMISSION_DENIED_MESSAGE.to_owned()
    } else {
        error.to_string()
    }
}

/// Pose `source` dans `directory` sous un nom libre (`<nom>`, puis `<nom> (2)`…), sans jamais écraser un fichier. Le
/// fichier est lié sous son nouveau nom puis délié de l'ancien (atomique, et un nom pris entre-temps échoue au lieu
/// d'écraser) ; sur un autre volume, il est copié puis retiré. Un refus de permission remonte tel quel.
pub(super) fn move_into(source: &Path, directory: &Path) -> io::Result<PathBuf> {
    // `symlink_metadata` ne suit pas les liens : un lien cassé du même nom occupe la place, `exists()` le dirait libre.
    move_into_checking(source, directory, |candidate| {
        fs::symlink_metadata(candidate).is_ok()
    })
}

/// Combien de noms `move_into` essaie avant d'abandonner. Un nom pris entre le choix et le lien se rejoue ; une
/// existence mal lue (`taken` qui se trompe) ne doit pas tourner sans fin.
const MAX_NAME_ATTEMPTS: usize = 100;

/// `move_into` avec `taken` pour décider qu'un nom est occupé (injecté pour tester le plafond).
fn move_into_checking(
    source: &Path,
    directory: &Path,
    taken: impl Fn(&Path) -> bool,
) -> io::Result<PathBuf> {
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    for _ in 0..MAX_NAME_ATTEMPTS {
        let destination = unique_destination(directory, &name, &[], &taken);
        match fs::hard_link(source, &destination) {
            Ok(()) => {
                let _ = fs::remove_file(source);
                return Ok(destination);
            }
            // Le nom a été pris entre le choix et le lien : un autre nom.
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) if error.kind() == ErrorKind::PermissionDenied => return Err(error),
            Err(_) => match copy_into_new(source, &destination) {
                Ok(()) => {
                    let _ = fs::remove_file(source);
                    return Ok(destination);
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            },
        }
    }
    Err(io::Error::new(
        ErrorKind::AlreadyExists,
        "aucun nom libre trouvé dans Téléchargements",
    ))
}

/// Copie `source` vers `destination`, qui ne doit pas exister, avec l'attribut de quarantaine ; un échec en cours de
/// route retire la copie partielle.
fn copy_into_new(source: &Path, destination: &Path) -> io::Result<()> {
    let mut from = fs::File::open(source)?;
    let mut to = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    // Une copie ne reprend pas les attributs étendus : sans cette étape, un téléchargement qui change de volume
    // perdrait la quarantaine de Gatekeeper. Un lien dur (le cas courant) garde l'inode, donc l'attribut.
    let copied = io::copy(&mut from, &mut to)
        .map(|_| ())
        .and_then(|()| copy_quarantine(source, destination));
    if let Err(error) = copied {
        drop(to);
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

/// Le nom de l'attribut étendu que macOS pose sur un fichier téléchargé.
const QUARANTINE_ATTRIBUTE: &std::ffi::CStr = c"com.apple.quarantine";

fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "chemin avec un octet nul"))
}

/// La valeur de la quarantaine de `path`, `None` quand le fichier n'en porte pas.
fn read_quarantine(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let path = c_path(path)?;
    // SAFETY: chemin et nom terminés par un octet nul ; une taille nulle demande seulement la longueur de la valeur.
    let size = unsafe {
        libc::getxattr(
            path.as_ptr(),
            QUARANTINE_ATTRIBUTE.as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            0,
        )
    };
    if size < 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOATTR) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let mut value = vec![0u8; size as usize];
    // SAFETY: `value` a la taille annoncée par le premier appel ; le noyau n'écrit jamais plus que `value.len()`.
    let read = unsafe {
        libc::getxattr(
            path.as_ptr(),
            QUARANTINE_ATTRIBUTE.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    value.truncate(read as usize);
    Ok(Some(value))
}

/// Reporte la quarantaine de `source` sur `destination` : une copie ne l'emporte pas d'elle-même.
fn copy_quarantine(source: &Path, destination: &Path) -> io::Result<()> {
    let Some(value) = read_quarantine(source)? else {
        return Ok(());
    };
    let destination = c_path(destination)?;
    // SAFETY: chemin et nom terminés par un octet nul ; `value` porte exactement `value.len()` octets lisibles.
    let status = unsafe {
        libc::setxattr(
            destination.as_ptr(),
            QUARANTINE_ATTRIBUTE.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Le dernier geste d'un téléchargement fini, sur son fil : le fichier de dépôt va dans `downloads` (le chemin final), ou
/// l'échec dit pourquoi. Le dépôt (le dossier du téléchargement) disparaît dans les deux cas.
pub(super) fn finalize(staging: &Path, downloads: Option<&Path>) -> Result<PathBuf, String> {
    let outcome = match downloads {
        Some(directory) => move_into(staging, directory).map_err(|error| failure_message(&error)),
        None => Err("dossier Téléchargements introuvable".to_owned()),
    };
    discard_staging(staging);
    outcome
}

/// Retire le fichier de dépôt et son dossier (`<racine>/<uuid>`), qui n'est qu'à lui.
pub(super) fn discard_staging(staging: &Path) {
    if let Some(directory) = staging.parent() {
        let _ = fs::remove_dir_all(directory);
    }
}

/// Âge minimal d'un dépôt pour être jugé orphelin : un autre IdemA lancé en même temps peut avoir un téléchargement en
/// cours dans la même racine, jamais vieux d'un jour. Les 24 h sont un choix de Claude, sans source dans la spec.
pub(super) const ORPHAN_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// La dernière écriture d'une entrée du dépôt : pour un dossier, celle de son fichier le plus récent (un téléchargement
/// en cours réécrit son fichier, pas son dossier), à défaut celle du dossier ; pour un fichier ou un lien, la sienne.
fn last_touched(path: &Path, is_dir: bool) -> Option<SystemTime> {
    let modified = |path: &Path| fs::symlink_metadata(path).and_then(|m| m.modified()).ok();
    if is_dir && let Ok(children) = fs::read_dir(path) {
        let newest = children
            .flatten()
            .filter_map(|child| modified(&child.path()))
            .max();
        if newest.is_some() {
            return newest;
        }
    }
    modified(path)
}

/// Retire de `root` les dépôts laissés par un IdemA qui s'est arrêté en plein téléchargement : tout ce qui n'a pas été
/// touché depuis `min_age` (date du fichier de dépôt, voir `last_touched`). Au lancement, hors du fil principal ; une
/// racine absente ou illisible n'est pas une erreur.
pub(super) fn sweep_orphaned_staging(root: &Path, min_age: Duration) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let age = last_touched(&entry.path(), kind.is_dir())
            .and_then(|modified| now.duration_since(modified).ok());
        if !age.is_some_and(|age| age >= min_age) {
            continue;
        }
        // `file_type` ne suit pas les liens : un lien vers un dossier est retiré comme un fichier, jamais traversé.
        let _ = if kind.is_dir() {
            fs::remove_dir_all(entry.path())
        } else {
            fs::remove_file(entry.path())
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Un dossier neuf sous le dossier temporaire, propre à chaque test.
    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("idema-dl-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn staged(root: &Path, uuid: &str, name: &str, content: &str) -> PathBuf {
        let path = staging_path(root, uuid, name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    fn quarantine_value() -> Vec<u8> {
        b"0083;5f2b9c10;IdemA;3F2504E0-4F89-41D3-9A0C-0305E82C3301".to_vec()
    }

    fn set_quarantine(path: &Path, value: &[u8]) {
        let path = c_path(path).unwrap();
        // SAFETY: chemin et nom terminés par un octet nul, `value` lisible sur sa longueur.
        let status = unsafe {
            libc::setxattr(
                path.as_ptr(),
                QUARANTINE_ATTRIBUTE.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        assert_eq!(status, 0, "{}", io::Error::last_os_error());
    }

    #[test]
    fn the_copy_fallback_keeps_the_quarantine_attribute() {
        let area = scratch("quarantine");
        let source = area.join("source.bin");
        fs::write(&source, "contenu").unwrap();
        set_quarantine(&source, &quarantine_value());
        let destination = area.join("copie.bin");
        copy_into_new(&source, &destination).unwrap();
        assert_eq!(fs::read_to_string(&destination).unwrap(), "contenu");
        assert_eq!(
            read_quarantine(&destination).unwrap(),
            Some(quarantine_value())
        );
    }

    #[test]
    fn the_copy_fallback_adds_no_quarantine_to_a_clean_file() {
        let area = scratch("no-quarantine");
        let source = area.join("source.bin");
        fs::write(&source, "contenu").unwrap();
        let destination = area.join("copie.bin");
        copy_into_new(&source, &destination).unwrap();
        assert_eq!(read_quarantine(&destination).unwrap(), None);
    }

    #[test]
    fn a_download_stages_in_its_own_folder_under_a_safe_name() {
        let root = Path::new("/cache/downloads");
        assert_eq!(
            staging_path(root, "AAAA", "report.pdf"),
            Path::new("/cache/downloads/AAAA/report.pdf")
        );
        // Un nom proposé par le serveur ne sort jamais de son dossier.
        assert_eq!(
            staging_path(root, "AAAA", "../../etc/passwd"),
            Path::new("/cache/downloads/AAAA/passwd")
        );
        assert_eq!(
            staging_path(root, "BBBB", ""),
            Path::new("/cache/downloads/BBBB/download")
        );
    }

    #[test]
    fn a_finished_download_lands_in_downloads_and_its_staging_folder_goes() {
        let area = scratch("lands");
        let (root, downloads) = (area.join("staging"), area.join("Downloads"));
        fs::create_dir_all(&downloads).unwrap();
        let file = staged(&root, "U1", "rapport.txt", "contenu");
        let landed = finalize(&file, Some(&downloads)).unwrap();
        assert_eq!(landed, downloads.join("rapport.txt"));
        assert_eq!(fs::read_to_string(&landed).unwrap(), "contenu");
        assert!(!file.parent().unwrap().exists(), "le dépôt reste");
    }

    #[test]
    fn a_taken_name_is_never_overwritten() {
        let area = scratch("taken");
        let (root, downloads) = (area.join("staging"), area.join("Downloads"));
        fs::create_dir_all(&downloads).unwrap();
        fs::write(downloads.join("rapport.txt"), "ancien").unwrap();
        fs::write(downloads.join("rapport (2).txt"), "ancien 2").unwrap();
        let file = staged(&root, "U2", "rapport.txt", "neuf");
        let landed = finalize(&file, Some(&downloads)).unwrap();
        assert_eq!(landed, downloads.join("rapport (3).txt"));
        assert_eq!(fs::read_to_string(&landed).unwrap(), "neuf");
        assert_eq!(
            fs::read_to_string(downloads.join("rapport.txt")).unwrap(),
            "ancien"
        );
        assert_eq!(
            fs::read_to_string(downloads.join("rapport (2).txt")).unwrap(),
            "ancien 2"
        );
    }

    #[test]
    fn a_dangling_symlink_with_the_download_name_counts_as_a_taken_name() {
        let area = scratch("dangling");
        let (root, downloads) = (area.join("staging"), area.join("Downloads"));
        fs::create_dir_all(&downloads).unwrap();
        // Un lien cassé : `exists()` le suit et le dit libre, alors que le nom est pris.
        std::os::unix::fs::symlink(area.join("nulle-part"), downloads.join("rapport.txt")).unwrap();
        let file = staged(&root, "U6", "rapport.txt", "contenu");
        // Dans un fil à part : une boucle sans fin fait échouer le test au lieu de le suspendre.
        let (sender, receiver) = std::sync::mpsc::channel();
        let target = downloads.clone();
        std::thread::spawn(move || {
            let _ = sender.send(finalize(&file, Some(&target)));
        });
        let outcome = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("le déplacement ne finit pas : la boucle des noms tourne sans fin");
        let landed = outcome.unwrap();
        assert_eq!(landed, downloads.join("rapport (2).txt"));
        assert_eq!(fs::read_to_string(&landed).unwrap(), "contenu");
        assert!(
            fs::symlink_metadata(downloads.join("rapport.txt"))
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    fn a_name_that_stays_taken_ends_in_an_error_instead_of_looping() {
        let area = scratch("capped");
        let (root, downloads) = (area.join("staging"), area.join("Downloads"));
        fs::create_dir_all(&downloads).unwrap();
        fs::write(downloads.join("rapport.txt"), "ancien").unwrap();
        let file = staged(&root, "U7", "rapport.txt", "neuf");
        // Une existence qui répond toujours « libre » : le lien échoue chaque fois sur le fichier présent.
        let error = move_into_checking(&file, &downloads, |_| false).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(downloads.join("rapport.txt")).unwrap(),
            "ancien"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "neuf");
    }

    const THREE_DAYS: Duration = Duration::from_secs(3 * 24 * 3600);

    /// Date le fichier ou le dossier `path` de `age` en arrière.
    fn age_path(path: &Path, age: Duration) {
        fs::File::open(path)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    #[test]
    fn startup_sweeps_old_orphaned_staging_folders_and_spares_recent_ones() {
        let area = scratch("sweep");
        let root = area.join("downloads");
        // Le fichier de dépôt est vieux, son dossier est neuf (créé à l'instant) : l'âge est celui du fichier.
        let old = staged(&root, "OLD", "gros.zip", "x");
        age_path(&old, THREE_DAYS);
        let recent = staged(&root, "NEW", "en-cours.zip", "y");
        // Un dossier vide et vieux n'a que sa propre date.
        let empty = root.join("EMPTY");
        fs::create_dir_all(&empty).unwrap();
        age_path(&empty, THREE_DAYS);
        let stray = root.join("stray.tmp");
        fs::write(&stray, "z").unwrap();
        age_path(&stray, THREE_DAYS);
        sweep_orphaned_staging(&root, ORPHAN_MIN_AGE);
        assert!(!old.parent().unwrap().exists(), "le dépôt orphelin reste");
        assert!(!empty.exists(), "le dossier vide et vieux reste");
        assert!(!stray.exists(), "le fichier orphelin reste");
        assert!(recent.exists(), "un dépôt récent ne se retire pas");
        // Une racine absente n'est pas une erreur.
        sweep_orphaned_staging(&area.join("absente"), ORPHAN_MIN_AGE);
    }

    #[test]
    fn a_file_still_being_written_keeps_its_old_folder() {
        let area = scratch("sweep-live");
        let root = area.join("downloads");
        // Le dossier date d'il y a trois jours, mais le fichier a été écrit à l'instant : le téléchargement vit.
        let live = staged(&root, "LIVE", "long.iso", "octets");
        age_path(live.parent().unwrap(), THREE_DAYS);
        sweep_orphaned_staging(&root, ORPHAN_MIN_AGE);
        assert!(live.exists(), "un téléchargement en cours a été retiré");
    }

    #[test]
    fn the_sweep_never_follows_a_link_out_of_the_staging_root() {
        let area = scratch("sweep-link");
        let (root, outside) = (area.join("downloads"), area.join("dehors"));
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("garde.txt"), "a garder").unwrap();
        let link = root.join("lien");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        sweep_orphaned_staging(&root, Duration::ZERO);
        assert!(fs::symlink_metadata(&link).is_err(), "le lien reste");
        assert_eq!(
            fs::read_to_string(outside.join("garde.txt")).unwrap(),
            "a garder"
        );
    }

    #[test]
    fn the_copy_fallback_for_another_volume_refuses_to_overwrite_too() {
        let area = scratch("copy");
        let source = area.join("source.txt");
        fs::write(&source, "contenu").unwrap();
        let free = area.join("copie.txt");
        copy_into_new(&source, &free).unwrap();
        assert_eq!(fs::read_to_string(&free).unwrap(), "contenu");
        let error = copy_into_new(&source, &free).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&free).unwrap(), "contenu");
    }

    #[test]
    fn a_refused_folder_says_so_and_leaves_no_staging_file() {
        let area = scratch("denied");
        let (root, downloads) = (area.join("staging"), area.join("Downloads"));
        fs::create_dir_all(&downloads).unwrap();
        // Un dossier sans droit d'écriture : le refus que TCC rend à IdemA sans consentement.
        fs::set_permissions(&downloads, fs::Permissions::from_mode(0o500)).unwrap();
        let file = staged(&root, "U3", "rapport.txt", "contenu");
        let outcome = finalize(&file, Some(&downloads));
        fs::set_permissions(&downloads, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(outcome, Err(PERMISSION_DENIED_MESSAGE.to_owned()));
        assert!(!file.parent().unwrap().exists(), "le dépôt reste");
        assert!(!downloads.join("rapport.txt").exists());
    }

    #[test]
    fn any_other_failure_reports_the_system_error_and_cleans_up() {
        let area = scratch("missing");
        let root = area.join("staging");
        let file = staged(&root, "U4", "rapport.txt", "contenu");
        // Un dossier de destination qui n'existe pas : ni refus de permission, ni succès.
        let outcome = finalize(&file, Some(&area.join("absent"))).unwrap_err();
        assert_ne!(outcome, PERMISSION_DENIED_MESSAGE);
        assert!(!outcome.is_empty());
        assert!(!file.parent().unwrap().exists());
        // Sans dossier Téléchargements du tout.
        let file = staged(&root, "U5", "rapport.txt", "contenu");
        assert_eq!(
            finalize(&file, None),
            Err("dossier Téléchargements introuvable".to_owned())
        );
        assert!(!file.parent().unwrap().exists());
    }

    #[test]
    fn only_a_permission_error_gets_the_privacy_sentence() {
        let denied = io::Error::from(ErrorKind::PermissionDenied);
        assert_eq!(failure_message(&denied), PERMISSION_DENIED_MESSAGE);
        let full = io::Error::other("No space left on device");
        assert_eq!(failure_message(&full), "No space left on device");
    }
}
