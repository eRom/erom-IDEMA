//! La page d'accueil du volet Browser (spec P4 §4.4) : un groupe « Serveurs de <projet> » par projet connu (le projet de
//! contexte d'abord), ses lignes de serveur et leurs actions, le sous-groupe des serveurs repérés hors de `launch.json`,
//! les erreurs de configuration, puis les favoris du `config.toml`. Les cotes sont celles de la maquette A
//! (`servers-placement.html`, `.srv`, `.sdot`, `.act`, `.group`), à l'échelle de l'interface comme toutes les autres.
//! Les fonctions pures de présentation (actions d'une ligne, textes, pastille du bouton) sont testées sans fenêtre.

use std::path::Path;
use std::time::{Duration, Instant};

use gpui::{
    ClickEvent, Context, Div, FontWeight, MouseButton, MouseDownEvent, Rgba, SharedString,
    Stateful, div, prelude::*, px, svg,
};
use url::Url;

use super::{BrowserEvent, BrowserPanel};
use crate::assets::Icon;
use crate::browser::BrowserSettings;
use crate::browser::tabs::OpenFrom;
use crate::config::Favorite;
use crate::help_bubble::HelpBubble;
use crate::servers::launch::EntryError;
use crate::servers::registry::{ExtraPort, Origin, ProjectGroup, RowKind, RowState, ServerRow};
use crate::terminal_element::TerminalFont;
use crate::theme::{
    BUTTON_HOVER, ERROR, FOCUS_TEXT, HOVER, RUNNING, TEXT_LABEL, TEXT_MIN, TEXT_SECONDARY, Theme,
    WAITING, color, white,
};

/// La ligne d'un serveur ou d'un favori : 46 de haut, rayon 7, survol blanc à 5 % (`.srv` de la maquette A).
const ROW_HEIGHT: f32 = 46.;
const ROW_RADIUS: f32 = 7.;
/// L'écart entre la pastille, le texte et chaque action (`.srv { gap: 12px }`, les actions en sont des enfants directs).
const ROW_GAP: f32 = 12.;
/// La pastille d'état (`.sdot` : 7 px, halo de 3 px à 16 % pour « en marche », anneau de 1,5 px à 32 % pour « arrêté »).
const DOT_SIZE: f32 = 7.;
const DOT_RING: f32 = 1.5;
const DOT_RING_ALPHA: f32 = 0.32;
const DOT_HALO: f32 = 3.;
const DOT_HALO_ALPHA: f32 = 0.16;
/// Un bouton d'action (`.act` : 26 × 26, rayon 6, glyphe de 17).
const ACTION_SIZE: f32 = 26.;
const ACTION_RADIUS: f32 = 6.;
const ACTION_ICON: f32 = 17.;
/// L'écart au-dessus d'un groupe qui en suit un autre (`margin-top: 18px` du sous-groupe de la maquette).
const GROUP_GAP: f32 = 18.;

/// Le texte du vide, et le lien qui suit.
const EMPTY_TEXT: &str = "Ajoute tes favoris dans config.toml, section [[browser.favorites]]";
const EMPTY_LINK: &str = "Ouvrir config.toml";
/// Le titre du sous-groupe des serveurs repérés (spec §4.4).
const DETECTED_TITLE: &str = "Repérés hors de launch.json";

/// Une action d'une ligne de serveur, dans l'ordre où la ligne les dessine de gauche à droite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    /// `terminal` : ouvre le journal du serveur.
    Journal,
    /// `refresh` : arrête puis redémarre par IdemA (entrées de configuration seulement).
    Restart,
    /// `stop`, en rouge.
    Stop,
    /// `arrow_outward` : ouvre l'adresse du serveur.
    Open,
    /// `play_arrow`, en vert.
    Start,
}

impl RowAction {
    /// Le nom d'élément du bouton, avec le rang de sa ligne pour identité.
    fn element_name(self) -> &'static str {
        match self {
            Self::Journal => "browser-server-journal",
            Self::Restart => "browser-server-restart",
            Self::Stop => "browser-server-stop",
            Self::Open => "browser-server-open",
            Self::Start => "browser-server-start",
        }
    }

    /// Le texte de la bulle d'aide du bouton.
    pub fn label(self) -> &'static str {
        match self {
            Self::Journal => "Journal",
            Self::Restart => "Relancer",
            Self::Stop => "Arrêter",
            Self::Open => "Ouvrir",
            Self::Start => "Démarrer",
        }
    }
}

