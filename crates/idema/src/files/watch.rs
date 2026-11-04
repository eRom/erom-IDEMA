//! La surveillance du disque (spec §4.3) : les changements sous la racine d'un projet, regroupés par
//! lots de 100 ms et sans doublon. L'arbre et les onglets s'y abonnent pour suivre ce que les agents
//! écrivent. Rien n'est jamais écrit ici : on observe seulement.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::hud;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Fenêtre de regroupement : le premier événement ouvre un lot, qui se ferme 100 ms plus tard.
const BATCH_WINDOW: Duration = Duration::from_millis(100);

/// Un lot de la surveillance : les chemins changés, et les dossiers que FSEvents ne sait plus décrire (débordement de
/// sa file : `Flag::Rescan`), à relire en entier, sous-dossiers compris.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WatchBatch {
    pub paths: Vec<PathBuf>,
    pub rescan: Vec<PathBuf>,
}

impl WatchBatch {
    fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.rescan.is_empty()
    }
}

/// Les dossiers Git d'une cible. Un dépôt ordinaire n'en a qu'un (`<racine>/.git`, deux fois). Un worktree lié a son
/// propre dossier (`<dépôt>/.git/worktrees/<nom>`, où vivent son index et son `HEAD`), hors de sa racine, et le dossier
/// commun du dépôt (`<dépôt>/.git`, où vivent les branches).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDirs {
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
}

impl GitDirs {
    /// Les dossiers d'un dépôt ordinaire ouvert à `root`.
    pub fn of_root(root: &Path) -> GitDirs {
        GitDirs {
            git_dir: root.join(".git"),
            common_dir: root.join(".git"),
        }
    }

    /// Les mêmes dossiers sous leur forme canonique, celle des chemins que FSEvents rapporte. Un dossier absent garde sa
    /// forme d'origine.
    fn canonical(&self) -> GitDirs {
        let resolve = |dir: &Path| std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        GitDirs {
            git_dir: resolve(&self.git_dir),
            common_dir: resolve(&self.common_dir),
        }
    }
}

/// La surveillance d'un projet. La lâcher arrête la surveillance : le watcher tombe, le canal de
/// `notify` se ferme, le fil `idema-watch` voit la fin du canal et s'arrête.
pub struct ProjectWatch {
    /// Gardée pour les essais, qui vérifient la forme canonique des chemins émis.
    #[cfg(test)]
    root: PathBuf,
    _watcher: RecommendedWatcher,
}

impl ProjectWatch {
    /// Surveille `root` récursivement. La racine est canonicalisée une fois : FSEvents rapporte des
    /// chemins résolus (`/private/var/...` pour `/var/...`), donc tout chemin émis commence par
    /// `self.root()`, sous cette forme canonique.
    pub fn start(root: &Path) -> notify::Result<(ProjectWatch, UnboundedReceiver<WatchBatch>)> {
        let root = std::fs::canonicalize(root)?;
        let dirs = GitDirs::of_root(&root);
        Self::start_inner(root, &dirs, false)
    }

    /// Comme `start`, mais le lot voit aussi ce que Git écrit dans ses dossiers quand un agent stage, commite ou change
    /// de branche (l'index et `HEAD` du dossier Git de la cible, les références du dossier commun) : c'est ce que lit le
    /// volet Changes. Les objets et les verrous restent du bruit. Le dossier commun est surveillé à part quand il est
    /// hors de la racine, comme pour un worktree lié.
    pub fn start_with_git(
        root: &Path,
        dirs: &GitDirs,
    ) -> notify::Result<(ProjectWatch, UnboundedReceiver<WatchBatch>)> {
        let root = std::fs::canonicalize(root)?;
        Self::start_inner(root, dirs, true)
    }

