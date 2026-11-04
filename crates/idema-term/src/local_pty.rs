//! Le PTY local du volet Terminal (spec §3.1, D6) : un shell dans sa propre session de process, avec
//! l'environnement et l'intégration zsh des panes, plus `TERM=xterm-256color`. Rien ne passe par tmux : il
//! meurt avec Idema (AC-23).

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use crate::emulator::GridSize;

/// Délai entre le SIGHUP et le SIGKILL de la fin d'un PTY (spec §3.1).
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// Ce que le fil de lecture d'un PTY local transmet.
#[derive(Debug, PartialEq, Eq)]
pub enum PtyEvent {
    Output(Vec<u8>),
    /// Le shell et tout ce qui tenait le terminal sont partis.
    Closed,
}

/// Ce que lance un PTY local.
#[derive(Debug, Clone, Copy)]
pub struct PtyCommand<'a> {
    /// Le shell (`$SHELL` de la liste blanche, `/bin/zsh` par défaut) et ses arguments (`-l` : de login).
    pub program: &'a Path,
    pub args: &'a [&'a str],
    pub directory: &'a Path,
    /// L'environnement des panes (liste blanche, `ZDOTDIR` de l'intégration) ; `TERM` est ajouté.
    pub env: &'a [(String, String)],
    pub size: GridSize,
}

pub struct LocalPty {
    /// Le maître, fermé par `terminate` ; les fils de lecture et d'écriture en tiennent chacun une copie.
    master: Option<OwnedFd>,
    /// Le shell ; `terminate` le confie à un fil qui le récolte, et `None` reste alors derrière.
    child: Option<Child>,
    shell: u32,
    /// Levé par le fil qui a récolté le shell après `terminate`.
    reaped: Arc<AtomicBool>,
    /// File d'écriture vers le shell : un fil à part écrit, le fil d'interface ne bloque jamais sur un tampon plein.
    input: Option<mpsc::Sender<Vec<u8>>>,
    terminated: bool,
}