/// Les actions d'une ligne selon son état (spec §4.4, §5.3, §5.4, §6) : ouvrir et arrêter pour un serveur en marche
/// (relancer en plus pour une entrée de configuration), démarrer pour une entrée arrêtée ou en échec, rien pour un
/// démarrage en cours ni pour une ligne repérée qui vient de disparaître. Un arrêt refusé laisse ouvrir et arrêter. Le
/// journal s'ajoute en tête quand il existe (`has_log`). Une entrée dont la session serveur d'IdemA vit (`session_alive`)
/// s'arrête toujours d'un clic, quel que soit l'état de sa ligne : un démarrage lent ou en échec qui n'a pas ouvert son port
/// ne tourne pas caché (P4-R7).
pub fn row_actions(
    state: &RowState,
    kind: &RowKind,
    has_log: bool,
    session_alive: bool,
) -> Vec<RowAction> {
    let configured = matches!(kind, RowKind::Config(_));
    let mut actions = Vec::new();
    if has_log {
        actions.push(RowAction::Journal);
    }
    match state {
        RowState::Running { .. } | RowState::StopRefused { .. } => {
            if configured {
                actions.push(RowAction::Restart);
            }
            actions.push(RowAction::Stop);
            actions.push(RowAction::Open);
        }
        RowState::Stopped | RowState::Failed => {
            if configured {
                if session_alive {
                    actions.push(RowAction::Stop);
                }
                actions.push(RowAction::Start);
            }
        }
        RowState::Starting => {
            if configured && session_alive {
                actions.push(RowAction::Stop);
            }
        }
    }
    actions
}

/// « lancé par <session> », « lancé par IdemA »… : d'où vient le processus qui écoute (spec §4.4, §5.2, §6).
/// `project_name` est le nom du projet de la ligne.
pub fn origin_text(origin: &Origin, project_name: &str) -> String {
    match origin {
        Origin::Session { display_name, .. } => format!("lancé par {display_name}"),
        Origin::TerminalPanel => "lancé par le volet Terminal".to_owned(),
        Origin::Idema => "lancé par IdemA".to_owned(),
        Origin::Project => format!("lancé dans {project_name}"),
        Origin::Outside { command } if command.is_empty() => "lancé hors d'IdemA".to_owned(),
        Origin::Outside { command } => format!("lancé hors d'IdemA · {command}"),
        Origin::PortTakenBy { command } => format!("port pris par {command}"),
    }
}

/// Le premier texte d'une ligne : `localhost:<port> · <nom>`, ou le nom seul tant que le port n'est pas connu (l'entrée
/// `dev` de `package.json` avant son premier démarrage).
pub fn row_title(row: &ServerRow) -> String {
    let title = match row.port {
        Some(port) => format!("localhost:{port} · {}", row.name),
        None => row.name.clone(),
    };
    // Les ports que la session ouvre en plus se lisent ici, sur la ligne courte : la deuxième ligne, longue de la commande,
    // les couperait (P4-R7 : ce qui tourne se voit).
    let extra = extra_ports_text(&row.extra_ports);
    if extra.is_empty() {
        title
    } else {
        format!("{title} · {extra}")
    }
}

