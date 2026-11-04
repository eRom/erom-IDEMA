//! L'habillage du volet Browser : l'en-tête des onglets (dessiné par `Workspace`, qui y met aussi agrandir et fermer)
//! et la rangée d'adresse (dessinée par le volet). Les cotes sont celles de la spec P4 §4.2 et §4.3 et de l'écran 02
//! de la maquette.

use std::net::IpAddr;
use std::sync::Arc;

use gpui::{
    Context, Div, Entity, Focusable, MouseButton, MouseDownEvent, Pixels, RenderImage, Stateful,
    Window, div, img, prelude::*, px, relative, svg,
};
use url::{Host, Url};

use super::find::{FindBar, status_label};
use super::{ADDRESS_KEY_CONTEXT, BrowserPanel, FIND_KEY_CONTEXT};
use crate::assets::Icon;
use crate::browser::tabs::{Tab, TabId, TabKind};
use crate::files::panel::{FIELD_HEIGHT, FIELD_PADDING, FIELD_RADIUS, field_input};
use crate::help_bubble::HelpBubble;
use crate::servers::registry::ServerKey;
use crate::terminal_panel::{header_button, header_button_at};
use crate::theme::{
    BUTTON_HOVER, CONTROL, ERROR_TEXT, FIELD_BACKGROUND, FIELD_RING, FOCUS, HOVER, HOVER_STRONG,
    ICON_CLOSE, ICON_HEADER, SEPARATOR, TEXT_DISABLED, TEXT_LABEL, TEXT_MIN, Theme, black, color,
    white,
};
use crate::workspace::Workspace;

/// Hauteur de la rangée d'adresse (la maquette A : 40 en tout, avec 8 de retrait sous les boutons, donc le champ en haut
/// de la rangée et non centré).
const NAV_ROW_HEIGHT: f32 = 40.;
const NAV_ROW_PADDING_X: f32 = 8.;
const NAV_ROW_PADDING_BOTTOM: f32 = 8.;
/// Un onglet : 28 de haut, rayon 6, bouton de fermeture de 18 ; sa largeur va de 72 à 280, au-delà la rangée défile.
const TAB_HEIGHT: f32 = 28.;
const TAB_RADIUS: f32 = 6.;
const TAB_CLOSE_SIZE: f32 = 18.;
const TAB_MIN_WIDTH: f32 = 72.;
const TAB_MAX_WIDTH: f32 = 280.;
/// L'écart entre deux onglets.
const TAB_GAP: f32 = 2.;
/// Sous cette largeur par onglet, le titre n'a plus la place de se lire (retrait 14, icône 15, écarts 14, croix 18, soit
/// 61 de fixe : il reste de quoi écrire quatre ou cinq caractères, puis une ellipse) : l'onglet passe en mode étroit,
/// comme dans Chrome, icône seule.
const TAB_NARROW_BELOW: f32 = 96.;
/// Largeur plancher d'un onglet étroit : l'icône centrée entre 10 et 10 de marge (inactif), ou l'icône et la croix (actif :
/// retrait 5, icône 15, écart 4, croix 18, retrait 5, soit 47).
const NARROW_TAB_MIN_WIDTH: f32 = 36.;
const NARROW_ACTIVE_TAB_MIN_WIDTH: f32 = 47.;
/// Ce que l'en-tête prend autour de la rangée d'onglets : le retrait droit (8), le « + », ⋮, agrandir et fermer (28 chacun)
/// et les cinq écarts de 4 entre les enfants de l'en-tête.
const HEADER_RESERVED: f32 = 140.;
/// Le filet de progression sous la rangée d'adresse.
const PROGRESS_HEIGHT: f32 = 2.;
/// Part minimale du filet pendant un chargement : le début d'un chargement se voit.
const PROGRESS_FLOOR: f32 = 0.04;

