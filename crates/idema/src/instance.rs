//! Instance unique (spec §3.1) et emplacements d'Idema. Le verrou `flock` est rendu par le noyau à la mort
//! du process, même par `kill -9` : il ne reste jamais de verrou périmé.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Socket du serveur tmux d'Idema (`tmux -L idema`).
pub const DEFAULT_SOCKET: &str = "idema";

/// Le verrou tenu pendant toute la vie du process.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

/// Prend `<dir>/idema.lock`. `None` : une autre instance le tient déjà.
pub fn acquire(dir: &Path) -> std::io::Result<Option<InstanceLock>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join("idema.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(InstanceLock { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Dossier d'Idema : `~/.config/idema`, ou `IDEMA_HOME` s'il est posé. L'acceptation s'en sert pour lancer
/// une instance isolée à côté de celle de l'utilisateur.
pub fn config_dir(home: Option<OsString>, idema_home: Option<OsString>) -> Option<PathBuf> {
    match idema_home.filter(|value| !value.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => home
            .filter(|value| !value.is_empty())
            .map(|home| PathBuf::from(home).join(".config").join("idema")),
    }
}

/// Socket tmux : `IDEMA_TMUX_SOCKET` s'il est posé, sinon `idema`. Même usage que `IDEMA_HOME`.
pub fn tmux_socket(value: Option<OsString>) -> String {
    value
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_SOCKET.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_is_refused_until_the_first_is_released() {
        let dir = std::env::temp_dir().join(format!("idema-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = acquire(&dir).unwrap();
        assert!(first.is_some());
        assert!(acquire(&dir).unwrap().is_none());
        drop(first);
        // Un test voisin qui lance tmux ou git fait un `fork` : pendant l'instant avant son `exec`, l'enfant
        // tient une copie du descripteur et le verrou reste pris. L'attente borne cette fenêtre.
        let retaken = (0..100).any(|_| {
            acquire(&dir).unwrap().is_some() || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                false
            }
        });
        assert!(retaken);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn idema_home_overrides_the_default_folder() {
        assert_eq!(
            config_dir(Some("/Users/r".into()), None),
            Some(PathBuf::from("/Users/r/.config/idema"))
        );
        assert_eq!(
            config_dir(Some("/Users/r".into()), Some("/tmp/acc".into())),
            Some(PathBuf::from("/tmp/acc"))
        );
        assert_eq!(config_dir(None, Some("".into())), None);
        assert_eq!(tmux_socket(None), "idema");
        assert_eq!(tmux_socket(Some("idema-acc-1".into())), "idema-acc-1");
    }
}
