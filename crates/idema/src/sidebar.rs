//! La barre latérale (spec §7.1) : actions, projets, avis du fichier d'état. Les cotes sont celles de la
//! maquette (`IDEWindow.dc.html`, lignes 26 à 75), toutes passées par `Theme::px` sauf la bande de titre.
//! Un projet déplié montre une ligne par session, ou « Aucune session ».

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    App, Bounds, BoxShadow, ClickEvent, Context, Div, ElementId, FontWeight, MouseButton,
    MouseDownEvent, Pixels, Point, Rgba, Stateful, Window, canvas, div, img, point, prelude::*, px,
    svg,
};
use idema_term::activity::ActivityState;

use crate::assets::logo_path;
use crate::config::AgentEntry;
use crate::machines::idnma::IdnmaAgentInfo;
use crate::machines::key::LOCAL_MACHINE;
use crate::machines::probe::RemoteFailure;
use crate::machines::registry::{MachineStatus, RemoteMachine};

use crate::assets::Icon;
use crate::errors::{Explained, explain_config};
use crate::ideation::SECTION_TITLE;
use crate::project;
use crate::sidebar_model::{collapsed_project_dot, session_row_highlighted};
use crate::state::StateNotice;
use crate::theme::{
    ACTION_HOVER, BUTTON_HOVER, CONTROL, ERROR, ERROR_TEXT, HOVER, HOVER_STRONG, ICON_MUTED,
    ICON_SECONDARY, RUNNING, SELECTED, SEPARATOR, SIDEBAR, TEXT_DISABLED, TEXT_FULL, TEXT_MIN,
    TEXT_PRIMARY, TEXT_PROJECT, TEXT_SECONDARY, Theme, WAITING, color, white,
};
use crate::workspace::{NewProject, OpenConfig, OpenFile, Workspace, drag_window};

/// Largeur de la barre dans la maquette.
pub const SIDEBAR_WIDTH: f32 = 248.;
/// Bande de titre native : feux et bouton de la barre, hors échelle.
pub const TITLE_BAND: f32 = 52.;
/// Bord de la pastille de repos : 32 % dans la maquette. Une pastille n'est pas un texte, le plancher de 55 % ne s'y applique pas.
const DOT_IDLE: f32 = 0.32;
/// Anneau de la pastille de connexion d'une machine non connectée : blanc 30 % dans la maquette.
const CONNECTION_OFF: f32 = 0.30;
/// Opacité du halo de la pastille « en cours » : `rgba(48,209,88,0.16)` dans la maquette.
const RUNNING_HALO: f32 = 0.16;

