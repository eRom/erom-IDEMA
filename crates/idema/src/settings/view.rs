//! La vue de la fenêtre « Personnaliser » : barre latérale (recherche, deux groupes, pied), en-tête de section et
//! aiguillage vers le contenu de chaque section. Les cotes sont celles de la maquette A (`maquette/settings-window.html`),
//! passées par `Theme::px` sauf la bande de titre.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, FontWeight, KeyBinding, MouseButton,
    MouseDownEvent, Render, SharedString, Subscription, Window, WindowHandle, actions, div,
    prelude::*, px, svg,
};
use gpui_component::input::{InputEvent, InputState};

use super::companion_section::CompanionState;
use super::idema_sections::IdemaState;
use super::mcp_section::McpState;
use super::nav::{NavGroup, SELECT_PROJECT_HELP, Section, SystemItem, filter_nav, filter_system};
use super::plugins_section::PluginsState;
use super::skills_section::SkillsState;
use crate::assets::Icon;
use crate::files::panel::{FIELD_HEIGHT, FIELD_PADDING, FIELD_RADIUS, field_input, home_dir};
use crate::help_bubble::HelpBubble;
use crate::sidebar::TITLE_BAND;
use crate::theme::{
    FIELD_BACKGROUND, FIELD_RING, HOVER, ICON_SECONDARY, SELECTED, SEPARATOR, SIDEBAR,
    TEXT_DISABLED, TEXT_FULL, TEXT_MIN, TEXT_PRIMARY, TEXT_SECONDARY, Theme, UI_FONT, WINDOW,
    black, color, white,
};
use crate::workspace::{OpenConfig, Workspace, drag_window};

actions!(settings, [CloseSettings]);

/// Contexte de touches de la fenêtre : `⌘W` la ferme, `⌘,` la ramène au premier plan.
pub const KEY_CONTEXT: &str = "Settings";

/// Largeur de la barre latérale dans la maquette.
const NAV_WIDTH: f32 = 232.;
/// Hauteur de l'en-tête de section, et de la bande de titre au-dessus de la barre latérale.
const HEADER_HEIGHT: f32 = 52.;
const PLACEHOLDER: &str = "Rechercher un réglage";
const EDIT_CONFIG_LABEL: &str = "Éditer config.toml";
const NO_MATCH: &str = "Aucun réglage ne correspond.";

/// Les liaisons de la fenêtre, à poser avec celles de l'app.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-w", CloseSettings, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-,", OpenConfig, Some(KEY_CONTEXT)),
    ]
}

/// L'icône de la ligne de barre latérale d'une section.
fn section_icon(section: Section) -> Icon {
    match section {
        Section::General => Icon::Tune,
        Section::Agents => Icon::SmartToy,
        Section::Machines => Icon::Public,
        Section::CommitMessage => Icon::EditNote,
        Section::Browser => Icon::Language,
        Section::Companion => Icon::Smartphone,
        Section::McpServers => Icon::Mcp,
        Section::Plugins => Icon::Extension,
        Section::Marketplaces => Icon::Storefront,
        Section::Skills => Icon::Bolt,
    }
}

/// L'icône de la ligne « Système ».
fn system_icon(item: SystemItem) -> Icon {
    match item {
        SystemItem::MacEnvironment => Icon::Terminal,
        SystemItem::IdnmaNetwork => Icon::Hub,
    }
}