    fn start_inner(
        root: PathBuf,
        dirs: &GitDirs,
        with_git: bool,
    ) -> notify::Result<(ProjectWatch, UnboundedReceiver<WatchBatch>)> {
        let dirs = dirs.canonical();
        let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |event| {
            // Le récepteur disparaît quand le fil de regroupement s'arrête : rien à signaler.
            let _ = raw_tx.send(event);
        })?;
        watcher.watch(&root, RecursiveMode::Recursive)?;
        // Le dossier commun d'un worktree lié est hors de la racine : sans cette seconde racine, ni les branches ni
        // l'index du worktree (sous lui) ne seraient vus.
        if with_git && !dirs.common_dir.starts_with(&root) {
            watcher.watch(&dirs.common_dir, RecursiveMode::Recursive)?;
        }

        let (batch_tx, batch_rx) = futures::channel::mpsc::unbounded();
        let thread_root = root.clone();
        std::thread::Builder::new()
            .name("idema-watch".into())
            .spawn(move || run_batching(&thread_root, &dirs, with_git, &raw_rx, &batch_tx))
            .map_err(notify::Error::io)?;

        Ok((
            ProjectWatch {
                #[cfg(test)]
                root,
                _watcher: watcher,
            },
            batch_rx,
        ))
    }

    /// La racine canonique, préfixe de tous les chemins émis.
    #[cfg(test)]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Boucle du fil `idema-watch` : attend un événement, ouvre une fenêtre de 100 ms, émet le lot.
/// S'arrête quand le watcher est lâché (canal brut fermé) ou quand plus personne n'écoute les lots.
fn run_batching(
    root: &Path,
    dirs: &GitDirs,
    with_git: bool,
    raw_rx: &mpsc::Receiver<notify::Result<Event>>,
    batch_tx: &UnboundedSender<WatchBatch>,
) {
    while let Ok(first) = raw_rx.recv() {
        let mut batch = Batch::new(root, dirs, with_git);
        batch.absorb(first);
        let deadline = Instant::now() + BATCH_WINDOW;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match raw_rx.recv_timeout(left) {
                Ok(event) => batch.absorb(event),
                Err(_) => break,
            }
        }
        if !batch.is_empty() && batch_tx.unbounded_send(batch.batch).is_err() {
            return;
        }
    }
}

/// Un lot en cours : chemins dans l'ordre d'arrivée, chacun une seule fois.
struct Batch {
    root: PathBuf,
    /// Les dossiers Git de la cible, fixés pour tout le lot.
    dirs: GitDirs,
    /// Les métadonnées de Git comptent (volet Changes).
    with_git: bool,
    batch: WatchBatch,
    seen: HashSet<PathBuf>,
}

impl Batch {
    fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }
}

impl Batch {
    fn new(root: &Path, dirs: &GitDirs, with_git: bool) -> Batch {
        Batch {
            root: root.to_path_buf(),
            dirs: dirs.clone(),
            with_git,
            batch: WatchBatch::default(),
            seen: HashSet::new(),
        }
    }

    fn absorb(&mut self, event: notify::Result<Event>) {
        let event = match event {
            Ok(event) => event,
            // Une erreur du watcher (débordement, dossier disparu) n'a pas de chemin : on la journalise.
            Err(error) => {
                hud::log_event(&format!(
                    "files watch failed root={}: {error}",
                    self.root.display()
                ));
                return;
            }
        };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        // Le débordement porte le dossier à relire en entier : un chemin ordinaire ne suffirait pas (le parent seul
        // serait relu, et les fichiers modifiés dessous resteraient inconnus).
        let rescan = event.flag() == Some(notify::event::Flag::Rescan);
        for path in event.paths {
            if !is_relevant(&self.dirs, &path, self.with_git) {
                continue;
            }
            if rescan && !self.batch.rescan.contains(&path) {
                self.batch.rescan.push(path.clone());
            }
            if self.seen.insert(path.clone()) {
                self.batch.paths.push(path);
            }
        }
    }
}