pub fn render_sidebar(
    workspace: &Workspace,
    theme: &Theme,
    width: Pixels,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let theme = *theme;
    let state = workspace.state();
    let workspace_entity = cx.entity();

    // Avec au moins une machine distante, Localhost devient une section parmi d'autres ; sans, la barre ne change pas.
    let remote = !workspace.machines.is_empty();
    let local_collapsed = workspace.local_collapsed();
    let local_sessions: Vec<ActivityState> = workspace
        .entries
        .iter()
        .filter(|entry| !entry.summary.ideation && entry.summary.machine.is_none())
        .map(|entry| entry.summary.state)
        .collect();

    let mut projects = div().flex().flex_col().gap(px(1.));
    for (index, entry) in state.ordered_projects().into_iter().enumerate() {
        let path = entry.path.clone();
        let expanded = state.is_expanded(&path);
        let has_sessions = workspace
            .entries
            .iter()
            .any(|entry| !entry.summary.ideation && entry.summary.project == path);
        // Replié, le projet porte la pastille de sa session la plus urgente, anneau de repos compris.
        let collapsed_dot = (!expanded)
            .then(|| {
                collapsed_project_dot(
                    workspace
                        .entries
                        .iter()
                        .filter(|entry| !entry.summary.ideation && entry.summary.project == path)
                        .map(|entry| entry.summary.state),
                )
            })
            .flatten();
        let missing = workspace.is_missing(&path);
        // Le projet que `⌘T` vise devant la session affichée porte le style de sélection des sessions.
        let targeted = workspace.is_targeted_project(&path);
        let clicked_path = path.clone();
        let chevron_path = path.clone();
        let menu_path = path.clone();
        let new_path = path.clone();
        let launcher_path = path.clone();
        let plus_group = format!("project-plus-{index}");
        let row = div()
            .id(("project", index))
            .flex()
            .items_center()
            .gap(theme.px(4.))
            .h(theme.px(30.))
            .pl(theme.px(8.))
            .pr(theme.px(4.))
            .rounded(theme.px(6.))
            .text_color(white(TEXT_PROJECT))
            .cursor_pointer()
            .when(targeted, |row| row.bg(white(SELECTED)))
            .when(!targeted, |row| row.hover(|style| style.bg(white(HOVER))))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.click_project(&clicked_path, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_project_menu(menu_path.clone(), event.position, window, cx);
                }),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    // Dossier introuvable : la ligne est grisée (le désactivé est exempté du plancher de 55 %).
                    .when(missing, |name| name.text_color(white(TEXT_DISABLED)))
                    .child(project::display_name(&path)),
            )
            // Le chevron plie ou déplie seulement : il ne sélectionne rien. Sa zone est celle du bouton « + ».
            .child(
                div()
                    .id(("project-chevron", index))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(theme.px(22.))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        cx.stop_propagation();
                        this.toggle_project(&chevron_path, cx);
                    }))
                    .child(
                        svg()
                            .path(if expanded {
                                Icon::ExpandMore.path()
                            } else {
                                Icon::ChevronRight.path()
                            })
                            .size(theme.px(16.))
                            .text_color(white(ICON_MUTED)),
                    ),
            )
            .child(div().flex_1())
            .children(collapsed_dot.map(|state| state_dot(&theme, state)))
            .children(missing.then(|| {
                div()
                    .flex_none()
                    .pr(theme.px(4.))
                    .text_size(theme.text(11.))
                    .text_color(white(TEXT_MIN))
                    .child("introuvable")
            }))
            .children((!missing).then(|| {
                div()
                    .id(("project-plus", index))
                    .group(plus_group.clone())
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(theme.px(22.))
                    .rounded(theme.px(5.))
                    .hover(|style| style.bg(white(BUTTON_HOVER)))
                    // Clic : l'agent par défaut ; clic droit : le lanceur. Aucun des deux ne replie la ligne ni
                    // n'ouvre le menu du projet.
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.new_session_default(new_path.clone(), window, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.open_launcher_menu(
                                launcher_path.clone(),
                                event.position,
                                window,
                                cx,
                            );
                        }),
                    )
                    .child(
                        svg()
                            .path(Icon::Add.path())
                            .size(theme.px(16.))
                            .text_color(white(ICON_SECONDARY))
                            .group_hover(plus_group, |style| style.text_color(white(TEXT_FULL))),
                    )
            }));
        let mut block = div().flex().flex_col().gap(px(1.)).child(row);
        if expanded {
            for at in (0..workspace.entries.len()).filter(|at| {
                let summary = &workspace.entries[*at].summary;
                !summary.ideation && summary.project == path
            }) {
                block = block.child(session_row(workspace, at, &theme, cx));
            }
        }
        if expanded && !has_sessions {
            block = block.child(no_session_line(&theme));
        }
        projects = projects.child(block);
    }

    let ideation = workspace
        .ideation()
        .is_some()
        .then(|| ideation_section(workspace, &theme, cx));

    let notice = workspace.notice().map(|notice| match notice {
        StateNotice::Rebuilt => {
            "state.json illisible : mis de côté en state.json.bak, liste reconstruite depuis tmux"
                .to_string()
        }
        StateNotice::ReadOnly { version } => format!(
            "state.json écrit par une version plus récente (v{version}) : lu sans être réécrit"
        ),
    });

    // Les bornes du bouton « … » de la légende, écrites à la préparation de la peinture et lues au clic.
    let menu_button: Rc<Cell<Bounds<Pixels>>> = Rc::default();
    let menu_button_writer = menu_button.clone();
    let projects_legend = div()
        .flex()
        .items_center()
        .h(theme.px(26.))
        .pl(theme.px(8.))
        .pr(theme.px(4.))
        .text_size(theme.text(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(white(TEXT_MIN))
        .child(div().flex_1().child("Projets"))
        .child(
            div()
                .id("projects-menu")
                .relative()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(theme.px(22.))
                .rounded(theme.px(5.))
                .cursor_pointer()
                .hover(|style| style.bg(white(BUTTON_HOVER)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        // Le menu s'ouvre sous le bouton (bord gauche, juste en dessous) : il ne le
                        // recouvre pas, un second clic au même point tombe sur le fond et le ferme.
                        let button = menu_button.get();
                        let anchor = point(button.left(), button.bottom() + theme.px(4.));
                        this.open_projects_menu(anchor, window, cx);
                    }),
                )
                // Mesure le bouton à la préparation de la peinture, pour que son menu s'y ancre.
                .child(
                    canvas(
                        move |bounds, _, _| menu_button_writer.set(bounds),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .child(
                    svg()
                        .path(Icon::MoreHoriz.path())
                        .size(theme.px(16.))
                        .text_color(white(TEXT_MIN)),
                ),
        );
    let legend = if remote {
        local_header(workspace, &theme, local_collapsed, local_sessions, cx).into_any_element()
    } else {
        projects_legend.into_any_element()
    };
    let machines: Vec<_> = (0..workspace.machines.len())
        .map(|index| machine_section(workspace, index, &theme, cx))
        .collect();
    let list = div()
        .id("sidebar-list")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .pt(theme.px(22.))
                .px(theme.px(10.))
                .children(ideation)
                .child(legend)
                .children((!local_collapsed).then_some(projects))
                .children(machines)
                .children(notice.map(|text| {
                    div()
                        .px(theme.px(8.))
                        .pt(theme.px(10.))
                        .text_size(theme.text(11.))
                        .text_color(white(TEXT_MIN))
                        .child(text)
                }))
                // La notice de la restauration, au gabarit de celle de `state.json` ; un clic sur le bloc la ferme.
                .children(workspace.restore_notice().map(|lines| {
                    div()
                        .id("restore-notice")
                        .flex()
                        .flex_col()
                        .px(theme.px(8.))
                        .pt(theme.px(10.))
                        .text_size(theme.text(11.))
                        .text_color(white(TEXT_MIN))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                this.close_restore_notice(cx);
                            }),
                        )
                        .children(lines.iter().map(|line| div().child(line.clone())))
                }))
                .children(workspace.config_error().map(|error| {
                    let Explained { text, detail } = explain_config(error);
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .px(theme.px(8.))
                        .pt(theme.px(10.))
                        .text_size(theme.text(11.))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(color(ERROR_TEXT))
                                .child("Config invalide"),
                        )
                        .child(div().text_color(white(TEXT_SECONDARY)).child(text))
                        .children(detail.map(|raw| div().text_color(white(TEXT_MIN)).child(raw)))
                })),
        );

    div()
        .relative()
        .flex()
        .flex_col()
        .flex_none()
        .w(width)
        .h_full()
        .bg(color(SIDEBAR))
        .border_r_1()
        .border_color(white(SEPARATOR))
        // Bande de titre : vide, elle porte le déplacement de la fenêtre.
        .child(
            div()
                .h(px(TITLE_BAND))
                .flex_none()
                .on_mouse_down(MouseButton::Left, drag_window),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .pt(theme.px(4.))
                .px(theme.px(10.))
                .child(action_row(
                    &theme,
                    "sidebar-new",
                    ActionIcon {
                        icon: Icon::AddBold,
                        size: 15.,
                        in_badge: true,
                        alpha: TEXT_PRIMARY,
                    },
                    "Ouvrir un projet",
                    "⌘O",
                    true,
                    false,
                    |window, cx| window.dispatch_action(Box::new(NewProject), cx),
                ))
                // Sans projet de contexte, le volet Fichiers n'a nulle part où s'ouvrir : la ligne est éteinte.
                .child(action_row(
                    &theme,
                    "sidebar-open-file",
                    ActionIcon {
                        icon: Icon::Description,
                        size: 16.,
                        in_badge: false,
                        alpha: TEXT_SECONDARY,
                    },
                    "Ouvrir un fichier",
                    "⌘⇧O",
                    workspace.panel_project().is_some(),
                    false,
                    |window, cx| window.dispatch_action(Box::new(OpenFile), cx),
                ))
                // La galerie Artifacts prend la colonne centrale ; la ligne est sélectionnée tant qu'elle y est.
                // Pas de raccourci.
                .child(action_row(
                    &theme,
                    "sidebar-artifacts",
                    ActionIcon {
                        icon: Icon::Stacks,
                        size: 16.,
                        in_badge: false,
                        alpha: TEXT_SECONDARY,
                    },
                    "Artifacts",
                    "",
                    true,
                    workspace.artifacts_shown(),
                    move |window, cx| {
                        workspace_entity.update(cx, |this, cx| this.show_artifacts(window, cx));
                    },
                ))
                .child(action_row(
                    &theme,
                    "sidebar-customize",
                    ActionIcon {
                        icon: Icon::Work,
                        size: 16.,
                        in_badge: false,
                        alpha: TEXT_SECONDARY,
                    },
                    "Personnaliser",
                    "⌘,",
                    true,
                    false,
                    |window, cx| window.dispatch_action(Box::new(OpenConfig), cx),
                )),
        )
        .child(list)
}

