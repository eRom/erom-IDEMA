//! L'onglet journal d'un serveur (spec P4 §4.5) : la sortie du serveur comme un terminal en lecture seule (couleurs ANSI,
//! défilement, sélection, `⌘C`, `⌘F`), sous une rangée qui reprend la ligne du serveur et ses actions.
//!
//! - Un serveur démarré par IdemA : le pane de sa session tmux, relu par un client de contrôle propre à l'onglet. La
//!   fenêtre de la session serveur prend la grille de l'onglet (`refresh-client -C`, comme une session de la barre), et
//!   la relecture de l'écran garde l'historique que tmux a gardé.
//! - Un serveur repéré dont la sortie va dans un fichier ordinaire : la fin de ce fichier, suivie (`FileTail`).
//! - Sinon, ou quand la ligne manque : un texte, « Journal indisponible : ce serveur écrit dans <cible> » ou « Serveur
//!   introuvable ».

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, Render, Subscription, Task, Window, canvas,
    div, prelude::*,
};
use idema_term::emulator::GridSize;
use idema_tmux::client::ControlClient;
use idema_tmux::protocol::{ControlEvent, PaneId};
use idema_tmux::server::TmuxServer;

use super::BrowserPanel;
use super::start_page::{RowSlot, server_row};
use crate::browser::tabs::{OpenFrom, Tab, TabId, TabKind};
use crate::hud;
use crate::pane_view::{PaneEvent, PaneView, SessionRef};
use crate::servers::model::JournalSource;
use crate::servers::registry::{RowKind, ServerRow};
use crate::terminal_element::TerminalFont;
use crate::terminal_panel::{
    BODY_MARGIN_SIDES, BODY_MARGIN_VERTICAL, body_grid, device_cell_pixels,
};
use crate::theme::{SEPARATOR, TEXT_LABEL, Theme, white};

/// Ce que dit l'onglet quand la ligne qu'il désigne n'existe pas (encore) : un journal restauré dont le projet ou
/// l'entrée a disparu.
pub const NOT_FOUND: &str = "Serveur introuvable";
/// Ce que dit l'onglet quand la session tmux du serveur n'a pas pu être rejointe.
const SESSION_GONE: &str = "Journal indisponible : la session de ce serveur n'existe plus";
/// La grille d'un journal avant sa première mesure : la taille d'une session tmux neuve.
const START_GRID: GridSize = GridSize {
    columns: 80,
    lines: 24,
};
/// Délai entre la dernière mesure de la zone et la taille demandée à tmux, comme les sessions de la barre : plus court
/// que `RESIZE_SETTLE`, pour que la relecture de l'écran attende la taille posée.
const SIZE_REQUEST_DELAY: Duration = Duration::from_millis(50);
/// Le temps que la fenêtre d'une session serveur prend pour adopter la grille avant que le pane ne soit créé. Attendre le `%end`
/// de `refresh-client -C` ne suffit pas : mesuré le 20/10 sur tmux 3.8, le `%layout-change` qui montre la fenêtre redimensionnée
/// arrive après lui, et il n'arrive pas du tout quand la grille demandée est celle qu'a déjà la fenêtre. D'où un délai, dont
/// l'ancre est la passe de l'AC-92 qui, sans lui, a lu deux fois la première ligne repliée (une relecture complète à 80 colonnes,
/// puis la relecture de l'écran après le repli de tmux). La demande part aussitôt, tmux la traite en quelques millisecondes.
const ATTACH_SETTLE: Duration = Duration::from_millis(250);

/// À quoi un journal est attaché (ou le texte qui le remplace).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalTarget {
    /// Le pane de la session serveur `name`.
    Session {
        name: String,
        pane: PaneId,
        project: PathBuf,
    },
    /// La fin du fichier où va la sortie du processus.
    File {
        path: PathBuf,
        project: PathBuf,
        name: String,
    },
    /// Rien à lire : le texte à montrer.
    Notice(String),
}