pub struct SettingsView {
    /// La fenêtre principale : « Éditer config.toml » lui fait ouvrir le fichier. Elle peut avoir disparu.
    workspace: WindowHandle<Workspace>,
    /// Les lignes « Système » étaient-elles allumées au dernier rendu ? Le changement seul rend la fenêtre.
    system_enabled: bool,
    _main_observation: Subscription,
    /// `config.toml`, que les sections IdemA lisent et écrivent (tâches 4 et suivantes).
    pub(super) config_path: PathBuf,
    section: Section,
    search: Entity<InputState>,
    /// Le texte de la recherche à la dernière frappe.
    query: String,
    /// Les compteurs de la barre latérale, posés par les sections qui en ont un (agents, serveurs, plugins, marketplaces,
    /// skills).
    counts: HashMap<Section, SharedString>,
    /// L'état des quatre sections IdemA (champs, erreurs, relecture de `config.toml`).
    pub(super) idema: IdemaState,
    /// L'état de la section « Compagnon » (état du service, QR code, code d'appairage, veille du `watch`).
    pub(super) companion: CompanionState,
    /// L'état de la section « Serveurs MCP » (liste lue dans `~/.claude.json`, lignes dépliées, appel CLI en cours).
    pub(super) mcp: McpState,
    /// L'état des sections « Plugins » et « Marketplaces » (listes lues dans le CLI, appel en cours, champs d'ajout).
    pub(super) plugins: PluginsState,
    /// L'état de la section « Skills » (dossier `~/.claude/skills`, `skillOverrides`, menu ouvert, dernier échec).
    pub(super) skills: SkillsState,
    focus_handle: FocusHandle,
    _search_subscription: Subscription,
    _activation: Subscription,
    _quit: Subscription,
}