/// La section « Bouillon de neurones » (spec §3.1) : légende au gabarit de « Projets », chevron qui la plie, « + » qui
/// crée un bouillon (clic gauche seulement, aucun menu), puis une ligne de session par bouillon.
fn ideation_section(
    workspace: &Workspace,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let theme = *theme;
    let expanded = !workspace.state().ui.ideation_collapsed;
    let rows: Vec<usize> = (0..workspace.entries.len())
        .filter(|at| workspace.entries[*at].summary.ideation)
        .collect();
    // Replié, la section porte la pastille de son bouillon le plus urgent, comme un projet replié.
    let collapsed_dot = (!expanded)
        .then(|| collapsed_project_dot(rows.iter().map(|at| workspace.entries[*at].summary.state)))
        .flatten();
    let mut block = div().flex().flex_col().gap(px(1.));
    if expanded {
        for at in &rows {
            block = block.child(session_row(workspace, *at, &theme, cx));
        }
        if rows.is_empty() {
            block = block.child(no_session_line(&theme));
        }
    }
    div()
        .flex()
        .flex_col()
        .gap(px(1.))
        .pb(theme.px(10.))
        .child(
            div()
                .flex()
                .items_center()
                .h(theme.px(26.))
                .pl(theme.px(8.))
                .pr(theme.px(4.))
                .text_size(theme.text(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(white(TEXT_MIN))
                .child(SECTION_TITLE)
                // Le chevron plie ou déplie seulement, comme celui d'un projet.
                .child(
                    div()
                        .id("ideation-chevron")
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(theme.px(22.))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _window, cx| {
                            cx.stop_propagation();
                            this.toggle_ideation(cx);
                        }))
                        .child(
                            svg()
                                .path(if expanded {
                                    Icon::ExpandMore.path()
                                } else {
                                    Icon::ChevronRight.path()
                                })
                                .size(theme.px(16.))
                                .text_color(white(ICON_MUTED)),
                        ),
                )
                .child(div().flex_1())
                .children(collapsed_dot.map(|state| state_dot(&theme, state)))
                .child(
                    div()
                        .id("ideation-plus")
                        .group("ideation-plus")
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(theme.px(22.))
                        .rounded(theme.px(5.))
                        .cursor_pointer()
                        .hover(|style| style.bg(white(BUTTON_HOVER)))
                        // Clic gauche : un nouveau bouillon. Aucun clic droit : pas de menu de lanceur.
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.new_ideation_session(window, cx);
                        }))
                        .child(
                            svg()
                                .path(Icon::Add.path())
                                .size(theme.px(16.))
                                .text_color(white(ICON_SECONDARY))
                                .group_hover("ideation-plus", |style| {
                                    style.text_color(white(TEXT_FULL))
                                }),
                        ),
                ),
        )
        .child(block)
}

