//! Le HUD de rendu, le journal `~/.config/idema/logs/idema.log` et le vidage de grille.
//!
//! Le journal écrit dans un tampon : un appel ne coûte jamais un appel système, sauf `log_event`,
//! réservé aux lignes rares (démarrage, changement de mode, relecture). Le vidage régulier du tampon
//! tourne hors du fil d'interface, sur la minuterie de la vue. Rien ici ne touche le disque à la peinture.

use std::collections::VecDeque;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{Div, IntoElement, actions, div, prelude::*, px};

use crate::theme::{HUD_BACKGROUND, HUD_TEXT, color_alpha};
use idema_term::stats::LatencyStats;

actions!(hud, [ToggleHud, DumpGrid]);

/// Nombre d'échantillons gardés pour la durée d'image et pour la latence d'écho.
const SAMPLE_WINDOW: usize = 512;

/// Fenêtre de calcul des images par seconde.
const FPS_WINDOW: Duration = Duration::from_secs(2);

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// À appeler au tout début de `main` : c'est l'origine de la durée de démarrage.
pub fn mark_process_start() {
    PROCESS_START.get_or_init(Instant::now);
}

/// Temps écoulé depuis `mark_process_start`.
pub fn since_process_start() -> Duration {
    PROCESS_START.get().map_or(Duration::ZERO, Instant::elapsed)
}

/// Mesures du HUD : images par seconde, durée d'image, latence d'écho et démarrage.
pub struct HudStats {
    pub visible: bool,
    frames: LatencyStats,
    echo: LatencyStats,
    /// Les mêmes latences d'écho, en images d'affichage (latence divisée par l'intervalle de l'écran) :
    /// GPUI ne peint qu'aux tops de l'écran, et le critère de la spec parle d'images (§12).
    echo_frames: VecDeque<f64>,
    /// Intervalle entre deux rafraîchissements de l'écran de la fenêtre, quand GPUI le donne.
    refresh_interval: Option<Duration>,
    /// Dates des dernières peintures, pour compter celles de la dernière seconde.
    paints: VecDeque<Instant>,
    /// Démarrage jusqu'au premier paint, avant que le contenu du pane ne soit là.
    startup: Option<Duration>,
    /// Démarrage jusqu'au premier paint qui montre le contenu : celui que le HUD affiche.
    content_startup: Option<Duration>,
}

impl HudStats {
    pub fn new() -> Self {
        Self {
            visible: false,
            frames: LatencyStats::new(SAMPLE_WINDOW),
            echo: LatencyStats::new(SAMPLE_WINDOW),
            echo_frames: VecDeque::new(),
            refresh_interval: None,
            paints: VecDeque::new(),
            startup: None,
            content_startup: None,
        }
    }

    pub fn record_frame(&mut self, now: Instant, duration: Duration) {
        self.frames.record(duration);
        self.paints.push_back(now);
        while self
            .paints
            .front()
            .is_some_and(|painted| now.duration_since(*painted) > FPS_WINDOW)
        {
            self.paints.pop_front();
        }
    }

    /// Latence d'écho : de l'arrivée de la sortie du pane sur le fil du client tmux à la fin de la peinture qui la
    /// montre. Elle ne compte pas la présentation GPU (le moment où l'image atteint l'écran), que GPUI ne donne pas.
    pub fn record_echo(&mut self, latency: Duration) {
        self.echo.record(latency);
        if let Some(interval) = self.refresh_interval {
            if self.echo_frames.len() == SAMPLE_WINDOW {
                self.echo_frames.pop_front();
            }
            self.echo_frames
                .push_back(latency.as_secs_f64() / interval.as_secs_f64());
        }
    }

    /// Retient l'intervalle d'affichage de l'écran de la fenêtre. Une valeur nulle ou absente l'efface.
    pub fn set_refresh_interval(&mut self, interval: Option<Duration>) {
        self.refresh_interval = interval.filter(|value| !value.is_zero());
    }

    /// Retient la durée de démarrage au premier appel seulement ; vrai si c'était le premier.
    pub fn record_startup(&mut self, startup: Duration) -> bool {
        if self.startup.is_some() {
            return false;
        }
        self.startup = Some(startup);
        true
    }

    /// Retient le démarrage du premier paint avec contenu, au premier appel seulement ; vrai si c'était le premier.
    pub fn record_content_startup(&mut self, startup: Duration) -> bool {
        if self.content_startup.is_some() {
            return false;
        }
        self.content_startup = Some(startup);
        true
    }