impl SettingsView {
    pub fn new(
        workspace: WindowHandle<Workspace>,
        main: Entity<Workspace>,
        system_enabled: bool,
        config_path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(PLACEHOLDER));
        // `Change` vient d'une frappe, d'un collage ou d'un annuler ; `↩` ouvre la première section qui reste.
        let subscription = cx.subscribe_in(
            &search,
            window,
            |this, state, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => {
                    this.query = state.read(cx).value().to_string();
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    if let Some(first) = filter_nav(&this.query).first() {
                        this.select(*first, cx);
                    }
                }
                _ => {}
            },
        );
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let idema = IdemaState::new(&config_path, window, cx);
        let plugins = PluginsState::new(window, cx);
        // Les listes de Claude Code changent hors de la fenêtre (un terminal, une autre session) : elles sont relues à
        // chaque retour au premier plan, sinon `⌘,` ramènerait une liste périmée.
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.refresh_displayed(cx);
            }
        });
        // `⌘Q` et la fermeture de la fenêtre principale ne passent pas par le bouton rouge de celle-ci : une saisie qui
        // attendait ses 400 ms part avant la sortie (modèle : `Workspace`).
        let quit = cx.on_app_quit(|this, cx| {
            this.flush_pending_on_quit(cx);
            std::future::ready(())
        });
        // La fenêtre principale se redessine à chaque changement de sélection : on ne rend la nôtre que si l'état des
        // lignes « Système » a changé, pas à chaque redessin du terminal. Jamais de `main.read` ici ni au rendu : la
        // fenêtre principale est en pleine mise à jour quand `⌘,` construit cette vue, la relire paniquerait. L'état
        // initial vient de l'appelant, la suite de l'observation (qui ne court qu'après la mise à jour).
        let main_observation = cx.observe(&main, |this, main, cx| {
            let enabled = main.read(cx).panel_project().is_some();
            if this.system_enabled != enabled {
                this.system_enabled = enabled;
                cx.notify();
            }
        });
        let mut view = Self {
            workspace,
            system_enabled,
            _main_observation: main_observation,
            config_path,
            section: Section::General,
            search,
            query: String::new(),
            counts: HashMap::new(),
            idema,
            companion: CompanionState::new(),
            mcp: McpState::new(),
            plugins,
            skills: SkillsState::new(),
            focus_handle,
            _search_subscription: subscription,
            _activation: activation,
            _quit: quit,
        };
        view.init_idema(window, cx);
        view.init_mcp(cx);
        view.init_plugins(cx);
        view.refresh_skills(cx);
        // Le bouton rouge ferme la fenêtre sans passer par `⌘W` : une saisie qui attendait ses 400 ms part avant.
        let closing = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            if let Some(settings) = closing.upgrade() {
                settings.update(cx, |this, cx| this.flush_pending(window, cx));
            }
            true
        });
        view
    }

    /// Relit les données de Claude Code de la section affichée. Sans effet pendant un appel (`refresh_*` s'en garde) et
    /// pour les sections IdemA, que leur propre veille de `config.toml` tient à jour.
    fn refresh_displayed(&mut self, cx: &mut Context<Self>) {
        match self.section {
            Section::McpServers => self.refresh_mcp(cx),
            Section::Plugins | Section::Marketplaces => self.refresh_plugins(cx),
            Section::Skills => self.refresh_skills(cx),
            Section::Companion => self.recheck_tailnet(cx),
            _ => {}
        }
    }

    /// La fenêtre principale, que la section « Compagnon » interroge pour l'état du service.
    pub(super) fn workspace_handle(&self) -> WindowHandle<Workspace> {
        self.workspace
    }

    /// Affiche `section` (utilisé aussi par le lien « Marketplaces » de la section Plugins).
    pub fn select(&mut self, section: Section, cx: &mut Context<Self>) {
        if self.section != section {
            self.section = section;
            // Le fichier de Claude Code est relu à l'ouverture de la section.
            self.refresh_displayed(cx);
            cx.notify();
        }
    }

    /// Pose, ou retire avec `None`, le compteur de la ligne de `section`.
    pub fn set_count(&mut self, section: Section, count: Option<String>, cx: &mut Context<Self>) {
        match count {
            Some(text) => self.counts.insert(section, text.into()),
            None => self.counts.remove(&section),
        };
        cx.notify();
    }

    /// « Éditer config.toml » : la fenêtre principale ouvre le fichier comme `⌘,` le faisait, puis passe devant. Sans
    /// fenêtre principale, rien.
    pub(super) fn edit_config_file(&mut self, cx: &mut Context<Self>) {
        let _ = self.workspace.update(cx, |workspace, window, cx| {
            workspace.open_config_file(window, cx);
            window.activate_window();
        });
    }

    /// Une ligne « Système » : la fenêtre principale ouvre le fichier en onglet permanent, puis passe devant. La fenêtre
    /// « Personnaliser » reste ouverte derrière. Sans projet dans la fenêtre principale, ou sans fichier, rien (jamais
    /// d'app externe).
    fn open_system_file(&mut self, item: SystemItem, cx: &mut Context<Self>) {
        let Some(home) = home_dir() else {
            return;
        };
        let path = item.path(&home);
        let _ = self.workspace.update(cx, |workspace, window, cx| {
            if workspace.open_system_file(&path, window, cx) {
                window.activate_window();
            }
        });
    }

    fn render_nav(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = *theme;
        let field_focus = self.search.focus_handle(cx);
        let shown = filter_nav(&self.query);
        let search_box = div()
            .id("settings-search-box")
            .flex()
            .flex_none()
            .items_center()
            .gap(theme.px(6.))
            .h(theme.px(FIELD_HEIGHT))
            .px(theme.px(FIELD_PADDING))
            .mb(theme.px(12.))
            .rounded(theme.px(FIELD_RADIUS))
            .border_1()
            .border_color(white(FIELD_RING))
            .bg(black(FIELD_BACKGROUND))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _: &MouseDownEvent, window, cx| {
                    window.focus(&field_focus, cx);
                    window.prevent_default();
                }),
            )
            .child(
                svg()
                    .path(Icon::Search.path())
                    .flex_none()
                    .size(theme.px(15.))
                    .text_color(white(TEXT_MIN)),
            )
            .child(field_input(&self.search, &theme));

        let system_enabled = self.system_enabled;
        let system_shown = filter_system(&self.query);
        let mut groups = div().flex().flex_col().flex_1().min_h_0();
        for group in NavGroup::ALL {
            let sections: Vec<Section> = shown
                .iter()
                .copied()
                .filter(|section| section.group() == group)
                .collect();
            let items: &[SystemItem] = if group == NavGroup::System {
                &system_shown
            } else {
                &[]
            };
            if sections.is_empty() && items.is_empty() {
                continue;
            }
            groups = groups.child(
                div()
                    .flex()
                    .items_center()
                    .h(theme.px(26.))
                    .mt(theme.px(6.))
                    .px(theme.px(8.))
                    .text_size(theme.text(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(white(TEXT_MIN))
                    .child(group.label()),
            );
            for section in sections {
                groups = groups.child(self.nav_row(section, &theme, cx));
            }
            for item in items {
                groups = groups.child(self.system_row(*item, system_enabled, &theme, cx));
            }
        }
        if shown.is_empty() && system_shown.is_empty() {
            groups = groups.child(
                div()
                    .px(theme.px(8.))
                    .py(theme.px(6.))
                    .text_size(theme.text(12.))
                    .text_color(white(TEXT_MIN))
                    .child(NO_MATCH),
            );
        }

        let footer = div()
            .flex_none()
            .pt(theme.px(8.))
            .border_t_1()
            .border_color(white(SEPARATOR))
            .child(
                div()
                    .id("settings-edit-config")
                    .group("settings-edit-config")
                    .flex()
                    .items_center()
                    .gap(theme.px(10.))
                    .h(theme.px(30.))
                    .px(theme.px(8.))
                    .rounded(theme.px(6.))
                    .text_color(white(TEXT_MIN))
                    .cursor_pointer()
                    .hover(|style| style.bg(white(HOVER)).text_color(white(TEXT_PRIMARY)))
                    .on_click(cx.listener(|this, _, _, cx| this.edit_config_file(cx)))
                    .child(
                        svg()
                            .path(Icon::Description.path())
                            .flex_none()
                            .size(theme.px(16.))
                            .text_color(white(ICON_SECONDARY)),
                    )
                    .child(EDIT_CONFIG_LABEL),
            );

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(theme.px(NAV_WIDTH))
            .h_full()
            .bg(color(SIDEBAR))
            .border_r_1()
            .border_color(white(SEPARATOR))
            // Bande de titre : vide, elle porte le déplacement de la fenêtre et les feux.
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
                    .flex_1()
                    .min_h_0()
                    .px(theme.px(10.))
                    .pb(theme.px(10.))
                    .child(search_box)
                    .child(groups)
                    .child(footer),
            )
    }

    /// Une ligne de section : icône, libellé, compteur à droite quand la section en a un.
    fn nav_row(
        &mut self,
        section: Section,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let selected = self.section == section;
        let count = self.counts.get(&section).cloned();
        div()
            .id(("settings-nav", section as usize))
            .flex()
            .items_center()
            .gap(theme.px(10.))
            .h(theme.px(30.))
            .px(theme.px(8.))
            .rounded(theme.px(6.))
            .cursor_pointer()
            .text_color(white(if selected { TEXT_FULL } else { TEXT_SECONDARY }))
            .when(selected, |row| row.bg(white(SELECTED)))
            .when(!selected, |row| row.hover(|style| style.bg(white(HOVER))))
            .on_click(cx.listener(move |this, _, _, cx| this.select(section, cx)))
            .child(
                svg()
                    .path(section_icon(section).path())
                    .flex_none()
                    .size(theme.px(16.))
                    .text_color(white(if selected { TEXT_FULL } else { ICON_SECONDARY })),
            )
            .child(div().min_w_0().truncate().child(section.label()))
            .child(div().flex_1())
            .children(count.map(|text| {
                div()
                    .flex_none()
                    .text_size(theme.text(11.))
                    .text_color(white(TEXT_MIN))
                    .child(text)
            }))
    }

    /// Une ligne « Système » : même gabarit qu'une ligne de section, mais une action, sans état choisi. Éteinte (aucun
    /// projet dans la fenêtre principale), elle prend le texte désactivé du thème, perd le survol et le curseur main, et
    /// son infobulle dit quoi faire ; un clic n'y fait rien.
    fn system_row(
        &mut self,
        item: SystemItem,
        enabled: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let text = if enabled {
            TEXT_SECONDARY
        } else {
            TEXT_DISABLED
        };
        let icon = if enabled {
            ICON_SECONDARY
        } else {
            TEXT_DISABLED
        };
        div()
            .id(("settings-system", item as usize))
            .flex()
            .items_center()
            .gap(theme.px(10.))
            .h(theme.px(30.))
            .px(theme.px(8.))
            .rounded(theme.px(6.))
            .text_color(white(text))
            .when(enabled, |row| {
                row.cursor_pointer()
                    .hover(|style| style.bg(white(HOVER)))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_system_file(item, cx)))
            })
            .when(!enabled, |row| {
                row.tooltip(|_, cx| cx.new(|_| HelpBubble::new(SELECT_PROJECT_HELP)).into())
            })
            .child(
                svg()
                    .path(system_icon(item).path())
                    .flex_none()
                    .size(theme.px(16.))
                    .text_color(white(icon)),
            )
            .child(div().min_w_0().truncate().child(item.label()))
    }

    /// L'en-tête de la section : titre, source en tertiaire, puis les actions à droite. Un bouton d'action arrête la
    /// propagation de son appui, sinon la bande se met à déplacer la fenêtre.
    fn render_header(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let section = self.section;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(theme.px(10.))
            .h(px(HEADER_HEIGHT))
            .px(theme.px(20.))
            .on_mouse_down(MouseButton::Left, drag_window)
            .child(
                div()
                    .text_size(theme.text(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(white(TEXT_PRIMARY))
                    .child(section.label()),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(theme.text(12.))
                    .text_color(white(TEXT_MIN))
                    .child(section.source()),
            )
            .child(div().flex_1())
            .child(self.render_actions(section, cx))
    }

    /// La zone d'actions de l'en-tête : « Ajouter » des agents, des favoris et des serveurs MCP, « Tout rafraîchir » des
    /// marketplaces.
    fn render_actions(&mut self, section: Section, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .children(self.idema_actions(section, cx))
            .children(self.mcp_actions(section, cx))
            .children(self.marketplaces_actions(section, cx))
            .into_any_element()
    }

    fn render_general(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        self.render_idema_section(Section::General, theme, cx)
    }

    fn render_agents(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        self.render_idema_section(Section::Agents, theme, cx)
    }

    fn render_machines(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        self.render_idema_section(Section::Machines, theme, cx)
    }

    fn render_commit_message(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        self.render_idema_section(Section::CommitMessage, theme, cx)
    }

    fn render_browser(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        self.render_idema_section(Section::Browser, theme, cx)
    }

    fn render_companion(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        self.render_companion_section(theme, cx)
    }

    fn render_mcp_servers(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        self.render_mcp_section(theme, cx)
    }

    fn render_plugins(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        self.render_plugins_section(theme, cx)
    }

    fn render_marketplaces(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        self.render_marketplaces_section(theme, cx)
    }

    /// Aiguille vers le contenu de la section affichée.
    fn render_section(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        match self.section {
            Section::General => self.render_general(theme, cx),
            Section::Agents => self.render_agents(theme, cx),
            Section::Machines => self.render_machines(theme, cx),
            Section::CommitMessage => self.render_commit_message(theme, cx),
            Section::Browser => self.render_browser(theme, cx),
            Section::Companion => self.render_companion(theme, cx),
            Section::McpServers => self.render_mcp_servers(theme, cx),
            Section::Plugins => self.render_plugins(theme, cx),
            Section::Marketplaces => self.render_marketplaces(theme, cx),
            Section::Skills => self.render_skills_section(theme, cx),
        }
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        let nav = self.render_nav(&theme, cx);
        let header = self.render_header(&theme, cx);
        let body = self.render_section(&theme, cx);
        div()
            .size_full()
            .flex()
            .bg(color(WINDOW))
            .font_family(UI_FONT)
            .text_size(theme.text(13.))
            .text_color(white(TEXT_PRIMARY))
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &CloseSettings, window, cx| {
                // Une saisie qui attendait ses 400 ms part avant que la fenêtre ne disparaisse.
                this.flush_pending(window, cx);
                window.remove_window();
            }))
            // `⌘,` et le menu « Personnaliser… » depuis cette fenêtre : elle est déjà ouverte, on la ramène devant.
            .on_action(cx.listener(|_, _: &OpenConfig, window, _| window.activate_window()))
            .child(nav)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(header)
                    .child(div().flex_1().min_h_0().child(body)),
            )
    }
}