/// Le chevron d'un en-tête de machine, qui plie ou déplie.
fn machine_chevron(theme: &Theme, expanded: bool) -> impl IntoElement {
    svg()
        .path(if expanded {
            Icon::ExpandMore.path()
        } else {
            Icon::ChevronRight.path()
        })
        .size(theme.px(16.))
        .text_color(white(ICON_MUTED))
}

/// Ce que porte une machine repliée à droite : la pastille de sa session la plus urgente et leur nombre (comme un
/// projet replié) ; rien sans session.
fn collapsed_summary(theme: &Theme, states: &[ActivityState]) -> Option<Div> {
    let dot = collapsed_project_dot(states.iter().copied())?;
    Some(
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(theme.px(6.))
            .pr(theme.px(4.))
            .text_size(theme.text(11.))
            .font_weight(FontWeight::NORMAL)
            .text_color(white(TEXT_MIN))
            .child(state_dot(theme, dot))
            .child(states.len().to_string()),
    )
}

/// Le bouton « ··· » d'un en-tête de machine : il ouvre son menu sous lui (bord gauche, juste en dessous) sans
/// replier l'en-tête.
fn header_menu_button(
    theme: &Theme,
    id: impl Into<ElementId>,
    cx: &mut Context<Workspace>,
    open: impl Fn(&mut Workspace, Point<Pixels>, &mut Window, &mut Context<Workspace>) + 'static,
) -> impl IntoElement {
    let theme = *theme;
    let bounds: Rc<Cell<Bounds<Pixels>>> = Rc::default();
    let writer = bounds.clone();
    div()
        .id(id)
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(theme.px(22.))
        .rounded(theme.px(5.))
        .cursor_pointer()
        .hover(|style| style.bg(white(BUTTON_HOVER)))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            cx.stop_propagation();
            let button = bounds.get();
            open(
                this,
                point(button.left(), button.bottom() + theme.px(4.)),
                window,
                cx,
            );
        }))
        .child(
            canvas(move |area, _, _| writer.set(area), |_, _, _, _| {})
                .absolute()
                .size_full(),
        )
        .child(
            svg()
                .path(Icon::MoreHoriz.path())
                .size(theme.px(16.))
                .text_color(white(TEXT_MIN)),
        )
}

