//! Les fenêtres d'une session vues par son client de contrôle (spec §4.4) : la fenêtre visible et la fenêtre
//! cachée, leurs layouts et le pane actif ; la relecture qui les décrit à l'attache ; les panes apparus ou
//! disparus ; et ce qu'Idema répare quand les panes changent sans lui.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::command::quote;
use crate::layout::{
    LayoutChange, PaneRect, Preset, WindowLayout, parse_layout, parse_layout_change,
};
use crate::protocol::{ControlEvent, PaneId, SessionId, WindowId};

/// Format des lignes de la relecture : identifiant, courante (`1`), cachée (`@idema_stash`), zoomée (`1`), layout
/// JSON complet, layout JSON affiché (le pane zoomé seul quand la fenêtre est zoomée).
const WINDOWS_FORMAT: &str = "#{window_id}\t#{window_active}\t#{@idema_stash}\t#{window_zoomed_flag}\t#{window_layout}\t#{window_visible_layout}";

/// Délai avant de retirer un pane absent des layouts : un pane qu'Idema cache ou rejoint quitte une fenêtre une
/// notification avant d'entrer dans l'autre.
pub const PANE_GONE_GRACE: Duration = Duration::from_secs(1);

/// Délai pendant lequel un `%layout-change` qui lève le zoom attend le suivant. Chaque `refresh-client -C` sur une
/// fenêtre zoomée émet un layout non zoomé (flags `*`), puis deux layouts zoomés (`*Z`) : sans cette attente, le
/// layout intermédiaire montre tous les panes un instant et le pane caché par le zoom serait vu (sonde tmux 3.8, spec
/// J4-13). Un vrai dézoom, lui, n'émet que le layout non zoomé : il s'applique à l'échéance. Une image : les trois
/// layouts d'une rafale arrivent à 0,12 ms au plus (40 rafales mesurées, tmux 3.8), et le dézoom ne traîne pas
/// derrière le zoom.
pub const UNZOOM_SETTLE: Duration = Duration::from_millis(16);

/// Une fenêtre de la session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowState {
    pub id: WindowId,
    /// Le layout complet : tous les panes de la fenêtre, même zoomée. La liste des panes, la réparation et le
    /// voisinage s'en servent (le layout affiché d'une fenêtre zoomée ne montre qu'un pane).
    pub layout: Option<WindowLayout>,
    /// Le pane que le zoom de tmux montre seul sur toute la fenêtre ; `None` sans zoom.
    pub zoomed: Option<PaneId>,
    /// Fenêtre courante de la session : la visible, sauf quand la visible a disparu.
    pub current: bool,
    /// `@idema_stash` ; `None` tant qu'une relecture ne l'a pas dit (fenêtre apparue depuis la dernière).
    pub stash: Option<bool>,
}

/// Relecture des fenêtres de la session, encadrée de marqueurs comme les relectures de pane : une capture d'écran
/// qui contiendrait des lignes de même forme ne peut pas s'y glisser.
#[derive(Debug)]
pub struct WindowsQuery {
    start: Vec<u8>,
    end: Vec<u8>,
    collected: Option<Vec<WindowState>>,
}

impl WindowsQuery {
    /// `token` doit être unique par relecture.
    pub fn new(token: &str) -> Self {
        Self {
            start: format!("idema-windows-{token}-start").into_bytes(),
            end: format!("idema-windows-{token}-end").into_bytes(),
            collected: None,
        }
    }

    /// Une ligne de commandes pour le client de contrôle : `list-windows` vise la session attachée.
    pub fn command_line(&self) -> String {
        let text = |bytes: &[u8]| quote(&String::from_utf8_lossy(bytes));
        format!(
            "display-message -p {} ; list-windows -F {} ; display-message -p {}",
            text(&self.start),
            quote(WINDOWS_FORMAT),
            text(&self.end)
        )
    }

    /// Reçoit les lignes de chaque réponse (`ok` dit si tmux l'a acceptée) ; rend les fenêtres quand le marqueur de
    /// fin passe. Une réponse en erreur arrivée après le marqueur de début est celle de `list-windows` : la chaîne
    /// s'arrête là et le marqueur de fin ne viendra jamais, la relecture est finie, en échec. Avant le marqueur de
    /// début, elle répond à une autre commande.
    pub fn advance(&mut self, ok: bool, lines: &[Vec<u8>]) -> QueryStep {
        if !ok {
            if self.collected.take().is_none() {
                return QueryStep::Pending;
            }
            let text: Vec<_> = lines
                .iter()
                .map(|line| String::from_utf8_lossy(line))
                .collect();
            return QueryStep::Failed(text.join("\n"));
        }
        for line in lines {
            if *line == self.start {
                self.collected = Some(Vec::new());
            } else if *line == self.end {
                return self
                    .collected
                    .take()
                    .map_or(QueryStep::Pending, QueryStep::Done);
            } else if let Some(windows) = self.collected.as_mut()
                && let Some(window) = parse_window_line(&String::from_utf8_lossy(line))
            {
                windows.push(window);
            }
        }
        QueryStep::Pending
    }
}

