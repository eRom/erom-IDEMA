//! La fin d'un fichier de sortie, suivie à mesure qu'elle grandit (spec P4 §4.5) : le journal d'un serveur repéré dont la
//! sortie standard va dans un fichier ordinaire (le `nohup … > log 2>&1 &` d'un agent). Un fil lit les 64 Kio de fin, puis
//! relit à la taille quand `notify` le réveille (ou toutes les `POLL`, au cas où l'événement manque). Une troncature
//! relit depuis le début après avoir effacé l'écran. Sans fenêtre : les octets partent vers un rappel.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::hud;

/// Ce qu'on lit de la fin d'un fichier à l'ouverture du journal.
pub const TAIL_BYTES: u64 = 64 * 1024;
/// Au plus ce qui est lu d'un coup : un fichier qui grossit de plusieurs Mio entre deux lectures se rattrape par morceaux.
const READ_CHUNK: u64 = 1024 * 1024;
/// Filet des événements de `notify` : le fichier est relu à ce rythme même sans événement.
const POLL: Duration = Duration::from_millis(500);
/// Efface l'écran et son historique, curseur en haut à gauche : ce qui précède la relecture d'un fichier tronqué.
pub const CLEAR_SCREEN: &[u8] = b"\x1b[2J\x1b[3J\x1b[H";

/// Un fichier suivi : l'état de lecture, sans fil ni événement, pour se tester sur de vrais fichiers.
struct Follower {
    path: PathBuf,
    /// Où la lecture en est dans le fichier, en octets.
    offset: u64,
    /// (périphérique, inode) du fichier lu : un autre couple, c'est un fichier remplacé (rotation).
    identity: Option<(u64, u64)>,
    /// Le dernier octet rendu était un `\r` : un `\n` qui ouvre le morceau suivant n'ajoute pas un second `\r`.
    after_cr: bool,
}

impl Follower {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            offset: 0,
            identity: None,
            after_cr: false,
        }
    }

    /// Les `TAIL_BYTES` de fin du fichier, coupés à une fin de ligne quand le fichier est plus long. Vide si le fichier
    /// n'existe pas encore.
    fn initial(&mut self) -> Vec<u8> {
        let Ok(mut file) = File::open(&self.path) else {
            return Vec::new();
        };
        let Ok(meta) = file.metadata() else {
            return Vec::new();
        };
        let len = meta.len();
        let start = len.saturating_sub(TAIL_BYTES);
        let mut bytes = read_range(&mut file, start, len - start);
        self.offset = start + bytes.len() as u64;
        // Le début du morceau tombe au milieu d'une ligne : on repart à la ligne suivante, sauf s'il n'y en a pas.
        if start > 0
            && let Some(newline) = bytes.iter().position(|&byte| byte == b'\n')
        {
            bytes.drain(..=newline);
        }
        self.identity = Some((meta.dev(), meta.ino()));
        self.render(&bytes)
    }

    /// Ce que le fichier a gagné depuis la dernière lecture. Un fichier plus court qu'avant (troncature) ou remplacé
    /// (autre inode) est relu depuis le début, l'écran effacé d'abord. `None` : rien de nouveau.
    fn poll(&mut self) -> Option<Vec<u8>> {
        let mut file = File::open(&self.path).ok()?;
        let meta = file.metadata().ok()?;
        let identity = (meta.dev(), meta.ino());
        let len = meta.len();
        let replaced = self.identity.is_some_and(|known| known != identity);
        let mut out = Vec::new();
        if replaced || len < self.offset {
            self.offset = 0;
            self.after_cr = false;
            out.extend_from_slice(CLEAR_SCREEN);
        }
        self.identity = Some(identity);
        if len > self.offset {
            let wanted = (len - self.offset).min(READ_CHUNK);
            let bytes = read_range(&mut file, self.offset, wanted);
            self.offset += bytes.len() as u64;
            out.extend(self.render(&bytes));
        }
        (!out.is_empty()).then_some(out)
    }

    /// Les octets du fichier pour l'émulateur : un terminal ramène le curseur en début de ligne par `\r`, un fichier n'a
    /// que des `\n`.
    fn render(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
        for &byte in bytes {
            if byte == b'\n' && !self.after_cr {
                out.push(b'\r');
            }
            out.push(byte);
            self.after_cr = byte == b'\r';
        }
        out
    }
}

/// Lit `count` octets à partir de `from` (moins si le fichier s'arrête avant).
fn read_range(file: &mut File, from: u64, count: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(from)).is_ok() {
        let _ = file.take(count).read_to_end(&mut bytes);
    }
    bytes
}

