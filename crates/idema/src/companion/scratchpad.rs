//! Le Scratchpad vu du téléphone : lire `scratchpad.md` et y ajouter une ligne. Le volet du Mac relit le disque et le
//! disque gagne (`scratchpad/store.rs`) : on n'a donc rien à lui signaler.

use std::io::{self, Read};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use crate::files::document::{self, LineEnding, MAX_EDITABLE_BYTES};
use crate::settings::toml_write;

/// La plus longue ligne admise, en octets (spec §3.9).
pub const LINE_MAX_BYTES: usize = 2_000;

/// Pourquoi une ligne n'a pas été ajoutée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendError {
    Empty,
    TooLong,
    ControlChar,
    Io(String),
}

impl AppendError {
    /// Le code que la route rend dans `error`.
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too_long",
            Self::ControlChar => "control_char",
            Self::Io(_) => "scratchpad_unwritable",
        }
    }
}

/// Les octets de la note, `None` si elle n'existe pas. Un tube, un périphérique ou un dossier est refusé avant toute
/// ouverture (ouvrir un tube attend un écrivain, donc bloquerait le fil), et une note de plus de 5 Mo (la borne de
/// l'éditeur du Mac) n'est pas chargée : `FileTooLarge`.
fn read_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the note is not a regular file",
        ));
    }
    if meta.len() > MAX_EDITABLE_BYTES {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    // La note a pu grossir entre la mesure et la lecture : la lecture se borne elle-même.
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_EDITABLE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_EDITABLE_BYTES {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    Ok(Some(bytes))
}

/// Le texte de la note. Une note absente se lit vide (le Mac la crée au premier ajout) ; un octet qui n'est pas de
/// l'UTF-8 s'affiche en U+FFFD plutôt que de rendre la note illisible. Une note de plus de 5 Mo rend
/// `ErrorKind::FileTooLarge`, un fichier qui n'est pas ordinaire (tube, dossier) `InvalidInput`.
pub fn read(path: &Path) -> io::Result<String> {
    Ok(read_bytes(path)?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default())
}

/// La date de dernière modification de la note (lien suivi), en millisecondes depuis l'époque Unix ; `None` si elle
/// n'existe pas encore.
pub fn modified_millis(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(super::fleet::unix_millis(modified))
}

/// Un seul ajout à la fois dans le process : lecture puis écriture atomique ne sont pas une opération indivisible, deux
/// ajouts du téléphone qui s'entrecroisent perdraient une ligne.
static APPEND_LOCK: Mutex<()> = Mutex::new(());

/// Ajoute `line` à la fin de la note : un saut de ligne avant si la note n'en finit pas par un, la fin de ligne
/// majoritaire du fichier (règle du Mac, `document::detect_line_ending`), puis écriture atomique sur la cible réelle
/// (un lien symbolique est suivi, comme pour `config.toml`), qui garde les droits du fichier.
///
/// Refusés en `Io`, sans rien écrire, ce que l'éditeur du Mac refuse aussi : un lien dont la cible manque (l'écriture
/// atomique remplacerait le lien par un fichier), une note en lecture seule, un fichier qui n'est pas ordinaire, une
/// note qui dépasserait 5 Mo, une note qui n'est pas de l'UTF-8.
pub fn append_line(path: &Path, line: &str) -> Result<(), AppendError> {
    if line.is_empty() {
        return Err(AppendError::Empty);
    }
    if line.len() > LINE_MAX_BYTES {
        return Err(AppendError::TooLong);
    }
    if line.chars().any(char::is_control) {
        return Err(AppendError::ControlChar);
    }
    let _guard = APPEND_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let io_error = |error: io::Error| AppendError::Io(error.to_string());
    let target = match std::fs::canonicalize(path) {
        Ok(target) => target,
        // Un lien dont la cible manque : écrire au chemin du lien le remplacerait par un fichier ordinaire, et la note
        // divergerait des dotfiles quand la cible reviendra.
        Err(_) if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink()) => {
            return Err(AppendError::Io("the note is a dangling symlink".into()));
        }
        Err(_) => path.to_path_buf(),
    };
    let mut text = match read_bytes(&target).map_err(io_error)? {
        Some(bytes) => String::from_utf8(bytes)
            .map_err(|_| AppendError::Io("the note is not valid UTF-8".into()))?,
        None => String::new(),
    };
    document::refuse_read_only(&target).map_err(io_error)?;
    let eol = match document::detect_line_ending(&text) {
        LineEnding::Crlf => "\r\n",
        LineEnding::Lf => "\n",
    };
    let needs_break = !text.is_empty() && !text.ends_with('\n');
    let added = line.len() + eol.len() * (1 + usize::from(needs_break));
    if (text.len() + added) as u64 > MAX_EDITABLE_BYTES {
        return Err(io_error(io::ErrorKind::FileTooLarge.into()));
    }
    if needs_break {
        text.push_str(eol);
    }
    text.push_str(line);
    text.push_str(eol);
    toml_write::write_atomically(&target, &text).map_err(io_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::test_support::TempDir;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn note(dir: &TempDir, content: &str) -> std::path::PathBuf {
        let path = dir.0.join("scratchpad.md");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn append_adds_a_newline_when_missing() {
        let dir = TempDir::new("scratchpad");
        let path = note(&dir, "first");
        append_line(&path, "second").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first\nsecond\n");
        append_line(&path, "third").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first\nsecond\nthird\n");
    }

    #[test]
    fn append_keeps_crlf() {
        let dir = TempDir::new("scratchpad");
        let path = note(&dir, "one\r\ntwo\r\n");
        append_line(&path, "three").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "one\r\ntwo\r\nthree\r\n"
        );
        // Une dernière ligne sans fin : le saut ajouté est un CRLF aussi.
        let path = note(&dir, "one\r\ntwo");
        append_line(&path, "three").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "one\r\ntwo\r\nthree\r\n"
        );
    }

    #[test]
    fn append_keeps_permissions() {
        let dir = TempDir::new("scratchpad");
        let path = note(&dir, "kept\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        append_line(&path, "more").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        assert_eq!(fs::read_to_string(&path).unwrap(), "kept\nmore\n");
        // Aucun fichier temporaire ne reste à côté.
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn append_writes_through_a_symlink() {
        let dir = TempDir::new("scratchpad");
        let real = dir.0.join("dotfiles");
        fs::create_dir_all(&real).unwrap();
        let target = real.join("notes.md");
        fs::write(&target, "linked\n").unwrap();
        let link = dir.0.join("scratchpad.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        append_line(&link, "through").unwrap();

        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), target);
        assert_eq!(fs::read_to_string(&target).unwrap(), "linked\nthrough\n");
        assert_eq!(read(&link).unwrap(), "linked\nthrough\n");
    }

    #[test]
    fn append_refuses_control_characters() {
        let dir = TempDir::new("scratchpad");
        let path = note(&dir, "untouched\n");
        for bad in [
            "two\nlines",
            "carriage\rreturn",
            "tab\there",
            "nul\0byte",
            "esc\x1b[31m",
            "del\x7f",
            "c1\u{85}",
        ] {
            assert_eq!(
                append_line(&path, bad),
                Err(AppendError::ControlChar),
                "{bad:?}"
            );
        }
        assert_eq!(append_line(&path, ""), Err(AppendError::Empty));
        let too_long = "x".repeat(LINE_MAX_BYTES + 1);
        assert_eq!(append_line(&path, &too_long), Err(AppendError::TooLong));
        assert_eq!(fs::read_to_string(&path).unwrap(), "untouched\n");
    }

    #[test]
    fn append_accepts_the_longest_line_counted_in_bytes() {
        let dir = TempDir::new("scratchpad");
        let path = note(&dir, "");
        // 1 000 caractères de 2 octets : 2 000 octets, pile la borne ; un de plus la dépasse.
        let line = "é".repeat(LINE_MAX_BYTES / 2);
        append_line(&path, &line).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{line}\n"));
        let over = format!("{line}a");
        assert_eq!(append_line(&path, &over), Err(AppendError::TooLong));
    }

    #[test]
    fn append_creates_a_missing_note() {
        let dir = TempDir::new("scratchpad");
        let path = dir.0.join("sub").join("scratchpad.md");
        append_line(&path, "born").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "born\n");
        assert_eq!(read(&dir.0.join("absent.md")).unwrap(), "");
        assert_eq!(modified_millis(&dir.0.join("absent.md")), None);
    }

    #[test]
    fn append_leaves_a_note_that_is_not_utf8_alone() {
        let dir = TempDir::new("scratchpad");
        let path = dir.0.join("scratchpad.md");
        fs::write(&path, b"bad \xff byte\n").unwrap();
        assert!(matches!(
            append_line(&path, "more"),
            Err(AppendError::Io(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"bad \xff byte\n");
        assert_eq!(read(&path).unwrap(), "bad \u{fffd} byte\n");
    }

    #[test]
    fn concurrent_appends_keep_every_line() {
        let dir = TempDir::new("scratchpad");
        let path = note(&dir, "");
        std::thread::scope(|scope| {
            for index in 0..8 {
                let path = &path;
                scope.spawn(move || append_line(path, &format!("line {index}")).unwrap());
            }
        });
        let text = fs::read_to_string(&path).unwrap();
        for index in 0..8 {
            assert!(text.contains(&format!("line {index}\n")), "{text:?}");
        }
    }

    #[test]
    fn append_never_replaces_a_dangling_symlink() {
        let dir = TempDir::new("scratchpad");
        let missing = dir.0.join("dotfiles").join("notes.md");
        let link = dir.0.join("scratchpad.md");
        std::os::unix::fs::symlink(&missing, &link).unwrap();

        assert!(matches!(
            append_line(&link, "lost"),
            Err(AppendError::Io(_))
        ));

        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), missing);
        assert!(!missing.exists());
        assert_eq!(
            fs::read_dir(&dir.0).unwrap().count(),
            1,
            "no temporary left"
        );
    }

    #[test]
    fn append_refuses_a_read_only_note() {
        let dir = TempDir::new("scratchpad");
        let path = note(&dir, "locked\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        assert!(matches!(
            append_line(&path, "more"),
            Err(AppendError::Io(_))
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "locked\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o444
        );
    }

    #[test]
    fn append_follows_the_majority_line_ending_like_the_mac() {
        let dir = TempDir::new("scratchpad");
        let path = note(&dir, "a\r\nb\r\nc\n");
        append_line(&path, "d").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\r\nb\r\nc\nd\r\n");
        // Un seul CRLF contre deux LF nus : la majorité est LF, comme `document.rs`.
        let path = note(&dir, "a\nb\nc\r\n");
        append_line(&path, "d").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\nb\nc\r\nd\n");
        // Égalité : LF.
        let path = note(&dir, "a\r\nb\n");
        append_line(&path, "c").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\r\nb\nc\n");
    }

    #[test]
    fn append_refuses_to_grow_a_note_past_the_editable_size() {
        let dir = TempDir::new("scratchpad");
        let limit = usize::try_from(MAX_EDITABLE_BYTES).unwrap();
        let path = dir.0.join("scratchpad.md");
        fs::write(&path, vec![b'a'; limit]).unwrap();
        assert!(matches!(append_line(&path, "x"), Err(AppendError::Io(_))));
        assert_eq!(fs::metadata(&path).unwrap().len(), MAX_EDITABLE_BYTES);
        // Pile à la borne (3 octets : la ligne et son saut) : accepté.
        let mut text = vec![b'a'; limit - 4];
        text.push(b'\n');
        fs::write(&path, text).unwrap();
        append_line(&path, "xy").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), MAX_EDITABLE_BYTES);
    }

    #[test]
    fn read_refuses_a_note_past_the_editable_size() {
        let dir = TempDir::new("scratchpad");
        let path = dir.0.join("scratchpad.md");
        let limit = usize::try_from(MAX_EDITABLE_BYTES).unwrap();
        fs::write(&path, vec![b'a'; limit]).unwrap();
        assert_eq!(read(&path).unwrap().len(), limit);
        fs::write(&path, vec![b'a'; limit + 1]).unwrap();
        assert_eq!(read(&path).unwrap_err().kind(), io::ErrorKind::FileTooLarge);
    }

    /// Un tube comme note : ni la lecture ni l'ajout ne s'y bloquent (l'ouverture d'un tube attend un écrivain).
    #[test]
    fn a_fifo_note_is_refused_without_blocking() {
        let dir = TempDir::new("scratchpad");
        let path = dir.0.join("scratchpad.md");
        let made = std::process::Command::new("mkfifo").arg(&path).status();
        assert!(made.unwrap().success());
        let (sender, receiver) = std::sync::mpsc::channel();
        let probe = path.clone();
        std::thread::spawn(move || {
            let read_failed = read(&probe).is_err();
            let append_failed = matches!(append_line(&probe, "x"), Err(AppendError::Io(_)));
            let _ = sender.send((read_failed, append_failed));
        });
        let outcome = receiver
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("a FIFO note must not block");
        assert_eq!(outcome, (true, true));
    }
}