/// Un onglet tel que l'en-tête le dessine.
pub struct TabView {
    pub id: TabId,
    pub title: String,
    /// Le texte de la bulle au survol : le titre en entier (l'onglet le coupe), sinon l'adresse.
    pub tooltip: String,
    /// `home` pour l'Accueil, `terminal` pour un journal, `public` pour une page web (spec §4.2).
    pub icon: Icon,
    pub active: bool,
    /// Le favicon de la page, quand elle en a un : il prend la place du globe.
    pub favicon: Option<Arc<RenderImage>>,
}

impl TabView {
    pub fn of(tab: &Tab, active: bool, favicon: Option<Arc<RenderImage>>) -> Self {
        Self {
            id: tab.id,
            title: tab_title(tab),
            tooltip: tab_tooltip(tab),
            icon: tab_icon(&tab.kind),
            active,
            favicon,
        }
    }
}

/// L'icône d'un onglet (spec §4.2) : `home` pour l'Accueil, `terminal` pour un journal, `public` pour une page web.
pub fn tab_icon(kind: &TabKind) -> Icon {
    match kind {
        TabKind::Start => Icon::Home,
        TabKind::Web { .. } => Icon::Public,
        TabKind::Log { .. } => Icon::Terminal,
    }
}

/// « Journal · <serveur> », avec le port d'un serveur repéré : plusieurs serveurs repérés portent le même nom (`python3 -m http.server`).
/// Le port passe devant le nom : un titre trop long perd la fin du nom, jamais ce qui distingue les onglets.
fn journal_title(name: &str, port: Option<u16>) -> String {
    match port {
        Some(port) => format!("Journal · :{port} · {name}"),
        None => format!("Journal · {name}"),
    }
}

/// Le titre d'un onglet : celui de la page, sinon son hôte (le nom du fichier pour un fichier), « Accueil » pour la
/// page d'accueil, « Journal · <serveur> » pour un journal.
pub fn tab_title(tab: &Tab) -> String {
    match &tab.kind {
        TabKind::Start => "Accueil".to_owned(),
        TabKind::Log { name, port, .. } => journal_title(name, *port),
        TabKind::Web { url } => tab
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map_or_else(|| host_or_name(url), str::to_owned),
    }
}

/// Ce que dit la bulle d'un onglet : le titre complet de la page, son adresse quand elle n'en a pas, « Accueil » pour la
/// page d'accueil.
pub fn tab_tooltip(tab: &Tab) -> String {
    match &tab.kind {
        TabKind::Start => "Accueil".to_owned(),
        TabKind::Log { name, port, .. } => journal_title(name, *port),
        TabKind::Web { url } => tab
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map_or_else(|| url.clone(), str::to_owned),
    }
}

fn host_or_name(url: &str) -> String {
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
    parsed
        .host_str()
        .map_or_else(|| url.to_owned(), str::to_owned)
}

/// Une page `http` hors de cette machine et de son réseau : la seule que la barre marque. Une page `http` locale (un
/// serveur de développement) ne l'est pas, ce serait du bruit sur chaque onglet de l'utilisateur.
pub fn is_insecure(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "http" {
        return false;
    }
    match parsed.host() {
        Some(Host::Domain(domain)) => {
            let domain = domain.to_ascii_lowercase();
            !(domain == "localhost" || domain.ends_with(".localhost"))
        }
        Some(Host::Ipv4(ip)) => {
            !(ip.is_loopback() || ip.is_unspecified() || ip.is_private() || ip.is_link_local())
        }
        Some(Host::Ipv6(ip)) => !IpAddr::V6(ip).is_loopback(),
        None => false,
    }
}

/// La largeur que l'en-tête laisse à la rangée d'onglets, pour un volet de `panel_width` dont l'en-tête est retenu de
/// `header_inset` à gauche.
pub fn strip_width(panel_width: Pixels, header_inset: Pixels, theme: &Theme) -> Pixels {
    (panel_width - header_inset - theme.px(HEADER_RESERVED)).max(Pixels::ZERO)
}