/// Le gabarit d'un en-tête de machine : la légende « Projets » (11/600), chevron en tête, un clic plie ou déplie.
fn machine_header(
    theme: &Theme,
    id: impl Into<ElementId>,
    key: String,
    cx: &mut Context<Workspace>,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(theme.px(6.))
        .h(theme.px(26.))
        .pl(theme.px(4.))
        .pr(theme.px(4.))
        .rounded(theme.px(6.))
        .text_size(theme.text(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(white(TEXT_MIN))
        .cursor_pointer()
        .hover(|style| style.bg(white(HOVER)))
        .on_click(cx.listener(move |this, _, _window, cx| {
            this.toggle_machine(&key, cx);
        }))
}

/// L'en-tête de Localhost quand il y a des machines distantes : son label (`[machines] local_label`), replié ou non.
/// Déplié, « ··· » ouvre le menu « Ouvrir un projet » de l'ancienne légende ; replié, le résumé de ses sessions le remplace.
fn local_header(
    workspace: &Workspace,
    theme: &Theme,
    collapsed: bool,
    states: Vec<ActivityState>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let right = if collapsed {
        collapsed_summary(theme, &states).map(IntoElement::into_any_element)
    } else {
        Some(
            header_menu_button(theme, "local-menu", cx, |this, anchor, window, cx| {
                this.open_projects_menu(anchor, window, cx);
            })
            .into_any_element(),
        )
    };
    machine_header(theme, "local-machine", LOCAL_MACHINE.to_owned(), cx)
        .child(machine_chevron(theme, !collapsed))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(workspace.local_machine_label().to_owned()),
        )
        .children(right)
}

/// La pastille de connexion de 7 px : pleine `#30D158` connectée, anneau blanc 30 % sinon.
fn connection_dot(theme: &Theme, connected: bool) -> Div {
    let dot = div().flex_none().size(theme.px(7.)).rounded_full();
    if connected {
        dot.bg(color(RUNNING))
    } else {
        dot.border(px(1.5)).border_color(white(CONNECTION_OFF))
    }
}

/// La ligne d'une machine en échec : sa cause, avec une majuscule, puis « Réessayer ».
pub(crate) fn failure_line(failure: &RemoteFailure) -> String {
    let cause = failure.short();
    let mut chars = cause.chars();
    let head: String = chars
        .next()
        .into_iter()
        .flat_map(char::to_uppercase)
        .collect();
    format!("{head}{} · Réessayer", chars.as_str())
}

/// Le texte que la barre met sous une machine qui ne répond pas : sa cause et « Réessayer » quand elle a échoué, « Connexion… »
/// tant que l'essai suivant est en vol (la cause périmée ne reste pas affichée : un clic sur « Réessayer » se voit).
/// Rien pour une machine connectée, ni pour une première connexion.
pub(crate) fn machine_status_line(machine: &RemoteMachine) -> Option<String> {
    match (&machine.status, machine.failure()) {
        (MachineStatus::Failed(failure), _) => Some(failure_line(failure)),
        (MachineStatus::Connecting, Some(_)) => Some("Connexion…".to_owned()),
        _ => None,
    }
}

/// Le logo d'un agent d'une machine : celui de son `avatar` dans le `~/.idnma.yml` de la machine, sinon celui de sa clé en
/// minuscules (la règle locale). `None` : l'agent n'a pas de logo.
pub(crate) fn remote_agent_logo(agent: &IdnmaAgentInfo) -> Option<&'static str> {
    agent
        .avatar
        .as_deref()
        .and_then(|avatar| logo_path(&avatar.to_lowercase()))
        .or_else(|| logo_path(&agent.key.to_lowercase()))
}

/// Le « + » d'un en-tête de machine, du même dessin que celui d'un projet : un clic crée une session avec l'agent par
/// défaut de la machine, un clic droit ouvre le menu de ses agents. Aucun des deux ne replie l'en-tête.
fn machine_plus(
    theme: &Theme,
    index: usize,
    connection: &str,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let theme = *theme;
    let plus_group = format!("machine-plus-{index}");
    let (new_target, menu_target) = (connection.to_owned(), connection.to_owned());
    div()
        .id(("machine-plus", index))
        .group(plus_group.clone())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(theme.px(22.))
        .rounded(theme.px(5.))
        .hover(|style| style.bg(white(BUTTON_HOVER)))
        .on_click(cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            this.new_remote_session_default(&new_target, window, cx);
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.open_remote_launcher_menu(&menu_target, event.position, window, cx);
            }),
        )
        .child(
            svg()
                .path(Icon::Add.path())
                .size(theme.px(16.))
                .text_color(white(ICON_SECONDARY))
                .group_hover(plus_group, |style| style.text_color(white(TEXT_FULL))),
        )
}