/// La cible du journal d'une ligne d'après sa source (`Servers::journal_source`).
pub fn journal_target(row: &ServerRow, source: &JournalSource) -> JournalTarget {
    match source {
        JournalSource::Session { name, pane } => JournalTarget::Session {
            name: name.clone(),
            pane: *pane,
            project: row.project.clone(),
        },
        JournalSource::File(path) => JournalTarget::File {
            path: path.clone(),
            project: row.project.clone(),
            name: row.name.clone(),
        },
        JournalSource::Unavailable { target } => {
            JournalTarget::Notice(JournalSource::unavailable_text(target.as_deref()))
        }
        JournalSource::Stopped => JournalTarget::Notice(JournalSource::STOPPED_TEXT.to_owned()),
    }
}

/// Ce qui identifie le serveur d'un onglet journal dans `state.json` : son projet, son nom et, pour un serveur repéré
/// seulement, son port (deux serveurs repérés de même nom se distinguent par lui). Le port d'une entrée de configuration
/// n'en fait pas partie : l'entrée `dev` de `package.json` le découvre au démarrage, et l'onglet resterait un autre.
pub fn tab_identity(row: &ServerRow) -> (PathBuf, String, Option<u16>) {
    let port = match row.kind {
        RowKind::Detected { .. } => row.port,
        RowKind::Config(_) => None,
    };
    (row.project.clone(), row.name.clone(), port)
}

/// Le corps d'un journal : un pane en lecture seule, ou le texte qui le remplace.
pub struct JournalView {
    tmux: Arc<TmuxServer>,
    /// À quoi la vue est attachée. Jamais un `Notice` quand `pane` existe : un journal déjà lu reste lisible quand son
    /// serveur s'arrête et que sa session se ferme.
    target: Option<JournalTarget>,
    pane: Option<Entity<PaneView>>,
    /// Le client de contrôle de la session tmux (journal d'une session serveur seulement).
    client: Option<ControlClient>,
    /// La taille demandée à tmux, et le compteur qui annule la demande d'une mesure dépassée.
    requested: Option<GridSize>,
    size_generation: u64,
    /// La dernière grille mesurée à la peinture : la demande à tmux ne repart qu'à un changement.
    measured: Rc<Cell<Option<GridSize>>>,
    /// La session serveur à rejoindre dès que la zone est mesurée (voir `begin_attach`), et le compteur qui annule une
    /// attache dépassée par un autre changement de cible.
    pending: Option<PendingAttach>,
    attach_generation: u64,
    _reader: Option<Task<()>>,
    _pane_events: Option<Subscription>,
}

/// La session serveur d'un journal qui attend la première mesure de sa zone.
struct PendingAttach {
    name: String,
    pane: PaneId,
    project: PathBuf,
}

/// Ce que le journal demande au volet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalEvent {
    /// `⌘`-clic sur une adresse `http` ou `https` de la sortie (P4-7) : un onglet de plus.
    OpenUrl(String),
}

impl EventEmitter<JournalEvent> for JournalView {}

impl JournalView {
    /// Relaie au volet le `⌘`-clic d'une adresse du pane (le journal n'ouvre rien lui-même).
    fn watch_pane(&mut self, view: &Entity<PaneView>, cx: &mut Context<Self>) {
        self._pane_events = Some(cx.subscribe(view, |_, _, event: &PaneEvent, cx| {
            if let PaneEvent::OpenUrl(url) = event {
                cx.emit(JournalEvent::OpenUrl(url.clone()));
            }
        }));
    }

    pub fn new(tmux: Arc<TmuxServer>) -> Self {
        Self {
            tmux,
            target: None,
            pane: None,
            client: None,
            requested: None,
            size_generation: 0,
            measured: Rc::new(Cell::new(None)),
            pending: None,
            attach_generation: 0,
            _reader: None,
            _pane_events: None,
        }
    }

