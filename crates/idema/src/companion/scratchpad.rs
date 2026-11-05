//! Le Scratchpad vu du téléphone : lire `scratchpad.md` et y ajouter une ligne. Le volet du Mac relit le disque et le
//! disque gagne (`scratchpad/store.rs`) : on n'a donc rien à lui signaler.

use std::io;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

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

/// Le texte de la note. Une note absente se lit vide (le Mac la crée au premier ajout) ; un octet qui n'est pas de
/// l'UTF-8 s'affiche en U+FFFD plutôt que de rendre la note illisible.
pub fn read(path: &Path) -> io::Result<String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
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

/// Ajoute `line` à la fin de la note : un saut de ligne avant si la note n'en finit pas par un, la fin de ligne du
/// fichier (CRLF si sa première ligne en porte un, LF sinon), puis écriture atomique sur la cible réelle (un lien
/// symbolique est suivi, comme pour `config.toml`), qui garde les droits du fichier.
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
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut text = match std::fs::read(&target) {
        Ok(bytes) => String::from_utf8(bytes)
            .map_err(|_| AppendError::Io("the note is not valid UTF-8".into()))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(AppendError::Io(error.to_string())),
    };
    let eol = if text.find('\n').is_some_and(|at| text[..at].ends_with('\r')) {
        "\r\n"
    } else {
        "\n"
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push_str(eol);
    }
    text.push_str(line);
    text.push_str(eol);
    toml_write::write_atomically(&target, &text).map_err(|error| AppendError::Io(error.to_string()))
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
}