/// Le texte sous le titre : la commande, puis l'origine d'un serveur en marche ou son état (« arrêté », « démarrage »,
/// « échec au démarrage », « arrêt refusé »). Une commande déjà citée par l'origine (« lancé hors d'IdemA · <commande> »
/// d'un serveur repéré) n'est pas répétée. Un serveur repéré ne donne que son origine : son titre nomme déjà l'outil et
/// le port.
pub fn status_text(row: &ServerRow, project_name: &str) -> String {
    let status = match row.state {
        RowState::Stopped => "arrêté".to_owned(),
        RowState::Starting => "démarrage".to_owned(),
        RowState::Failed => "échec au démarrage".to_owned(),
        RowState::StopRefused { .. } => "arrêt refusé".to_owned(),
        RowState::Running { .. } => match &row.origin {
            Some(Origin::Outside { command }) if *command == row.command => {
                "lancé hors d'IdemA".to_owned()
            }
            Some(origin) => origin_text(origin, project_name),
            None => String::new(),
        },
    };
    let command = match row.kind {
        RowKind::Detected { .. } => "",
        RowKind::Config(_) => row.command.as_str(),
    };
    [command, status.as_str()]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// La cause d'un échec, sur sa propre ligne sous le texte d'état : longue (un message de tmux, un port qui n'a pas
/// répondu), elle serait coupée à la largeur du volet si elle suivait la commande. `None` sans cause.
pub fn cause_text(row: &ServerRow) -> Option<&str> {
    row.failure.as_deref().filter(|cause| !cause.is_empty())
}

/// « écoute :5173 » : les ports HTTP qu'une session ouvre sans que sa ligne les montre (elle déclare 3000, le serveur
/// prend 5173). Vide sans port en plus.
pub fn extra_ports_text(extra_ports: &[ExtraPort]) -> String {
    if extra_ports.is_empty() {
        return String::new();
    }
    let ports: Vec<String> = extra_ports
        .iter()
        .map(|extra| format!(":{}", extra.port))
        .collect();
    format!("écoute {}", ports.join(", "))
}

/// Le chiffre de la pastille du bouton Browser : le nombre de serveurs en marche, rien à zéro (spec §4.1, P4-14).
pub fn servers_badge_count(running: usize) -> Option<usize> {
    (running > 0).then_some(running)
}

/// « Serveurs de <projet> », élidé devant une voyelle : « Serveurs d'alpha » (spec §4.4). Ni le h, qui peut être aspiré,
/// ni le y ne déclenchent l'élision.
pub fn group_title(project: &Path) -> String {
    let name = crate::project::display_name(project);
    let elides = name
        .chars()
        .next()
        .and_then(|first| first.to_lowercase().next())
        .is_some_and(|first| "aeiouàáâãäåæéèêëìíîïòóôõöøœùúûüÿ".contains(first));
    if elides {
        format!("Serveurs d'{name}")
    } else {
        format!("Serveurs de {name}")
    }
}

/// La ligne d'erreur d'un `launch.json` ou d'un `package.json` illisible, dans le groupe du projet : le fichier, sa ligne
/// quand on la connaît, puis la cause (spec §4.4, §6).
pub fn error_text(error: &EntryError) -> String {
    let file = error.file.file_name().map_or_else(
        || error.file.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    match error.line {
        Some(line) => format!("{file}, ligne {line} : {}", error.message),
        None => format!("{file} : {}", error.message),
    }
}

/// Ce que devient un démarrage que la page suit (voir `StartWatch::follow`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartFollow {
    /// Pas encore : la ligne n'a pas bougé.
    Wait,
    /// Le serveur écoute.
    Running,
    /// Le démarrage a échoué.
    Failed,
    /// Plus rien à attendre : trop long, ou la ligne n'existe plus.
    GiveUp,
}

/// Combien de temps la page suit un démarrage qu'elle a demandé : le démarrage lui-même échoue après 60 s (spec §5.3).
pub const FOLLOW_START_LIMIT: Duration = Duration::from_secs(90);

/// Un démarrage (▶, « Démarrer » d'une erreur, relance) que la page suit jusqu'à son issue : le serveur écoute, ou il a
/// échoué et son journal s'ouvre (spec §5.3).
#[derive(Debug, Clone)]
pub struct StartWatch {
    since: Instant,
    /// La ligne a été vue en « démarrage » : un échec n'est alors plus celui d'avant le geste.
    seen_starting: bool,
    /// La ligne était déjà en échec au geste : son état « échec » ne compte pas avant qu'elle ait redémarré.
    failed_at_request: bool,
    /// Une relance part d'un serveur en marche : « en marche » ne vaut qu'une fois la ligne passée par l'arrêt.
    waiting_to_leave_running: bool,
}

impl StartWatch {
    /// Le démarrage demandé à `now`, la ligne étant alors dans l'état `state`. `restart` : un geste « relancer ».
    pub fn new(state: &RowState, restart: bool, now: Instant) -> Self {
        Self {
            since: now,
            seen_starting: false,
            failed_at_request: matches!(state, RowState::Failed),
            waiting_to_leave_running: restart
                && matches!(
                    state,
                    RowState::Running { .. } | RowState::StopRefused { .. }
                ),
        }
    }

    /// Ce que dit la ligne (`None` : elle a disparu) à l'instant `now`.
    pub fn follow(&mut self, row: Option<&RowState>, now: Instant) -> StartFollow {
        let Some(state) = row else {
            return StartFollow::GiveUp;
        };
        let expired = now.saturating_duration_since(self.since) >= FOLLOW_START_LIMIT;
        match state {
            RowState::Running { .. } | RowState::StopRefused { .. }
                if self.waiting_to_leave_running =>
            {
                if expired {
                    StartFollow::GiveUp
                } else {
                    StartFollow::Wait
                }
            }
            RowState::Running { .. } | RowState::StopRefused { .. } => StartFollow::Running,
            RowState::Starting => {
                self.seen_starting = true;
                self.waiting_to_leave_running = false;
                StartFollow::Wait
            }
            RowState::Failed if self.seen_starting || !self.failed_at_request => {
                StartFollow::Failed
            }
            RowState::Failed | RowState::Stopped => {
                self.waiting_to_leave_running = false;
                if expired {
                    StartFollow::GiveUp
                } else {
                    StartFollow::Wait
                }
            }
        }
    }
}

pub fn render(
    panel: &BrowserPanel,
    theme: &Theme,
    cx: &mut Context<BrowserPanel>,
) -> impl IntoElement {
    let favorites: Vec<Favorite> = cx.global::<BrowserSettings>().0.clone();
    let mono = cx.global::<TerminalFont>().family.clone();
    let groups: Vec<ProjectGroup> = panel
        .servers
        .as_ref()
        .map(|servers| servers.read(cx).groups(panel.context.as_deref()))
        .unwrap_or_default();
    let has_servers = !groups.is_empty();

    let mut index = 0;
    let mut page = div()
        .id("browser-start")
        .size_full()
        .overflow_y_scroll()
        // La maquette A : retrait de 24 en haut et en bas, 20 sur les côtés, une colonne à écart de 2.
        .px(theme.px(20.))
        .py(theme.px(24.))
        .flex()
        .flex_col()
        .gap(theme.px(2.));
    for (position, group) in groups.into_iter().enumerate() {
        let project_name = crate::project::display_name(&group.project);
        page = page.child(group_title_element(
            group_title(&group.project),
            position > 0,
            theme,
        ));
        for row in group.rows {
            let slot = RowSlot {
                project_name: &project_name,
                index,
                interactive: true,
                has_log: panel.has_journal(&row, cx),
            };
            page = page.child(server_row(&row, slot, &mono, theme, cx));
            index += 1;
        }
        for error in &group.errors {
            page = page.child(error_line(error, theme));
        }
        if !group.detected.is_empty() {
            page = page.child(group_title_element(DETECTED_TITLE.to_owned(), true, theme));
            for row in group.detected {
                let slot = RowSlot {
                    project_name: &project_name,
                    index,
                    interactive: true,
                    has_log: panel.has_journal(&row, cx),
                };
                page = page.child(server_row(&row, slot, &mono, theme, cx));
                index += 1;
            }
        }
    }
    page.child(favorites_section(favorites, has_servers, &mono, theme, cx))
}

/// Le titre d'un groupe (`.group` de la maquette A) : retrait de 10 de chaque côté, 8 sous le texte, 11/600 à 55 %.
/// Un groupe qui en suit un autre prend l'écart de 18.
fn group_title_element(text: String, after_another: bool, theme: &Theme) -> Div {
    div()
        .px(theme.px(10.))
        .pb(theme.px(8.))
        .when(after_another, |title| title.mt(theme.px(GROUP_GAP)))
        .text_size(theme.text(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(white(TEXT_MIN))
        .child(text)
}

/// La ligne d'erreur d'une configuration, à la place des entrées qu'elle prive : la pastille d'échec (`ERROR`, `#FF453A` du handoff,
/// README « Pastille d'état », la même que celle d'une ligne en échec) dans la colonne des pastilles, puis le texte au retrait
/// et au corps secondaire de la deuxième ligne d'une ligne de serveur (11,5 à `TEXT_MIN`). Seule la pastille est rouge : l'erreur
/// ne crie pas plus fort qu'une ligne en échec.
fn error_line(error: &EntryError, theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(theme.px(ROW_GAP))
        .px(theme.px(10.))
        .py(theme.px(4.))
        .child(state_dot(&RowState::Failed, theme))
        .child(
            div()
                .min_w_0()
                .text_size(theme.text(11.5))
                .text_color(white(TEXT_MIN))
                .child(error_text(error)),
        )
}

/// La section « Favoris » : leurs lignes dans l'ordre du fichier, ou le texte du vide et le lien qui ouvre le fichier.
fn favorites_section(
    favorites: Vec<Favorite>,
    after_servers: bool,
    mono: &SharedString,
    theme: &Theme,
    cx: &mut Context<BrowserPanel>,
) -> Div {
    let title = group_title_element("Favoris".to_owned(), after_servers, theme);
    let content =
        if favorites.is_empty() {
            div()
                .flex()
                .flex_col()
                .gap(theme.px(6.))
                .px(theme.px(10.))
                .text_size(theme.text(12.5))
                .child(div().text_color(white(TEXT_MIN)).child(EMPTY_TEXT))
                .child(
                    div()
                        .id("browser-open-config")
                        .cursor_pointer()
                        .text_color(color(FOCUS_TEXT))
                        .hover(|style| style.underline())
                        .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                            cx.emit(BrowserEvent::OpenConfig);
                        }))
                        .child(EMPTY_LINK),
                )
        } else {
            div().flex().flex_col().gap(theme.px(2.)).children(
                favorites.into_iter().enumerate().map(|(index, favorite)| {
                    let url = favorite.url.clone();
                    div()
                        .id(("browser-favorite", index))
                        .flex()
                        .flex_col()
                        .justify_center()
                        .h(theme.px(ROW_HEIGHT))
                        .px(theme.px(10.))
                        .rounded(theme.px(ROW_RADIUS))
                        .cursor_pointer()
                        .hover(|style| style.bg(white(HOVER)))
                        .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                            panel.open_url(&url, OpenFrom::Here, window, cx);
                        }))
                        .child(
                            div()
                                .text_size(theme.text(13.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(white(TEXT_LABEL))
                                .child(favorite.name),
                        )
                        .child(
                            div()
                                .font_family(mono.clone())
                                .text_size(theme.text(11.5))
                                .text_color(white(TEXT_MIN))
                                .child(host_of(&favorite.url)),
                        )
                }),
            )
        };
    div().child(title).child(content)
}