/// Où en est une relecture des fenêtres après une réponse.
#[derive(Debug, PartialEq, Eq)]
pub enum QueryStep {
    /// Les marqueurs n'ont pas tous passé.
    Pending,
    /// Le marqueur de fin est passé : les fenêtres lues.
    Done(Vec<WindowState>),
    /// `list-windows` a échoué : le texte de l'erreur de tmux.
    Failed(String),
}

fn parse_window_line(line: &str) -> Option<WindowState> {
    let mut fields = line.splitn(6, '\t');
    let id = WindowId(fields.next()?.strip_prefix('@')?.parse().ok()?);
    let current = fields.next()? == "1";
    let stash = fields.next()? == "1";
    let zoomed = fields.next()? == "1";
    let layout = parse_layout(fields.next()?);
    let visible = parse_layout(fields.next()?);
    Some(WindowState {
        id,
        layout,
        zoomed: visible
            .as_ref()
            .and_then(|visible| zoomed_pane(zoomed, visible)),
        current,
        stash: Some(stash),
    })
}

/// Le pane que montre une fenêtre zoomée : l'unique pane de son layout affiché.
fn zoomed_pane(zoomed: bool, visible: &WindowLayout) -> Option<PaneId> {
    match visible.panes().as_slice() {
        [only] if zoomed => Some(only.pane),
        _ => None,
    }
}

/// Les fenêtres d'une session, tenues à jour par les notifications de son client de contrôle.
#[derive(Debug, Default)]
pub struct SessionWindows {
    windows: Vec<WindowState>,
    /// La session à laquelle le client est attaché, apprise de `%session-changed` : tmux envoie les changements
    /// de fenêtre courante de toutes les sessions à chaque client de contrôle, et seuls les siens comptent.
    session: Option<SessionId>,
    /// Le `%layout-change` non zoomé d'une fenêtre zoomée, retenu jusqu'au suivant ou à l'échéance.
    held_unzoom: Option<HeldUnzoom>,
}

/// Un layout non zoomé en attente (voir `UNZOOM_SETTLE`).
#[derive(Debug)]
struct HeldUnzoom {
    window: WindowId,
    event: ControlEvent,
    since: Instant,
}

impl SessionWindows {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remplace tout par une relecture : elle décrit la session à l'instant de sa réponse, et les notifications
    /// suivantes s'appliquent par-dessus.
    pub fn replace(&mut self, windows: Vec<WindowState>) {
        self.windows = windows;
        // La relecture date d'après tout ce que le flux a dit jusque-là : un layout retenu est périmé.
        self.held_unzoom = None;
    }

    /// Applique une notification du flux. Comme `on_event`, sauf qu'un layout qui lèverait le zoom d'une fenêtre
    /// zoomée est retenu : s'il est suivi d'un autre layout de la même fenêtre, il est abandonné (c'est le
    /// layout intermédiaire d'un redimensionnement), sinon `release_due` l'applique à l'échéance. Tout autre
    /// événement applique d'abord le layout retenu, pour garder l'ordre du flux. Vrai si les fenêtres ont changé.
    pub fn on_notification(&mut self, event: &ControlEvent, now: Instant) -> bool {
        let lifting = self.lifting_zoom(event);
        let mut changed = false;
        if let Some(held) = self.held_unzoom.take() {
            let superseded = matches!(
                event,
                ControlEvent::LayoutChange { window, .. } if *window == held.window
            );
            if !superseded {
                changed |= self.on_event(&held.event);
            }
        }
        if let Some(window) = lifting {
            self.held_unzoom = Some(HeldUnzoom {
                window,
                event: event.clone(),
                since: now,
            });
            return changed;
        }
        changed | self.on_event(event)
    }

