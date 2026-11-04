//! Un pane relié à tmux : flux, relectures, saisie et taille. Sans GPUI : testable seul. Les panes d'une session
//! partagent son client de contrôle ; la taille demandée à tmux (`refresh-client -C`) est l'affaire de la session,
//! le pane suit la taille que le layout de tmux lui donne (`fit`).

use std::time::Instant;

use alacritty_terminal::term::TermMode;

use crate::emulator::{
    GridSize, Link, PaneEmulator, PathMatch, SearchMatch, SelectionKind, Source, ViewPoint,
};
use idema_tmux::command::send_keys_hex;
use idema_tmux::protocol::{ControlEvent, PaneId};
use idema_tmux::sync::{
    PaneLink, PaneState, Snapshot, SyncKind, SyncProgress, SyncRequest, repaint_bytes, replay_bytes,
};

/// Historique de l'émulateur : celui de tmux (`history-limit`).
pub const EMULATOR_HISTORY: usize = 10_000;

/// Reçoit les lignes de commandes destinées à tmux : le client de contrôle, ou un faux en test.
pub trait CommandSink {
    fn send(&mut self, line: &str);
}

/// Préfixe du nom de l'abonnement à la taille réelle d'un pane.
const SIZE_WATCH_PREFIX: &str = "idema-size-";

/// Le pane dont `name` est l'abonnement à la taille (`%subscription-changed`) : la session s'en sert pour router
/// l'événement vers le bon pane.
pub fn size_watch_pane(name: &str) -> Option<PaneId> {
    Some(PaneId(name.strip_prefix(SIZE_WATCH_PREFIX)?.parse().ok()?))
}

/// Ce que l'interface doit faire après un événement.
#[derive(Debug)]
pub enum Effect {
    Nothing,
    Redraw,
    /// tmux a changé la taille du pane sans que la fenêtre ait été redimensionnée (bordure posée ou retirée,
    /// autre client) : l'interface lance `resync_visible` une fois la taille stable, avec le même minuteur
    /// que pour un redimensionnement.
    ResyncVisible,
    /// Construire un émulateur neuf hors du fil d'interface (`build_emulator`),
    /// puis le rendre par `install` avec la même génération.
    Build {
        generation: u64,
        snapshot: Box<Snapshot>,
    },
    Closed,
}

pub struct PaneSession<S: CommandSink> {
    sink: S,
    pane: PaneId,
    /// Vrai une fois l'abonnement à la taille du pane envoyé (après la première installation).
    size_subscribed: bool,
    /// La dernière taille que le layout a donnée au pane (`fit`). L'émulateur peut en différer : il prend celle que
    /// tmux rapporte à la relecture.
    fitted: GridSize,
    link: PaneLink,
    emulator: PaneEmulator,
    next_token: u64,
    /// Nature de la dernière relecture lancée, pour la relancer à l'identique si elle échoue.
    sync_kind: SyncKind,
    /// Vrai une fois la relance consommée : une seconde panne abandonne au lieu de boucler.
    retried: bool,
    generation: u64,
    /// Sortie arrivée pendant la construction d'un émulateur neuf, rejouée à l'installation.
    held: Option<Vec<Vec<u8>>>,
    /// Arrivée de la dernière sortie appliquée, pour la latence d'écho du HUD.
    pub last_output_at: Option<Instant>,
}

impl<S: CommandSink> PaneSession<S> {
    /// Relie le pane à son client et lance sa relecture complète. `size` est la taille que le layout donne au
    /// pane : l'émulateur d'attente la prend, celui de la relecture prendra celle que tmux rapporte.
    pub fn attach(sink: S, pane: PaneId, size: GridSize) -> Self {
        let mut session = Self {
            sink,
            pane,
            size_subscribed: false,
            fitted: size,
            link: PaneLink::default(),
            emulator: PaneEmulator::new(size, EMULATOR_HISTORY, Source::TmuxPane),
            next_token: 0,
            sync_kind: SyncKind::Full,
            retried: false,
            generation: 0,
            held: None,
            last_output_at: None,
        };
        session.start_sync(SyncKind::Full);
        session
    }

    /// Un nouveau client de contrôle pour la session (reconnexion) : la relecture en vol est abandonnée, une
    /// complète repart sur le nouveau client, et l'abonnement à la taille sera repris à l'installation.
    /// L'émulateur courant reste affiché jusque-là.
    pub fn rebind(&mut self, sink: S) {
        self.sink = sink;
        self.link.abort_sync();
        self.size_subscribed = false;
        self.start_sync(SyncKind::Full);
    }

    pub fn emulator(&self) -> &PaneEmulator {
        &self.emulator
    }

    pub fn pane(&self) -> PaneId {
        self.pane
    }

    /// Fait défiler la vue dans l'historique de l'émulateur (positif : vers le haut).
    pub fn scroll_history(&mut self, lines: i32) {
        self.emulator.scroll_history(lines);
    }

    /// Ramène la vue en bas, sur l'écran courant (à la frappe, par exemple).
    pub fn scroll_to_bottom(&mut self) {
        self.emulator.scroll_to_bottom();
    }

    /// Commence une sélection à la cellule `at` de la vue.
    pub fn start_selection(&mut self, at: ViewPoint, kind: SelectionKind) {
        self.emulator.start_selection(at, kind);
    }

    /// Étend la sélection en cours jusqu'à la cellule `at`.
    pub fn extend_selection(&mut self, at: ViewPoint) {
        self.emulator.extend_selection(at);
    }

    pub fn clear_selection(&mut self) {
        self.emulator.clear_selection();
    }

    /// Sélectionne tout l'historique et l'écran.
    pub fn select_all(&mut self) {
        self.emulator.select_all();
    }

    /// Le lien sous la cellule `at` de la vue (OSC 8, sinon une URL du texte).
    pub fn link_at(&mut self, at: ViewPoint) -> Option<Link> {
        self.emulator.link_at(at)
    }

    /// Le chemin de fichier sous la cellule `at` de la vue (sans lien ni URL au même endroit).
    pub fn path_at(&mut self, at: ViewPoint) -> Option<PathMatch> {
        self.emulator.path_at(at)
    }

    /// Toutes les occurrences de `query` dans l'historique et l'écran de l'émulateur (recherche ⌘F).
    pub fn search(&mut self, query: &str) -> Vec<SearchMatch> {
        self.emulator.search(query)
    }

    /// Une recherche est ouverte ou fermée : ses coordonnées restent exactes quand la sortie défile un historique plein.
    pub fn track_scroll(&mut self, on: bool) {
        self.emulator.track_scroll(on);
    }

    /// Les occurrences de la partie affichée, cherchées à chaque appel.
    pub fn visible_matches(&mut self, query: &str) -> Vec<(ViewPoint, ViewPoint)> {
        self.emulator.visible_matches(query)
    }

    /// Les cellules de la vue qu'occupe une occurrence trouvée, si elle y est.
    pub fn match_in_view(&self, found: &SearchMatch) -> Option<(ViewPoint, ViewPoint)> {
        self.emulator.match_in_view(found)
    }

    /// Fait défiler la vue jusqu'à l'occurrence.
    pub fn scroll_to_match(&mut self, found: &SearchMatch) {
        self.emulator.scroll_to_match(found);
    }