/// Une machine distante : son en-tête (chevron, pastille de connexion, label, « ··· » ou résumé), puis, dépliée, ses
/// sessions à plat, « Aucune session » si elle est connectée sans session, ou la ligne de sa cause d'échec.
fn machine_section(
    workspace: &Workspace,
    index: usize,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let theme = *theme;
    let machine = &workspace.machines[index];
    let connection = machine.entry.connection.clone();
    let collapsed = workspace.state().is_machine_collapsed(&connection);
    let rows: Vec<usize> = (0..workspace.entries.len())
        .filter(|at| workspace.entries[*at].summary.machine.as_deref() == Some(connection.as_str()))
        .collect();
    let failure = machine.failure().cloned();
    let connecting = machine.status == MachineStatus::Connecting;
    let right = if collapsed {
        match &failure {
            Some(failure) => Some(
                div()
                    .flex_none()
                    .pr(theme.px(4.))
                    .font_weight(FontWeight::NORMAL)
                    .child(if connecting {
                        "connexion…"
                    } else {
                        failure.short()
                    })
                    .into_any_element(),
            ),
            None => {
                let states: Vec<ActivityState> = rows
                    .iter()
                    .map(|at| workspace.entries[*at].summary.state)
                    .collect();
                collapsed_summary(&theme, &states).map(IntoElement::into_any_element)
            }
        }
    } else {
        let target = connection.clone();
        // Créer une session suppose une machine connectée : sinon seul « ··· » reste.
        let plus = (machine.status == MachineStatus::Connected)
            .then(|| machine_plus(&theme, index, &connection, cx));
        let menu = header_menu_button(
            &theme,
            ("machine-menu", index),
            cx,
            move |this, anchor, window, cx| {
                this.open_machine_menu(target.clone(), anchor, window, cx);
            },
        );
        Some(
            div()
                .flex()
                .items_center()
                .child(menu)
                .children(plus)
                .into_any_element(),
        )
    };
    let header = machine_header(&theme, ("machine", index), connection.clone(), cx)
        .child(machine_chevron(&theme, !collapsed))
        .child(connection_dot(
            &theme,
            machine.status == MachineStatus::Connected,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(machine.entry.label.clone()),
        )
        .children(right);
    let mut block = div().flex().flex_col().gap(px(1.)).child(header);
    if !collapsed {
        let empty = rows.is_empty();
        for at in rows {
            block = block.child(session_row(workspace, at, &theme, cx));
        }
        match machine_status_line(machine) {
            Some(text) => {
                let target = connection;
                block = block.child(
                    div()
                        .id(("machine-failure", index))
                        .flex()
                        .items_center()
                        .h(theme.px(28.))
                        .pl(theme.px(18.))
                        .rounded(theme.px(6.))
                        .text_size(theme.text(12.))
                        .text_color(white(TEXT_MIN))
                        .cursor_pointer()
                        .hover(|style| style.bg(white(HOVER)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.retry_machine(&target, window, cx);
                        }))
                        .child(text),
                );
            }
            None if empty && machine.status == MachineStatus::Connected => {
                block = block.child(no_session_line(&theme));
            }
            None => {}
        }
    }
    block.pt(theme.px(10.))
}

/// Pastille d'état de 9 (maquette, « Pastille d'état 9 px ») : anneau blanc au repos, plein et haloté en cours,
/// jaune quand la session attend, rouge en erreur.
pub(crate) fn state_dot(theme: &Theme, state: ActivityState) -> Div {
    let dot = div().flex_none().size(theme.px(9.)).rounded_full();
    match state {
        ActivityState::Idle => dot.border(px(1.5)).border_color(white(DOT_IDLE)),
        ActivityState::Running => dot.bg(color(RUNNING)).shadow(vec![BoxShadow {
            color: Rgba {
                a: RUNNING_HALO,
                ..color(RUNNING)
            }
            .into(),
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(3.),
            inset: false,
        }]),
        ActivityState::Waiting => dot.bg(color(WAITING)),
        ActivityState::Error => dot.bg(color(ERROR)),
    }
}

/// Ce que la ligne d'une session montre d'un agent qui y tourne : son logo, ou son nom quand il n'en a pas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentMark {
    Logo(&'static str),
    Name(String),
}

/// Les marques des agents `names` (noms d'entrées du lanceur), dans leur ordre : le logo de l'entrée (`icon_name`), ou
/// son nom. Un nom qui n'est plus au lanceur (configuration changée depuis le relevé) garde son nom. Deux entrées au
/// même logo (Antigravity et Gemini) n'en montrent qu'un.
pub(crate) fn agent_marks(names: &[String], launchers: &[AgentEntry]) -> Vec<AgentMark> {
    let mut marks = Vec::new();
    for name in names {
        let mark = launchers
            .iter()
            .find(|entry| &entry.name == name)
            .and_then(|entry| logo_path(&entry.icon_name()))
            .map_or_else(|| AgentMark::Name(name.clone()), AgentMark::Logo);
        if !marks.contains(&mark) {
            marks.push(mark);
        }
    }
    marks
}

/// Les marques des agents `names` d'une session distante : le logo de l'`avatar` de l'agent dans le `~/.idnma.yml` de sa
/// machine, sinon le logo de son nom en minuscules (la règle locale), sinon son nom. Dédoublonné comme `agent_marks`.
pub(crate) fn remote_agent_marks(names: &[String], agents: &[IdnmaAgentInfo]) -> Vec<AgentMark> {
    let mut marks = Vec::new();
    for name in names {
        let mark = agents
            .iter()
            .find(|agent| &agent.key == name)
            .and_then(|agent| agent.avatar.as_deref())
            .and_then(|avatar| logo_path(&avatar.to_lowercase()))
            .or_else(|| logo_path(&name.to_lowercase()))
            .map_or_else(|| AgentMark::Name(name.clone()), AgentMark::Logo);
        if !marks.contains(&mark) {
            marks.push(mark);
        }
    }
    marks
}