/// La pastille d'état d'une ligne.
fn state_dot(state: &RowState, theme: &Theme) -> Div {
    // Une case de 7 : le halo la déborde sans rien déplacer (`box-shadow: 0 0 0 3px` de la maquette), en disque concentrique.
    let slot = div().relative().flex_none().size(theme.px(DOT_SIZE));
    let dot = div().size_full().rounded_full();
    match state {
        RowState::Running { .. } | RowState::StopRefused { .. } => {
            let spread = theme.px(DOT_HALO);
            let halo = theme.px(DOT_SIZE) + spread * 2.;
            slot.child(
                div()
                    .absolute()
                    .top(-spread)
                    .left(-spread)
                    .size(halo)
                    .rounded_full()
                    .bg(Rgba {
                        a: DOT_HALO_ALPHA,
                        ..color(RUNNING)
                    }),
            )
            .child(dot.bg(color(RUNNING)))
        }
        RowState::Stopped => {
            slot.child(dot.border(px(DOT_RING)).border_color(white(DOT_RING_ALPHA)))
        }
        RowState::Starting => slot.child(dot.bg(color(WAITING))),
        RowState::Failed => slot.child(dot.bg(color(ERROR))),
    }
}

/// Où et comment une ligne se dessine : le nom du projet de la ligne, son rang (il donne leur identité aux éléments),
/// la ligne entière réagit-elle (fond de survol et clic qui ouvre le serveur : la page d'accueil, non l'en-tête d'un
/// journal) et l'existence d'un journal à ouvrir.
#[derive(Clone, Copy)]
pub(super) struct RowSlot<'a> {
    pub project_name: &'a str,
    pub index: usize,
    pub interactive: bool,
    pub has_log: bool,
}