/// Les onglets sont-ils trop serrés pour un titre lisible ? Se calcule sur la largeur de la rangée et le nombre d'onglets,
/// jamais sur du texte mesuré : chaque onglet reçoit `(rangée - écarts) / nombre`, et sous `narrow_below` l'onglet ne
/// montre plus que son icône. Toutes les cotes en pixels de fenêtre.
pub fn tabs_are_narrow(strip_width: f32, tab_count: usize, gap: f32, narrow_below: f32) -> bool {
    if tab_count == 0 {
        return false;
    }
    let gaps = gap * (tab_count - 1) as f32;
    (strip_width - gaps) / (tab_count as f32) < narrow_below
}

/// La rangée des onglets de l'en-tête : défile horizontalement quand ils ne tiennent plus à leur largeur plancher.
/// `strip_width` est la place que l'en-tête lui laisse (`None` : volet agrandi, la largeur n'est pas connue, les onglets
/// restent larges). Quand elle est trop juste pour des titres lisibles, les onglets passent en mode étroit : icône seule,
/// la croix pour le seul onglet actif, et la rangée prend toute la place pour que les onglets se la partagent.
pub fn tab_strip(
    tabs: Vec<TabView>,
    panel: &Entity<BrowserPanel>,
    strip_width: Option<Pixels>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let narrow = strip_width.filter(|width| {
        tabs_are_narrow(
            f32::from(*width),
            tabs.len(),
            f32::from(theme.px(TAB_GAP)),
            f32::from(theme.px(TAB_NARROW_BELOW)),
        )
    });
    div()
        .id("browser-tabs")
        .flex()
        .items_center()
        .gap(theme.px(TAB_GAP))
        .min_w_0()
        .overflow_x_scroll()
        .when_some(narrow, |strip, width| strip.w(width))
        .children(
            tabs.into_iter()
                .map(|view| tab_item(view, panel, narrow.is_some(), theme, cx)),
        )
}

fn tab_item(
    view: TabView,
    panel: &Entity<BrowserPanel>,
    narrow: bool,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let TabView {
        id,
        title,
        tooltip,
        icon,
        active,
        favicon,
    } = view;
    let text = if active { TEXT_LABEL } else { TEXT_MIN };
    let to_activate = panel.clone();
    let to_close = panel.clone();
    // Étroit, seul l'onglet actif garde sa croix (un clic active l'onglet voisin, qui la montre à son tour).
    let show_close = !narrow || active;
    let tab_min_width = if active {
        NARROW_ACTIVE_TAB_MIN_WIDTH
    } else {
        NARROW_TAB_MIN_WIDTH
    };
    div()
        .id(("browser-tab", id))
        .flex()
        .items_center()
        .gap(theme.px(if narrow { 4. } else { 7. }))
        .h(theme.px(TAB_HEIGHT))
        .when(!narrow, |tab| {
            tab.min_w(theme.px(TAB_MIN_WIDTH))
                .max_w(theme.px(TAB_MAX_WIDTH))
                .pl(theme.px(9.))
                .pr(theme.px(5.))
        })
        .when(narrow, |tab| {
            tab.flex_1()
                .justify_center()
                .min_w(theme.px(tab_min_width))
                .max_w(theme.px(TAB_NARROW_BELOW))
                .px(theme.px(5.))
        })
        .rounded(theme.px(TAB_RADIUS))
        .occlude()
        .cursor_pointer()
        .tooltip(move |_, cx| cx.new(|_| HelpBubble::new(tooltip.clone())).into())
        .when(active, |tab| tab.bg(white(BUTTON_HOVER)))
        .when(!active, |tab| tab.hover(|style| style.bg(white(HOVER))))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.focus_browser(window, cx);
                to_activate.update(cx, |panel, cx| panel.activate_tab(id, window, cx));
            }),
        )
        .child(match favicon {
            // Le favicon de la page, tel quel : le demi-ton de 50 % ne vaut que pour le glyphe monochrome.
            Some(image) => img(image)
                .flex_none()
                .size(theme.px(15.))
                .rounded(theme.px(3.))
                .into_any_element(),
            None => svg()
                .path(icon.path())
                .flex_none()
                .size(theme.px(15.))
                .text_color(white(ICON_HEADER))
                .into_any_element(),
        })
        .when(!narrow, |tab| {
            tab.child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(theme.text(12.5))
                    .text_color(white(text))
                    .child(title),
            )
        })
        .when(show_close, |tab| {
            tab.child(
                div()
                    .id(("browser-tab-close", id))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(theme.px(TAB_CLOSE_SIZE))
                    .rounded(theme.px(4.))
                    .hover(|style| style.bg(white(HOVER_STRONG)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.focus_browser(window, cx);
                            to_close.update(cx, |panel, cx| panel.close_tab(id, window, cx));
                        }),
                    )
                    .child(
                        svg()
                            .path(Icon::Close.path())
                            .size(theme.px(14.))
                            .text_color(white(ICON_CLOSE)),
                    ),
            )
        })
}