    /// Rythme des peintures sur les deux dernières secondes : une fenêtre d'une seconde afficherait 0 ou 1 au
    /// hasard au repos, où la vue ne repeint qu'une fois par seconde.
    fn fps(&self, now: Instant) -> usize {
        let recent = self
            .paints
            .iter()
            .filter(|painted| now.saturating_duration_since(**painted) <= FPS_WINDOW)
            .count();
        (recent as f64 / FPS_WINDOW.as_secs_f64()).round() as usize
    }

    /// Centile (rang supérieur) des latences d'écho en images ; `None` sans mesure en images.
    fn echo_frames_percentile(&self, p: f64) -> Option<f64> {
        if self.echo_frames.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = self.echo_frames.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
        Some(sorted[rank.clamp(1, sorted.len()) - 1])
    }

    /// Le texte du HUD (UI en français).
    pub fn hud_text(&self, now: Instant) -> String {
        format!(
            "{} Hz · {} ms/image · écho p50 {} / p95 {} ms · démarrage {} ms",
            self.fps(now),
            millis(self.frames.percentile(50.0)),
            millis(self.echo.percentile(50.0)),
            millis(self.echo.percentile(95.0)),
            whole_millis(self.content_startup),
        )
    }

    /// La ligne de mesure du journal, lisible par une machine.
    pub fn log_line(&self, now: Instant) -> String {
        format!(
            "stats fps={} frame_p50_ms={} frame_p95_ms={} echo_p50_ms={} echo_p95_ms={} echo_samples={} refresh_hz={} echo_p50_frames={} echo_p95_frames={} startup_ms={} content_startup_ms={}",
            self.fps(now),
            millis(self.frames.percentile(50.0)),
            millis(self.frames.percentile(95.0)),
            millis(self.echo.percentile(50.0)),
            millis(self.echo.percentile(95.0)),
            self.echo.len(),
            self.refresh_interval.map_or_else(
                || "-".to_owned(),
                |interval| format!("{:.0}", 1.0 / interval.as_secs_f64())
            ),
            frames(self.echo_frames_percentile(50.0)),
            frames(self.echo_frames_percentile(95.0)),
            whole_millis(self.startup),
            whole_millis(self.content_startup),
        )
    }
}

/// Des millisecondes entières, ou un tiret tant que la mesure manque.
fn whole_millis(sample: Option<Duration>) -> String {
    sample.map_or_else(
        || "-".to_owned(),
        |duration| duration.as_millis().to_string(),
    )
}

/// Des images à deux décimales, ou un tiret sans mesure.
fn frames(sample: Option<f64>) -> String {
    sample.map_or_else(|| "-".to_owned(), |value| format!("{value:.2}"))
}

/// Des millisecondes à une décimale, ou un tiret sans mesure.
fn millis(sample: Option<Duration>) -> String {
    sample.map_or_else(
        || "-".to_owned(),
        |duration| format!("{:.1}", duration.as_secs_f64() * 1000.),
    )
}

/// SF Mono quand la machine le fournit (il n'est pas installé d'office), sinon Menlo : GPUI retombe sinon
/// sur une police proportionnelle, et les chiffres du HUD danseraient d'une image à l'autre.
pub fn hud_font(installed: &[String]) -> &'static str {
    if installed.iter().any(|name| name == "SF Mono") {
        "SF Mono"
    } else {
        "Menlo"
    }
}

/// Le calque du HUD : en haut à droite, texte blanc à 55 % sur fond noir à 60 %.
pub fn hud_layer(text: String, font_family: &'static str) -> impl IntoElement {
    let layer: Div = div()
        .absolute()
        .top(px(8.))
        .right(px(8.))
        .px(px(8.))
        .py(px(4.))
        .rounded(px(6.))
        .bg(color_alpha(HUD_BACKGROUND))
        .text_color(color_alpha(HUD_TEXT))
        .font_family(font_family)
        .text_size(px(11.));
    layer.child(text)
}

/// Mémorise les modes du programme du pane pour n'en journaliser que les changements (mesure R1).
#[derive(Default)]
pub struct ModeWatch {
    bracketed_paste: bool,
    extended_keys: bool,
}