/// Une ligne de serveur (`.srv`) : pastille, `localhost:<port> · <nom>` en mono, la commande et l'origine dessous, puis
/// les actions de l'état. Un clic sur un serveur en marche l'ouvre dans l'onglet courant, sur un serveur arrêté il ne
/// fait rien (spec §4.4).
pub(super) fn server_row(
    row: &ServerRow,
    slot: RowSlot,
    mono: &SharedString,
    theme: &Theme,
    cx: &mut Context<BrowserPanel>,
) -> Stateful<Div> {
    let RowSlot {
        project_name,
        index,
        interactive,
        has_log,
    } = slot;
    let running = matches!(
        row.state,
        RowState::Running { .. } | RowState::StopRefused { .. }
    );
    let title_color = if running { TEXT_LABEL } else { TEXT_SECONDARY };
    let actions = row_actions(&row.state, &row.kind, has_log, row.session_alive);
    let open_url = (interactive && running && !row.url.is_empty()).then(|| row.url.clone());
    let cause = cause_text(row).map(str::to_owned);
    let buttons: Vec<Stateful<Div>> = actions
        .into_iter()
        .map(|action| action_button(action, row, index, theme, cx))
        .collect();
    div()
        .id(("browser-server", index))
        .flex()
        .flex_none()
        .items_center()
        .gap(theme.px(ROW_GAP))
        // Une ligne en échec qui dit sa cause grandit de la hauteur de la cause (qui passe à la ligne).
        .when(cause.is_none(), |line| line.h(theme.px(ROW_HEIGHT)))
        .when(cause.is_some(), |line| {
            line.min_h(theme.px(ROW_HEIGHT)).py(theme.px(6.))
        })
        .px(theme.px(10.))
        .rounded(theme.px(ROW_RADIUS))
        .when(interactive, |line| {
            line.hover(|style| style.bg(white(HOVER)))
        })
        .when_some(open_url, |line, url| {
            line.cursor_pointer()
                .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                    panel.open_url(&url, OpenFrom::Here, window, cx);
                }))
        })
        .child(state_dot(&row.state, theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .font_family(mono.clone())
                        .text_size(theme.text(12.5))
                        .text_color(white(title_color))
                        .child(row_title(row)),
                )
                .child(
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_size(theme.text(11.5))
                        .text_color(white(TEXT_MIN))
                        .child(status_text(row, project_name)),
                )
                .when_some(cause, |column, cause| {
                    column.child(
                        div()
                            .text_size(theme.text(11.5))
                            .text_color(white(TEXT_MIN))
                            .child(cause),
                    )
                }),
        )
        .children(buttons)
}

/// Un bouton d'action (`.act`) : 26 × 26, rayon 6, glyphe de 17 à 60 %, rouge pour arrêter, vert pour démarrer, fond à
/// 8 % au survol. Un appui ne remonte pas à la ligne (qui ouvre le serveur au clic).
fn action_button(
    action: RowAction,
    row: &ServerRow,
    index: usize,
    theme: &Theme,
    cx: &mut Context<BrowserPanel>,
) -> Stateful<Div> {
    let (icon, tint) = match action {
        RowAction::Journal => (Icon::Terminal, white(TEXT_SECONDARY)),
        RowAction::Restart => (Icon::Refresh, white(TEXT_SECONDARY)),
        RowAction::Stop => (Icon::Stop, color(ERROR)),
        RowAction::Open => (Icon::ArrowOutward, white(TEXT_SECONDARY)),
        RowAction::Start => (Icon::PlayArrow, color(RUNNING)),
    };
    let target = row.clone();
    let label = action.label();
    div()
        .id((action.element_name(), index))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(theme.px(ACTION_SIZE))
        .rounded(theme.px(ACTION_RADIUS))
        .cursor_pointer()
        .hover(|style| style.bg(white(BUTTON_HOVER)))
        .tooltip(move |_, cx| cx.new(|_| HelpBubble::new(label)).into())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |panel, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                panel.run_row_action(action, &target, window, cx);
            }),
        )
        .child(
            svg()
                .path(icon.path())
                .size(theme.px(ACTION_ICON))
                .text_color(tint),
        )
}