/// Le bouton « + » qui suit le dernier onglet.
pub fn new_tab_button(
    panel: &Entity<BrowserPanel>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let panel = panel.clone();
    header_button(
        theme,
        "browser-new-tab",
        Icon::Add,
        18.,
        move |this, window, cx| {
            this.focus_browser(window, cx);
            panel.update(cx, |panel, cx| panel.new_tab(window, cx));
        },
        cx,
    )
}

/// La rangée d'adresse : précédent, suivant, recharger (arrêter pendant un chargement), puis le champ ; sous elle, le
/// filet de progression pendant un chargement.
pub fn nav_row(
    panel: &BrowserPanel,
    theme: &Theme,
    cx: &mut Context<BrowserPanel>,
) -> impl IntoElement + use<> {
    let slot = panel.active_slot();
    let has_page = slot.is_some();
    let (can_go_back, can_go_forward, loading, progress) =
        slot.map_or((false, false, false, 0.), |slot| {
            (
                slot.state.can_go_back,
                slot.state.can_go_forward,
                slot.state.loading,
                slot.state.progress,
            )
        });
    let reload_icon = if loading { Icon::Close } else { Icon::Refresh };
    let row = div()
        .relative()
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.px(2.))
        .h(theme.px(NAV_ROW_HEIGHT))
        .px(theme.px(NAV_ROW_PADDING_X))
        .pb(theme.px(NAV_ROW_PADDING_BOTTOM))
        .child(nav_button(
            "browser-back",
            Icon::ArrowBack,
            can_go_back,
            theme,
            |panel, _, _| panel.go_back(),
            cx,
        ))
        .child(nav_button(
            "browser-forward",
            Icon::ArrowForward,
            can_go_forward,
            theme,
            |panel, _, _| panel.go_forward(),
            cx,
        ))
        .child(nav_button(
            "browser-reload",
            reload_icon,
            has_page,
            theme,
            |panel, window, cx| panel.reload_or_stop(window, cx),
            cx,
        ))
        .child(address_box(panel, theme, cx))
        .when(loading, |row| {
            row.child(
                div()
                    .absolute()
                    .left_0()
                    .bottom_0()
                    .h(theme.px(PROGRESS_HEIGHT))
                    .w(relative((progress as f32).clamp(PROGRESS_FLOOR, 1.)))
                    .bg(color(FOCUS)),
            )
        });
    // Le filet d'un pixel entre la rangée et la page (la maquette : blanc à 6 %).
    div()
        .flex_none()
        .flex()
        .flex_col()
        .child(row)
        .child(div().flex_none().h(px(1.)).bg(white(SEPARATOR)))
}

/// Un bouton de la rangée d'adresse : 28 × 28, rayon 6, icône à 55 %, éteint à 25 % (ni survol ni clic).
fn nav_button<F>(
    id: &'static str,
    icon: Icon,
    enabled: bool,
    theme: &Theme,
    on_press: F,
    cx: &mut Context<BrowserPanel>,
) -> impl IntoElement + use<F>
where
    F: Fn(&mut BrowserPanel, &mut Window, &mut Context<BrowserPanel>) + 'static,
{
    div()
        .id(id)
        .group(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(theme.px(28.))
        .rounded(theme.px(6.))
        .occlude()
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(white(HOVER_STRONG)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        window.focus(&this.focus_handle, cx);
                        on_press(this, window, cx);
                    }),
                )
        })
        .child(
            svg()
                .path(icon.path())
                .size(theme.px(18.))
                .text_color(white(if enabled { TEXT_MIN } else { TEXT_DISABLED })),
        )
}