/// Un chemin compte-t-il pour le lot ? Hors des dossiers Git, oui. Dessous, tout est du bruit, sauf `info/exclude` du
/// dossier commun : c'est lui, avec les `.gitignore`, qui change ce que l'arbre atténue. Avec `with_git`, passent aussi
/// l'`index`, `HEAD` et `MERGE_HEAD` du dossier Git de la cible, et les références (`refs/`) et `packed-refs` du
/// dossier commun : ce que Git réécrit quand un agent stage ou commite. Les objets, l'index d'un autre worktree et tout
/// fichier `.lock` restent du bruit : le renommage qui libère un verrou émet, lui, le vrai nom.
fn is_relevant(dirs: &GitDirs, path: &Path, with_git: bool) -> bool {
    let in_git_dir = path.strip_prefix(&dirs.git_dir).ok();
    let in_common_dir = path.strip_prefix(&dirs.common_dir).ok();
    if in_git_dir.is_none() && in_common_dir.is_none() {
        return true;
    }
    if path
        .extension()
        .is_some_and(|extension| extension == "lock")
    {
        return false;
    }
    // Pour un dépôt ordinaire les deux dossiers sont le même : un chemin qui échoue à l'un des deux tests peut réussir
    // à l'autre.
    let in_git_dir_matches = in_git_dir.is_some_and(|inside| {
        inside == Path::new("index")
            || inside == Path::new("HEAD")
            || inside == Path::new("MERGE_HEAD")
    });
    let in_common_dir_matches = in_common_dir
        .is_some_and(|inside| inside.starts_with("refs") || inside == Path::new("packed-refs"));
    if in_common_dir.is_some_and(|inside| inside == Path::new("info/exclude")) {
        return true;
    }
    with_git && (in_git_dir_matches || in_common_dir_matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("idema-watch-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Attend un lot au plus `timeout` : `None` si rien n'arrive. Minuteur sur un fil, course avec
    /// le flux dans `block_on`.
    fn next_batch(
        rx: &mut UnboundedReceiver<WatchBatch>,
        timeout: Duration,
    ) -> Option<Vec<PathBuf>> {
        next_full_batch(rx, timeout).map(|batch| batch.paths)
    }

    fn next_full_batch(
        rx: &mut UnboundedReceiver<WatchBatch>,
        timeout: Duration,
    ) -> Option<WatchBatch> {
        let (timer_tx, timer_rx) = futures::channel::oneshot::channel::<()>();
        std::thread::spawn(move || {
            std::thread::sleep(timeout);
            let _ = timer_tx.send(());
        });
        futures::executor::block_on(async {
            match futures::future::select(rx.next(), timer_rx).await {
                futures::future::Either::Left((batch, _)) => batch,
                futures::future::Either::Right(_) => None,
            }
        })
    }

    /// Lit des lots jusqu'à en voir un qui contient `wanted`, en vérifiant qu'aucun n'a de doublon.
    fn wait_for_path(
        rx: &mut UnboundedReceiver<WatchBatch>,
        wanted: &Path,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while let Some(batch) = next_batch(rx, deadline.saturating_duration_since(Instant::now())) {
            let unique: HashSet<&PathBuf> = batch.iter().collect();
            assert_eq!(unique.len(), batch.len(), "duplicate path in {batch:?}");
            if batch.iter().any(|path| path == wanted) {
                return true;
            }
        }
        false
    }

    #[test]
    fn a_created_file_arrives_in_one_batch_within_two_seconds() {
        let dir = temp_dir("created");
        let (watch, mut rx) = ProjectWatch::start(&dir).expect("start watch");
        let file = watch.root().join("a/b.txt");
        fs::create_dir(watch.root().join("a")).expect("create dir");
        fs::write(&file, "one").expect("create file");
        fs::write(&file, "two").expect("write file");
        assert!(
            wait_for_path(&mut rx, &file, Duration::from_secs(2)),
            "no batch with a/b.txt"
        );
        drop(watch);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_under_dot_git_are_ignored() {
        let dir = temp_dir("dot-git");
        fs::create_dir_all(dir.join(".git/objects")).expect("create .git");
        // Laisse retomber les événements de la préparation avant de surveiller.
        std::thread::sleep(Duration::from_millis(500));
        let (watch, mut rx) = ProjectWatch::start(&dir).expect("start watch");
        fs::write(watch.root().join(".git/objects/x"), "blob").expect("write in .git");
        assert_eq!(next_batch(&mut rx, Duration::from_secs(1)), None);
        drop(watch);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_metadata_is_relevant_only_when_asked() {
        let root = Path::new("/p");
        let git = GitDirs::of_root(root);
        // Ce qui bouge quand un agent stage, commite ou change de branche.
        for name in [
            ".git/index",
            ".git/HEAD",
            ".git/refs/heads/main",
            ".git/packed-refs",
            ".git/MERGE_HEAD",
        ] {
            let path = root.join(name);
            assert!(is_relevant(&git, &path, true), "{name} with git");
            assert!(!is_relevant(&git, &path, false), "{name} without git");
        }
        // Le bruit : les objets, et les verrous (le renommage qui les libère émet, lui, le vrai nom).
        for name in [
            ".git/objects/ab/cd",
            ".git/index.lock",
            ".git/refs/heads/main.lock",
        ] {
            let path = root.join(name);
            assert!(!is_relevant(&git, &path, true), "{name} with git");
            assert!(!is_relevant(&git, &path, false), "{name} without git");
        }
        // Les fichiers de travail et `info/exclude` passent dans les deux modes.
        for with_git in [false, true] {
            assert!(is_relevant(&git, &root.join("src/a.rs"), with_git));
            assert!(is_relevant(&git, &root.join(".git/info/exclude"), with_git));
        }
    }

    #[test]
    fn worktree_git_metadata_is_relevant() {
        let dirs = GitDirs {
            git_dir: PathBuf::from("/r/.git/worktrees/demo"),
            common_dir: PathBuf::from("/r/.git"),
        };
        // L'index et `HEAD` du worktree, les branches du dépôt, et ses fichiers de travail.
        for name in [
            "/r/.git/worktrees/demo/index",
            "/r/.git/worktrees/demo/HEAD",
            "/r/.git/refs/heads/demo",
            "/r/.git/packed-refs",
            "/r/.worktrees/demo/b.md",
        ] {
            assert!(is_relevant(&dirs, Path::new(name), true), "{name} with git");
        }
        // Le bruit : l'index du dépôt principal et d'un autre worktree, un verrou, les objets.
        for name in [
            "/r/.git/index",
            "/r/.git/worktrees/other/index",
            "/r/.git/worktrees/demo/index.lock",
            "/r/.git/objects/ab/cd",
        ] {
            assert!(
                !is_relevant(&dirs, Path::new(name), true),
                "{name} with git"
            );
        }
        // Sans `with_git`, aucun chemin Git ne compte, sauf `info/exclude`.
        for name in [
            "/r/.git/worktrees/demo/index",
            "/r/.git/worktrees/demo/HEAD",
            "/r/.git/refs/heads/demo",
            "/r/.git/packed-refs",
        ] {
            assert!(
                !is_relevant(&dirs, Path::new(name), false),
                "{name} without git"
            );
        }
        assert!(is_relevant(&dirs, Path::new("/r/.git/info/exclude"), false));
        assert!(is_relevant(
            &dirs,
            Path::new("/r/.worktrees/demo/b.md"),
            false
        ));
    }

    #[test]
    fn a_linked_worktree_index_change_arrives() {
        use crate::changes::model;
        use crate::changes::test_repo::linked_worktree_repo;

        let repo = linked_worktree_repo();
        let demo = repo.path().join(".worktrees/demo");
        let snapshot = model::load(&demo).expect("load the worktree");
        let dirs = GitDirs {
            git_dir: snapshot.git_dir.clone(),
            common_dir: snapshot.common_dir.clone(),
        };
        // Laisse retomber les événements de la préparation avant de surveiller.
        std::thread::sleep(Duration::from_millis(500));
        let (watch, mut rx) = ProjectWatch::start_with_git(&demo, &dirs).expect("start watch");

        repo.git(&["-C", ".worktrees/demo", "add", "b.md"]);

        let git_dir = fs::canonicalize(&snapshot.git_dir).expect("canonicalize git dir");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut seen = Vec::new();
        while let Some(batch) =
            next_batch(&mut rx, deadline.saturating_duration_since(Instant::now()))
        {
            seen.extend(batch);
            if seen.iter().any(|path| path.starts_with(&git_dir)) {
                break;
            }
        }
        assert!(
            seen.iter().any(|path| path.starts_with(&git_dir)),
            "no path under the worktree git dir: {seen:?}"
        );
        drop(watch);
    }

    #[test]
    fn start_with_git_reports_the_index_but_not_the_objects() {
        let dir = temp_dir("with-git");
        fs::create_dir_all(dir.join(".git/objects")).expect("create .git");
        std::thread::sleep(Duration::from_millis(500));
        let (watch, mut rx) =
            ProjectWatch::start_with_git(&dir, &GitDirs::of_root(&dir)).expect("start watch");
        fs::write(watch.root().join(".git/objects/x"), "blob").expect("write object");
        fs::write(watch.root().join(".git/index"), "index").expect("write index");
        let index = watch.root().join(".git/index");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut seen = Vec::new();
        while let Some(batch) =
            next_batch(&mut rx, deadline.saturating_duration_since(Instant::now()))
        {
            seen.extend(batch);
            if seen.contains(&index) {
                break;
            }
        }
        assert!(seen.contains(&index), "no batch with .git/index: {seen:?}");
        assert!(
            !seen.contains(&watch.root().join(".git/objects/x")),
            "an object write reached the batch: {seen:?}"
        );
        drop(watch);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_info_exclude_is_not_ignored() {
        let dir = temp_dir("exclude");
        fs::create_dir_all(dir.join(".git/info")).expect("create .git/info");
        std::thread::sleep(Duration::from_millis(500));
        let (watch, mut rx) = ProjectWatch::start(&dir).expect("start watch");
        let exclude = watch.root().join(".git/info/exclude");
        fs::write(&exclude, "target/\n").expect("write exclude");
        assert!(
            wait_for_path(&mut rx, &exclude, Duration::from_secs(2)),
            "no batch with .git/info/exclude"
        );
        drop(watch);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn emitted_paths_start_with_the_canonical_root_even_through_a_symlink() {
        let real = temp_dir("symlink-real");
        let link =
            std::env::temp_dir().join(format!("idema-watch-{}-symlink-link", std::process::id()));
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).expect("create symlink");

        let (watch, mut rx) = ProjectWatch::start(&link).expect("start watch");
        let canonical = fs::canonicalize(&real).expect("canonicalize");
        assert_eq!(watch.root(), canonical);

        // On écrit par le lien : le chemin émis doit quand même être sous la racine canonique.
        fs::write(link.join("via-link.txt"), "x").expect("write through link");
        let wanted = canonical.join("via-link.txt");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut found = false;
        while let Some(batch) =
            next_batch(&mut rx, deadline.saturating_duration_since(Instant::now()))
        {
            for path in &batch {
                assert!(
                    path.starts_with(watch.root()),
                    "{path:?} is outside {:?}",
                    watch.root()
                );
            }
            found |= batch.contains(&wanted);
            if found {
                break;
            }
        }
        assert!(found, "no batch with via-link.txt");
        drop(watch);
        let _ = fs::remove_file(&link);
        let _ = fs::remove_dir_all(&real);
    }

    #[test]
    fn dropping_the_watch_ends_the_stream() {
        let dir = temp_dir("drop");
        let (watch, mut rx) = ProjectWatch::start(&dir).expect("start watch");
        drop(watch);
        // Flux clos : `next` rend `None` tout de suite, sans attendre le minuteur.
        let started = Instant::now();
        assert_eq!(next_batch(&mut rx, Duration::from_secs(2)), None);
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Lance la boucle de regroupement sur un canal brut que le test alimente à la main.
    fn spawn_batching(
        root: &Path,
    ) -> (
        mpsc::Sender<notify::Result<Event>>,
        UnboundedReceiver<WatchBatch>,
    ) {
        let (raw_tx, raw_rx) = mpsc::channel();
        let (batch_tx, batch_rx) = futures::channel::mpsc::unbounded();
        let root = root.to_path_buf();
        std::thread::spawn(move || {
            run_batching(&root, &GitDirs::of_root(&root), false, &raw_rx, &batch_tx)
        });
        (raw_tx, batch_rx)
    }

    fn created(path: &str) -> notify::Result<Event> {
        Ok(
            Event::new(EventKind::Create(notify::event::CreateKind::File))
                .add_path(PathBuf::from(path)),
        )
    }

    #[test]
    fn the_same_path_three_times_is_one_entry() {
        let (tx, mut rx) = spawn_batching(Path::new("/p"));
        for _ in 0..3 {
            tx.send(created("/p/a.txt")).unwrap();
        }
        assert_eq!(
            next_batch(&mut rx, Duration::from_secs(1)),
            Some(vec![PathBuf::from("/p/a.txt")])
        );
    }

    #[test]
    fn events_ten_milliseconds_apart_share_one_batch() {
        let (tx, mut rx) = spawn_batching(Path::new("/p"));
        for name in ["a", "b", "c"] {
            tx.send(created(&format!("/p/{name}"))).unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            next_batch(&mut rx, Duration::from_secs(1)),
            Some(vec!["/p/a".into(), "/p/b".into(), "/p/c".into()])
        );
        assert_eq!(next_batch(&mut rx, Duration::from_millis(300)), None);
    }

    #[test]
    fn a_gap_longer_than_the_window_makes_two_batches() {
        let (tx, mut rx) = spawn_batching(Path::new("/p"));
        tx.send(created("/p/a")).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        tx.send(created("/p/b")).unwrap();
        assert_eq!(
            next_batch(&mut rx, Duration::from_secs(1)),
            Some(vec![PathBuf::from("/p/a")])
        );
        assert_eq!(
            next_batch(&mut rx, Duration::from_secs(1)),
            Some(vec![PathBuf::from("/p/b")])
        );
    }

    #[test]
    fn an_access_event_produces_no_batch() {
        let (tx, mut rx) = spawn_batching(Path::new("/p"));
        let access = Event::new(EventKind::Access(notify::event::AccessKind::Any))
            .add_path(PathBuf::from("/p/a"));
        tx.send(Ok(access)).unwrap();
        assert_eq!(next_batch(&mut rx, Duration::from_millis(400)), None);
    }

    #[test]
    fn a_rescan_event_names_the_folder_to_reread_in_full() {
        let (tx, mut rx) = spawn_batching(Path::new("/p"));
        // Ce que FSEvents émet quand sa file déborde (`MUST_SCAN_SUBDIRS`) : le dossier, avec le drapeau `Rescan`.
        let overflow = Event::new(EventKind::Other)
            .set_flag(notify::event::Flag::Rescan)
            .add_path(PathBuf::from("/p/src"));
        tx.send(Ok(overflow)).unwrap();
        // Un événement ordinaire au même lot n'est pas pris pour un débordement.
        tx.send(created("/p/other.txt")).unwrap();
        // Le bruit de `.git/` ne l'est pas non plus.
        let git_overflow = Event::new(EventKind::Other)
            .set_flag(notify::event::Flag::Rescan)
            .add_path(PathBuf::from("/p/.git/objects"));
        tx.send(Ok(git_overflow)).unwrap();
        let batch = next_full_batch(&mut rx, Duration::from_secs(1)).expect("a batch");
        assert_eq!(batch.rescan, vec![PathBuf::from("/p/src")]);
        assert_eq!(
            batch.paths,
            vec![PathBuf::from("/p/src"), PathBuf::from("/p/other.txt")]
        );
    }

    #[test]
    fn a_watcher_error_is_skipped_and_the_loop_goes_on() {
        let (tx, mut rx) = spawn_batching(Path::new("/p"));
        tx.send(Err(notify::Error::generic("overflow"))).unwrap();
        assert_eq!(next_batch(&mut rx, Duration::from_millis(400)), None);
        tx.send(created("/p/after-error")).unwrap();
        assert_eq!(
            next_batch(&mut rx, Duration::from_secs(1)),
            Some(vec![PathBuf::from("/p/after-error")])
        );
    }
}