/// La ligne grisée d'un projet ou de la section Bouillon de neurones qui n'a aucune session.
fn no_session_line(theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(theme.px(28.))
        .pl(theme.px(37.))
        .text_size(theme.text(12.))
        .text_color(white(TEXT_MIN))
        .child("Aucune session")
}

/// Une ligne de session (maquette `IDEWindow.dc.html`, lignes 56 à 63) : pastille, nom, puis les agents qui tournent
/// dans ses panes (un logo par agent, à la place du libellé du lanceur).
fn session_row(
    workspace: &Workspace,
    at: usize,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let entry = &workspace.entries[at];
    let name = entry.summary.name.clone();
    // La galerie Artifacts au centre : un seul surlignage dans la barre, celui de sa ligne.
    let selected = !workspace.artifacts_shown()
        && session_row_highlighted(
            workspace.selected_name() == Some(name.as_str()),
            workspace.project_wins(),
            entry.summary.ideation,
        );
    // Un bouillon porte toujours l'astérisque de Claude : la détection des process ne le reconnaîtrait pas forcément.
    let marks = if entry.summary.ideation {
        vec![AgentMark::Logo(
            logo_path("claude").expect("the claude logo is bundled"),
        )]
    } else {
        workspace.session_agent_marks(&name, cx)
    };
    let field = workspace.rename_field_of(&name);
    let renaming = field.is_some();
    let menu_name = name.clone();
    div()
        .id(("session", at))
        .flex()
        .items_center()
        .gap(theme.px(10.))
        .h(theme.px(30.))
        .pl(theme.px(18.))
        .pr(theme.px(8.))
        .rounded(theme.px(6.))
        .when(!renaming, |row| row.cursor_pointer())
        .when(selected, |row| row.bg(white(SELECTED)))
        .when(!selected, |row| {
            row.hover(|style| style.bg(white(HOVER_STRONG)))
        })
        // Pendant le renommage, un clic dans la ligne ne doit pas reprendre le focus au champ.
        .when(!renaming, |row| {
            row.on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() == 2 {
                    this.start_rename(&name, window, cx);
                } else {
                    this.select(&name, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_session_menu(&menu_name, event.position, window, cx);
                }),
            )
        })
        .child(state_dot(theme, entry.summary.state))
        .child(match field {
            Some(field) => field.into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(theme.text(13.))
                .text_color(white(if selected { TEXT_FULL } else { TEXT_PRIMARY }))
                .child(entry.summary.display_name.clone())
                .into_any_element(),
        })
        .when(!marks.is_empty(), |row| {
            row.child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme.px(5.))
                    .children(marks.into_iter().map(|mark| {
                        match mark {
                            AgentMark::Logo(path) => {
                                img(path).flex_none().size(theme.px(16.)).into_any_element()
                            }
                            AgentMark::Name(name) => div()
                                .max_w(theme.px(96.))
                                .truncate()
                                .text_size(theme.text(11.))
                                .text_color(white(TEXT_MIN))
                                .child(name)
                                .into_any_element(),
                        }
                    })),
            )
        })
}

/// L'icône d'une ligne d'action : sa taille, son niveau de blanc, et la pastille ronde éventuelle.
struct ActionIcon {
    icon: Icon,
    size: f32,
    in_badge: bool,
    alpha: f32,
}