    /// Attache la vue à `target`. Une cible inchangée ne coûte rien ; un texte ne remplace pas un journal déjà
    /// attaché (le serveur s'est arrêté, ce qu'il a écrit se relit encore).
    pub fn show(&mut self, target: JournalTarget, window: &mut Window, cx: &mut Context<Self>) {
        if self.target.as_ref() == Some(&target) {
            return;
        }
        if matches!(target, JournalTarget::Notice(_)) && self.pane.is_some() {
            return;
        }
        self.detach();
        match &target {
            JournalTarget::Session {
                name,
                pane,
                project,
            } => {
                // La session se rejoint à la première mesure de la zone : sa fenêtre prend d'abord la grille de l'onglet,
                // puis l'écran se relit une seule fois, à cette taille (voir `begin_attach`).
                self.pending = Some(PendingAttach {
                    name: name.clone(),
                    pane: *pane,
                    project: project.clone(),
                });
                self.target = Some(target);
            }
            JournalTarget::File {
                path,
                project,
                name,
            } => {
                let view = PaneView::spawn_tail(path, name, project, START_GRID, window, cx);
                self.watch_pane(&view, cx);
                self.pane = Some(view);
                self.target = Some(target);
            }
            JournalTarget::Notice(_) => self.target = Some(target),
        }
        cx.notify();
    }

    /// Quitte la cible courante : le pane et son client s'en vont.
    fn detach(&mut self) {
        self.pane = None;
        self.pending = None;
        self.attach_generation += 1;
        self._reader = None;
        self._pane_events = None;
        self.requested = None;
        self.measured.set(None);
        self.drop_client();
    }

    /// Le client de contrôle quitte tmux (`detach-client`, jusqu'à 500 ms) hors du fil d'interface.
    fn drop_client(&mut self) {
        if let Some(client) = self.client.take() {
            std::thread::spawn(move || drop(client));
        }
    }

    /// La zone du journal vient d'être mesurée (ou a changé) : une session serveur qui attendait est rejointe, sinon sa
    /// fenêtre prend la nouvelle grille.
    fn on_measured(&mut self, grid: GridSize, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            self.begin_attach(grid, window, cx);
        } else {
            self.request_client_size(grid, cx);
        }
    }

    /// Rejoint la session serveur par un client de contrôle, en deux temps. La fenêtre de la session prend d'abord la
    /// grille de l'onglet (`refresh-client -C`) : tmux y repousse dans son historique les premières lignes d'une sortie qui
    /// se replie, et un écran relu avant cet instant, puis relu de nouveau à la nouvelle taille, montrerait des lignes en
    /// double. Le pane n'est créé qu'une fois la taille posée (`finish_attach`) : sa relecture complète voit la fenêtre
    /// définitive, historique compris, et son émulateur a déjà la bonne taille.
    fn begin_attach(&mut self, grid: GridSize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        // Le rappel du client tourne sur son fil de lecture et ne doit jamais bloquer : un canal sans borne, vidé côté
        // interface. Une erreur d'envoi veut dire que la vue est fermée.
        let (sender, receiver) = mpsc::unbounded::<(Instant, ControlEvent)>();
        let spawned = ControlClient::spawn(&self.tmux, &pending.name, move |event| {
            let _ = sender.unbounded_send((Instant::now(), event));
        });
        let mut client = match spawned {
            Ok(client) => client,
            Err(error) => {
                hud::log_event(&format!(
                    "browser journal attach failed session={}: {error}",
                    pending.name
                ));
                self.target = Some(JournalTarget::Notice(SESSION_GONE.to_owned()));
                cx.notify();
                return;
            }
        };
        let _ = client.send(&format!(
            "refresh-client -C {}x{}",
            grid.columns, grid.lines
        ));
        self.client = Some(client);
        self.requested = Some(grid);
        let generation = self.attach_generation;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(ATTACH_SETTLE).await;
            let _ = this.update_in(cx, |journal, window, cx| {
                journal.finish_attach(generation, pending, grid, receiver, window, cx);
            });
        })
        .detach();
    }

    /// La taille est posée : le pane est créé à la grille de la zone, et les événements du client lui sont livrés. Ceux
    /// qui sont arrivés avant sont jetés : la relecture complète du pane les couvre.
    fn finish_attach(
        &mut self,
        generation: u64,
        pending: PendingAttach,
        grid: GridSize,
        mut receiver: mpsc::UnboundedReceiver<(Instant, ControlEvent)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.attach_generation {
            return;
        }
        let Some(writer) = self.client.as_ref().map(ControlClient::writer) else {
            return;
        };
        while receiver.try_recv().is_ok() {}
        let PendingAttach {
            name,
            pane,
            project,
        } = pending;
        let view = cx.new(|cx| {
            PaneView::attach_journal(
                writer,
                pane,
                grid,
                SessionRef {
                    name: &name,
                    project: &project,
                    shell: None,
                    remote: false,
                },
                window,
                cx,
            )
        });
        let weak = view.downgrade();
        self._reader = Some(cx.spawn_in(window, async move |_, cx| {
            while let Some((arrival, event)) = receiver.next().await {
                let delivered = weak.update(cx, |pane, cx| pane.on_event(&event, arrival, cx));
                if delivered.is_err() {
                    break;
                }
            }
        }));
        self.watch_pane(&view, cx);
        self.pane = Some(view);
        cx.notify();
    }

    /// La zone du journal a une nouvelle grille : la fenêtre de la session serveur la prend, 50 ms après la dernière
    /// mesure (spec §3.3 pour les sessions). Sans client (un fichier suivi), rien à demander.
    fn request_client_size(&mut self, grid: GridSize, cx: &mut Context<Self>) {
        if self.client.is_none() || self.requested == Some(grid) {
            return;
        }
        self.requested = Some(grid);
        self.size_generation += 1;
        let generation = self.size_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SIZE_REQUEST_DELAY).await;
            let _ = this.update(cx, |journal, _| {
                if journal.size_generation == generation
                    && let Some(client) = journal.client.as_mut()
                {
                    let _ = client.send(&format!(
                        "refresh-client -C {}x{}",
                        grid.columns, grid.lines
                    ));
                }
            });
        })
        .detach();
    }
}