    /// La dernière écriture OSC 52 du programme, à mettre dans le presse-papier de macOS.
    pub fn take_clipboard_store(&mut self) -> Option<String> {
        self.emulator.take_clipboard_store()
    }

    pub fn handle(&mut self, event: ControlEvent) -> Effect {
        self.handle_at(event, Instant::now())
    }

    /// Comme `handle`, avec l'instant où l'événement est arrivé du client tmux (et non celui où l'interface le traite).
    pub fn handle_at(&mut self, event: ControlEvent, arrival: Instant) -> Effect {
        match event {
            ControlEvent::Output { pane, data } if pane == self.pane => {
                let Some(bytes) = self.link.on_output(&data) else {
                    return Effect::Nothing;
                };
                if let Some(held) = self.held.as_mut() {
                    held.push(bytes.to_vec());
                    return Effect::Nothing;
                }
                self.emulator.feed(bytes);
                // La plus ancienne sortie pas encore peinte : une rafale ne repousse pas la mesure.
                self.last_output_at.get_or_insert(arrival);
                Effect::Redraw
            }
            // tmux abandonne le reste de la ligne de commandes sur une erreur : le marqueur de fin
            // ne viendrait jamais. On lève la relecture au lieu d'attendre.
            ControlEvent::Reply {
                ok: false, lines, ..
            } => {
                if self.link.on_failed_reply(&lines) {
                    self.recover_failed_sync();
                }
                Effect::Nothing
            }
            ControlEvent::Reply { lines, .. } => match self.link.on_reply_progress(&lines) {
                SyncProgress::Done(snapshot) => self.apply(snapshot),
                SyncProgress::Failed => {
                    self.recover_failed_sync();
                    Effect::Nothing
                }
                SyncProgress::Pending => Effect::Nothing,
            },
            ControlEvent::Pause { pane } if pane == self.pane => {
                // tmux a jeté la sortie de la pause : on reprend le flux, puis on relit tout.
                self.sink
                    .send(&format!("refresh-client -A '%{}:continue'", pane.0));
                self.start_sync(SyncKind::Full);
                Effect::Nothing
            }
            // tmux redimensionne un pane de lui-même sans que la taille demandée bouge, donc sans que `resize`
            // relise quoi que ce soit. `%layout-change` ne le dit pas (bordure posée ou retirée : aucun événement,
            // mesuré sur tmux 3.8) ; un abonnement à `pane_width x pane_height` le dit, une fois par changement.
            // Une valeur égale à la taille de l'émulateur ne demande rien : c'est aussi la valeur initiale.
            ControlEvent::SubscriptionChanged { name, value }
                if name == self.size_subscription() =>
            {
                match parse_pane_size(&value) {
                    Some(size) if size != self.emulator.size() => Effect::ResyncVisible,
                    _ => Effect::Nothing,
                }
            }
            ControlEvent::Exit { .. } => Effect::Closed,
            _ => Effect::Nothing,
        }
    }

    /// Installe l'émulateur construit pour `generation`, puis rejoue la sortie tenue en attente.
    /// Une génération dépassée (une autre relecture a suivi) est ignorée.
    pub fn install(&mut self, generation: u64, emulator: PaneEmulator) -> Effect {
        if generation != self.generation {
            return Effect::Nothing;
        }
        // L'émulateur prend la taille de l'instantané : celle que tmux donne au pane à cet instant.
        self.emulator = emulator;
        self.subscribe_to_pane_size();
        for bytes in self.held.take().unwrap_or_default() {
            self.emulator.feed(&bytes);
        }
        Effect::Redraw
    }

    fn size_subscription(&self) -> String {
        format!("{SIZE_WATCH_PREFIX}{}", self.pane.0)
    }

    /// Abonne le client à la taille réelle du pane. Après la première installation seulement : la valeur initiale
    /// arrive alors égale à celle de l'émulateur, et l'attache ne coûte pas une seconde relecture.
    fn subscribe_to_pane_size(&mut self) {
        if self.size_subscribed {
            return;
        }
        self.size_subscribed = true;
        let line = format!(
            "refresh-client -B '{}:%{}:#{{pane_width}}x#{{pane_height}}'",
            self.size_subscription(),
            self.pane.0
        );
        self.sink.send(&line);
    }

    /// Retire l'abonnement à la taille réelle du pane (le nom seul, sans format) : le pane quitte le suivi.
    pub fn unsubscribe_size(&mut self) {
        let line = format!("refresh-client -B {}", self.size_subscription());
        self.sink.send(&line);
    }

    /// Envoie une ligne de commandes telle quelle au client de contrôle (abonnements `PaneWatch`).
    pub fn send_command(&mut self, line: &str) {
        self.sink.send(line);
    }

    pub fn send_bytes(&mut self, bytes: &[u8]) {
        for line in send_keys_hex(self.pane, bytes) {
            self.sink.send(&line);
        }
    }

    /// La taille que le layout de tmux donne au pane. Vrai si elle a changé depuis le dernier appel : l'émulateur
    /// la prend, et l'interface appelle `resync_visible` quand la taille est stable, pour effacer l'écart entre le
    /// reflow de tmux et le sien. La comparaison porte sur la taille donnée, pas sur celle de l'émulateur : tmux rend
    /// parfois un pane plus petit que sa cellule du layout (bordure `pane-border-status`, sans `%layout-change`), et
    /// l'émulateur garde alors la taille relue ; comparer à l'émulateur relancerait une relecture à chaque appel
    /// (boucle du jalon 1).
    pub fn fit(&mut self, size: GridSize) -> bool {
        if size == self.fitted {
            return false;
        }
        self.fitted = size;
        self.emulator.resize(size);
        true
    }

    /// Recapture l'écran visible, une fois la taille stable (spec §6.2).
    pub fn resync_visible(&mut self) {
        let kind = self.safe_kind(SyncKind::Visible);
        self.start_sync(kind);
    }

    /// Exploite une relecture réussie : construction hors fil pour la complète, repeinte sur place pour la visible.
    fn apply(&mut self, snapshot: Snapshot) -> Effect {
        if snapshot.kind == SyncKind::Full {
            self.generation += 1;
            self.held = Some(Vec::new());
            return Effect::Build {
                generation: self.generation,
                snapshot: Box::new(snapshot),
            };
        }
        // La capture date d'avant un redimensionnement : ses rangées ne sont plus celles de l'émulateur.
        // Une relance, puis la complète, qui adopte la taille de tmux si elle ne concorde jamais.
        let size = self.emulator.size();
        let state = &snapshot.state;
        if usize::from(state.width) != size.columns || usize::from(state.height) != size.lines {
            if self.retried {
                self.start_sync(SyncKind::Full);
            } else {
                self.retried = true;
                self.sync(SyncKind::Visible);
            }
            return Effect::Nothing;
        }
        // Une repeinte ne fait qu'allumer des modes et ne change jamais d'écran : si le programme a quitté
        // l'écran alterné ou éteint un mode pendant la fenêtre où la sortie est jetée, seule la complète répare.
        if !modes_match(self.emulator.mode(), state) {
            self.start_sync(SyncKind::Full);
            return Effect::Nothing;
        }
        self.emulator.feed(&repaint_bytes(&snapshot));
        self.emulator
            .set_extended_keys_level(extended_level(&snapshot.state.key_mode));
        Effect::Redraw
    }