/// Sous le nom d'un favori : l'hôte (avec son port), le nom du fichier pour un fichier local, l'adresse telle quelle
/// quand elle ne se lit pas.
fn host_of(url: &str) -> String {
    let Ok(parsed) = Url::parse(url) else {
        return url.to_owned();
    };
    if parsed.scheme() == "file" {
        return parsed
            .to_file_path()
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| url.to_owned());
    }
    match (parsed.host_str(), parsed.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        (None, _) => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::servers::registry::ServerKey;

    fn key() -> RowKind {
        RowKind::Config(ServerKey {
            project: PathBuf::from("/work/alpha"),
            name: "web".to_owned(),
        })
    }

    fn detected() -> RowKind {
        RowKind::Detected {
            pid: 42,
            port: 8814,
        }
    }

    fn running() -> RowState {
        RowState::Running {
            pid: 42,
            port: 8811,
        }
    }

    fn row(kind: RowKind, state: RowState, origin: Option<Origin>) -> ServerRow {
        ServerRow {
            kind,
            project: PathBuf::from("/work/alpha"),
            name: "web".to_owned(),
            command: "python3 -m http.server 8811".to_owned(),
            port: Some(8811),
            url: "http://localhost:8811".to_owned(),
            state,
            origin,
            listener_pid: Some(42),
            output: None,
            session_alive: false,
            extra_ports: Vec::new(),
            failure: None,
        }
    }

    #[test]
    fn a_running_entry_can_be_restarted_stopped_and_opened() {
        assert_eq!(
            row_actions(&running(), &key(), false, false),
            [RowAction::Restart, RowAction::Stop, RowAction::Open]
        );
    }

    #[test]
    fn a_detected_server_cannot_be_restarted() {
        assert_eq!(
            row_actions(&running(), &detected(), false, false),
            [RowAction::Stop, RowAction::Open]
        );
    }

    #[test]
    fn a_stopped_or_failed_entry_can_only_be_started_and_a_detected_one_has_nothing() {
        assert_eq!(
            row_actions(&RowState::Stopped, &key(), false, false),
            [RowAction::Start]
        );
        assert_eq!(
            row_actions(&RowState::Failed, &key(), false, false),
            [RowAction::Start]
        );
        assert!(row_actions(&RowState::Stopped, &detected(), false, false).is_empty());
        assert!(row_actions(&RowState::Failed, &detected(), true, false) == [RowAction::Journal]);
    }

    #[test]
    fn a_starting_entry_offers_nothing_but_its_log() {
        assert!(row_actions(&RowState::Starting, &key(), false, false).is_empty());
        assert_eq!(
            row_actions(&RowState::Starting, &key(), true, false),
            [RowAction::Journal]
        );
    }

    #[test]
    fn a_refused_stop_still_allows_opening_and_stopping() {
        let refused = RowState::StopRefused {
            pid: 42,
            port: 8811,
        };
        assert_eq!(
            row_actions(&refused, &detected(), false, false),
            [RowAction::Stop, RowAction::Open]
        );
    }

    #[test]
    fn a_live_session_can_always_be_stopped_whatever_the_state_of_its_row() {
        // Un démarrage lent, un échec ou une ligne arrêtée dont la session vit : Arrêter est offert (P4-R7).
        assert_eq!(
            row_actions(&RowState::Starting, &key(), false, true),
            [RowAction::Stop]
        );
        assert_eq!(
            row_actions(&RowState::Failed, &key(), true, true),
            [RowAction::Journal, RowAction::Stop, RowAction::Start]
        );
        assert_eq!(
            row_actions(&RowState::Stopped, &key(), false, true),
            [RowAction::Stop, RowAction::Start]
        );
        // Sans session vivante, rien ne change ; un serveur repéré n'a pas de session à lui.
        assert_eq!(
            row_actions(&RowState::Failed, &key(), false, false),
            [RowAction::Start]
        );
        assert!(row_actions(&RowState::Starting, &detected(), false, true).is_empty());
    }

    #[test]
    fn the_status_line_names_the_cause_of_a_failure() {
        let mut failed = row(key(), RowState::Failed, None);
        failed.failure = Some("dossier introuvable : /work/alpha".to_owned());
        // La cause a sa propre ligne : la ligne d'état ne la porte plus, elle serait coupée à la largeur du volet.
        assert_eq!(
            status_text(&failed, "alpha"),
            "python3 -m http.server 8811 · échec au démarrage"
        );
        assert_eq!(
            cause_text(&failed),
            Some("dossier introuvable : /work/alpha")
        );
        assert_eq!(cause_text(&row(key(), RowState::Failed, None)), None);
    }

    #[test]
    fn the_title_says_the_ports_a_session_opened_besides_its_own() {
        let mut elsewhere = row(key(), RowState::Starting, None);
        assert_eq!(row_title(&elsewhere), "localhost:8811 · web");
        elsewhere.extra_ports = vec![
            ExtraPort {
                pid: 60,
                port: 5173,
            },
            ExtraPort {
                pid: 61,
                port: 5174,
            },
        ];
        // Sur le titre, court : la deuxième ligne porte la commande, que la largeur couperait avant le port.
        assert_eq!(
            row_title(&elsewhere),
            "localhost:8811 · web · écoute :5173, :5174"
        );
        assert_eq!(
            status_text(&elsewhere, "alpha"),
            "python3 -m http.server 8811 · démarrage"
        );
        assert_eq!(extra_ports_text(&[]), "");
    }

    #[test]
    fn the_log_action_comes_first_whatever_the_state() {
        for state in [running(), RowState::Stopped, RowState::Failed] {
            let actions = row_actions(&state, &key(), true, false);
            assert_eq!(actions.first(), Some(&RowAction::Journal), "{state:?}");
        }
    }

    #[test]
    fn the_origin_text_names_who_started_the_process() {
        let session = Origin::Session {
            session: "idema-1".to_owned(),
            display_name: "Refonte showreel".to_owned(),
        };
        assert_eq!(origin_text(&session, "alpha"), "lancé par Refonte showreel");
        assert_eq!(origin_text(&Origin::Idema, "alpha"), "lancé par IdemA");
        assert_eq!(
            origin_text(&Origin::TerminalPanel, "alpha"),
            "lancé par le volet Terminal"
        );
        assert_eq!(origin_text(&Origin::Project, "alpha"), "lancé dans alpha");
        assert_eq!(
            origin_text(
                &Origin::Outside {
                    command: "node a.js".to_owned()
                },
                "alpha"
            ),
            "lancé hors d'IdemA · node a.js"
        );
        assert_eq!(
            origin_text(
                &Origin::Outside {
                    command: String::new()
                },
                "alpha"
            ),
            "lancé hors d'IdemA"
        );
        assert_eq!(
            origin_text(
                &Origin::PortTakenBy {
                    command: "nc -l 8811".to_owned()
                },
                "alpha"
            ),
            "port pris par nc -l 8811"
        );
    }

    #[test]
    fn the_status_line_gives_the_command_then_the_origin_or_the_state() {
        let started = row(key(), running(), Some(Origin::Idema));
        assert_eq!(
            status_text(&started, "alpha"),
            "python3 -m http.server 8811 · lancé par IdemA"
        );
        let stopped = row(key(), RowState::Stopped, None);
        assert_eq!(
            status_text(&stopped, "alpha"),
            "python3 -m http.server 8811 · arrêté"
        );
        let failed = row(key(), RowState::Failed, None);
        assert!(status_text(&failed, "alpha").ends_with("échec au démarrage"));
        let starting = row(key(), RowState::Starting, None);
        assert!(status_text(&starting, "alpha").ends_with("démarrage"));
        let refused = row(
            key(),
            RowState::StopRefused {
                pid: 42,
                port: 8811,
            },
            Some(Origin::Idema),
        );
        assert!(status_text(&refused, "alpha").ends_with("arrêt refusé"));
    }

    #[test]
    fn an_outside_process_is_named_by_its_command_only_when_it_differs_from_the_entry() {
        let other = row(
            key(),
            running(),
            Some(Origin::Outside {
                command: "nc -l 8811".to_owned(),
            }),
        );
        assert_eq!(
            status_text(&other, "alpha"),
            "python3 -m http.server 8811 · lancé hors d'IdemA · nc -l 8811"
        );
        let same = row(
            key(),
            running(),
            Some(Origin::Outside {
                command: "python3 -m http.server 8811".to_owned(),
            }),
        );
        assert_eq!(
            status_text(&same, "alpha"),
            "python3 -m http.server 8811 · lancé hors d'IdemA"
        );
    }

    #[test]
    fn the_title_has_the_port_when_it_is_known() {
        let mut with_port = row(key(), RowState::Stopped, None);
        assert_eq!(row_title(&with_port), "localhost:8811 · web");
        with_port.port = None;
        assert_eq!(row_title(&with_port), "web");
    }

    #[test]
    fn the_badge_counts_running_servers_and_is_absent_at_zero() {
        assert_eq!(servers_badge_count(0), None);
        assert_eq!(servers_badge_count(1), Some(1));
        assert_eq!(servers_badge_count(12), Some(12));
    }

    #[test]
    fn a_config_error_names_its_file_and_line() {
        let error = EntryError {
            file: PathBuf::from("/work/alpha/.claude/launch.json"),
            line: Some(3),
            message: "JSON invalide".to_owned(),
        };
        assert_eq!(error_text(&error), "launch.json, ligne 3 : JSON invalide");
        let no_line = EntryError {
            line: None,
            ..error
        };
        assert_eq!(error_text(&no_line), "launch.json : JSON invalide");
    }

    #[test]
    fn the_group_title_names_the_project_folder_and_elides_before_a_vowel() {
        assert_eq!(group_title(Path::new("/work/beta")), "Serveurs de beta");
        assert_eq!(group_title(Path::new("/work/alpha")), "Serveurs d'alpha");
        assert_eq!(group_title(Path::new("/work/Écrin")), "Serveurs d'Écrin");
        assert_eq!(group_title(Path::new("/work/Alpha")), "Serveurs d'Alpha");
        assert_eq!(group_title(Path::new("/work/Ørsted")), "Serveurs d'Ørsted");
        // Un h peut être aspiré, un y est une consonne : pas d'élision.
        assert_eq!(group_title(Path::new("/work/hugo")), "Serveurs de hugo");
        assert_eq!(group_title(Path::new("/work/yarn")), "Serveurs de yarn");
    }

    #[test]
    fn a_detected_server_gives_only_its_origin_under_its_title() {
        let detected_row = row(detected(), running(), Some(Origin::Idema));
        assert_eq!(status_text(&detected_row, "alpha"), "lancé par IdemA");
        let outside = row(
            detected(),
            running(),
            Some(Origin::Outside {
                command: "python3 -m http.server 8811".to_owned(),
            }),
        );
        assert_eq!(status_text(&outside, "alpha"), "lancé hors d'IdemA");
    }

    #[test]
    fn a_followed_start_waits_then_ends_in_running_or_failure() {
        let t0 = Instant::now();
        let mut watch = StartWatch::new(&RowState::Stopped, false, t0);
        assert_eq!(
            watch.follow(Some(&RowState::Stopped), t0),
            StartFollow::Wait
        );
        assert_eq!(
            watch.follow(Some(&RowState::Starting), t0),
            StartFollow::Wait
        );
        assert_eq!(watch.follow(Some(&running()), t0), StartFollow::Running);
        let mut failing = StartWatch::new(&RowState::Stopped, false, t0);
        assert_eq!(
            failing.follow(Some(&RowState::Failed), t0),
            StartFollow::Failed
        );
    }

    #[test]
    fn a_line_already_failed_at_the_request_is_not_a_new_failure_until_it_restarted() {
        let t0 = Instant::now();
        let mut watch = StartWatch::new(&RowState::Failed, false, t0);
        assert_eq!(watch.follow(Some(&RowState::Failed), t0), StartFollow::Wait);
        assert_eq!(
            watch.follow(Some(&RowState::Starting), t0),
            StartFollow::Wait
        );
        assert_eq!(
            watch.follow(Some(&RowState::Failed), t0),
            StartFollow::Failed
        );
    }

    #[test]
    fn a_restart_is_over_only_once_the_old_process_has_gone_and_a_new_one_listens() {
        let t0 = Instant::now();
        let mut watch = StartWatch::new(&running(), true, t0);
        // L'ancien processus tient encore le port : ce n'est pas la fin de la relance.
        assert_eq!(watch.follow(Some(&running()), t0), StartFollow::Wait);
        assert_eq!(
            watch.follow(Some(&RowState::Stopped), t0),
            StartFollow::Wait
        );
        assert_eq!(watch.follow(Some(&running()), t0), StartFollow::Running);
        // Une relance dont le démarrage échoue ouvre le journal comme un démarrage.
        let mut failing = StartWatch::new(&running(), true, t0);
        assert_eq!(
            failing.follow(Some(&RowState::Stopped), t0),
            StartFollow::Wait
        );
        assert_eq!(
            failing.follow(Some(&RowState::Failed), t0),
            StartFollow::Failed
        );
    }

    #[test]
    fn a_followed_start_is_dropped_when_the_line_vanishes_or_nothing_happens() {
        let t0 = Instant::now();
        let mut watch = StartWatch::new(&RowState::Stopped, false, t0);
        assert_eq!(watch.follow(None, t0), StartFollow::GiveUp);
        assert_eq!(
            watch.follow(Some(&RowState::Stopped), t0 + FOLLOW_START_LIMIT),
            StartFollow::GiveUp
        );
        let mut stuck = StartWatch::new(&running(), true, t0);
        assert_eq!(
            stuck.follow(Some(&running()), t0 + FOLLOW_START_LIMIT),
            StartFollow::GiveUp
        );
    }

    #[test]
    fn a_favorite_shows_its_host_and_port_or_its_file_name() {
        assert_eq!(host_of("https://vercel.com/dashboard?x=1"), "vercel.com");
        assert_eq!(host_of("http://localhost:8812/"), "localhost:8812");
        assert_eq!(host_of("http://[::1]:3000/"), "[::1]:3000");
        assert_eq!(host_of("file:///tmp/rapport.pdf"), "rapport.pdf");
        assert_eq!(host_of("pas une adresse"), "pas une adresse");
    }
}