/// Une ligne « Ouvrir un projet », « Artifacts » ou « Personnaliser » : icône dans une boîte de 20, libellé, raccourci.
/// `selected` la dessine comme une session sélectionnée (fond `SELECTED`, texte plein, pas de survol).
#[allow(clippy::too_many_arguments)]
fn action_row(
    theme: &Theme,
    id: &'static str,
    icon: ActionIcon,
    label: &'static str,
    shortcut: &'static str,
    enabled: bool,
    selected: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    // Éteinte : tout passe à 25 %, ni survol ni clic (comme les boutons de la barre d'outils).
    let glyph = svg()
        .path(icon.icon.path())
        .size(theme.px(icon.size))
        .text_color(white(if enabled { icon.alpha } else { TEXT_DISABLED }));
    let mut frame = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(theme.px(20.));
    if icon.in_badge {
        frame = frame.rounded_full().bg(white(CONTROL));
    }
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(theme.px(10.))
        .h(theme.px(30.))
        .px(theme.px(8.))
        .rounded(theme.px(6.))
        .when(selected, |row| {
            row.bg(white(SELECTED)).text_color(white(TEXT_FULL))
        })
        .when(enabled, |row| {
            row.cursor_pointer()
                .when(!selected, |row| {
                    row.hover(|style| style.bg(white(ACTION_HOVER)))
                })
                .on_click(move |_, window, cx| on_click(window, cx))
        })
        .when(!enabled, |row| row.text_color(white(TEXT_DISABLED)))
        .child(frame.child(glyph))
        .child(div().flex_1().child(label))
        .child(
            div()
                .text_size(theme.text(12.))
                .text_color(white(if enabled { TEXT_MIN } else { TEXT_DISABLED }))
                .child(shortcut),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_shows_its_logo_or_its_name() {
        let entry = |name: &str, icon: Option<&str>| AgentEntry {
            name: name.into(),
            harness: None,
            command: format!("idnma run {}", name.to_lowercase()),
            default: false,
            icon: icon.map(str::to_string),
        };
        let launchers = [
            entry("GLM", None),
            entry("Antigravity", Some("gemini")),
            entry("Maison", None),
        ];
        let launchers = [launchers.as_slice(), &[entry("Gemini", None)]].concat();
        let names = ["GLM", "Antigravity", "Maison", "Gemini", "Disparu"].map(String::from);
        // Gemini partage le logo d'Antigravity : un seul losange.
        assert_eq!(
            agent_marks(&names, &launchers),
            [
                AgentMark::Logo("logos/glm.png"),
                AgentMark::Logo("logos/gemini.png"),
                AgentMark::Name("Maison".into()),
                AgentMark::Name("Disparu".into()),
            ]
        );
    }

    #[test]
    fn remote_marks_use_the_avatar() {
        let agent = |key: &str, avatar: Option<&str>| IdnmaAgentInfo {
            key: key.into(),
            label: None,
            avatar: avatar.map(str::to_string),
        };
        let agents = [
            agent("sonnet", Some("claude")),
            agent("opus", Some("claude")),
            agent("glm", Some("glm")),
        ];
        let names = ["sonnet", "opus", "glm", "mystery"].map(String::from);
        assert_eq!(
            remote_agent_marks(&names, &agents),
            [
                AgentMark::Logo(logo_path("claude").unwrap()),
                AgentMark::Logo(logo_path("glm").unwrap()),
                AgentMark::Name("mystery".into()),
            ]
        );
    }

    #[test]
    fn a_remote_agent_without_avatar_falls_back_to_the_local_rule() {
        let agents = [IdnmaAgentInfo {
            key: "Claude".into(),
            label: None,
            avatar: None,
        }];
        // Sans avatar : le nom en minuscules ; sans fichier d'agents de la machine : pareil.
        let names = ["Claude".to_string()];
        let expected = [AgentMark::Logo(logo_path("claude").unwrap())];
        assert_eq!(remote_agent_marks(&names, &agents), expected);
        assert_eq!(remote_agent_marks(&names, &[]), expected);
    }

    #[test]
    fn a_retry_in_flight_shows_connecting_instead_of_the_stale_cause() {
        use crate::config::RemoteMachineEntry;
        use crate::machines::registry::RemoteMachine;
        let mut machine = RemoteMachine::new(RemoteMachineEntry {
            label: "Dell".into(),
            connection: "dell-lan".into(),
        });
        // Première connexion, rien à dire.
        assert_eq!(machine_status_line(&machine), None);
        machine.status = MachineStatus::Failed(RemoteFailure::Unreachable);
        machine.last_failure = Some(RemoteFailure::Unreachable);
        assert_eq!(
            machine_status_line(&machine).as_deref(),
            Some("Injoignable · Réessayer")
        );
        // « Réessayer » est cliqué : l'essai suivant est en vol, la cause périmée cède la place à « Connexion… ».
        machine.status = MachineStatus::Connecting;
        assert_eq!(machine_status_line(&machine).as_deref(), Some("Connexion…"));
        machine.status = MachineStatus::Connected;
        machine.last_failure = None;
        assert_eq!(machine_status_line(&machine), None);
    }

    #[test]
    fn a_remote_agent_logo_follows_the_avatar_then_the_key() {
        let agent = |key: &str, avatar: Option<&str>| IdnmaAgentInfo {
            key: key.into(),
            label: None,
            avatar: avatar.map(str::to_string),
        };
        assert_eq!(
            remote_agent_logo(&agent("sonnet", Some("Claude"))),
            logo_path("claude")
        );
        assert_eq!(remote_agent_logo(&agent("glm", None)), logo_path("glm"));
        assert_eq!(remote_agent_logo(&agent("mystery", None)), None);
    }

    #[test]
    fn a_failed_machine_line_names_the_cause_with_a_capital() {
        use crate::machines::probe::RemoteFailure;
        assert_eq!(
            failure_line(&RemoteFailure::Unreachable),
            "Injoignable · Réessayer"
        );
        assert_eq!(
            failure_line(&RemoteFailure::AuthRefused),
            "Authentification refusée · Réessayer"
        );
    }
}