    /// Une relecture visible ne passe jamais par-dessus une complète : sa repeinte irait dans l'ancien
    /// émulateur, que `install` remplacerait par un instantané plus vieux, et la complète encore en vol
    /// (attache) perdrait son historique. Tant qu'une complète est en vol ou en construction, on en relance une.
    /// La complète suivante bumpe `generation` : l'`install` périmé est alors ignoré.
    fn safe_kind(&self, wanted: SyncKind) -> SyncKind {
        let full_in_flight =
            self.held.is_some() || (self.link.is_syncing() && self.sync_kind == SyncKind::Full);
        if wanted == SyncKind::Visible && full_in_flight {
            SyncKind::Full
        } else {
            wanted
        }
    }

    /// Relecture demandée par l'interface ou par tmux (attache, pause, taille stable) : budget de relance neuf.
    fn start_sync(&mut self, kind: SyncKind) {
        self.retried = false;
        self.sync(kind);
    }

    /// Une relecture a échoué et la sortie repasse. Une seule relance, de même nature ;
    /// si elle échoue aussi, on abandonne jusqu'à la prochaine relecture demandée.
    fn recover_failed_sync(&mut self) {
        if self.retried {
            eprintln!("resync failed again for pane %{}, giving up", self.pane.0);
            return;
        }
        eprintln!("resync failed for pane %{}, retrying", self.pane.0);
        self.retried = true;
        // Relance d'une visible pendant qu'une construction complète est en vol : même règle que `resync_visible`.
        self.sync(self.safe_kind(self.sync_kind));
    }

    fn sync(&mut self, kind: SyncKind) {
        self.sync_kind = kind;
        self.next_token += 1;
        let token = sync_token(self.pane, self.next_token);
        let line = self
            .link
            .start_sync(SyncRequest::new(self.pane, kind, &token));
        self.sink.send(&line);
    }
}

/// Le jeton des marqueurs d'une relecture. Le client de contrôle remet chaque réponse à tous les panes de la
/// session et chacun reconnaît la sienne à ce jeton : il porte donc le pane, sans quoi deux panes dont les
/// compteurs sont égaux (attachés ensemble) prendraient la relecture l'un de l'autre.
fn sync_token(pane: PaneId, counter: u64) -> String {
    format!("{}-{}-{}", std::process::id(), pane.0, counter)
}

impl CommandSink for idema_tmux::client::ClientWriter {
    fn send(&mut self, line: &str) {
        // Une écriture qui échoue veut dire que le client est mort : son fil de lecture
        // le signale déjà par `Exit`, qui ferme la session. Rien de plus à faire ici.
        let _ = idema_tmux::client::ClientWriter::send(self, line);
    }
}

/// Construit l'émulateur d'une relecture complète. Pur : à lancer hors du fil d'interface.
pub fn build_emulator(snapshot: &Snapshot) -> PaneEmulator {
    let size = GridSize {
        columns: usize::from(snapshot.state.width.max(1)),
        lines: usize::from(snapshot.state.height.max(1)),
    };
    let mut emulator = PaneEmulator::new(size, EMULATOR_HISTORY, Source::TmuxPane);
    emulator.feed(&replay_bytes(snapshot));
    emulator.set_extended_keys_level(extended_level(&snapshot.state.key_mode));
    emulator
}

/// Lit « 80x23 », valeur de l'abonnement à la taille du pane.
fn parse_pane_size(value: &str) -> Option<GridSize> {
    let (columns, lines) = value.trim().split_once('x')?;
    Some(GridSize {
        columns: columns.parse().ok()?,
        lines: lines.parse().ok()?,
    })
}

/// L'écran et les modes de l'émulateur sont ceux que tmux rapporte (ceux que `repaint_bytes` rétablit).
fn modes_match(mode: TermMode, state: &PaneState) -> bool {
    [
        (TermMode::ALT_SCREEN, state.alternate_on),
        (TermMode::SHOW_CURSOR, state.cursor_visible),
        (TermMode::APP_CURSOR, state.app_cursor),
        (TermMode::APP_KEYPAD, state.app_keypad),
        (TermMode::BRACKETED_PASTE, state.bracketed_paste),
        (TermMode::MOUSE_REPORT_CLICK, state.mouse_standard),
        (TermMode::MOUSE_DRAG, state.mouse_button),
        (TermMode::MOUSE_MOTION, state.mouse_all),
        (TermMode::SGR_MOUSE, state.mouse_sgr),
    ]
    .iter()
    .all(|&(flag, expected)| mode.contains(flag) == expected)
}

