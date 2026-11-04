//! Les opérations de fichiers de l'arbre du volet Fichiers (spec P2 §4.3) : créer, renommer, déplacer, mettre à la
//! Corbeille. Aucune dépendance à GPUI : tout se teste sur le disque. Aucune suppression définitive ici, et aucune
//! écriture par-dessus un fichier existant : un nom pris est un refus, jamais un écrasement.

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use objc2_foundation::{NSFileManager, NSString, NSURL};

/// Pourquoi un nom est refusé ; `message` est le texte affiché sous le champ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    Invalid,
    Taken,
}

impl NameError {
    pub fn message(self) -> &'static str {
        match self {
            NameError::Invalid => "Nom invalide.",
            NameError::Taken => "Ce nom existe déjà.",
        }
    }
}

#[derive(Debug)]
pub enum OpError {
    Name(NameError),
    Io(io::Error),
}

impl From<io::Error> for OpError {
    fn from(error: io::Error) -> Self {
        OpError::Io(error)
    }
}

/// Un nom d'entrée : ni vide, ni `.` ni `..`, ni `/`, ni caractère de contrôle. Le champ rend déjà le nom rogné.
pub fn validate_name(name: &str) -> Result<(), NameError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.chars().any(char::is_control)
    {
        return Err(NameError::Invalid);
    }
    Ok(())
}

/// La longueur en octets de la partie du nom sans l'extension, celle qu'on sélectionne à l'ouverture du champ.
/// Un dossier, ou un nom sans `.` après le premier caractère (`.env`, `Makefile`), se sélectionne en entier.
pub fn stem_len(name: &str, is_dir: bool) -> usize {
    if is_dir {
        return name.len();
    }
    // Le premier caractère est écarté : un point en tête n'ouvre pas une extension.
    let skip = name.chars().next().map_or(0, char::len_utf8);
    match name[skip..].rfind('.') {
        Some(dot) => skip + dot,
        None => name.len(),
    }
}

/// Crée un fichier vide dans `dir`. Un nom pris est refusé, le fichier existant n'est pas touché.
pub fn create_file(dir: &Path, name: &str) -> Result<PathBuf, OpError> {
    validate_name(name).map_err(OpError::Name)?;
    let path = dir.join(name);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(taken_or_io)?;
    Ok(path)
}

/// Crée un dossier dans `dir`. Un nom pris est refusé.
pub fn create_dir(dir: &Path, name: &str) -> Result<PathBuf, OpError> {
    validate_name(name).map_err(OpError::Name)?;
    let path = dir.join(name);
    fs::create_dir(&path).map_err(taken_or_io)?;
    Ok(path)
}

fn taken_or_io(error: io::Error) -> OpError {
    if error.kind() == io::ErrorKind::AlreadyExists {
        OpError::Name(NameError::Taken)
    } else {
        OpError::Io(error)
    }
}

/// Renomme une entrée dans son dossier. Le même nom ne touche pas au disque ; un changement de casse seul réussit.
pub fn rename_entry(path: &Path, new_name: &str) -> Result<PathBuf, OpError> {
    validate_name(new_name).map_err(OpError::Name)?;
    if path.file_name().is_some_and(|name| name == new_name) {
        return Ok(path.to_path_buf());
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    let target = parent.join(new_name);
    rename_exclusive(path, &target)?;
    Ok(target)
}

/// Ce que devient un déplacement vers un dossier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MovePlan {
    /// L'entrée est déjà dans ce dossier : rien à faire.
    Stay,
    /// Un dossier ne se range ni dans lui-même ni en dessous.
    Refused,
    To(PathBuf),
}

/// Le plan d'un déplacement, sans toucher au disque.
pub fn move_plan(source: &Path, target_dir: &Path) -> MovePlan {
    let Some(name) = source.file_name() else {
        return MovePlan::Refused;
    };
    if target_dir.starts_with(source) {
        return MovePlan::Refused;
    }
    if source.parent() == Some(target_dir) {
        return MovePlan::Stay;
    }
    MovePlan::To(target_dir.join(name))
}

/// Déplace une entrée dans un dossier. `Some(destination)` si elle a bougé, `None` si le plan était `Stay` ou `Refused`.
pub fn move_entry(source: &Path, target_dir: &Path) -> Result<Option<PathBuf>, OpError> {
    match move_plan(source, target_dir) {
        MovePlan::To(dest) => {
            rename_exclusive(source, &dest)?;
            Ok(Some(dest))
        }
        MovePlan::Stay | MovePlan::Refused => Ok(None),
    }
}