impl Drop for JournalView {
    fn drop(&mut self) {
        self.drop_client();
    }
}

impl Render for JournalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        if self.pane.is_none() && self.pending.is_none() {
            let text = match &self.target {
                Some(JournalTarget::Notice(text)) => text.clone(),
                _ => String::new(),
            };
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .px(theme.px(24.))
                .text_size(theme.text(13.5))
                .text_color(white(TEXT_LABEL))
                .text_center()
                .child(text)
                .into_any_element();
        }
        let fitted = self.pane.clone();
        let journal = cx.entity();
        let measured = Rc::clone(&self.measured);
        div()
            .size_full()
            .px(theme.px(BODY_MARGIN_SIDES))
            .py(theme.px(BODY_MARGIN_VERTICAL))
            .child(
                div()
                    .relative()
                    .size_full()
                    // Mesure les bornes du corps à chaque image : la grille qui y tient devient celle du pane, et celle
                    // que la session serveur reçoit de tmux. Un `fit` à la même taille ne fait rien.
                    .child(
                        canvas(
                            move |bounds, window, cx| {
                                let grid = body_grid(bounds, window, cx);
                                let (cell_width, cell_height) = device_cell_pixels(window, cx);
                                if let Some(pane) = &fitted {
                                    pane.update(cx, |pane, cx| {
                                        pane.set_cell_pixels(cell_width, cell_height);
                                        pane.fit(grid, cx);
                                    });
                                }
                                if measured.get() != Some(grid) {
                                    measured.set(Some(grid));
                                    let journal = journal.clone();
                                    window.defer(cx, move |window, cx| {
                                        journal.update(cx, |journal, cx| {
                                            journal.on_measured(grid, window, cx);
                                        });
                                    });
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .children(self.pane.clone()),
            )
            .into_any_element()
    }
}

impl BrowserPanel {
    /// Le journal de cette ligne se lit-il (session tmux, ou sortie dans un fichier) ? L'action « journal » en dépend.
    pub(super) fn has_journal(&self, row: &ServerRow, cx: &App) -> bool {
        self.servers
            .as_ref()
            .is_some_and(|servers| servers.read(cx).journal_source(row).is_available())
    }

    /// Ouvre le journal d'un serveur dans un onglet : celui qui le montre déjà devient actif, sinon un onglet naît après
    /// l'actif.
    pub(super) fn open_journal(
        &mut self,
        row: &ServerRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (project, name, port) = tab_identity(row);
        let before = super::TabSnapshot::of(&self.tabs);
        self.tabs.open_log(project, name, port);
        before.log_changes(&self.tabs);
        self.after_tab_change(&before, window, cx);
    }

    /// La cible que le journal de l'onglet doit avoir maintenant.
    fn journal_target_of(&self, kind: &TabKind, cx: &App) -> Option<JournalTarget> {
        let TabKind::Log {
            project,
            name,
            port,
        } = kind
        else {
            return None;
        };
        let target = self
            .servers
            .as_ref()
            .and_then(|servers| {
                let servers = servers.read(cx);
                let row = servers.find_row(project, name, *port)?;
                Some(journal_target(&row, &servers.journal_source(&row)))
            })
            .unwrap_or_else(|| JournalTarget::Notice(NOT_FOUND.to_owned()));
        Some(target)
    }

    /// Met le journal de l'onglet `id` d'accord avec la ligne du serveur : il naît à la première apparition de l'onglet,
    /// se rattache à la session d'un serveur relancé, et se retire avec l'onglet.
    pub(super) fn sync_journal(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(id) else {
            return;
        };
        let Some(target) = self.journal_target_of(&tab.kind.clone(), cx) else {
            return;
        };
        let Some(tmux) = self.tmux.clone() else {
            return;
        };
        let journal = match self.journals.get(&id) {
            Some(journal) => journal.clone(),
            None => {
                let journal = cx.new(|_| JournalView::new(tmux));
                let subscription = cx.subscribe_in(&journal, window, Self::on_journal_event);
                self.journal_subscriptions.insert(id, subscription);
                self.journals.insert(id, journal.clone());
                journal
            }
        };
        journal.update(cx, |journal, cx| journal.show(target, window, cx));
    }

    /// Ce que dit un journal : l'adresse ⌘-cliquée dans sa sortie s'ouvre dans un onglet de plus (P4-7). L'ouverture part au tour
    /// suivant : cacher ou créer une vue native touche AppKit, qui peut rappeler GPUI en pleine mise à jour (P4-R4).
    fn on_journal_event(
        &mut self,
        _: &Entity<JournalView>,
        event: &JournalEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let JournalEvent::OpenUrl(url) = event;
        let (panel, url) = (cx.weak_entity(), url.clone());
        window.defer(cx, move |window, cx| {
            let _ = panel.update(cx, |panel, cx| {
                panel.open_url(&url, OpenFrom::Link, window, cx)
            });
        });
    }

    /// Les journaux ouverts suivent leur ligne : un serveur relancé a une nouvelle session. Le journal de l'onglet actif
    /// naît s'il manque.
    pub(super) fn sync_journals(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut ids: Vec<TabId> = self.journals.keys().copied().collect();
        let active = self.tabs.active();
        if matches!(active.kind, TabKind::Log { .. }) && !ids.contains(&active.id) {
            ids.push(active.id);
        }
        for id in ids {
            self.sync_journal(id, window, cx);
        }
    }

    /// Le corps d'un onglet journal : la rangée du serveur (pastille, `localhost:<port> · <nom>`, actions), un filet,
    /// puis le journal.
    pub(super) fn journal_body(
        &self,
        tab: &Tab,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let TabKind::Log {
            project,
            name,
            port,
        } = &tab.kind
        else {
            return div().size_full().into_any_element();
        };
        let row = self
            .servers
            .as_ref()
            .and_then(|servers| servers.read(cx).find_row(project, name, *port));
        let mono = cx.global::<TerminalFont>().family.clone();
        let project_name = crate::project::display_name(project);
        let header = row.map(|row| {
            let slot = RowSlot {
                project_name: &project_name,
                index: 0,
                interactive: false,
                // L'onglet est déjà le journal : l'action « journal » n'a rien à y ouvrir.
                has_log: false,
            };
            div()
                .flex_none()
                // La pastille de la rangée (retrait de 10 dans la ligne) tombe sur le bord gauche du texte du journal (marge du corps).
                .px(theme.px(BODY_MARGIN_SIDES - 10.))
                .pt(theme.px(4.))
                .pb(theme.px(4.))
                .child(server_row(&row, slot, &mono, theme, cx))
        });
        let body = self.journals.get(&tab.id).cloned();
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(header)
            .child(div().flex_none().h(gpui::px(1.)).bg(white(SEPARATOR)))
            .child(div().relative().flex_1().min_h_0().children(body))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::servers::registry::{RowState, ServerKey};

    fn row(kind: RowKind, port: Option<u16>) -> ServerRow {
        ServerRow {
            kind,
            project: PathBuf::from("/work/alpha"),
            name: "web".to_owned(),
            command: "python3 -m http.server".to_owned(),
            port,
            url: String::new(),
            state: RowState::Stopped,
            origin: None,
            listener_pid: None,
            output: None,
            session_alive: false,
            extra_ports: Vec::new(),
            failure: None,
        }
    }

    fn config() -> RowKind {
        RowKind::Config(ServerKey {
            project: PathBuf::from("/work/alpha"),
            name: "web".to_owned(),
        })
    }

    #[test]
    fn each_journal_source_gives_its_target() {
        let row = row(config(), Some(8811));
        assert_eq!(
            journal_target(
                &row,
                &JournalSource::Session {
                    name: "idema-srv-1".to_owned(),
                    pane: PaneId(3)
                }
            ),
            JournalTarget::Session {
                name: "idema-srv-1".to_owned(),
                pane: PaneId(3),
                project: PathBuf::from("/work/alpha")
            }
        );
        assert_eq!(
            journal_target(&row, &JournalSource::File(PathBuf::from("/tmp/a.log"))),
            JournalTarget::File {
                path: PathBuf::from("/tmp/a.log"),
                project: PathBuf::from("/work/alpha"),
                name: "web".to_owned()
            }
        );
    }

    #[test]
    fn a_stopped_entry_gives_the_stopped_notice_whatever_its_kind() {
        let text = "Ce serveur est arrêté : aucune sortie à afficher.";
        for row in [row(config(), Some(8811)), row(config(), None)] {
            assert_eq!(
                journal_target(&row, &JournalSource::Stopped),
                JournalTarget::Notice(text.to_owned())
            );
        }
    }

    #[test]
    fn an_unavailable_journal_says_where_the_server_writes() {
        let row = row(config(), Some(8811));
        assert_eq!(
            journal_target(
                &row,
                &JournalSource::Unavailable {
                    target: Some("/dev/null".to_owned())
                }
            ),
            JournalTarget::Notice(
                "Journal indisponible : ce serveur écrit dans /dev/null".to_owned()
            )
        );
    }

    #[test]
    fn a_tab_names_a_detected_server_by_its_port_and_an_entry_without_it() {
        let detected = row(RowKind::Detected { pid: 9, port: 8814 }, Some(8814));
        assert_eq!(
            tab_identity(&detected),
            (PathBuf::from("/work/alpha"), "web".to_owned(), Some(8814))
        );
        // `dev` n'a pas de port avant son premier démarrage : l'onglet ne doit pas changer d'identité quand il vient.
        let before = tab_identity(&row(config(), None));
        let after = tab_identity(&row(config(), Some(3000)));
        assert_eq!(before, after);
        assert_eq!(before.2, None);
    }
}