/// La rangée de recherche `⌘F`, sous la rangée d'adresse : une rangée du volet, pas une couche posée sur la page.
/// Le champ, le texte du résultat, précédent, suivant, fermer.
pub fn find_row(
    bar: &FindBar,
    theme: &Theme,
    cx: &mut Context<BrowserPanel>,
) -> impl IntoElement + use<> {
    let field_focus = bar.field.focus_handle(cx);
    div()
        .key_context(FIND_KEY_CONTEXT)
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.px(2.))
        .h(theme.px(NAV_ROW_HEIGHT))
        .px(theme.px(NAV_ROW_PADDING_X))
        .child(
            field_frame("browser-find-box", theme)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_, _: &MouseDownEvent, window, cx| {
                        window.focus(&field_focus, cx);
                        window.prevent_default();
                    }),
                )
                .child(field_input(&bar.field, theme)),
        )
        .child(
            div()
                .flex_none()
                .px(theme.px(6.))
                .text_size(theme.text(12.))
                .text_color(white(TEXT_MIN))
                .child(status_label(bar.status)),
        )
        .child(nav_button(
            "browser-find-previous",
            Icon::ExpandLess,
            true,
            theme,
            |panel, window, cx| panel.step_find(true, window, cx),
            cx,
        ))
        .child(nav_button(
            "browser-find-next",
            Icon::ExpandMore,
            true,
            theme,
            |panel, window, cx| panel.step_find(false, window, cx),
            cx,
        ))
        .child(nav_button(
            "browser-find-close",
            Icon::Close,
            true,
            theme,
            |panel, window, cx| panel.close_find(window, cx),
            cx,
        ))
}

/// La ligne « Téléchargement échoué : <cause> » sous la rangée d'adresse, en rouge d'erreur.
pub fn download_line(line: String, theme: &Theme) -> impl IntoElement + use<> {
    div()
        .id("browser-download-error")
        .flex_none()
        .px(theme.px(12.))
        .pb(theme.px(6.))
        .text_size(theme.text(12.))
        .text_color(color(ERROR_TEXT))
        .child(line)
}

/// Le corps d'un onglet dont la page ne s'affiche pas : la cause au centre, et le bouton qui recharge l'adresse
/// (« Réessayer » après un échec, « Recharger » après un arrêt du processus de la page). Les cotes (écart de 14, retrait
/// de 24, bouton de 28 de haut au rayon 6, texte de 13,5 et 12,5) sont un choix de Claude : la spec §6 ne dit que les
/// textes, la maquette n'a pas d'écran d'erreur. Les fonds et textes sont des jetons existants (`HOVER_STRONG`,
/// `CONTROL`, `TEXT_LABEL`), déjà dans `contrast_table` sur `PANEL`.
pub fn error_body(
    message: &str,
    button: Option<(&'static str, ErrorAction)>,
    theme: &Theme,
    cx: &mut Context<BrowserPanel>,
) -> impl IntoElement + use<> {
    div()
        .id("browser-error")
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(theme.px(14.))
        .px(theme.px(24.))
        .child(
            div()
                .max_w_full()
                .text_size(theme.text(13.5))
                .text_color(white(TEXT_LABEL))
                .text_center()
                .child(message.to_owned()),
        )
        .children(button.map(|(label, action)| {
            div()
                .id("browser-retry")
                .flex()
                .items_center()
                .justify_center()
                .h(theme.px(28.))
                .px(theme.px(14.))
                .rounded(theme.px(6.))
                .bg(white(HOVER_STRONG))
                .hover(|style| style.bg(white(CONTROL)))
                .cursor_pointer()
                .text_size(theme.text(12.5))
                .text_color(white(TEXT_LABEL))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        match &action {
                            ErrorAction::Retry => this.retry(window, cx),
                            ErrorAction::StartServer(key) => {
                                this.start_server_from_error(key.clone(), window, cx);
                            }
                        }
                    }),
                )
                .child(label)
        }))
}