/// Renomme sans jamais écraser (`fs::rename` écraserait en silence). Une cible qui désigne déjà la source est un
/// renommage de casse sur un volume insensible à la casse : il passe.
fn rename_exclusive(from: &Path, to: &Path) -> Result<(), OpError> {
    // Seul un changement de casse dans le même dossier passe par `fs::rename`. Deux liens physiques d'un même
    // fichier ont aussi le même inode, mais `rename` ne ferait alors rien : c'est un nom pris.
    if is_case_only_rename(from, to)
        && let (Ok(source), Ok(target)) = (from.symlink_metadata(), to.symlink_metadata())
        && source.dev() == target.dev()
        && source.ino() == target.ino()
    {
        fs::rename(from, to)?;
        return Ok(());
    }
    let from_c = path_to_c(from)?;
    let to_c = path_to_c(to)?;
    // SAFETY : deux chaînes C terminées par NUL, vivantes pendant l'appel.
    let status = unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_EXCL) };
    if status == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EEXIST) => Err(OpError::Name(NameError::Taken)),
        // Un volume qui ne sait pas renommer sans écraser (exFAT) : on contrôle la cible à la main.
        Some(libc::ENOTSUP | libc::EINVAL) => {
            if to.symlink_metadata().is_ok() {
                Err(OpError::Name(NameError::Taken))
            } else {
                fs::rename(from, to)?;
                Ok(())
            }
        }
        _ => Err(OpError::Io(error)),
    }
}

/// Même dossier, noms égaux à la casse près.
fn is_case_only_rename(from: &Path, to: &Path) -> bool {
    match (from.parent(), to.parent(), from.file_name(), to.file_name()) {
        (Some(from_dir), Some(to_dir), Some(from_name), Some(to_name)) => {
            from_dir == to_dir
                && from_name.to_string_lossy().to_lowercase()
                    == to_name.to_string_lossy().to_lowercase()
        }
        _ => false,
    }
}