/// `pane_key_mode` vaut « Ext 1 » ou « Ext 2 » quand le programme a demandé les touches étendues.
/// Valeurs à confirmer sur tmux 3.8 (tâche 5).
fn extended_level(key_mode: &str) -> u16 {
    key_mode
        .strip_prefix("Ext ")
        .and_then(|level| level.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

    #[derive(Default)]
    struct FakeSink {
        lines: Vec<String>,
    }

    impl CommandSink for &mut FakeSink {
        fn send(&mut self, line: &str) {
            self.lines.push(line.to_string());
        }
    }

    const SIZE: GridSize = GridSize {
        columns: 20,
        lines: 5,
    };

    /// Réponse complète à la dernière relecture de la session : l'état, puis les lignes visibles.
    fn sync_reply<S: CommandSink>(
        session: &PaneSession<S>,
        state: &str,
        visible: &[&str],
    ) -> ControlEvent {
        let prefix = format!(
            "idema-sync-{}-",
            sync_token(session.pane, session.next_token)
        );
        reply_with(&prefix, state, visible)
    }

    fn output(pane: u32, text: &str) -> ControlEvent {
        ControlEvent::Output {
            pane: PaneId(pane),
            data: text.as_bytes().to_vec(),
        }
    }

    #[test]
    fn attach_starts_a_full_resync_and_never_sizes_the_client() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
            assert_eq!(session.emulator().size(), SIZE);
            session.fit(GridSize {
                columns: 30,
                lines: 8,
            });
        }
        assert_eq!(sink.lines.len(), 1);
        assert!(sink.lines[0].contains("capture-pane -p -e -J -N -S -2000 -E -1 -t %3"));
        assert!(
            !sink
                .lines
                .iter()
                .any(|line| line.starts_with("refresh-client -C"))
        );
    }

    #[test]
    fn full_resync_builds_off_thread_and_replays_held_output_in_order() {
        let mut sink = FakeSink::default();
        let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
        assert!(matches!(
            session.handle(output(3, "dropped")),
            Effect::Nothing
        ));
        let reply = sync_reply(
            &session,
            "0 1 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
            &["$ ls"],
        );
        let Effect::Build {
            generation,
            snapshot,
        } = session.handle(reply)
        else {
            panic!("an emulator build was expected");
        };
        assert!(matches!(session.handle(output(3, "held")), Effect::Nothing));
        assert!(matches!(
            session.install(generation, build_emulator(&snapshot)),
            Effect::Redraw
        ));
        assert!(session.emulator().dump_text().starts_with("$ ls\nheld"));
        assert!(matches!(session.handle(output(3, "!")), Effect::Redraw));
        assert!(session.emulator().dump_text().starts_with("$ ls\nheld!"));
    }

    #[test]
    fn a_plain_first_screen_row_does_not_inherit_the_attributes_of_the_history() {
        // Sonde du 16/10 (tmux 3.8) : l'historique finit sur du rouge encore actif, la capture de l'écran part de
        // l'état par défaut et n'émet aucun SGR pour une première rangée sans attribut.
        let mut sink = FakeSink::default();
        let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
        let prefix = format!(
            "idema-sync-{}-",
            sync_token(session.pane, session.next_token)
        );
        let reply = reply_with_history(
            &prefix,
            "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
            &["\x1b[31mred1", "red2", "red3"],
            &["plain1", "plain2", "plain3", "plain4", ""],
        );
        let Effect::Build { snapshot, .. } = session.handle(reply) else {
            panic!("an emulator build was expected");
        };
        let emulator = build_emulator(&snapshot);
        let first_row = emulator
            .dump_text()
            .lines()
            .position(|row| row == "plain1")
            .expect("the first screen row is on screen");
        let foreground = |row: usize| {
            emulator
                .renderable()
                .display_iter
                .find(|cell| cell.point.line.0 == row as i32 && cell.point.column.0 == 0)
                .expect("a cell")
                .cell
                .fg
        };
        let default = alacritty_terminal::vte::ansi::Color::Named(
            alacritty_terminal::vte::ansi::NamedColor::Foreground,
        );
        assert_eq!(foreground(first_row), default);
        assert_eq!(foreground(first_row + 3), default);
    }

    #[test]
    fn a_background_does_not_spill_onto_the_row_a_scrolling_replay_adds() {
        // Sonde du 16/10 (tmux 3.8, `capture-pane -p -e -N`) : l'état d'une ligne se prolonge sur la suivante sans être
        // répété (« suite » ne réémet que le gras, « fin » ne réémet que `49`). Le saut de ligne qui fait défiler
        // l'écran remplit la rangée neuve avec le fond actif : sans remise à zéro, « fin » héritait du bleu.
        let blue_rows = [
            ("44", Color::Named(NamedColor::Blue)),
            ("48;5;12", Color::Indexed(12)),
            (
                "48;2;10;20;30",
                Color::Spec(Rgb {
                    r: 10,
                    g: 20,
                    b: 30,
                }),
            ),
        ];
        for (background, expected) in blue_rows {
            let mut sink = FakeSink::default();
            let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
            let prefix = format!(
                "idema-sync-{}-",
                sync_token(session.pane, session.next_token)
            );
            let bold_row = format!(
                "\x1b[1m\x1b[{background}mgras\x1b[0m\x1b[{background}m{}",
                " ".repeat(16)
            );
            let continued_row = format!("\x1b[1msuite\x1b[0m\x1b[{background}m{}", " ".repeat(15));
            let reply = reply_with_history(
                &prefix,
                "0 5 1 0 0 0 0 0 0 0 0 20 6 0 5 0 default VT10x",
                &["1", "2", "3", "4", "5", "6", "7", "8"],
                &[
                    "plain-a",
                    "plain-b",
                    &bold_row,
                    &continued_row,
                    "\x1b[49mfin  ",
                    "$",
                ],
            );
            let Effect::Build { snapshot, .. } = session.handle(reply) else {
                panic!("an emulator build was expected");
            };
            let emulator = build_emulator(&snapshot);
            let rows: Vec<String> = emulator.dump_text().lines().map(str::to_owned).collect();
            let row_of = |text: &str| rows.iter().position(|row| row == text).expect("a row");
            let cells = |row: usize| -> Vec<(char, Color, bool)> {
                emulator
                    .renderable()
                    .display_iter
                    .filter(|cell| cell.point.line.0 == row as i32)
                    .map(|cell| (cell.c, cell.cell.bg, cell.cell.flags.contains(Flags::BOLD)))
                    .collect()
            };
            // La ligne qui continue le fond de la précédente le garde, texte gras compris, jusqu'au bord.
            let continued_cells = cells(row_of("suite"));
            assert!(
                continued_cells.iter().all(|&(_, bg, _)| bg == expected),
                "{background} : {continued_cells:?}"
            );
            assert!(continued_cells[..5].iter().all(|&(_, _, bold)| bold));
            // La rangée suivante est revenue au fond par défaut sur toute sa largeur, y compris après « fin ».
            let default = Color::Named(NamedColor::Background);
            for text in ["fin", "$"] {
                let after = cells(row_of(text));
                assert!(
                    after.iter().all(|&(_, bg, _)| bg == default),
                    "{background} : row {text:?} carries a background: {after:?}"
                );
            }
        }
    }

    #[test]
    fn burst_of_output_keeps_the_first_arrival() {
        let mut sink = FakeSink::default();
        let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
        let reply = sync_reply(
            &session,
            "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
            &["$"],
        );
        let Effect::Build {
            generation,
            snapshot,
        } = session.handle(reply)
        else {
            panic!("an emulator build was expected");
        };
        session.install(generation, build_emulator(&snapshot));
        let first = Instant::now();
        let later = first + std::time::Duration::from_millis(5);
        session.handle_at(output(3, "a"), first);
        session.handle_at(output(3, "b"), later);
        assert_eq!(session.last_output_at, Some(first));
        // Une fois consommée par la peinture, la sortie suivante rouvre la mesure à son propre instant.
        session.last_output_at = None;
        session.handle_at(output(3, "c"), later);
        assert_eq!(session.last_output_at, Some(later));
    }

    #[test]
    fn stale_build_is_ignored() {
        let mut sink = FakeSink::default();
        let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
        let reply = sync_reply(
            &session,
            "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
            &["old"],
        );
        let Effect::Build {
            generation,
            snapshot,
        } = session.handle(reply)
        else {
            panic!("an emulator build was expected");
        };
        assert!(matches!(
            session.install(generation + 1, build_emulator(&snapshot)),
            Effect::Nothing
        ));
    }

    #[test]
    fn visible_resync_repaints_in_place() {
        let mut sink = FakeSink::default();
        let mut session = PaneSession::attach(&mut sink, PaneId(1), SIZE);
        let reply = sync_reply(
            &session,
            "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
            &["boot"],
        );
        let Effect::Build {
            generation,
            snapshot,
        } = session.handle(reply)
        else {
            panic!("an emulator build was expected");
        };
        session.install(generation, build_emulator(&snapshot));
        session.resync_visible();
        let reply = sync_reply(
            &session,
            "2 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
            &["new", "screen"],
        );
        assert!(matches!(session.handle(reply), Effect::Redraw));
        assert!(session.emulator().dump_text().starts_with("new\nscreen\n"));
    }

    #[test]
    fn output_of_another_pane_is_ignored() {
        let mut sink = FakeSink::default();
        let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
        assert!(matches!(session.handle(output(9, "x")), Effect::Nothing));
    }

    #[test]
    fn pause_continues_the_pane_then_resyncs() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(4), SIZE);
            session.handle(ControlEvent::Pause { pane: PaneId(4) });
        }
        assert!(
            sink.lines
                .contains(&"refresh-client -A '%4:continue'".to_string())
        );
        assert_eq!(
            sink.lines
                .iter()
                .filter(|line| line.contains("-S -2000"))
                .count(),
            2
        );
    }

    #[test]
    fn fit_resizes_the_emulator_once_and_visible_resync_skips_history() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(1), SIZE);
            // La relecture d'attache doit être terminée : une visible ne passe pas par-dessus une complète.
            let reply = sync_reply(
                &session,
                "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &[],
            );
            let Effect::Build {
                generation,
                snapshot,
            } = session.handle(reply)
            else {
                panic!("an emulator build was expected");
            };
            session.install(generation, build_emulator(&snapshot));
            let grown = GridSize {
                columns: 30,
                lines: 8,
            };
            assert!(session.fit(grown));
            assert!(!session.fit(grown));
            assert_eq!(session.emulator().size(), grown);
            session.resync_visible();
        }
        assert!(
            !sink
                .lines
                .iter()
                .any(|line| line.starts_with("refresh-client -C"))
        );
        let last = sink.lines.last().unwrap();
        // La relecture complète lit aussi l'écran sans -J : seule la visible n'a pas de capture d'historique.
        assert!(last.contains("capture-pane -p -e -N -t %1") && !last.contains("-S -2000"));
    }

    #[test]
    fn input_goes_out_as_hex_send_keys() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(2), SIZE);
            session.send_bytes(b"ls\r");
        }
        assert_eq!(sink.lines.last().unwrap(), "send-keys -t %2 -H 6c 73 0d");
    }

    #[test]
    fn exit_closes_the_session() {
        let mut sink = FakeSink::default();
        let mut session = PaneSession::attach(&mut sink, PaneId(2), SIZE);
        assert!(matches!(
            session.handle(ControlEvent::Exit { reason: None }),
            Effect::Closed
        ));
    }

    /// Réponse dont le marqueur de fin passe, mais dont la ligne d'état est illisible.
    fn malformed_sync_reply<S: CommandSink>(session: &PaneSession<S>) -> ControlEvent {
        sync_reply(session, "not a state line", &["x"])
    }

    /// Erreur d'une commande de la relecture en vol : tmux a déjà rendu son marqueur de début.
    fn failed_reply<S: CommandSink>(session: &PaneSession<S>) -> ControlEvent {
        let start = format!(
            "idema-sync-{}-start",
            sync_token(session.pane, session.next_token)
        );
        ControlEvent::Reply {
            number: 1,
            ok: false,
            lines: vec![start.into_bytes()],
        }
    }

    #[test]
    fn an_error_of_another_pane_leaves_a_sync_that_has_not_started_alone() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
            let unrelated = ControlEvent::Reply {
                number: 9,
                ok: false,
                lines: vec![b"can't find pane: %7".to_vec()],
            };
            assert!(matches!(session.handle(unrelated), Effect::Nothing));
            let reply = sync_reply(
                &session,
                "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["$ ok"],
            );
            assert!(matches!(session.handle(reply), Effect::Build { .. }));
        }
        assert_eq!(full_sync_count(&sink), 1);
    }

    /// Un puits que deux sessions de panes différents se partagent, comme le client de contrôle d'une session.
    #[derive(Clone, Default)]
    struct SharedSink {
        lines: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }

    impl CommandSink for SharedSink {
        fn send(&mut self, line: &str) {
            self.lines.borrow_mut().push(line.to_string());
        }
    }

    /// Le préfixe des marqueurs que la commande de relecture `line` demande à tmux d'écrire.
    fn markers_of(line: &str) -> String {
        let from = line.find("idema-sync-").expect("a sync marker");
        let to = line[from..].find("-start").expect("a start marker") + from;
        format!("{}-", &line[from..to])
    }

    fn reply_with(prefix: &str, state: &str, visible: &[&str]) -> ControlEvent {
        reply_with_history(prefix, state, &[], visible)
    }

    /// Réponse de tmux : l'état, l'historique, le marqueur d'écran, puis les rangées de l'écran.
    fn reply_with_history(
        prefix: &str,
        state: &str,
        history: &[&str],
        screen: &[&str],
    ) -> ControlEvent {
        let mut lines = vec![format!("{prefix}start"), state.to_string()];
        lines.extend(history.iter().map(|line| line.to_string()));
        lines.push(format!("{prefix}screen"));
        lines.extend(screen.iter().map(|line| line.to_string()));
        lines.push(format!("{prefix}sep"));
        lines.push(format!("{prefix}end"));
        ControlEvent::Reply {
            number: 1,
            ok: true,
            lines: lines.into_iter().map(String::into_bytes).collect(),
        }
    }

    #[test]
    fn panes_attached_together_never_take_each_others_replay() {
        // Chaque réponse du client est remise à toutes les sessions de panes (la vue de session les route ainsi) :
        // deux panes dont les compteurs de relecture sont égaux ne doivent reconnaître que leurs propres marqueurs.
        let sink = SharedSink::default();
        let mut first = PaneSession::attach(sink.clone(), PaneId(3), SIZE);
        let mut second = PaneSession::attach(sink.clone(), PaneId(4), SIZE);
        let sent = sink.lines.borrow().clone();
        assert_eq!(sent.len(), 2);
        let state = "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x";
        let replies = [
            reply_with(&markers_of(&sent[0]), state, &["first pane"]),
            reply_with(&markers_of(&sent[1]), state, &["second pane"]),
        ];
        let (mut first_builds, mut second_builds) = (Vec::new(), Vec::new());
        for reply in replies {
            if let Effect::Build { snapshot, .. } = first.handle(reply.clone()) {
                first_builds.push(snapshot);
            }
            if let Effect::Build { snapshot, .. } = second.handle(reply) {
                second_builds.push(snapshot);
            }
        }
        assert_eq!(first_builds.len(), 1);
        assert_eq!(second_builds.len(), 1);
        assert!(
            build_emulator(&first_builds[0])
                .dump_text()
                .starts_with("first pane")
        );
        assert!(
            build_emulator(&second_builds[0])
                .dump_text()
                .starts_with("second pane")
        );
    }

    fn full_sync_count(sink: &FakeSink) -> usize {
        sink.lines
            .iter()
            .filter(|line| line.contains("-S -2000"))
            .count()
    }

    #[test]
    fn malformed_sync_reply_retries_once_then_the_retry_completes() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
            let reply = malformed_sync_reply(&session);
            assert!(matches!(session.handle(reply), Effect::Nothing));
            // La relance est en vol : la sortie est encore jetée, la capture l'inclura.
            assert!(matches!(
                session.handle(output(3, "dropped")),
                Effect::Nothing
            ));
            let reply = sync_reply(
                &session,
                "4 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["$ ok"],
            );
            let Effect::Build {
                generation,
                snapshot,
            } = session.handle(reply)
            else {
                panic!("the retry should have ended in a build");
            };
            session.install(generation, build_emulator(&snapshot));
            assert!(matches!(session.handle(output(3, "!")), Effect::Redraw));
            assert!(session.emulator().dump_text().starts_with("$ ok!"));
        }
        assert_eq!(full_sync_count(&sink), 2);
    }

    #[test]
    fn rejected_reply_mid_sync_retries_the_same_kind() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(1), SIZE);
            let reply = sync_reply(
                &session,
                "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["boot"],
            );
            let Effect::Build {
                generation,
                snapshot,
            } = session.handle(reply)
            else {
                panic!("an emulator build was expected");
            };
            session.install(generation, build_emulator(&snapshot));
            session.resync_visible();
            assert!(matches!(
                session.handle(failed_reply(&session)),
                Effect::Nothing
            ));
        }
        let visible = sink
            .lines
            .iter()
            .filter(|line| {
                line.contains("capture-pane -p -e -N -t %1") && !line.contains("-S -2000")
            })
            .count();
        assert_eq!(visible, 2);
        assert_eq!(full_sync_count(&sink), 1);
    }

    #[test]
    fn second_failure_gives_up_and_output_flows() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
            session.handle(failed_reply(&session));
            let reply = malformed_sync_reply(&session);
            assert!(matches!(session.handle(reply), Effect::Nothing));
            assert!(matches!(session.handle(output(3, "flows")), Effect::Redraw));
            assert!(session.emulator().dump_text().starts_with("flows"));
            // Rien n'est en vol : un échec tardif n'ouvre pas de troisième relecture.
            assert!(matches!(
                session.handle(failed_reply(&session)),
                Effect::Nothing
            ));
        }
        assert_eq!(full_sync_count(&sink), 2);
    }

    #[test]
    fn requested_sync_restores_the_retry_budget() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
            session.handle(failed_reply(&session));
            session.handle(failed_reply(&session));
            // Budget épuisé, plus rien en vol : une relecture demandée repart avec une relance neuve.
            session.resync_visible();
            session.handle(failed_reply(&session));
        }
        let visible = sink
            .lines
            .iter()
            .filter(|line| {
                line.contains("capture-pane -p -e -N -t %3") && !line.contains("-S -2000")
            })
            .count();
        assert_eq!(visible, 2);
    }

    fn visible_sync_count(sink: &FakeSink) -> usize {
        sink.lines
            .iter()
            .filter(|line| line.contains("-e -N -t") && !line.contains("-S -2000"))
            .count()
    }

    #[test]
    fn visible_resync_during_a_build_becomes_a_full_one_and_the_stale_install_is_ignored() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
            let reply = sync_reply(
                &session,
                "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["old"],
            );
            let Effect::Build {
                generation: stale,
                snapshot: stale_snapshot,
            } = session.handle(reply)
            else {
                panic!("an emulator build was expected");
            };
            session.resync_visible();
            let reply = sync_reply(
                &session,
                "3 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["new"],
            );
            let Effect::Build {
                generation,
                snapshot,
            } = session.handle(reply)
            else {
                panic!("the replacement full resync should have built");
            };
            assert!(matches!(session.handle(output(3, "held")), Effect::Nothing));
            assert!(matches!(
                session.install(stale, build_emulator(&stale_snapshot)),
                Effect::Nothing
            ));
            assert!(matches!(
                session.install(generation, build_emulator(&snapshot)),
                Effect::Redraw
            ));
            assert!(session.emulator().dump_text().starts_with("newheld"));
        }
        assert_eq!(full_sync_count(&sink), 2);
        assert_eq!(visible_sync_count(&sink), 0);
    }

    #[test]
    fn visible_resync_while_the_attach_sync_is_pending_sends_a_full_one() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
            session.resync_visible();
            let reply = sync_reply(
                &session,
                "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["hist"],
            );
            let Effect::Build { snapshot, .. } = session.handle(reply) else {
                panic!("an emulator build was expected");
            };
            assert_eq!(snapshot.kind, SyncKind::Full);
        }
        assert_eq!(full_sync_count(&sink), 2);
        assert_eq!(visible_sync_count(&sink), 0);
    }

    #[test]
    fn output_after_a_give_up_during_a_build_reaches_the_installed_emulator() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
            let reply = sync_reply(
                &session,
                "3 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["abc"],
            );
            let Effect::Build {
                generation,
                snapshot,
            } = session.handle(reply)
            else {
                panic!("an emulator build was expected");
            };
            session.resync_visible();
            session.handle(failed_reply(&session));
            let reply = malformed_sync_reply(&session);
            session.handle(reply);
            assert!(matches!(session.handle(output(3, "kept")), Effect::Nothing));
            assert!(matches!(
                session.install(generation, build_emulator(&snapshot)),
                Effect::Redraw
            ));
            assert!(session.emulator().dump_text().starts_with("abckept"));
        }
        assert_eq!(full_sync_count(&sink), 3);
        assert_eq!(visible_sync_count(&sink), 0);
    }

    #[test]
    fn rejected_reply_without_a_sync_in_flight_is_ignored() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(3), SIZE);
            session.handle(failed_reply(&session));
            session.handle(failed_reply(&session));
            // Relance consommée puis levée : rien en vol, la sortie passe.
            assert!(matches!(session.handle(output(3, "x")), Effect::Redraw));
            session.handle(failed_reply(&session));
        }
        assert_eq!(full_sync_count(&sink), 2);
    }

    #[test]
    fn extended_level_reads_tmux_key_mode() {
        assert_eq!(extended_level("Ext 2"), 2);
        assert_eq!(extended_level("Ext 1"), 1);
        assert_eq!(extended_level("VT10x"), 0);
    }
    #[test]
    fn full_resync_keeps_the_scroll_region_for_later_output() {
        let mut sink = FakeSink::default();
        let mut session = PaneSession::attach(
            &mut sink,
            PaneId(1),
            GridSize {
                columns: 10,
                lines: 6,
            },
        );
        // Région de défilement sur les rangées 2 à 4 (tmux compte à partir de 0).
        let reply = sync_reply(
            &session,
            "0 3 1 0 0 0 0 0 0 0 0 10 6 1 3 0 default VT10x",
            &["top", "a", "b", "c", "", "bottom"],
        );
        let Effect::Build {
            generation,
            snapshot,
        } = session.handle(reply)
        else {
            panic!("an emulator build was expected");
        };
        session.install(generation, build_emulator(&snapshot));
        session.handle(output(1, "\x1b[4;1H\nd"));
        assert_eq!(session.emulator().dump_text(), "top\nb\nc\nd\n\nbottom");
    }
    /// Installe l'émulateur d'une attache réussie, avec l'état donné et ces lignes visibles.
    fn attach_installed<'a>(
        sink: &'a mut FakeSink,
        size: GridSize,
        state: &str,
        visible: &[&str],
    ) -> PaneSession<&'a mut FakeSink> {
        let mut session = PaneSession::attach(sink, PaneId(1), size);
        let reply = sync_reply(&session, state, visible);
        let Effect::Build {
            generation,
            snapshot,
        } = session.handle(reply)
        else {
            panic!("an emulator build was expected");
        };
        session.install(generation, build_emulator(&snapshot));
        session
    }

    #[test]
    fn visible_capture_taken_before_a_resize_is_dropped_and_asked_again() {
        let mut sink = FakeSink::default();
        {
            let mut session = attach_installed(
                &mut sink,
                SIZE,
                "0 4 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["h1", "h2", "h3", "h4", "$"],
            );
            session.resync_visible();
            session.fit(GridSize {
                columns: 20,
                lines: 3,
            });
            let history = session.emulator().history_size();
            // La capture date d'avant le redimensionnement : cinq rangées, pour un émulateur de trois.
            let stale = sync_reply(
                &session,
                "1 4 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["A", "B", "C", "D", "E"],
            );
            assert!(matches!(session.handle(stale), Effect::Nothing));
            assert_eq!(session.emulator().history_size(), history);
            // Une nouvelle relecture visible est partie : sa réponse, à la bonne taille, repeint.
            let fresh = sync_reply(
                &session,
                "1 2 1 0 0 0 0 0 0 0 0 20 3 0 2 0 default VT10x",
                &["X", "Y", "Z"],
            );
            assert!(matches!(session.handle(fresh), Effect::Redraw));
            assert!(session.emulator().dump_text().starts_with("X\nY\nZ"));
            assert_eq!(session.emulator().history_size(), history);
        }
        assert_eq!(visible_sync_count(&sink), 2);
        assert_eq!(full_sync_count(&sink), 1);
    }

    #[test]
    fn size_that_never_matches_escalates_to_a_full_resync() {
        let mut sink = FakeSink::default();
        {
            let mut session = attach_installed(
                &mut sink,
                SIZE,
                "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["$"],
            );
            session.resync_visible();
            // tmux s'obstine à rapporter 30x5 : la relance échoue aussi, la complète adopte la taille de tmux.
            for _ in 0..2 {
                let reply = sync_reply(
                    &session,
                    "0 0 1 0 0 0 0 0 0 0 0 30 5 0 4 0 default VT10x",
                    &["wide"],
                );
                assert!(matches!(session.handle(reply), Effect::Nothing));
            }
        }
        assert_eq!(visible_sync_count(&sink), 2);
        assert_eq!(full_sync_count(&sink), 2);
    }

    /// Répond à la relecture en vol comme le ferait tmux : une visible repeint ou escalade, une complète se construit.
    fn answer_pending_sync<S: CommandSink>(
        session: &mut PaneSession<S>,
        state: &str,
        visible: &[&str],
    ) {
        let reply = sync_reply(session, state, visible);
        if let Effect::Build {
            generation,
            snapshot,
        } = session.handle(reply)
        {
            session.install(generation, build_emulator(&snapshot));
        }
    }

    #[test]
    fn a_bordered_pane_smaller_than_its_layout_cell_does_not_loop_resyncs() {
        let mut sink = FakeSink::default();
        {
            // Sonde du 15/10 : avec `pane-border-status top`, le layout donne la cellule (20x5) et tmux rend un pane de
            // 20x4, sans `%layout-change` quand la bordure apparaît ou disparaît.
            let cell = SIZE;
            let tmux_state = "0 0 1 0 0 0 0 0 0 0 0 20 4 0 3 0 default VT10x";
            let mut session = PaneSession::attach(&mut sink, PaneId(1), cell);
            answer_pending_sync(&mut session, tmux_state, &["$ ls", "", "", ""]);
            // Ce que la vue fait à chaque changement de layout et à chaque pas : `fit` de la cellule.
            for _ in 0..10 {
                if session.fit(cell) {
                    session.resync_visible();
                    answer_pending_sync(&mut session, tmux_state, &["$ ls", "", "", ""]);
                }
            }
            assert_eq!(
                session.emulator().size(),
                GridSize {
                    columns: 20,
                    lines: 4
                }
            );
            assert!(session.emulator().dump_text().starts_with("$ ls"));
        }
        assert_eq!(full_sync_count(&sink), 1);
        assert_eq!(visible_sync_count(&sink), 0);
    }

    #[test]
    fn a_bordered_pane_given_a_new_cell_adopts_the_tmux_size_with_one_full_resync() {
        let mut sink = FakeSink::default();
        {
            let mut session = attach_installed(
                &mut sink,
                SIZE,
                "0 0 1 0 0 0 0 0 0 0 0 20 4 0 3 0 default VT10x",
                &["$", "", "", ""],
            );
            let grown = GridSize {
                columns: 30,
                lines: 8,
            };
            // La cellule grandit à 30x8, tmux rend 30x7 : la visible ne concorde pas, sa relance non plus, la
            // complète adopte la taille de tmux.
            assert!(session.fit(grown));
            session.resync_visible();
            let pane_state = "0 0 1 0 0 0 0 0 0 0 0 30 7 0 6 0 default VT10x";
            for _ in 0..3 {
                answer_pending_sync(&mut session, pane_state, &["$", "", "", "", "", "", ""]);
            }
            assert_eq!(
                session.emulator().size(),
                GridSize {
                    columns: 30,
                    lines: 7
                }
            );
            for _ in 0..10 {
                assert!(!session.fit(grown));
            }
        }
        assert_eq!(full_sync_count(&sink), 2);
    }

    #[test]
    fn visible_capture_at_the_emulator_size_is_applied() {
        let mut sink = FakeSink::default();
        {
            let mut session = attach_installed(
                &mut sink,
                SIZE,
                "0 0 1 0 0 0 0 0 0 0 0 20 4 0 3 0 default VT10x",
                &["old", "", "", ""],
            );
            session.resync_visible();
            let reply = sync_reply(
                &session,
                "0 0 1 0 0 0 0 0 0 0 0 20 4 0 3 0 default VT10x",
                &["new", "", "", ""],
            );
            assert!(matches!(session.handle(reply), Effect::Redraw));
            assert!(session.emulator().dump_text().starts_with("new"));
        }
        assert_eq!(full_sync_count(&sink), 1);
    }

    #[test]
    fn a_pane_resized_by_the_layout_is_fitted_then_repainted_without_a_full_resync() {
        let mut sink = FakeSink::default();
        {
            let tmux_state = "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x";
            let mut session = attach_installed(&mut sink, SIZE, tmux_state, &["$"]);
            let grown = GridSize {
                columns: 30,
                lines: 7,
            };
            assert!(session.fit(grown));
            session.resync_visible();
            let pane_state = "0 0 1 0 0 0 0 0 0 0 0 30 7 0 6 0 default VT10x";
            let reply = sync_reply(&session, pane_state, &["$", "", "", "", "", "", ""]);
            assert!(matches!(session.handle(reply), Effect::Redraw));
            assert_eq!(session.emulator().size(), grown);
        }
        assert_eq!(full_sync_count(&sink), 1);
        assert_eq!(visible_sync_count(&sink), 1);
    }

    #[test]
    fn fit_reports_a_change_of_the_layout_size_only() {
        let mut sink = FakeSink::default();
        let mut session = PaneSession::attach(&mut sink, PaneId(1), SIZE);
        assert!(!session.fit(SIZE));
        assert!(session.fit(GridSize {
            columns: 20,
            lines: 4
        }));
        assert!(!session.fit(GridSize {
            columns: 20,
            lines: 4
        }));
    }

    #[test]
    fn rebinding_to_a_new_client_resyncs_on_it_and_subscribes_again() {
        let mut first = FakeSink::default();
        let mut second = FakeSink::default();
        {
            let mut session = attach_installed(
                &mut first,
                SIZE,
                "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["before"],
            );
            session.resync_visible();
            session.rebind(&mut second);
            // La sortie est jetée pendant la relecture du nouveau client, puis l'émulateur neuf la contient.
            assert!(matches!(session.handle(output(1, "lost")), Effect::Nothing));
            answer_pending_sync(
                &mut session,
                "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["after"],
            );
            assert!(session.emulator().dump_text().starts_with("after"));
        }
        assert_eq!(full_sync_count(&second), 1);
        assert_eq!(
            second
                .lines
                .iter()
                .filter(|line| line.contains("refresh-client -B 'idema-size-1:"))
                .count(),
            1
        );
    }

    #[test]
    fn the_size_subscription_of_a_pane_names_that_pane() {
        let mut sink = FakeSink::default();
        let session = PaneSession::attach(&mut sink, PaneId(7), SIZE);
        assert_eq!(
            size_watch_pane(&session.size_subscription()),
            Some(PaneId(7))
        );
        assert_eq!(size_watch_pane("idema-cmd-7"), None);
        assert_eq!(size_watch_pane("idema-size-x"), None);
    }

    #[test]
    fn unsubscribing_the_size_names_only_the_subscription() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(7), SIZE);
            session.unsubscribe_size();
        }
        assert_eq!(
            sink.lines.last().map(String::as_str),
            Some("refresh-client -B idema-size-7")
        );
    }

    fn size_event(pane: u32, value: &str) -> ControlEvent {
        ControlEvent::SubscriptionChanged {
            name: format!("idema-size-{pane}"),
            value: value.to_string(),
        }
    }

    #[test]
    fn size_subscription_is_sent_once_after_the_first_install() {
        let mut sink = FakeSink::default();
        {
            let mut session = PaneSession::attach(&mut sink, PaneId(7), SIZE);
            assert!(
                !session
                    .sink
                    .lines
                    .iter()
                    .any(|line| line.contains("refresh-client -B"))
            );
            answer_pending_sync(
                &mut session,
                "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["$"],
            );
            session.resync_visible();
            answer_pending_sync(
                &mut session,
                "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["$"],
            );
        }
        let subscriptions: Vec<_> = sink
            .lines
            .iter()
            .filter(|line| line.contains("refresh-client -B"))
            .collect();
        assert_eq!(
            subscriptions,
            ["refresh-client -B 'idema-size-7:%7:#{pane_width}x#{pane_height}'"]
        );
    }

    #[test]
    fn size_notification_asks_for_a_resync_only_when_the_pane_differs_from_the_emulator() {
        let mut sink = FakeSink::default();
        let mut session = PaneSession::attach(&mut sink, PaneId(1), SIZE);
        // Même taille que l'émulateur (valeur initiale de l'abonnement) : rien à faire.
        assert!(matches!(
            session.handle(size_event(1, "20x5")),
            Effect::Nothing
        ));
        assert!(matches!(
            session.handle(size_event(1, "20x6")),
            Effect::ResyncVisible
        ));
        // Un autre abonnement, un pane voisin ou une valeur illisible ne comptent pas.
        assert!(matches!(
            session.handle(size_event(2, "20x6")),
            Effect::Nothing
        ));
        assert!(matches!(
            session.handle(size_event(1, "grand")),
            Effect::Nothing
        ));
    }

    #[test]
    fn pane_grown_back_by_tmux_is_adopted_and_repeated_notifications_stay_bounded() {
        let mut sink = FakeSink::default();
        {
            // Fenêtre 20x5, bordure : 20x4. La bordure disparaît, tmux rend 20x5 sans que la fenêtre ait bougé.
            let mut session = attach_installed(
                &mut sink,
                SIZE,
                "0 0 1 0 0 0 0 0 0 0 0 20 4 0 3 0 default VT10x",
                &["$ ls", "", "", ""],
            );
            let pane_state = "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x";
            // Une salve d'événements identiques n'envoie rien d'elle-même : l'interface les regroupe en une relecture.
            let lines_before = session.sink.lines.len();
            for _ in 0..20 {
                assert!(matches!(
                    session.handle(size_event(1, "20x5")),
                    Effect::ResyncVisible
                ));
            }
            assert_eq!(session.sink.lines.len(), lines_before);
            session.resync_visible();
            for _ in 0..3 {
                answer_pending_sync(&mut session, pane_state, &["$ ls", "", "", "", ""]);
            }
            assert_eq!(session.emulator().size(), SIZE);
            // Une fois adoptée, la même notification ne demande plus rien : aucune relecture de plus.
            let lines_after = session.sink.lines.len();
            for _ in 0..10 {
                assert!(matches!(
                    session.handle(size_event(1, "20x5")),
                    Effect::Nothing
                ));
            }
            assert_eq!(session.sink.lines.len(), lines_after);
        }
        assert_eq!(full_sync_count(&sink), 2);
    }

    #[test]
    fn visible_capture_after_the_program_left_the_alternate_screen_is_replaced_by_a_full_resync() {
        use alacritty_terminal::term::TermMode;
        let mut sink = FakeSink::default();
        {
            let size = GridSize {
                columns: 20,
                lines: 3,
            };
            // Une TUI tourne : écran alterné, souris et collage balisé.
            let mut session = attach_installed(
                &mut sink,
                size,
                "0 0 1 1 0 0 1 1 0 0 1 20 3 0 2 0 default VT10x",
                &["tui"],
            );
            session.resync_visible();
            // Elle quitte pendant la relecture : sa sortie est jetée, tmux répond écran principal, modes éteints.
            session.handle(output(1, "\x1b[?1049l\x1b[?1000l\x1b[?1006l\x1b[?2004l"));
            let visible = sync_reply(
                &session,
                "2 0 1 0 0 0 0 0 0 0 0 20 3 0 2 0 default VT10x",
                &["$ ls", "", ""],
            );
            assert!(matches!(session.handle(visible), Effect::Nothing));
            // La complète rebâtit l'émulateur d'après l'état de tmux.
            let full = sync_reply(
                &session,
                "2 0 1 0 0 0 0 0 0 0 0 20 3 0 2 0 default VT10x",
                &["$ ls", "", ""],
            );
            let Effect::Build {
                generation,
                snapshot,
            } = session.handle(full)
            else {
                panic!("a full resync was expected");
            };
            assert_eq!(snapshot.kind, SyncKind::Full);
            session.install(generation, build_emulator(&snapshot));
            let mode = session.emulator().mode();
            for stale in [
                TermMode::ALT_SCREEN,
                TermMode::MOUSE_REPORT_CLICK,
                TermMode::SGR_MOUSE,
                TermMode::BRACKETED_PASTE,
            ] {
                assert!(!mode.contains(stale), "{stale:?} is still on");
            }
        }
        assert_eq!(full_sync_count(&sink), 2);
    }

    #[test]
    fn visible_capture_with_a_mode_the_emulator_lacks_is_replaced_by_a_full_resync() {
        let mut sink = FakeSink::default();
        {
            let mut session = attach_installed(
                &mut sink,
                SIZE,
                "0 0 1 0 0 0 0 0 0 0 0 20 5 0 4 0 default VT10x",
                &["$"],
            );
            session.resync_visible();
            // Le programme a activé le collage balisé pendant la relecture : sa demande a été jetée.
            let reply = sync_reply(
                &session,
                "0 0 1 0 0 0 1 0 0 0 0 20 5 0 4 0 default VT10x",
                &["$"],
            );
            assert!(matches!(session.handle(reply), Effect::Nothing));
        }
        assert_eq!(full_sync_count(&sink), 2);
    }
}