/// Ce que fait le bouton du corps d'une erreur : rejouer l'adresse, ou démarrer le serveur dont c'est le port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorAction {
    Retry,
    StartServer(ServerKey),
}

/// Le bouton `more_vert` de l'en-tête : le menu s'ouvre là où l'appui est tombé.
pub fn more_button(
    panel: &Entity<BrowserPanel>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let panel = panel.clone();
    header_button_at(
        theme,
        "browser-more",
        Icon::MoreVert,
        18.,
        move |position, this, window, cx| {
            this.focus_browser(window, cx);
            // `Placement::Toolbar` est câblé sur le bouton `⋮` de la barre d'outils de la colonne principale (cotes de
            // `popover.rs`, menu enfant de ce bouton) : il ne sait pas s'ancrer ici, et l'étendre est hors de cette tâche.
            // Le menu se pose donc à l'appui, soit sur le bouton ; plus bas, la vue native de la page recouvrirait
            // davantage d'entrées (l'instantané sous un menu est à la tâche 9).
            panel.update(cx, |panel, cx| panel.open_menu(position, cx));
        },
        cx,
    )
}

/// La boîte d'un champ du volet : celle du filtre de Fichiers (28 de haut, rayon 7, creux noir à 28 %, contour blanc à
/// 8 %). Celle de l'adresse et celle de la recherche partagent ces cotes.
fn field_frame(id: &'static str, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap(theme.px(6.))
        .h(theme.px(FIELD_HEIGHT))
        .px(theme.px(FIELD_PADDING))
        .rounded(theme.px(FIELD_RADIUS))
        .border_1()
        .border_color(white(FIELD_RING))
        .bg(black(FIELD_BACKGROUND))
        .cursor_text()
}