/// Le suivi d'un fichier, sur son fil. Lâcher la valeur l'arrête.
pub struct FileTail {
    stop: Arc<AtomicBool>,
    wake: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
    _watcher: Option<RecommendedWatcher>,
}

impl FileTail {
    /// Suit `path` : `sender` reçoit d'abord les 64 Kio de fin, puis chaque ajout. Il rend `false` quand plus personne
    /// n'écoute (le suivi s'arrête alors de lui-même).
    pub fn spawn(path: &Path, mut sender: impl FnMut(Vec<u8>) -> bool + Send + 'static) -> Self {
        let (wake, woken) = mpsc::channel::<()>();
        let watcher = watch(path, wake.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let mut follower = Follower::new(path);
        let name = path.display().to_string();
        let thread = std::thread::Builder::new()
            .name("idema-file-tail".to_string())
            .spawn(move || {
                let first = follower.initial();
                if !first.is_empty() && !sender(first) {
                    return;
                }
                loop {
                    // Un événement ou le filet : dans les deux cas le fichier est relu à sa taille.
                    let _ = woken.recv_timeout(POLL);
                    if stopped.load(Ordering::Relaxed) {
                        return;
                    }
                    // Les événements s'accumulent pendant une lecture : un seul passage les couvre.
                    while woken.try_recv().is_ok() {}
                    if let Some(bytes) = follower.poll() {
                        // Une ligne (niveau debug) par ajout livré à la vue : l'acceptation y lit la latence d'une requête de plus.
                        hud::log_debug(&format!("servers tail sent bytes={}", bytes.len()));
                        // Le journal de debug est tamponné : sans vidage, la ligne attend un autre événement (aucun pane
                        // ne s'affiche derrière un onglet journal) et l'acceptation ne la lit pas à temps. Ce fil n'est pas
                        // celui de l'interface.
                        if hud::debug_enabled() {
                            hud::flush();
                        }
                        if !sender(bytes) {
                            return;
                        }
                    }
                }
            })
            .inspect_err(|error| hud::log_event(&format!("servers tail spawn failed: {error}")))
            .ok();
        hud::log_debug(&format!("servers tail started file={name}"));
        Self {
            stop,
            wake,
            thread,
            _watcher: watcher,
        }
    }
}

impl Drop for FileTail {
    /// Le fil est prévenu tout de suite (il ne livre plus rien), mais sa jointure et la chute de l'observateur se font sur un
    /// autre fil : la vue d'un journal tombe sur le fil d'interface, et le fil du suivi peut être au milieu d'une lecture ou
    /// d'une livraison, que la jointure attendrait (même raison que `stop_watch` du volet Changes).
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.wake.send(());
        let thread = self.thread.take();
        let watcher = self._watcher.take();
        if thread.is_none() && watcher.is_none() {
            return;
        }
        let release = move || {
            drop(watcher);
            if let Some(thread) = thread {
                let _ = thread.join();
            }
        };
        if std::thread::Builder::new()
            .name("idema-file-tail-release".to_string())
            .spawn(release)
            .is_err()
        {
            hud::log_event("servers tail release spawn failed");
        }
    }
}