impl ModeWatch {
    /// Les lignes de journal des modes qui viennent de changer.
    pub fn changes(&mut self, bracketed_paste: bool, extended_keys: bool) -> Vec<String> {
        let mut lines = Vec::new();
        if bracketed_paste != self.bracketed_paste {
            lines.push(format!("mode 2004 {}", on_off(bracketed_paste)));
        }
        if extended_keys != self.extended_keys {
            lines.push(format!("mode extended_keys {}", on_off(extended_keys)));
        }
        self.bracketed_paste = bracketed_paste;
        self.extended_keys = extended_keys;
        lines
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

struct Journal {
    dir: PathBuf,
    debug: bool,
    writer: Mutex<BufWriter<File>>,
    /// Levé à la première écriture ratée : l'erreur est signalée une fois, puis le journal se tait.
    failed: AtomicBool,
}

impl Journal {
    fn report_failure(&self, error: &io::Error) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            eprintln!("journal write failed: {error}");
        }
    }
}

static JOURNAL: OnceLock<Journal> = OnceLock::new();

/// Ouvre `<dir>/logs/idema.log` en ajout. `IDEMA_LOG=debug` ajoute une ligne par `%output`.
pub fn init_journal(dir: &Path) -> io::Result<()> {
    let logs = dir.join("logs");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&logs)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(logs.join("idema.log"))?;
    let debug = std::env::var("IDEMA_LOG").is_ok_and(|level| level == "debug");
    let journal = Journal {
        dir: dir.to_owned(),
        debug,
        writer: Mutex::new(BufWriter::new(file)),
        failed: AtomicBool::new(false),
    };
    // Une seconde initialisation garde la première : un seul journal par processus.
    if JOURNAL.set(journal).is_ok() {
        // Marque le début de ce lancement : la porte découpe ses fenêtres de mesure entre deux `start`.
        log_event(&format!("start pid={}", std::process::id()));
    }
    Ok(())
}

pub fn debug_enabled() -> bool {
    JOURNAL.get().is_some_and(|journal| journal.debug)
}

/// Écrit une ligne horodatée dans le tampon, sans appel système tant que le tampon n'est pas plein.
pub fn log_line(message: &str) {
    write_line(message, false);
}

/// Écrit une ligne et vide le tampon : pour les événements rares qu'on veut lire tout de suite.
pub fn log_event(message: &str) {
    write_line(message, true);
}

/// Une ligne par `%output`, seulement au niveau `IDEMA_LOG=debug`.
pub fn log_debug(message: &str) {
    if debug_enabled() {
        write_line(message, false);
    }
}

/// Une étape de l'amorçage, au niveau `IDEMA_LOG=debug` : instant de fin depuis le départ du process et durée.
/// Elle sert à mesurer avant d'optimiser (P-1) : « startup step=<nom> at_ms=<n> took_ms=<n> ».
pub fn log_step(name: &str, started: Instant) {
    log_debug(&format!(
        "startup step={name} at_ms={} took_ms={}",
        since_process_start().as_millis(),
        started.elapsed().as_millis()
    ));
}

/// Vide le tampon sur disque ; la vue l'appelle hors du fil d'interface.
pub fn flush() {
    if let Some(journal) = JOURNAL.get()
        && !journal.failed.load(Ordering::Relaxed)
        && let Ok(mut writer) = journal.writer.lock()
        && let Err(error) = writer.flush()
    {
        journal.report_failure(&error);
    }
}

fn write_line(message: &str, flush: bool) {
    let Some(journal) = JOURNAL.get() else {
        return;
    };
    if journal.failed.load(Ordering::Relaxed) {
        return;
    }
    let stamp = format_timestamp(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default(),
    );
    // Le journal est un auxiliaire : une écriture qui échoue est signalée une fois, sans gêner le terminal.
    if let Ok(mut writer) = journal.writer.lock() {
        let result = writeln!(writer, "{stamp} {message}")
            .and_then(|()| if flush { writer.flush() } else { Ok(()) });
        if let Err(error) = result {
            journal.report_failure(&error);
        }
    }
}

/// Écrit le vidage de grille dans `<dir>/debug/grid-<pane>.txt` et rend son chemin.
pub fn write_grid_dump(pane: &str, text: &str) -> io::Result<PathBuf> {
    let journal = JOURNAL
        .get()
        .ok_or_else(|| io::Error::other("the journal is not initialised"))?;
    let debug = journal.dir.join("debug");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&debug)?;
    let path = debug.join(format!("grid-{pane}.txt"));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(text.as_bytes())?;
    Ok(path)
}