fn path_to_c(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// Met l'entrée à la Corbeille (jamais de suppression définitive) et rend le chemin où elle a atterri.
pub fn move_to_trash(path: &Path) -> io::Result<PathBuf> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let mut landed = None;
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, Some(&mut landed))
        .map_err(|error| io::Error::other(error.localizedDescription().to_string()))?;
    landed
        .and_then(|url| url.path())
        .map(|path| PathBuf::from(path.to_string()))
        .ok_or_else(|| io::Error::other("the Trash returned no path"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Un dossier jetable propre au test, retiré à la fin.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "idema-ops-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn is_taken<T>(result: Result<T, OpError>) -> bool {
        matches!(result, Err(OpError::Name(NameError::Taken)))
    }

    #[test]
    fn names_with_a_slash_a_control_character_or_dots_only_are_invalid() {
        let control = format!("a{}b", char::from(7u8));
        for bad in ["", ".", "..", "a/b", control.as_str()] {
            assert_eq!(validate_name(bad), Err(NameError::Invalid), "{bad:?}");
        }
        for good in ["brouillon.md", ".env", "é tè.md", "a:b"] {
            assert_eq!(validate_name(good), Ok(()), "{good:?}");
        }
    }

    #[test]
    fn the_stem_leaves_the_extension_out() {
        assert_eq!(stem_len("brouillon.md", false), 9);
        assert_eq!(stem_len(".env", false), 4);
        assert_eq!(stem_len("archive.tar.gz", false), 11);
        assert_eq!(stem_len("Makefile", false), 8);
        assert_eq!(stem_len("notes.d", true), 7);
    }

    #[test]
    fn creating_an_existing_name_is_refused_and_leaves_it_untouched() {
        let scratch = Scratch::new();
        let existing = scratch.path().join("a.md");
        fs::write(&existing, "x").unwrap();

        assert!(is_taken(create_file(scratch.path(), "a.md")));
        assert!(is_taken(create_dir(scratch.path(), "a.md")));
        assert_eq!(fs::read_to_string(&existing).unwrap(), "x");

        let created = create_file(scratch.path(), "b.md").unwrap();
        assert_eq!(created, scratch.path().join("b.md"));
        assert_eq!(fs::read_to_string(&created).unwrap(), "");

        let folder = create_dir(scratch.path(), "notes").unwrap();
        assert!(folder.is_dir());
    }

    #[test]
    fn renaming_onto_an_existing_name_never_overwrites() {
        let scratch = Scratch::new();
        let a = scratch.path().join("a.md");
        let b = scratch.path().join("b.md");
        fs::write(&a, "A").unwrap();
        fs::write(&b, "B").unwrap();
        assert!(is_taken(rename_entry(&a, "b.md")));
        assert_eq!(fs::read_to_string(&a).unwrap(), "A");
        assert_eq!(fs::read_to_string(&b).unwrap(), "B");

        let sub = scratch.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let moved = sub.join("a.md");
        fs::write(&moved, "C").unwrap();
        assert!(is_taken(move_entry(&moved, scratch.path())));
        assert_eq!(fs::read_to_string(&a).unwrap(), "A");
        assert_eq!(fs::read_to_string(&b).unwrap(), "B");
        assert_eq!(fs::read_to_string(&moved).unwrap(), "C");
    }

    #[test]
    fn a_hard_link_is_a_taken_name() {
        let scratch = Scratch::new();
        let a = scratch.path().join("a.md");
        let b = scratch.path().join("b.md");
        fs::write(&a, "A").unwrap();
        fs::hard_link(&a, &b).unwrap();

        assert!(is_taken(rename_entry(&a, "b.md")));
        assert!(a.exists());
        assert!(b.exists());
    }

    #[test]
    fn a_case_only_rename_is_not_a_taken_name() {
        let scratch = Scratch::new();
        let path = scratch.path().join("brouillon.md");
        fs::write(&path, "texte").unwrap();

        let renamed = rename_entry(&path, "Brouillon.md").unwrap();
        assert_eq!(renamed, scratch.path().join("Brouillon.md"));

        let names: Vec<String> = fs::read_dir(scratch.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"Brouillon.md".to_string()), "{names:?}");
        assert!(!names.contains(&"brouillon.md".to_string()), "{names:?}");
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "texte");
    }

    #[test]
    fn a_folder_never_moves_into_itself_or_below() {
        let p = Path::new;
        assert_eq!(move_plan(p("/p/notes"), p("/p/notes")), MovePlan::Refused);
        assert_eq!(
            move_plan(p("/p/notes"), p("/p/notes/sub")),
            MovePlan::Refused
        );
        assert_eq!(move_plan(p("/p/notes/a.md"), p("/p/notes")), MovePlan::Stay);
        assert_eq!(
            move_plan(p("/p/notes/a.md"), p("/p")),
            MovePlan::To(PathBuf::from("/p/a.md"))
        );
        assert_eq!(
            move_plan(p("/p/notes"), p("/p/notes2")),
            MovePlan::To(PathBuf::from("/p/notes2/notes"))
        );
    }

    #[test]
    fn moving_a_folder_keeps_its_content() {
        let scratch = Scratch::new();
        let notes = scratch.path().join("notes");
        let dest = scratch.path().join("dest");
        fs::create_dir_all(&notes).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(notes.join("x.md"), "x").unwrap();

        let moved = move_entry(&notes, &dest).unwrap();
        assert_eq!(moved, Some(dest.join("notes")));
        assert_eq!(
            fs::read_to_string(dest.join("notes").join("x.md")).unwrap(),
            "x"
        );
        assert!(!notes.exists());
    }

    #[test]
    fn trash_moves_the_entry_out_and_returns_where_it_went() {
        let source =
            std::env::temp_dir().join(format!("idema-trash-test-{}.txt", std::process::id()));
        fs::write(&source, "à jeter").unwrap();

        let dest = move_to_trash(&source).unwrap();
        assert!(!source.exists());
        assert!(
            dest.components()
                .any(|component| component.as_os_str() == ".Trash"),
            "{dest:?}"
        );
        // TCC protège ~/.Trash : on n'y lit rien sans accord, et ce n'est pas ce qu'on teste ici.
        match dest.try_exists() {
            Ok(present) => {
                assert!(present, "{dest:?}");
                // Au mieux : ce seul chemin, celui que la Corbeille vient de nous rendre.
                let _ = fs::remove_file(&dest);
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("Trash protected by TCC, existence not checked: {error}");
            }
            Err(error) => panic!("{error}"),
        }
    }

    #[test]
    fn trash_of_a_missing_file_fails() {
        let missing =
            std::env::temp_dir().join(format!("idema-trash-missing-{}.txt", std::process::id()));
        assert!(move_to_trash(&missing).is_err());
    }
}