/// Un observateur du fichier qui réveille le fil. Sans lui (fichier absent, plus de descripteurs), le filet de
/// `POLL` suffit.
fn watch(path: &Path, wake: mpsc::Sender<()>) -> Option<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |_event: notify::Result<notify::Event>| {
        let _ = wake.send(());
    })
    .ok()?;
    watcher.watch(path, RecursiveMode::NonRecursive).ok()?;
    Some(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;
    use std::time::Instant;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("idema-tail-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_short_file_is_read_whole_with_carriage_returns() {
        let dir = temp_dir("short");
        let path = dir.join("log");
        std::fs::write(&path, "un\ndeux\n").unwrap();
        let mut follower = Follower::new(&path);
        assert_eq!(follower.initial(), b"un\r\ndeux\r\n");
        assert_eq!(follower.poll(), None);
    }

    #[test]
    fn a_long_file_starts_at_the_first_whole_line_of_its_last_64_kib() {
        let dir = temp_dir("long");
        let path = dir.join("log");
        let line = "x".repeat(99) + "\n";
        let count = (TAIL_BYTES as usize / line.len()) + 50;
        std::fs::write(&path, line.repeat(count)).unwrap();
        let mut follower = Follower::new(&path);
        let first = follower.initial();
        assert!(first.len() as u64 <= TAIL_BYTES + count as u64);
        // Chaque ligne rendue est entière : aucune ne commence au milieu d'une autre.
        let text = String::from_utf8(first).unwrap();
        assert!(text.lines().all(|row| row.len() == 99));
        assert!(text.ends_with("\r\n"));
        assert_eq!(follower.poll(), None, "la fin lue ne se relit pas");
    }

    #[test]
    fn what_the_file_gains_is_read_from_where_the_last_read_stopped() {
        let dir = temp_dir("grow");
        let path = dir.join("log");
        std::fs::write(&path, "a\n").unwrap();
        let mut follower = Follower::new(&path);
        follower.initial();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"b\nc").unwrap();
        assert_eq!(follower.poll().unwrap(), b"b\r\nc");
        file.write_all(b"d\n").unwrap();
        assert_eq!(follower.poll().unwrap(), b"d\r\n");
        assert_eq!(follower.poll(), None);
    }

    #[test]
    fn a_line_feed_after_a_carriage_return_gets_no_second_one_even_across_two_reads() {
        let dir = temp_dir("crlf");
        let path = dir.join("log");
        std::fs::write(&path, "a\r").unwrap();
        let mut follower = Follower::new(&path);
        assert_eq!(follower.initial(), b"a\r");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"\nb\n").unwrap();
        assert_eq!(follower.poll().unwrap(), b"\nb\r\n");
    }

    #[test]
    fn a_truncated_file_is_read_again_from_its_start_after_the_screen_is_cleared() {
        let dir = temp_dir("truncate");
        let path = dir.join("log");
        std::fs::write(&path, "ancien contenu long\n").unwrap();
        let mut follower = Follower::new(&path);
        follower.initial();
        std::fs::write(&path, "neuf\n").unwrap();
        let mut expected = CLEAR_SCREEN.to_vec();
        expected.extend_from_slice(b"neuf\r\n");
        assert_eq!(follower.poll().unwrap(), expected);
        assert_eq!(follower.poll(), None);
    }

    #[test]
    fn a_replaced_file_is_read_from_its_start_even_when_it_is_longer() {
        let dir = temp_dir("rotate");
        let path = dir.join("log");
        std::fs::write(&path, "a\n").unwrap();
        let mut follower = Follower::new(&path);
        follower.initial();
        std::fs::rename(&path, dir.join("log.1")).unwrap();
        std::fs::write(&path, "plus long que l'ancien\n").unwrap();
        let mut expected = CLEAR_SCREEN.to_vec();
        expected.extend_from_slice("plus long que l'ancien\r\n".as_bytes());
        assert_eq!(follower.poll().unwrap(), expected);
    }

    #[test]
    fn a_missing_file_gives_nothing_until_it_exists() {
        let dir = temp_dir("absent");
        let path = dir.join("log");
        let mut follower = Follower::new(&path);
        assert!(follower.initial().is_empty());
        assert_eq!(follower.poll(), None);
        std::fs::write(&path, "né\n").unwrap();
        assert_eq!(follower.poll().unwrap(), "né\r\n".as_bytes());
    }

    #[test]
    fn dropping_a_tail_busy_delivering_does_not_wait_for_it() {
        let dir = temp_dir("slow-drop");
        let path = dir.join("log");
        std::fs::write(&path, "début\n").unwrap();
        let delivering = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&delivering);
        // La livraison des 64 Kio de fin dure 800 ms : le fil du suivi y est occupé quand la vue tombe.
        let tail = FileTail::spawn(&path, move |_| {
            flag.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(800));
            true
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !delivering.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "la fin du fichier n'est pas livrée"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = Instant::now();
        drop(tail);
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "la chute du suivi a attendu son fil : {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_tail_thread_sends_the_end_then_each_addition_and_stops_when_dropped() {
        let dir = temp_dir("thread");
        let path = dir.join("log");
        std::fs::write(&path, "début\n").unwrap();
        let received = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&received);
        let tail = FileTail::spawn(&path, move |bytes| {
            sink.lock().unwrap().extend(bytes);
            true
        });
        let wait_for = |expected: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if String::from_utf8_lossy(&received.lock().unwrap()).contains(expected) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        };
        assert!(wait_for("début"), "la fin du fichier arrive");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"suite\n").unwrap();
        assert!(wait_for("suite"), "un ajout arrive en moins de 5 s");
        drop(tail);
        file.write_all(b"apres\n").unwrap();
        std::thread::sleep(Duration::from_millis(700));
        assert!(
            !String::from_utf8_lossy(&received.lock().unwrap()).contains("apres"),
            "le suivi lâché ne lit plus"
        );
    }
}