/// `2026-10-14T09:30:05.123Z` : UTC, à la milliseconde, triable en texte.
fn format_timestamp(since_epoch: Duration) -> String {
    let seconds = since_epoch.as_secs();
    let (hour, minute, second) = (
        (seconds % 86_400) / 3600,
        (seconds % 3600) / 60,
        seconds % 60,
    );
    // Jour civil depuis le nombre de jours écoulés (algorithme de Howard Hinnant).
    let z = (seconds / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        since_epoch.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_formats_known_instants() {
        assert_eq!(format_timestamp(Duration::ZERO), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_timestamp(Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20.000Z"
        );
        // Le 29 février d'une année bissextile séculaire.
        assert_eq!(
            format_timestamp(Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00.000Z"
        );
        assert_eq!(
            format_timestamp(Duration::from_millis(1_500)),
            "1970-01-01T00:00:01.500Z"
        );
    }

    #[test]
    fn hud_font_falls_back_to_a_monospaced_system_font() {
        assert_eq!(
            hud_font(&["Menlo".to_owned(), "SF Mono".to_owned()]),
            "SF Mono"
        );
        assert_eq!(hud_font(&["Menlo".to_owned()]), "Menlo");
    }

    #[test]
    fn mode_watch_reports_only_changes() {
        let mut watch = ModeWatch::default();
        assert!(watch.changes(false, false).is_empty());
        assert_eq!(watch.changes(true, false), ["mode 2004 on"]);
        assert!(watch.changes(true, false).is_empty());
        assert_eq!(
            watch.changes(false, true),
            ["mode 2004 off", "mode extended_keys on"]
        );
    }

    #[test]
    fn fps_is_the_paint_rate_over_the_window() {
        let start = Instant::now();
        let mut stats = HudStats::new();
        // Dix peintures par seconde pendant deux secondes.
        for tick in 0..20 {
            stats.record_frame(
                start + Duration::from_millis(tick * 100),
                Duration::from_millis(2),
            );
        }
        assert_eq!(stats.fps(start + Duration::from_millis(1_900)), 10);
        // Une fois les peintures sorties de la fenêtre, le rythme retombe à zéro.
        assert_eq!(stats.fps(start + Duration::from_millis(4_500)), 0);
    }

    #[test]
    fn hud_text_shows_dashes_without_samples_then_values() {
        let now = Instant::now();
        let mut stats = HudStats::new();
        assert!(stats.hud_text(now).contains("écho p50 - / p95 - ms"));
        stats.record_echo(Duration::from_millis(12));
        assert!(stats.record_startup(Duration::from_millis(480)));
        assert!(!stats.record_startup(Duration::from_millis(9_000)));
        // Avant le premier paint avec contenu, le HUD n'affiche pas le paint à vide comme un démarrage.
        assert!(stats.hud_text(now).ends_with("démarrage - ms"));
        assert!(stats.record_content_startup(Duration::from_millis(650)));
        assert!(!stats.record_content_startup(Duration::from_millis(9_000)));
        let text = stats.hud_text(now);
        assert!(text.contains("écho p50 12.0 / p95 12.0 ms"), "{text}");
        assert!(text.ends_with("démarrage 650 ms"), "{text}");
        let line = stats.log_line(now);
        assert!(line.contains("echo_samples=1"), "{line}");
        assert!(
            line.ends_with("startup_ms=480 content_startup_ms=650"),
            "{line}"
        );
    }
    #[test]
    fn echo_is_also_read_in_display_frames() {
        let now = Instant::now();
        let mut stats = HudStats::new();
        // Sans intervalle d'affichage connu, la lecture en images manque : un tiret, pas un zéro.
        stats.record_echo(Duration::from_millis(5));
        assert!(stats.log_line(now).contains("echo_p95_frames=-"));
        // Écran à 100 Hz : une image dure 10 ms.
        stats.set_refresh_interval(Some(Duration::from_millis(10)));
        for ms in [5, 8, 12] {
            stats.record_echo(Duration::from_millis(ms));
        }
        let line = stats.log_line(now);
        assert!(line.contains("refresh_hz=100"), "{line}");
        // Trois mesures en images (0,5, 0,8 et 1,2) : la première, faite sans intervalle connu, n'en fait pas partie.
        assert!(line.contains("echo_p95_frames=1.20"), "{line}");
        assert!(line.contains("echo_p50_frames=0.80"), "{line}");
    }
}