    /// Applique le layout retenu quand `UNZOOM_SETTLE` s'est écoulé. Vrai si les fenêtres ont changé.
    pub fn release_due(&mut self, now: Instant) -> bool {
        let due = self
            .held_unzoom
            .as_ref()
            .is_some_and(|held| now.saturating_duration_since(held.since) >= UNZOOM_SETTLE);
        if !due {
            return false;
        }
        self.held_unzoom
            .take()
            .is_some_and(|held| self.on_event(&held.event))
    }

    /// Un layout est retenu : il faut revenir à l'échéance.
    pub fn has_held_layout(&self) -> bool {
        self.held_unzoom.is_some()
    }

    /// La fenêtre dont ce `%layout-change` lèverait le zoom ; `None` pour tout autre événement.
    fn lifting_zoom(&self, event: &ControlEvent) -> Option<WindowId> {
        let ControlEvent::LayoutChange { window, rest } = event else {
            return None;
        };
        let was_zoomed = self
            .windows
            .iter()
            .any(|state| state.id == *window && state.zoomed.is_some());
        let change = parse_layout_change(rest)?;
        (was_zoomed && zoomed_pane(change.zoomed, &change.visible).is_none()).then_some(*window)
    }

    /// Applique une notification. Vrai si les fenêtres ont changé.
    pub fn on_event(&mut self, event: &ControlEvent) -> bool {
        match event {
            ControlEvent::LayoutChange { window, rest } => {
                let Some(LayoutChange {
                    layout,
                    visible,
                    current,
                    zoomed,
                }) = parse_layout_change(rest)
                else {
                    return false;
                };
                let state = self.entry(*window);
                state.layout = Some(layout);
                state.zoomed = zoomed_pane(zoomed, &visible);
                if current == Some(true) {
                    self.set_current(*window);
                } else if let Some(current) = current {
                    self.entry(*window).current = current;
                }
                true
            }
            ControlEvent::WindowAdd { window } => {
                self.entry(*window);
                true
            }
            ControlEvent::WindowClose { window } => {
                let before = self.windows.len();
                self.windows.retain(|state| state.id != *window);
                self.windows.len() != before
            }
            ControlEvent::SessionChanged { session } => {
                self.session = Some(*session);
                false
            }
            ControlEvent::SessionWindowChanged { session, window } => {
                // Une fenêtre courante d'une autre session ne dit rien des nôtres : la marquer courante ici ferait
                // disparaître les panes visibles le temps d'une relecture.
                if self.session.is_some_and(|own| own != *session) {
                    return false;
                }
                self.entry(*window);
                self.set_current(*window);
                true
            }
            ControlEvent::WindowPaneChanged { window, pane } => {
                match self
                    .windows
                    .iter_mut()
                    .find(|state| state.id == *window)
                    .and_then(|state| state.layout.as_mut())
                {
                    Some(layout) if layout.active != Some(*pane) => {
                        layout.active = Some(*pane);
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    fn entry(&mut self, window: WindowId) -> &mut WindowState {
        let at = match self.windows.iter().position(|state| state.id == window) {
            Some(at) => at,
            None => {
                self.windows.push(WindowState {
                    id: window,
                    layout: None,
                    zoomed: None,
                    current: false,
                    stash: None,
                });
                self.windows.len() - 1
            }
        };
        &mut self.windows[at]
    }

    fn set_current(&mut self, window: WindowId) {
        for state in &mut self.windows {
            state.current = state.id == window;
        }
    }

    /// Une fenêtre dont on ignore si elle est la cachée : une relecture le dira.
    pub fn needs_query(&self) -> bool {
        self.windows.iter().any(|state| state.stash.is_none())
    }

    /// La fenêtre visible : la courante, si elle n'est pas la cachée.
    pub fn visible(&self) -> Option<&WindowState> {
        self.windows
            .iter()
            .find(|state| state.current && state.stash == Some(false))
    }

    /// La fenêtre visible a disparu et tmux montre la cachée : il faut en sortir un pane (spec §4.4).
    pub fn current_is_stash(&self) -> bool {
        self.windows
            .iter()
            .any(|state| state.current && state.stash == Some(true))
    }

    pub fn visible_layout(&self) -> Option<&WindowLayout> {
        self.visible()?.layout.as_ref()
    }

    /// Les panes visibles, dans l'ordre du layout.
    pub fn visible_panes(&self) -> Vec<PaneRect> {
        self.visible_layout()
            .map(WindowLayout::panes)
            .unwrap_or_default()
    }

    /// Les panes de la fenêtre cachée : ceux qu'Idema a cachés et peut rejoindre. Une fenêtre ouverte hors d'Idema
    /// n'en fait pas partie ; ses panes restent suivis (`all_panes`).
    pub fn hidden_panes(&self) -> Vec<PaneRect> {
        self.windows
            .iter()
            .filter(|state| state.stash == Some(true))
            .filter_map(|state| state.layout.as_ref())
            .flat_map(WindowLayout::panes)
            .collect()
    }

    /// Tous les panes de la session, avec leur taille.
    pub fn all_panes(&self) -> Vec<PaneRect> {
        self.windows
            .iter()
            .filter_map(|state| state.layout.as_ref())
            .flat_map(WindowLayout::panes)
            .collect()
    }

    /// Le pane que le zoom montre seul dans la fenêtre visible ; `None` sans zoom.
    pub fn zoomed_pane(&self) -> Option<PaneId> {
        self.visible()?.zoomed
    }

    /// Les panes affichés : le pane zoomé sur toute la fenêtre s'il y en a un, sinon les panes visibles.
    pub fn displayed_panes(&self) -> Vec<PaneRect> {
        match (self.zoomed_pane(), self.visible_layout()) {
            (Some(pane), Some(layout)) => vec![PaneRect {
                pane,
                x: 0,
                y: 0,
                width: layout.width(),
                height: layout.height(),
            }],
            _ => self.visible_panes(),
        }
    }

    /// Le pane actif de la fenêtre visible : celui qui a le focus.
    pub fn active_pane(&self) -> Option<PaneId> {
        self.visible_layout()?.active
    }
}

/// Les panes suivis par la vue d'une session. Un pane absent des layouts n'est retiré qu'après
/// `PANE_GONE_GRACE` : un pane caché ou rejoint manque un instant entre deux notifications.
#[derive(Debug, Default)]
pub struct PaneRoster {
    known: Vec<PaneId>,
    missing_since: HashMap<PaneId, Instant>,
}

/// Ce qui change dans les panes suivis.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RosterChange {
    pub added: Vec<PaneId>,
    pub removed: Vec<PaneId>,
}

impl PaneRoster {
    pub fn new() -> Self {
        Self::default()
    }

    /// `present` : les panes des layouts à l'instant `now`. À rappeler à chaque changement et à chaque pas
    /// d'horloge, pour que les absences dépassées partent.
    pub fn update(&mut self, present: &[PaneId], now: Instant) -> RosterChange {
        let mut change = RosterChange::default();
        for pane in present {
            self.missing_since.remove(pane);
            if !self.known.contains(pane) {
                self.known.push(*pane);
                change.added.push(*pane);
            }
        }
        let missing: Vec<PaneId> = self
            .known
            .iter()
            .filter(|pane| !present.contains(pane))
            .copied()
            .collect();
        for pane in missing {
            let since = *self.missing_since.entry(pane).or_insert(now);
            if now.saturating_duration_since(since) >= PANE_GONE_GRACE {
                self.missing_since.remove(&pane);
                self.known.retain(|known| *known != pane);
                change.removed.push(pane);
            }
        }
        change
    }
}

/// Ce qu'Idema fait quand les panes d'une session ont changé sans lui (spec §4.4, fermeture d'un pane).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutRepair {
    Nothing,
    /// La fenêtre visible a disparu : rejoindre un pane caché.
    Restore,
    /// Appliquer le premier preset du menu qui a autant de panes qu'il en reste.
    Apply(Preset),
}

pub fn layout_repair(windows: &SessionWindows, preset: Preset) -> LayoutRepair {
    if windows.current_is_stash() {
        return LayoutRepair::Restore;
    }
    let Some(layout) = windows.visible_layout() else {
        return LayoutRepair::Nothing;
    };
    let count = layout.panes().len();
    if count == preset.panes() {
        return LayoutRepair::Nothing;
    }
    Preset::first_with(count).map_or(LayoutRepair::Nothing, LayoutRepair::Apply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single(pane: u32) -> String {
        format!(
            r#"{{"V":2,"L":{{"t":"p","w":120,"h":40,"x":0,"y":0,"a":true,"i":0,"I":"%{pane}"}}}}"#
        )
    }

    fn columns(left: u32, right: u32) -> String {
        format!(
            r#"{{"V":2,"L":{{"t":"h","w":120,"h":40,"x":0,"y":0,"c":[{{"t":"p","w":60,"h":40,"x":0,"y":0,"a":true,"i":0,"I":"%{left}"}},{{"t":"p","w":59,"h":40,"x":61,"y":0,"i":1,"I":"%{right}"}}]}}}}"#
        )
    }

    fn layout_change(window: u32, json: &str, flags: &str) -> ControlEvent {
        ControlEvent::LayoutChange {
            window: WindowId(window),
            rest: format!("{json} {json} {flags}"),
        }
    }

    fn lines(items: &[String]) -> Vec<Vec<u8>> {
        items.iter().map(|line| line.as_bytes().to_vec()).collect()
    }

    fn panes(rects: Vec<PaneRect>) -> Vec<u32> {
        rects.iter().map(|rect| rect.pane.0).collect()
    }

    #[test]
    fn the_query_reads_windows_between_its_markers_only() {
        let mut query = WindowsQuery::new("7");
        let line = query.command_line();
        assert!(line.starts_with("display-message -p 'idema-windows-7-start' ; list-windows -F '"));
        assert!(!line.contains('\n'));
        let reply = lines(&[
            format!("@0\t1\t\t0\t{}\t{}", columns(0, 1), columns(0, 1)),
            "idema-windows-7-start".into(),
            format!("@0\t1\t\t0\t{}\t{}", columns(0, 1), columns(0, 1)),
            format!("@1\t0\t1\t0\t{}\t{}", single(2), single(2)),
        ]);
        assert_eq!(query.advance(true, &reply), QueryStep::Pending);
        let QueryStep::Done(windows) = query.advance(true, &lines(&["idema-windows-7-end".into()]))
        else {
            panic!("the end marker closes the query");
        };
        assert_eq!(windows.len(), 2);
        assert_eq!((windows[0].current, windows[0].stash), (true, Some(false)));
        assert_eq!((windows[1].current, windows[1].stash), (false, Some(true)));
    }

    #[test]
    fn a_session_shows_its_visible_window_and_counts_hidden_panes() {
        let mut windows = SessionWindows::new();
        windows.replace(vec![
            WindowState {
                id: WindowId(0),
                layout: parse_layout(&columns(0, 1)),
                zoomed: None,
                current: true,
                stash: Some(false),
            },
            WindowState {
                id: WindowId(1),
                layout: parse_layout(&single(2)),
                zoomed: None,
                current: false,
                stash: Some(true),
            },
        ]);
        assert_eq!(panes(windows.visible_panes()), [0, 1]);
        assert_eq!(panes(windows.hidden_panes()), [2]);
        assert_eq!(panes(windows.all_panes()), [0, 1, 2]);
        assert_eq!(windows.active_pane(), Some(PaneId(0)));
        assert!(!windows.needs_query() && !windows.current_is_stash());
    }

    #[test]
    fn notifications_follow_hiding_a_pane_then_closing_the_visible_window() {
        // L'enchaînement relevé par la sonde 1 du 15/10, réduit à deux panes.
        let mut windows = SessionWindows::new();
        windows.on_event(&ControlEvent::SessionChanged {
            session: SessionId(0),
        });
        assert!(windows.on_event(&layout_change(0, &columns(0, 1), "*")));
        assert_eq!(windows.visible(), None, "unknown until queried");
        windows.replace(vec![WindowState {
            id: WindowId(0),
            layout: parse_layout(&columns(0, 1)),
            zoomed: None,
            current: true,
            stash: Some(false),
        }]);
        windows.on_event(&layout_change(0, &single(0), "*"));
        windows.on_event(&ControlEvent::WindowAdd {
            window: WindowId(1),
        });
        windows.on_event(&layout_change(1, &single(1), ""));
        assert!(windows.needs_query());
        assert_eq!(panes(windows.visible_panes()), [0]);
        assert_eq!(panes(windows.all_panes()), [0, 1]);
        // La visible se ferme : tmux passe à la cachée.
        windows.on_event(&ControlEvent::SessionWindowChanged {
            session: SessionId(0),
            window: WindowId(1),
        });
        windows.on_event(&ControlEvent::WindowClose {
            window: WindowId(0),
        });
        windows.replace(vec![WindowState {
            id: WindowId(1),
            layout: parse_layout(&single(1)),
            zoomed: None,
            current: true,
            stash: Some(true),
        }]);
        assert!(windows.current_is_stash());
        assert_eq!(windows.visible(), None);
        assert_eq!(panes(windows.hidden_panes()), [1]);
    }

    #[test]
    fn another_sessions_window_change_is_ignored() {
        // tmux envoie `%session-window-changed` de toutes les sessions à chaque client de contrôle (relevé sur
        // tmux 3.8 : le client de `$0` a reçu `%session-window-changed $1 @2`).
        let mut windows = SessionWindows::new();
        windows.on_event(&ControlEvent::SessionChanged {
            session: SessionId(0),
        });
        windows.replace(vec![WindowState {
            id: WindowId(0),
            layout: parse_layout(&single(0)),
            zoomed: None,
            current: true,
            stash: Some(false),
        }]);
        let foreign = ControlEvent::SessionWindowChanged {
            session: SessionId(1),
            window: WindowId(2),
        };
        assert!(!windows.on_event(&foreign), "nothing changed");
        assert_eq!(
            panes(windows.visible_panes()),
            [0],
            "the visible window stays"
        );
        assert!(!windows.needs_query(), "no unknown window was created");
        // Le même événement de notre session s'applique.
        let own = ControlEvent::SessionWindowChanged {
            session: SessionId(0),
            window: WindowId(2),
        };
        assert!(windows.on_event(&own));
        assert_eq!(windows.visible(), None);
    }

    #[test]
    fn a_window_opened_outside_idema_is_tracked_but_not_hidden() {
        let mut windows = SessionWindows::new();
        windows.replace(vec![
            WindowState {
                id: WindowId(0),
                layout: parse_layout(&single(0)),
                zoomed: None,
                current: true,
                stash: Some(false),
            },
            WindowState {
                id: WindowId(4),
                layout: parse_layout(&single(9)),
                zoomed: None,
                current: false,
                stash: Some(false),
            },
        ]);
        assert!(windows.hidden_panes().is_empty());
        assert_eq!(panes(windows.all_panes()), [0, 9]);
    }

    #[test]
    fn the_active_pane_follows_tmux() {
        let mut windows = SessionWindows::new();
        windows.replace(vec![WindowState {
            id: WindowId(0),
            layout: parse_layout(&columns(0, 1)),
            zoomed: None,
            current: true,
            stash: Some(false),
        }]);
        assert!(windows.on_event(&ControlEvent::WindowPaneChanged {
            window: WindowId(0),
            pane: PaneId(1),
        }));
        assert_eq!(windows.active_pane(), Some(PaneId(1)));
        assert!(!windows.on_event(&ControlEvent::WindowPaneChanged {
            window: WindowId(0),
            pane: PaneId(1),
        }));
        assert!(!windows.on_event(&ControlEvent::SessionsChanged));
    }

    #[test]
    fn a_pane_moving_between_windows_is_kept_and_a_dead_one_leaves_after_the_grace() {
        let mut roster = PaneRoster::new();
        let start = Instant::now();
        let change = roster.update(&[PaneId(0), PaneId(1)], start);
        assert_eq!(change.added, [PaneId(0), PaneId(1)]);
        // %1 manque un instant (il passe dans la fenêtre cachée), puis revient.
        assert_eq!(roster.update(&[PaneId(0)], start), RosterChange::default());
        assert_eq!(
            roster.update(&[PaneId(0), PaneId(1)], start + Duration::from_millis(20)),
            RosterChange::default()
        );
        // %0 meurt : il part une fois la grâce passée, pas avant.
        let later = start + Duration::from_millis(100);
        assert_eq!(roster.update(&[PaneId(1)], later), RosterChange::default());
        assert_eq!(
            roster.update(&[PaneId(1)], later + PANE_GONE_GRACE / 2),
            RosterChange::default()
        );
        let change = roster.update(&[PaneId(1)], later + PANE_GONE_GRACE);
        assert_eq!(change.removed, [PaneId(0)]);
        assert!(change.added.is_empty());
    }

    #[test]
    fn repair_restores_a_closed_visible_window_and_fits_the_preset_to_the_panes_left() {
        let mut windows = SessionWindows::new();
        windows.replace(vec![WindowState {
            id: WindowId(0),
            layout: parse_layout(&columns(0, 1)),
            zoomed: None,
            current: true,
            stash: Some(false),
        }]);
        assert_eq!(
            layout_repair(&windows, Preset::Columns2),
            LayoutRepair::Nothing
        );
        assert_eq!(
            layout_repair(&windows, Preset::Rows2),
            LayoutRepair::Nothing
        );
        assert_eq!(
            layout_repair(&windows, Preset::Columns3),
            LayoutRepair::Apply(Preset::Columns2)
        );
        windows.replace(vec![WindowState {
            id: WindowId(1),
            layout: parse_layout(&single(1)),
            zoomed: None,
            current: true,
            stash: Some(true),
        }]);
        assert_eq!(
            layout_repair(&windows, Preset::Columns3),
            LayoutRepair::Restore
        );
        assert_eq!(
            layout_repair(&SessionWindows::new(), Preset::Single),
            LayoutRepair::Nothing
        );
    }

    /// La sonde du 16/10 (tmux 3.8) : le layout complet, puis le pane actif seul.
    const ZOOM_FULL: &str = r#"{"V":2,"L":{"t":"h","w":120,"h":40,"x":0,"y":0,"c":[{"t":"p","w":60,"h":40,"x":0,"y":0,"a":true,"i":0,"I":"%0"},{"t":"p","w":59,"h":40,"x":61,"y":0,"l":0,"i":1,"I":"%1"}]}}"#;
    const ZOOM_VISIBLE: &str =
        r#"{"V":2,"L":{"t":"p","w":120,"h":40,"x":0,"y":0,"a":true,"i":0,"I":"%0"}}"#;

    fn zoom_change(flags: &str) -> ControlEvent {
        let visible = if flags.contains('Z') {
            ZOOM_VISIBLE
        } else {
            ZOOM_FULL
        };
        ControlEvent::LayoutChange {
            window: WindowId(0),
            rest: format!("{ZOOM_FULL} {visible} {flags}"),
        }
    }

    fn visible_window(layout: &str) -> SessionWindows {
        let mut windows = SessionWindows::new();
        windows.replace(vec![WindowState {
            id: WindowId(0),
            layout: parse_layout(layout),
            zoomed: None,
            current: true,
            stash: Some(false),
        }]);
        windows
    }

    #[test]
    fn a_zoom_keeps_the_full_layout_and_names_the_zoomed_pane() {
        let mut windows = visible_window(ZOOM_FULL);
        assert!(windows.on_event(&zoom_change("*Z")));
        assert_eq!(windows.zoomed_pane(), Some(PaneId(0)));
        assert_eq!(panes(windows.visible_panes()), [0, 1], "roster stays full");
        let displayed = windows.displayed_panes();
        assert_eq!(
            displayed
                .iter()
                .map(|r| (r.pane.0, r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>(),
            [(0, 0, 0, 120, 40)],
            "the zoomed pane fills the window"
        );
    }

    #[test]
    fn an_unzoom_clears_the_zoomed_pane() {
        let mut windows = visible_window(ZOOM_FULL);
        windows.on_event(&zoom_change("*Z"));
        assert!(windows.on_event(&zoom_change("*")));
        assert_eq!(windows.zoomed_pane(), None);
        assert_eq!(panes(windows.displayed_panes()), [0, 1]);
    }

    #[test]
    fn a_lone_unzoom_is_held_then_applied_when_due() {
        let mut windows = visible_window(ZOOM_FULL);
        let start = Instant::now();
        windows.on_notification(&zoom_change("*Z"), start);
        assert!(!windows.on_notification(&zoom_change("*"), start));
        assert!(windows.has_held_layout());
        assert_eq!(windows.zoomed_pane(), Some(PaneId(0)), "still zoomed");
        assert!(!windows.release_due(start + UNZOOM_SETTLE / 2));
        assert!(windows.release_due(start + UNZOOM_SETTLE));
        assert_eq!(windows.zoomed_pane(), None);
        assert!(!windows.has_held_layout());
        assert!(
            !windows.release_due(start + UNZOOM_SETTLE * 2),
            "released once"
        );
    }

    #[test]
    fn a_zoomed_layout_right_after_an_unzoom_cancels_it() {
        let mut windows = visible_window(ZOOM_FULL);
        let start = Instant::now();
        windows.on_notification(&zoom_change("*Z"), start);
        windows.on_notification(&zoom_change("*"), start);
        assert!(windows.on_notification(&zoom_change("*Z"), start));
        assert!(!windows.has_held_layout());
        assert_eq!(windows.zoomed_pane(), Some(PaneId(0)));
        assert!(
            !windows.release_due(start + UNZOOM_SETTLE * 2),
            "nothing left to apply"
        );
        assert_eq!(windows.zoomed_pane(), Some(PaneId(0)));
    }

    #[test]
    fn a_later_unzoom_replaces_the_held_one_and_restarts_the_wait() {
        let mut windows = visible_window(ZOOM_FULL);
        let start = Instant::now();
        windows.on_notification(&zoom_change("*Z"), start);
        windows.on_notification(&zoom_change("*"), start);
        let later = start + UNZOOM_SETTLE / 2;
        windows.on_notification(&zoom_change("*"), later);
        assert!(
            !windows.release_due(start + UNZOOM_SETTLE),
            "the wait restarted"
        );
        assert!(windows.release_due(later + UNZOOM_SETTLE));
        assert_eq!(windows.zoomed_pane(), None);
    }

    #[test]
    fn another_event_applies_the_held_unzoom_first_to_keep_the_stream_order() {
        let mut windows = visible_window(ZOOM_FULL);
        let start = Instant::now();
        windows.on_notification(&zoom_change("*Z"), start);
        windows.on_notification(&zoom_change("*"), start);
        let added = ControlEvent::WindowAdd {
            window: WindowId(1),
        };
        assert!(windows.on_notification(&added, start));
        assert_eq!(windows.zoomed_pane(), None, "the unzoom came first");
        assert!(!windows.has_held_layout());
    }

    #[test]
    fn a_failed_list_windows_ends_the_query_as_failed() {
        let mut query = WindowsQuery::new("f");
        let error = lines(&["can't find session: nosuch".into()]);
        // Avant le marqueur de début, une réponse en erreur est celle d'une autre commande : elle n'est pas à la relecture.
        assert_eq!(query.advance(false, &error), QueryStep::Pending);
        assert_eq!(
            query.advance(true, &lines(&["idema-windows-f-start".into()])),
            QueryStep::Pending
        );
        assert_eq!(
            query.advance(false, &error),
            QueryStep::Failed("can't find session: nosuch".into())
        );
        // La relecture est finie : une réponse en erreur de plus ne la relance pas.
        assert_eq!(query.advance(false, &error), QueryStep::Pending);
    }

    #[test]
    fn a_windows_query_drops_the_held_unzoom() {
        let mut windows = visible_window(ZOOM_FULL);
        let start = Instant::now();
        windows.on_notification(&zoom_change("*Z"), start);
        windows.on_notification(&zoom_change("*"), start);
        windows.replace(vec![WindowState {
            id: WindowId(0),
            layout: parse_layout(ZOOM_FULL),
            zoomed: Some(PaneId(0)),
            current: true,
            stash: Some(false),
        }]);
        assert!(!windows.has_held_layout());
        assert!(!windows.release_due(start + UNZOOM_SETTLE));
        assert_eq!(windows.zoomed_pane(), Some(PaneId(0)));
    }

    #[test]
    fn an_unzoom_of_an_unzoomed_window_is_applied_at_once() {
        let mut windows = visible_window(ZOOM_FULL);
        assert!(windows.on_notification(&zoom_change("*"), Instant::now()));
        assert!(!windows.has_held_layout());
    }

    #[test]
    fn the_windows_query_reads_the_zoom() {
        let mut query = WindowsQuery::new("z");
        let reply = lines(&[
            "idema-windows-z-start".into(),
            format!("@0\t1\t\t1\t{ZOOM_FULL}\t{ZOOM_VISIBLE}"),
            format!("@1\t0\t1\t0\t{}\t{}", single(2), single(2)),
            "idema-windows-z-end".into(),
        ]);
        let QueryStep::Done(states) = query.advance(true, &reply) else {
            panic!("the end marker closes the query");
        };
        let mut windows = SessionWindows::new();
        windows.replace(states);
        assert_eq!(windows.zoomed_pane(), Some(PaneId(0)));
        assert_eq!(panes(windows.visible_panes()), [0, 1]);
    }

    #[test]
    fn a_zoomed_grid_is_not_mistaken_for_a_single_pane_preset() {
        let grid = r#"{"V":2,"L":{"t":"v","w":120,"h":40,"x":0,"y":0,"c":[{"t":"h","w":120,"h":20,"x":0,"y":0,"c":[{"t":"p","w":60,"h":20,"x":0,"y":0,"a":true,"i":0,"I":"%0"},{"t":"p","w":59,"h":20,"x":61,"y":0,"i":1,"I":"%1"}]},{"t":"h","w":120,"h":19,"x":0,"y":21,"c":[{"t":"p","w":60,"h":19,"x":0,"y":21,"i":2,"I":"%2"},{"t":"p","w":59,"h":19,"x":61,"y":21,"i":3,"I":"%3"}]}]}}"#;
        let mut windows = visible_window(grid);
        windows.on_event(&ControlEvent::LayoutChange {
            window: WindowId(0),
            rest: format!("{grid} {ZOOM_VISIBLE} *Z"),
        });
        assert_eq!(windows.zoomed_pane(), Some(PaneId(0)));
        assert_eq!(
            layout_repair(&windows, Preset::Grid2x2),
            LayoutRepair::Nothing
        );
    }
}