/// La boîte du champ d'adresse, le champ du kit dedans. Un appui n'importe où dans la boîte donne le focus au champ.
fn address_box(
    panel: &BrowserPanel,
    theme: &Theme,
    cx: &mut Context<BrowserPanel>,
) -> impl IntoElement + use<> {
    let field_focus = panel.address.focus_handle(cx);
    let insecure = matches!(
        &panel.tabs.active().kind,
        TabKind::Web { url } if is_insecure(url)
    );
    let empty = panel.address.read(cx).value().is_empty();
    field_frame("browser-address-box", theme)
        .key_context(ADDRESS_KEY_CONTEXT)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_, _: &MouseDownEvent, window, cx| {
                window.focus(&field_focus, cx);
                // Le corps du volet reprendrait le focus à la remontée de l'appui.
                window.prevent_default();
            }),
        )
        .when(insecure, |address| {
            address.child(
                svg()
                    .path(Icon::LockOpen.path())
                    .flex_none()
                    .size(theme.px(15.))
                    .text_color(white(TEXT_MIN)),
            )
        })
        // Le champ vide (`.url.empty` de la maquette A) : la loupe devant le texte d'invite.
        .when(empty && !insecure, |address| {
            address.child(
                svg()
                    .path(Icon::Search.path())
                    .flex_none()
                    .size(theme.px(15.))
                    .text_color(white(TEXT_MIN)),
            )
        })
        .child(field_input(&panel.address, theme))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(kind: TabKind, title: Option<&str>) -> Tab {
        Tab {
            id: 1,
            kind,
            title: title.map(str::to_owned),
        }
    }

    fn web(url: &str) -> TabKind {
        TabKind::Web { url: url.into() }
    }

    #[test]
    fn a_tab_is_titled_by_its_page_then_its_host_and_the_start_page_is_home() {
        assert_eq!(tab_title(&tab(TabKind::Start, Some("ignoré"))), "Accueil");
        assert_eq!(
            tab_title(&tab(web("http://localhost:8811/"), Some("Fixture IdemA"))),
            "Fixture IdemA"
        );
        assert_eq!(
            tab_title(&tab(web("http://localhost:8811/"), None)),
            "localhost"
        );
        assert_eq!(
            tab_title(&tab(web("https://vercel.com/x?y=1"), Some("  "))),
            "vercel.com"
        );
        assert_eq!(tab_title(&tab(web("http://[::1]:3000/"), None)), "[::1]");
        assert_eq!(
            tab_title(&tab(web("file:///tmp/rapport%20v2.pdf"), None)),
            "rapport v2.pdf"
        );
    }

    #[test]
    fn a_tab_tooltip_is_the_full_title_then_the_address_and_home_for_the_start_page() {
        let long = "Un titre de page bien trop long pour tenir dans un onglet de 72 points";
        assert_eq!(tab_tooltip(&tab(TabKind::Start, Some("ignoré"))), "Accueil");
        assert_eq!(
            tab_tooltip(&tab(web("http://localhost:8811/"), Some(long))),
            long
        );
        assert_eq!(
            tab_tooltip(&tab(web("https://vercel.com/x?y=1"), Some("  "))),
            "https://vercel.com/x?y=1"
        );
        assert_eq!(
            tab_tooltip(&tab(web("http://localhost:8811/p"), None)),
            "http://localhost:8811/p"
        );
    }

    #[test]
    fn a_journal_tab_is_titled_after_its_server_and_wears_the_terminal_icon() {
        let journal = TabKind::Log {
            project: std::path::PathBuf::from("/work/alpha"),
            name: "web".to_owned(),
            port: Some(8811),
        };
        // Le titre de la page n'existe pas pour un journal : même un titre retenu ne le change pas.
        let tab = tab(journal.clone(), Some("ignoré"));
        assert_eq!(tab_title(&tab), "Journal · :8811 · web");
        assert_eq!(tab_tooltip(&tab), "Journal · :8811 · web");
        // Une entrée de configuration n'a pas de port dans l'onglet : son nom suffit.
        assert_eq!(journal_title("web", None), "Journal · web");
        assert_eq!(tab_icon(&journal), Icon::Terminal);
        assert_eq!(tab_icon(&TabKind::Start), Icon::Home);
        assert_eq!(tab_icon(&web("http://localhost:8811/")), Icon::Public);
    }

    #[test]
    fn tabs_turn_narrow_when_their_share_of_the_strip_cannot_hold_a_title() {
        // Rangée de 380, écart de 2, seuil de 96.
        let narrow = |count| tabs_are_narrow(380., count, 2., 96.);
        assert!(!narrow(0));
        // 1 à 3 onglets : 380, 189, 125 de large chacun, assez pour un titre.
        assert!(!narrow(1) && !narrow(2) && !narrow(3));
        // 4 onglets : (380 - 6) / 4 = 93,5 < 96.
        assert!(narrow(4));
        assert!(narrow(5) && narrow(12));
        // Le seuil lui-même est encore large : 3 onglets de 96 pile dans 292 (96 * 3 + 4).
        assert!(!tabs_are_narrow(292., 3, 2., 96.));
        assert!(tabs_are_narrow(291., 3, 2., 96.));
        // Une rangée sans place ne plante pas.
        assert!(tabs_are_narrow(0., 2, 2., 96.));
    }

    #[test]
    fn only_remote_plain_http_pages_are_marked_insecure() {
        assert!(is_insecure("http://example.com/"));
        assert!(is_insecure("http://93.184.216.34/"));
        for local in [
            "http://localhost:3000/",
            "http://app.localhost/",
            "http://127.0.0.1:8080/",
            "http://192.168.1.20/",
            "http://10.0.0.5:3000/",
            "http://[::1]:3000/",
        ] {
            assert!(!is_insecure(local), "{local}");
        }
        assert!(!is_insecure("https://example.com/"));
        assert!(!is_insecure("file:///tmp/a.html"));
        assert!(!is_insecure("pas une adresse"));
    }
}