fn winsize(size: GridSize) -> libc::winsize {
    libc::winsize {
        ws_row: u16::try_from(size.lines).unwrap_or(u16::MAX),
        ws_col: u16::try_from(size.columns).unwrap_or(u16::MAX),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

// `ptsname_r` existe sur macOS mais `libc` ne le déclare pas pour Apple.
unsafe extern "C" {
    fn ptsname_r(fd: libc::c_int, buf: *mut libc::c_char, buflen: libc::size_t) -> libc::c_int;
}

/// Ouvre le maître et l'esclave d'un PTY, tous deux `O_CLOEXEC` dès leur création : un process lancé ensuite par
/// Idema (le serveur tmux, une commande) ne doit jamais garder le maître ouvert, sans quoi le terminal ne
/// raccrocherait pas à la mort d'Idema. Un `fcntl` après coup laissait une fenêtre où un `fork` concurrent
/// héritait des descripteurs ; ici il n'y en a plus. Le terminal de contrôle vient de `TIOCSCTTY` dans `pre_exec`.
fn open_pty(size: GridSize) -> io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY : `posix_openpt` ne lit rien ; le descripteur rendu est à nous.
    let raw = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if raw == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY : `posix_openpt` vient de rendre ce descripteur, il n'est à personne d'autre.
    let master = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY : `grantpt` et `unlockpt` portent sur un maître que nous possédons.
    if unsafe { libc::grantpt(master.as_raw_fd()) } == -1
        || unsafe { libc::unlockpt(master.as_raw_fd()) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    let mut name = [0 as libc::c_char; 128];
    // SAFETY : le noyau écrit au plus `name.len()` octets, terminaison comprise.
    let named = unsafe { ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) };
    if named != 0 {
        // Sur macOS `ptsname_r` rend -1 et pose `errno` (glibc rend le code lui-même) : `named` n'est pas une erreur.
        return Err(io::Error::last_os_error());
    }
    // SAFETY : `ptsname_r` a rendu un texte C terminé dans `name`.
    let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
    // SAFETY : `path` est un texte C valide.
    let raw = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if raw == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY : `open` vient de rendre ce descripteur, il n'est à personne d'autre.
    let slave = unsafe { OwnedFd::from_raw_fd(raw) };
    let size = winsize(size);
    // SAFETY : `TIOCSWINSZ` lit un `winsize` valide sur un maître que nous possédons.
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok((master, slave))
}

impl LocalPty {
    /// Ouvre le PTY et lance le shell. `on_event` est appelé depuis le fil de lecture, puis une fois avec
    /// `Closed` ; il ne doit jamais bloquer.
    pub fn spawn(
        command: &PtyCommand,
        mut on_event: impl FnMut(PtyEvent) + Send + 'static,
    ) -> io::Result<Self> {
        let (master, slave) = open_pty(command.size)?;

        let mut process = Command::new(command.program);
        process
            .args(command.args)
            .current_dir(command.directory)
            .env_clear()
            .envs(
                command
                    .env
                    .iter()
                    .filter(|(key, _)| key != "TERM")
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        idema_tmux::server::reset_signal_state(&mut process);
        // SAFETY : `setsid` et `ioctl` sont async-signal-safe ; l'entrée standard est déjà l'esclave.
        unsafe {
            process.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = process.spawn()?;

        let mut reader = File::from(master.try_clone()?);
        std::thread::Builder::new()
            .name("idema-pty-read".into())
            .spawn(move || {
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => on_event(PtyEvent::Output(buffer[..read].to_vec())),
                        Err(error) if error.kind() == ErrorKind::Interrupted => {}
                        // EIO : plus personne ne tient l'esclave.
                        Err(_) => break,
                    }
                }
                on_event(PtyEvent::Closed);
            })?;

        let (sender, receiver) = mpsc::channel::<Vec<u8>>();
        let mut writer = File::from(master.try_clone()?);
        std::thread::Builder::new()
            .name("idema-pty-write".into())
            .spawn(move || {
                for bytes in receiver {
                    if writer.write_all(&bytes).is_err() {
                        break;
                    }
                }
            })?;

        Ok(Self {
            master: Some(master),
            shell: child.id(),
            child: Some(child),
            reaped: Arc::new(AtomicBool::new(false)),
            input: Some(sender),
            terminated: false,
        })
    }

    /// Octets pour le shell : saisie, collage, réponses de l'émulateur.
    pub fn write(&self, bytes: &[u8]) {
        if let Some(input) = &self.input {
            let _ = input.send(bytes.to_vec());
        }
    }

    /// Nouvelle taille de la grille : le noyau envoie SIGWINCH au groupe de premier plan.
    pub fn resize(&self, size: GridSize) -> io::Result<()> {
        let Some(master) = &self.master else {
            return Ok(());
        };
        let size = winsize(size);
        // SAFETY : `TIOCSWINSZ` lit un `winsize` valide sur un maître que nous possédons.
        if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Le shell (chef de sa session et de son groupe).
    pub fn shell_pid(&self) -> u32 {
        self.shell
    }

    /// Le dossier courant du groupe au premier plan du terminal (le shell au prompt, sinon la commande en cours) :
    /// `tcgetpgrp` du maître, puis `proc_pidinfo` sur ce groupe. `None` si le terminal est fermé ou le process parti.
    pub fn foreground_cwd(&self) -> Option<PathBuf> {
        let master = self.master.as_ref()?;
        // SAFETY : lecture du groupe de premier plan de notre terminal.
        let group = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
        if group <= 0 {
            return None;
        }
        // SAFETY : `proc_vnodepathinfo` n'est fait que d'entiers et de tableaux d'octets, le zéro y est valide.
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // SAFETY : le noyau écrit au plus `size` octets dans `info`, qui en fait exactement cette taille.
        let written = unsafe {
            libc::proc_pidinfo(
                group,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut info as *mut libc::proc_vnodepathinfo).cast(),
                size,
            )
        };
        if written < size {
            return None;
        }
        // Le chemin est un texte C dans un tableau 32 x 32 déclaré en deux dimensions par `libc`.
        let bytes: Vec<u8> = info
            .pvi_cdir
            .vip_path
            .iter()
            .flatten()
            .map(|&byte| byte as u8)
            .take_while(|&byte| byte != 0)
            .collect();
        (!bytes.is_empty()).then(|| PathBuf::from(OsString::from_vec(bytes)))
    }

    /// Fin du PTY (spec §3.1) : le maître se ferme, SIGHUP part au groupe du shell et au groupe de premier plan
    /// (la commande en cours a son propre groupe), puis SIGKILL au bout de `grace`, envoyé par un process
    /// détaché qui survit à Idema.
    pub fn terminate(&mut self, grace: Duration) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        // Un shell déjà parti est récolté ici : son pgid est libre, aucun signal ne lui est destiné (le noyau a
        // raccroché le terminal à sa sortie) et le guetteur de SIGKILL n'a rien à surveiller.
        if self
            .child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(Some(_))))
        {
            self.input = None;
            self.master = None;
            self.child = None;
            self.reaped.store(true, Ordering::SeqCst);
            return;
        }
        // SAFETY : lecture du groupe de premier plan de notre terminal.
        let foreground = self
            .master
            .as_ref()
            .map_or(-1, |master| unsafe { libc::tcgetpgrp(master.as_raw_fd()) });
        let shell = i32::try_from(self.shell).unwrap_or(0);
        // L'écrivain s'arrête avec sa file, et notre maître se ferme.
        self.input = None;
        self.master = None;
        let groups: Vec<i32> = [shell, foreground]
            .into_iter()
            .filter(|group| *group > 1)
            .fold(Vec::new(), |mut groups, group| {
                if !groups.contains(&group) {
                    groups.push(group);
                }
                groups
            });
        for group in &groups {
            // SAFETY : un signal à un groupe de process ; un groupe disparu rend une erreur ignorée.
            unsafe {
                libc::killpg(*group, libc::SIGHUP);
            }
        }
        let script = groups
            .iter()
            .map(|group| format!("kill -KILL -- -{group} 2>/dev/null"))
            .collect::<Vec<_>>()
            .join("; ");
        let mut reaper = Command::new("/bin/sh");
        reaper
            .arg("-c")
            .arg(format!("sleep {}; {script}; exit 0", grace.as_secs_f32()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        idema_tmux::server::reset_signal_state(&mut reaper);
        // SAFETY : `setsid` est async-signal-safe ; le guetteur ne doit pas mourir avec le groupe d'Idema.
        unsafe {
            reaper.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        if let Ok(mut reaper) = reaper.spawn() {
            // Le guetteur est récolté par un fil, pour ne pas laisser de zombie si Idema vit encore.
            let spawned = std::thread::Builder::new()
                .name("idema-pty-reaper".into())
                .spawn(move || {
                    let _ = reaper.wait();
                });
            if let Err(error) = spawned {
                eprintln!("pty reaper thread not spawned: {error}");
            }
        }
        // Le shell aussi est récolté par un fil : sans cela, chaque shell de volet parti resterait un zombie
        // jusqu'à la fin d'Idema.
        if let Some(mut child) = self.child.take() {
            let reaped = Arc::clone(&self.reaped);
            let spawned = std::thread::Builder::new()
                .name("idema-pty-wait".into())
                .spawn(move || {
                    let _ = child.wait();
                    reaped.store(true, Ordering::SeqCst);
                });
            if let Err(error) = spawned {
                eprintln!("pty shell wait thread not spawned: {error}");
            }
        }
    }

    /// Le shell a-t-il fini ? Le récolte s'il l'a fait.
    pub fn has_exited(&mut self) -> bool {
        match &mut self.child {
            Some(child) => matches!(child.try_wait(), Ok(Some(_))),
            None => self.reaped.load(Ordering::SeqCst),
        }
    }
}

impl Drop for LocalPty {
    fn drop(&mut self) {
        self.terminate(KILL_GRACE);
    }
}
