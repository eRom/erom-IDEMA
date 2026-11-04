//! La section « Plugins » de « Personnaliser » : les plugins de portée utilisateur lus par `claude plugin list --json`,
//! un interrupteur par plugin, l'installation par `nom@marketplace` et la désinstallation, le tout par le CLI `claude`
//! (portée `user`). Cet état sert aussi à la section « Marketplaces » (`marketplaces_section.rs`) : les deux listes se
//! lisent ensemble, et un seul appel CLI court à la fois pour les deux, car ils écrivent le même `settings.json`
//! (retirer un marketplace désinstalle ses plugins). Hors du fil d'interface ; après chaque appel, les deux listes sont
//! relues pour que l'écran montre l'état réel.

use std::path::{Path, PathBuf};

use gpui::{
    Anchor, AnchoredPositionMode, AnyElement, AnyWindowHandle, BoxShadow, Context, Entity,
    FontWeight, MouseButton, PromptButton, PromptLevel, SharedString, Subscription, Task, Window,
    anchored, deferred, div, point, prelude::*, px, svg,
};
use gpui_component::input::{InputEvent, InputState};

use super::claude_cli::{
    CliArgument, CliError, MarketplaceEntry, PluginEntry, describe_failure, marketplace_add_args,
    marketplace_list_args, marketplace_remove_args, marketplace_update_args, parse_marketplaces,
    parse_plugins, plugin_install_args, plugin_list_args, plugin_toggle_args,
    plugin_uninstall_args, reject_leading_dash, resolve_claude, run_claude,
    user_declared_marketplaces, validate_plugin_id,
};
use super::controls::{ButtonKind, action_button, error_line, field_box, icon_button, toggle};
use super::mcp_section::{busy_label, notice_banner};
use super::nav::Section;
use super::view::SettingsView;
use crate::assets::Icon;
use crate::files::panel::home_dir;
use crate::theme::{
    CONTROL, ERROR_TEXT, FOCUS_TEXT, HOVER, POPOVER, SEPARATOR, TEXT_MIN, TEXT_PRIMARY, Theme,
    color, color_alpha, white,
};

const NO_PLUGIN: &str = "Aucun plugin installé.";
const READING: &str = "Lecture de la liste...";
const INSTALL_PLACEHOLDER: &str = "nom@marketplace";
const OPEN_MARKETPLACES: &str = "Ouvrir Marketplaces";
pub(super) const ROW_HEIGHT: f32 = 40.;
/// Les colonnes de la liste (maquette `.prow` : `1fr 120px 44px 28px`, la colonne du marketplace élargie pour les noms
/// longs comme `claude-plugins-official`).
const MARKETPLACE_COLUMN: f32 = 170.;
pub(super) const TOGGLE_COLUMN: f32 = 44.;
pub(super) const MENU_COLUMN: f32 = 28.;
/// La largeur minimale du menu d'une ligne.
const MENU_WIDTH: f32 = 140.;

/// Vrai si le marketplace de `id` (après l'`@`) n'est pas dans `known`. Un identifiant sans `@` n'est pas un cas de
/// marketplace inconnu : `check_install_id` le refuse avant tout appel.
pub fn unknown_marketplace(id: &str, known: &[MarketplaceEntry]) -> bool {
    match id.split_once('@') {
        Some((_, marketplace)) => !known.iter().any(|entry| entry.name == marketplace),
        None => false,
    }
}

/// Le compteur de la barre latérale : plugins actifs sur plugins installés.
pub fn plugin_counter(plugins: &[PluginEntry]) -> String {
    let enabled = plugins.iter().filter(|plugin| plugin.enabled).count();
    format!("{enabled}/{}", plugins.len())
}

/// Ce que le geste de l'interrupteur demande à `id` : l'inverse de l'état lu. `None` pour un identifiant que la dernière
/// liste ne porte pas, car `claude plugin enable <inconnu>` sort en 0 et écrit l'identifiant dans `settings.json`.
pub fn toggle_target(plugins: &[PluginEntry], id: &str) -> Option<bool> {
    plugins
        .iter()
        .find(|plugin| plugin.id == id)
        .map(|plugin| !plugin.enabled)
}

/// Garde de l'activation, sur une liste lue à l'instant : `claude plugin enable <inconnu>` sort en 0 et écrit
/// `"<id>": true` dans `settings.json` (constaté avec le vrai CLI), donc on n'active qu'un plugin que la liste fraîche
/// porte encore en portée utilisateur.
pub fn check_enable_allowed(fresh: &[PluginEntry], id: &str) -> Result<(), String> {
    if fresh.iter().any(|plugin| plugin.id == id) {
        Ok(())
    } else {
        Err(format!(
            "« {id} » n'est plus installé en portée utilisateur : rien n'a été activé."
        ))
    }
}

/// Lit la liste des plugins avec `run` puis applique `check_enable_allowed`. Une liste illisible refuse aussi : sans
/// elle, on ne sait pas si l'identifiant existe encore.
fn guard_enable(
    run: &mut dyn FnMut(&[String]) -> Result<String, CliError>,
    id: &str,
) -> Result<(), CliError> {
    let refusal = |stderr: String| CliError { code: None, stderr };
    let json = run(&plugin_list_args())?;
    let fresh = parse_plugins(&json)
        .map_err(|error| refusal(format!("Sortie illisible de claude : {error}")))?;
    check_enable_allowed(&fresh, id).map_err(refusal)
}

/// L'identifiant saisi dans le champ d'installation : `nom@marketplace` sans espace, et pas pris pour une option du CLI.
pub fn check_install_id(id: &str) -> Result<(), &'static str> {
    reject_leading_dash(id, CliArgument::Id)?;
    validate_plugin_id(id)
}

/// Une écriture (ou la simple relecture) que la section demande au CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginOp {
    Read,
    Toggle { id: String, enable: bool },
    Install { id: String },
    Uninstall { id: String },
    MarketplaceAdd { source: String },
    MarketplaceRemove { name: String },
    MarketplaceUpdate { name: Option<String> },
}

impl PluginOp {
    /// Les arguments du CLI ; `None` pour la relecture seule.
    pub fn args(&self) -> Option<Vec<String>> {
        match self {
            Self::Read => None,
            Self::Toggle { id, enable } => Some(plugin_toggle_args(id, *enable)),
            Self::Install { id } => Some(plugin_install_args(id)),
            Self::Uninstall { id } => Some(plugin_uninstall_args(id)),
            Self::MarketplaceAdd { source } => Some(marketplace_add_args(source)),
            Self::MarketplaceRemove { name } => Some(marketplace_remove_args(name)),
            Self::MarketplaceUpdate { name } => Some(marketplace_update_args(name.as_deref())),
        }
    }
}

/// Les deux listes lues dans le CLI ; chacune garde son erreur.
pub struct Listing {
    pub plugins: Result<Vec<PluginEntry>, String>,
    pub marketplaces: Result<Vec<MarketplaceEntry>, String>,
}

/// Lit `plugin list --json` et `plugin marketplace list --json` avec `run`. `settings` (`~/.claude/settings.json`, lu
/// seulement) dit quels marketplaces l'utilisateur a déclarés, donc lesquels se retirent depuis Idema.
pub fn read_listing(
    run: &mut dyn FnMut(&[String]) -> Result<String, CliError>,
    settings: &Path,
) -> Listing {
    let plugins = run(&plugin_list_args())
        .map_err(|error| describe_failure(&error))
        .and_then(|json| {
            parse_plugins(&json).map_err(|error| format!("Sortie illisible de claude : {error}"))
        });
    let declared = user_declared_marketplaces(settings);
    let marketplaces = run(&marketplace_list_args())
        .map_err(|error| describe_failure(&error))
        .and_then(|json| {
            parse_marketplaces(&json, &declared)
                .map_err(|error| format!("Sortie illisible de claude : {error}"))
        });
    Listing {
        plugins,
        marketplaces,
    }
}

/// Le résultat d'un appel : l'écriture, puis les listes relues dans tous les cas.
struct Outcome {
    result: Result<(), CliError>,
    listing: Listing,
}

/// Joue `op` puis relit les listes. Bloquant : à appeler hors du fil d'interface.
fn run_op(bin: &Path, settings: &Path, op: &PluginOp) -> Outcome {
    let mut run = |args: &[String]| run_claude(bin, args);
    let result = match op.args() {
        // La liste en mémoire peut dater : un plugin désinstallé ailleurs ne doit pas être « activé » dans settings.json.
        Some(args) => match op {
            PluginOp::Toggle { id, enable: true } => {
                guard_enable(&mut run, id).and_then(|()| run(&args).map(|_| ()))
            }
            _ => run(&args).map(|_| ()),
        },
        None => Ok(()),
    };
    Outcome {
        result,
        listing: read_listing(&mut run, settings),
    }
}

/// La ligne, ou le champ, qu'un appel concerne.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Row {
    Plugin(String),
    NewPlugin,
    Marketplace(String),
    NewMarketplace,
    AllMarketplaces,
    /// La simple relecture : aucune ligne n'est concernée.
    Reading,
}

/// L'échec du dernier geste : le texte du CLI, tel quel, sous la ligne qu'il concerne.
pub(super) struct Failure {
    pub(super) row: Row,
    pub(super) text: String,
    /// Vrai pour une installation dont le marketplace est inconnu : le message est suivi du lien « Ouvrir Marketplaces ».
    pub(super) link: bool,
}

/// L'état des sections Plugins et Marketplaces, tenu par `SettingsView`.
pub(super) struct PluginsState {
    home: PathBuf,
    /// Le binaire `claude`, ou les chemins essayés.
    pub(super) claude: Result<PathBuf, Vec<PathBuf>>,
    pub(super) plugins: Result<Vec<PluginEntry>, String>,
    pub(super) marketplaces: Result<Vec<MarketplaceEntry>, String>,
    /// Faux tant que la première lecture n'a pas rendu : on n'affiche pas « Aucun plugin » avant de savoir.
    pub(super) loaded: bool,
    /// L'appel CLI en cours : un seul à la fois pour les deux sections.
    pub(super) busy: Option<Row>,
    pub(super) failure: Option<Failure>,
    /// Le plugin dont le menu `more_horiz` est ouvert.
    pub(super) menu: Option<String>,
    pub(super) install: Entity<InputState>,
    pub(super) source: Entity<InputState>,
    window: AnyWindowHandle,
    _call: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl PluginsState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<SettingsView>) -> Self {
        let home = home_dir();
        let install = cx.new(|cx| InputState::new(window, cx).placeholder(INSTALL_PLACEHOLDER));
        let source = cx.new(|cx| {
            InputState::new(window, cx).placeholder("owner/repo, URL git ou https, chemin local")
        });
        // Une frappe efface le message du champ (refus ou échec) ; `↩` envoie.
        let subscriptions = vec![
            cx.subscribe_in(
                &install,
                window,
                |this, _, event: &InputEvent, _, cx| match event {
                    InputEvent::Change => this.clear_field_failure(Row::NewPlugin, cx),
                    InputEvent::PressEnter { .. } => this.submit_install(cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &source,
                window,
                |this, _, event: &InputEvent, _, cx| match event {
                    InputEvent::Change => this.clear_field_failure(Row::NewMarketplace, cx),
                    InputEvent::PressEnter { .. } => this.submit_source(cx),
                    _ => {}
                },
            ),
        ];
        Self {
            home: home.unwrap_or_default(),
            claude: Err(Vec::new()),
            plugins: Ok(Vec::new()),
            marketplaces: Ok(Vec::new()),
            loaded: false,
            busy: None,
            failure: None,
            menu: None,
            install,
            source,
            window: window.window_handle(),
            _call: None,
            _subscriptions: subscriptions,
        }
    }

    /// Les plugins lus, ou rien quand la lecture a échoué.
    pub(super) fn plugin_list(&self) -> Vec<PluginEntry> {
        self.plugins.clone().unwrap_or_default()
    }

    pub(super) fn marketplace_list(&self) -> Vec<MarketplaceEntry> {
        self.marketplaces.clone().unwrap_or_default()
    }

    /// Le message de la ligne `row`, s'il y en a un.
    pub(super) fn failure_of(&self, row: &Row) -> Option<&Failure> {
        self.failure.as_ref().filter(|failure| &failure.row == row)
    }
}

impl SettingsView {
    // ---- Lecture et appels ------------------------------------------------------------------------------------------

    /// La première lecture : les compteurs de la barre latérale.
    pub(super) fn init_plugins(&mut self, cx: &mut Context<Self>) {
        self.refresh_plugins(cx);
    }

    /// Relit les deux listes et cherche `claude` de nouveau : à l'ouverture d'une des deux sections et après chaque
    /// appel. Rien pendant un appel : sa relecture de fin montre l'état réel.
    pub(super) fn refresh_plugins(&mut self, cx: &mut Context<Self>) {
        if self.plugins.busy.is_some() {
            return;
        }
        let state = &mut self.plugins;
        state.claude = resolve_claude(&state.home, std::env::var_os("IDEMA_CLAUDE_BIN").as_deref());
        if state.claude.is_err() {
            self.set_count(Section::Plugins, None, cx);
            self.set_count(Section::Marketplaces, None, cx);
            cx.notify();
            return;
        }
        self.start_plugin_call(Row::Reading, PluginOp::Read, cx);
    }

    /// Lance `op` hors du fil d'interface. Un seul appel à la fois : un second geste pendant le premier ne fait rien.
    pub(super) fn start_plugin_call(&mut self, row: Row, op: PluginOp, cx: &mut Context<Self>) {
        if self.plugins.busy.is_some() {
            return;
        }
        let Ok(bin) = self.plugins.claude.clone() else {
            return;
        };
        self.plugins.busy = Some(row.clone());
        if op != PluginOp::Read {
            self.plugins.failure = None;
        }
        self.plugins.menu = None;
        let settings = self.plugins.home.join(".claude").join("settings.json");
        let window = self.plugins.window;
        let background = cx.background_executor().clone();
        let task = cx.spawn(async move |this, cx| {
            let to_run = op.clone();
            let outcome = background
                .spawn(async move { run_op(&bin, &settings, &to_run) })
                .await;
            let _ = cx.update_window(window, |_, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.finish_plugin_call(row, op, outcome, window, cx)
                });
            });
        });
        self.plugins._call = Some(task);
        cx.notify();
    }

    /// La fin d'un appel : en cas d'échec le texte du CLI se pose AVANT la relecture, pour que la ligne le porte même si
    /// l'échec la fait disparaître ; en cas de succès le champ de saisie se vide.
    fn finish_plugin_call(
        &mut self,
        row: Row,
        op: PluginOp,
        outcome: Outcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.plugins.busy = None;
        self.plugins._call = None;
        let Outcome { result, listing } = outcome;
        match result {
            Ok(()) => match &op {
                PluginOp::Install { .. } => self
                    .plugins
                    .install
                    .update(cx, |state, cx| state.set_value("", window, cx)),
                PluginOp::MarketplaceAdd { .. } => self
                    .plugins
                    .source
                    .update(cx, |state, cx| state.set_value("", window, cx)),
                _ => {}
            },
            Err(error) => {
                // Le lien vers Marketplaces n'a de sens que si la liste des marketplaces a pu être lue.
                let link = match (&op, &listing.marketplaces) {
                    (PluginOp::Install { id }, Ok(known)) => unknown_marketplace(id, known),
                    _ => false,
                };
                self.plugins.failure = Some(Failure {
                    row,
                    text: describe_failure(&error),
                    link,
                });
            }
        }
        self.apply_listing(listing, cx);
    }

    fn apply_listing(&mut self, listing: Listing, cx: &mut Context<Self>) {
        let state = &mut self.plugins;
        state.plugins = listing.plugins;
        state.marketplaces = listing.marketplaces;
        state.loaded = true;
        if let Some(open) = &state.menu
            && !state.plugin_list().iter().any(|plugin| &plugin.id == open)
        {
            state.menu = None;
        }
        let plugin_count = match &state.plugins {
            Ok(plugins) => Some(plugin_counter(plugins)),
            Err(_) => None,
        };
        let marketplace_count = match &state.marketplaces {
            Ok(marketplaces) => Some(marketplaces.len().to_string()),
            Err(_) => None,
        };
        self.set_count(Section::Plugins, plugin_count, cx);
        self.set_count(Section::Marketplaces, marketplace_count, cx);
        cx.notify();
    }

    /// Une frappe dans un champ d'ajout efface son message.
    fn clear_field_failure(&mut self, row: Row, cx: &mut Context<Self>) {
        if self.plugins.failure_of(&row).is_some() {
            self.plugins.failure = None;
        }
        cx.notify();
    }

    // ---- Gestes -----------------------------------------------------------------------------------------------------

    /// L'interrupteur d'un plugin : `enable` ou `disable`, seulement pour un identifiant de la dernière liste.
    fn toggle_plugin(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(enable) = toggle_target(&self.plugins.plugin_list(), id) else {
            return;
        };
        let op = PluginOp::Toggle {
            id: id.to_string(),
            enable,
        };
        self.start_plugin_call(Row::Plugin(id.to_string()), op, cx);
    }

    /// « Installer » : l'identifiant mal formé est refusé sous le champ, sans appel.
    pub(super) fn submit_install(&mut self, cx: &mut Context<Self>) {
        if self.plugins.busy.is_some() || self.plugins.claude.is_err() {
            return;
        }
        let id = self.plugins.install.read(cx).value().trim().to_string();
        if id.is_empty() {
            return;
        }
        match check_install_id(&id) {
            Err(message) => {
                self.plugins.failure = Some(Failure {
                    row: Row::NewPlugin,
                    text: message.to_string(),
                    link: false,
                });
                cx.notify();
            }
            Ok(()) => self.start_plugin_call(Row::NewPlugin, PluginOp::Install { id }, cx),
        }
    }

    /// Le menu `more_horiz` d'une ligne : ouvert, ou refermé s'il l'était.
    fn toggle_plugin_menu(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.plugins.busy.is_some() {
            return;
        }
        let open = self.plugins.menu.as_deref() == Some(id);
        self.plugins.menu = (!open).then(|| id.to_string());
        cx.notify();
    }

    /// « Supprimer » du menu : la confirmation, puis `uninstall`.
    fn confirm_plugin_removal(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.plugins.menu = None;
        cx.notify();
        if self.plugins.busy.is_some() {
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Supprimer « {id} » ?"),
            Some("Le plugin est désinstallé de la portée utilisateur de Claude Code."),
            &[
                PromptButton::new("Supprimer"),
                PromptButton::cancel("Annuler"),
            ],
            cx,
        );
        let id = id.to_string();
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, _, cx| {
                    let op = PluginOp::Uninstall { id: id.clone() };
                    this.start_plugin_call(Row::Plugin(id), op, cx);
                });
            }
        })
        .detach();
    }

    // ---- Rendu ------------------------------------------------------------------------------------------------------

    /// Le corps de la section : le bandeau, le champ d'installation et la liste, ou l'état vide quand `claude` manque.
    pub(super) fn render_plugins_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match (self.plugins.claude.clone(), self.plugins.plugins.clone()) {
            (Err(tried), _) => self.render_cli_missing(&tried, theme).into_any_element(),
            (Ok(_), Err(message)) => div()
                .pt(theme.px(8.))
                .text_color(color(ERROR_TEXT))
                .child(SharedString::from(message))
                .into_any_element(),
            (Ok(_), Ok(plugins)) => self
                .render_plugin_list(&plugins, theme, cx)
                .into_any_element(),
        };
        // Le menu d'une dernière ligne déborde sous la liste : la marge basse lui laisse la place de défiler.
        div()
            .id(("settings-body", Section::Plugins as usize))
            .size_full()
            .overflow_y_scroll()
            .px(theme.px(20.))
            .pb(theme.px(60.))
            .child(content)
            .into_any_element()
    }

    fn render_plugin_list(
        &mut self,
        plugins: &[PluginEntry],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let idle = self.plugins.busy.is_none();
        let typed = !self.plugins.install.read(cx).value().trim().is_empty();
        let refused = self.plugins.failure_of(&Row::NewPlugin);
        let field = field_box(
            "settings-plugin-install",
            &self.plugins.install,
            None,
            refused.is_some(),
            theme,
            cx,
        );
        let mut install = action_button(
            "settings-plugin-install-button",
            "Installer",
            None,
            ButtonKind::Primary,
            idle && typed,
            theme,
        );
        if idle && typed {
            install = install.on_click(cx.listener(|this, _, _, cx| this.submit_install(cx)));
        }
        let mut install_row = div().flex().flex_col().mb(theme.px(12.)).child(
            div()
                .flex()
                .items_center()
                .gap(theme.px(8.))
                .child(field)
                .children((self.plugins.busy == Some(Row::NewPlugin)).then(|| busy_label(theme)))
                .child(install),
        );
        if let Some(failure) = refused {
            install_row = install_row.child(error_line(failure.text.clone(), theme));
            if failure.link {
                install_row = install_row.child(
                    div()
                        .id("settings-open-marketplaces")
                        .mt(theme.px(4.))
                        .text_size(theme.text(12.))
                        .text_color(color(FOCUS_TEXT))
                        .cursor_pointer()
                        .hover(|style| style.opacity(0.8))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.select(Section::Marketplaces, cx);
                        }))
                        .child(OPEN_MARKETPLACES),
                );
            }
        }

        let head = div()
            .flex()
            .items_center()
            .h(theme.px(28.))
            .border_b_1()
            .border_color(white(SEPARATOR))
            .text_size(theme.text(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(white(TEXT_MIN))
            .child(div().flex_1().child("Plugin"))
            .child(
                div()
                    .w(theme.px(MARKETPLACE_COLUMN))
                    .flex_none()
                    .child("Marketplace"),
            )
            .child(div().w(theme.px(TOGGLE_COLUMN)).flex_none().child("Actif"))
            .child(div().w(theme.px(MENU_COLUMN)).flex_none());

        let mut list = div().flex().flex_col().child(head);
        for plugin in plugins {
            list = list.child(self.render_plugin_row(plugin, theme, cx));
        }
        let mut body = div()
            .flex()
            .flex_col()
            .child(notice_banner(theme))
            .child(install_row);
        // Un échec sans ligne à porter (le plugin a disparu entre-temps) se lit tout de même.
        if let Some(failure) = self.plugins.failure.as_ref().filter(|failure| {
            matches!(&failure.row, Row::Plugin(id) if !plugins.iter().any(|plugin| &plugin.id == id))
        }) {
            body = body.child(error_line(failure.text.clone(), theme));
        }
        body = body.child(list);
        if !self.plugins.loaded {
            body = body.child(
                div()
                    .py(theme.px(12.))
                    .text_color(white(TEXT_MIN))
                    .child(READING),
            );
        } else if plugins.is_empty() {
            body = body.child(
                div()
                    .py(theme.px(12.))
                    .text_color(white(TEXT_MIN))
                    .child(NO_PLUGIN),
            );
        }
        body
    }

    /// Une ligne de la liste : nom, marketplace en tertiaire, interrupteur, menu `more_horiz`.
    fn render_plugin_row(
        &mut self,
        plugin: &PluginEntry,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let idle = self.plugins.busy.is_none();
        let busy_here = self.plugins.busy == Some(Row::Plugin(plugin.id.clone()));
        let menu_open = self.plugins.menu.as_deref() == Some(plugin.id.as_str());
        let toggle_id = plugin.id.clone();
        let menu_id = plugin.id.clone();
        let remove_id = plugin.id.clone();

        let mut switch = div()
            .id(SharedString::from(format!(
                "settings-plugin-toggle-{}",
                plugin.id
            )))
            .flex()
            .items_center()
            .w(theme.px(TOGGLE_COLUMN))
            .flex_none()
            .child(toggle(plugin.enabled, theme));
        switch = if idle {
            switch
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_plugin(&toggle_id, cx);
                }))
        } else {
            switch.opacity(0.5)
        };

        let mut menu_button = icon_button(
            SharedString::from(format!("settings-plugin-menu-{}", plugin.id)),
            Icon::MoreHoriz,
            idle,
            theme,
        );
        if idle {
            menu_button = menu_button.on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_plugin_menu(&menu_id, cx);
            }));
        }
        if menu_open {
            menu_button = menu_button.child(self.render_row_menu(
                "settings-plugin-menu-remove",
                |this| this.plugins.menu = None,
                move |this, window, cx| this.confirm_plugin_removal(&remove_id, window, cx),
                theme,
                cx,
            ));
        }

        let row = div()
            .flex()
            .items_center()
            .h(theme.px(ROW_HEIGHT))
            .border_b_1()
            .border_color(white(SEPARATOR))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(theme.px(10.))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(white(if plugin.enabled {
                                TEXT_PRIMARY
                            } else {
                                TEXT_MIN
                            }))
                            .child(SharedString::from(plugin.name.clone())),
                    )
                    .children(busy_here.then(|| busy_label(theme))),
            )
            .child(
                div()
                    .w(theme.px(MARKETPLACE_COLUMN))
                    .flex_none()
                    .truncate()
                    .text_size(theme.text(11.5))
                    .text_color(white(TEXT_MIN))
                    .child(SharedString::from(plugin.marketplace.clone())),
            )
            .child(switch)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .w(theme.px(MENU_COLUMN))
                    .flex_none()
                    .child(menu_button),
            );
        let failure = self.plugins.failure_of(&Row::Plugin(plugin.id.clone()));
        div().flex().flex_col().child(row).children(
            failure.map(|failure| error_line(failure.text.clone(), theme).mb(theme.px(6.))),
        )
    }

    /// Le menu d'une ligne en couche flottante (la mécanique du menu de la fenêtre principale, `popover.rs`) : un fond
    /// transparent de la taille de la fenêtre avale tout appui hors du menu et le ferme, le panneau est ancré sous le
    /// bouton. Une seule entrée : « Supprimer ». Partagé avec la section Skills : `close` referme le menu de la section,
    /// `remove` est le geste de l'entrée.
    pub(super) fn render_row_menu(
        &mut self,
        item_id: &'static str,
        close: fn(&mut Self),
        remove: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = *theme;
        let backdrop = deferred(
            anchored()
                .position(point(px(0.), px(0.)))
                .position_mode(AnchoredPositionMode::Window)
                .child(
                    div()
                        .w(px(10_000.))
                        .h(px(10_000.))
                        .occlude()
                        .on_any_mouse_down(cx.listener(move |this, _, _, cx| {
                            close(this);
                            cx.notify();
                        })),
                ),
        )
        .with_priority(1);
        let item = div()
            .id(item_id)
            .flex()
            .items_center()
            .gap(theme.px(8.))
            .h(theme.px(28.))
            .px(theme.px(9.))
            .rounded(theme.px(5.))
            .text_color(color(ERROR_TEXT))
            .cursor_pointer()
            .hover(|style| style.bg(white(HOVER)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, cx| {
                remove(this, window, cx);
            }))
            .child(
                svg()
                    .path(Icon::Delete.path())
                    .flex_none()
                    .size(theme.px(16.))
                    .text_color(color(ERROR_TEXT)),
            )
            .child("Supprimer");
        let panel = div()
            .occlude()
            .flex()
            .flex_col()
            .min_w(theme.px(MENU_WIDTH))
            .p(theme.px(5.) - px(1.))
            .rounded(theme.px(9.))
            .bg(color_alpha(POPOVER))
            .border_1()
            .border_color(white(CONTROL))
            .shadow(vec![
                BoxShadow {
                    color: gpui::black().opacity(0.7),
                    offset: point(px(0.), px(0.)),
                    blur_radius: px(0.),
                    spread_radius: px(0.5),
                    inset: false,
                },
                BoxShadow {
                    color: gpui::black().opacity(0.5),
                    offset: point(px(0.), theme.px(14.)),
                    blur_radius: theme.px(36.),
                    spread_radius: px(0.),
                    inset: false,
                },
            ])
            .text_size(theme.text(13.))
            .child(item);
        // Le coin haut-droit du panneau se pose au coin droit du bouton, 28 plus bas.
        let anchor = anchored()
            .anchor(Anchor::TopRight)
            .offset(point(theme.px(24.), theme.px(28.)))
            .child(panel);
        div()
            .absolute()
            .size_0()
            .child(backdrop)
            .child(deferred(anchor).with_priority(2))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marketplace(name: &str) -> MarketplaceEntry {
        MarketplaceEntry {
            name: name.to_string(),
            source: "github".to_string(),
            location: "eRom/erom-marketplace".to_string(),
            removable: true,
        }
    }

    fn plugin(id: &str, enabled: bool) -> PluginEntry {
        let (name, marketplace) = id.split_once('@').unwrap();
        PluginEntry {
            id: id.to_string(),
            name: name.to_string(),
            marketplace: marketplace.to_string(),
            enabled,
        }
    }

    #[test]
    fn unknown_marketplace_is_detected_from_the_id() {
        let known = [marketplace("erom-marketplace")];
        assert!(unknown_marketplace("x@absent", &known));
        assert!(!unknown_marketplace("x@erom-marketplace", &known));
    }

    #[test]
    fn plugin_counter_reads_enabled_over_total() {
        let plugins = [
            plugin("a@m", true),
            plugin("b@m", true),
            plugin("c@m", false),
        ];
        assert_eq!(plugin_counter(&plugins), "2/3");
        assert_eq!(plugin_counter(&[]), "0/0");
    }

    #[test]
    fn toggle_only_acts_on_listed_plugins() {
        let plugins = [plugin("a@m", true), plugin("b@m", false)];
        // Le geste inverse l'état lu ; un identifiant absent de la liste ne part jamais au CLI (`enable` écrirait
        // l'identifiant dans settings.json même s'il n'existe pas).
        assert_eq!(toggle_target(&plugins, "a@m"), Some(false));
        assert_eq!(toggle_target(&plugins, "b@m"), Some(true));
        assert_eq!(toggle_target(&plugins, "ghost@m"), None);
    }

    #[test]
    fn enable_is_refused_when_the_fresh_listing_dropped_the_plugin() {
        let fresh = vec![plugin("a@m", false)];
        assert_eq!(check_enable_allowed(&fresh, "a@m"), Ok(()));
        let refusal = check_enable_allowed(&fresh, "foo@m").unwrap_err();
        assert!(refusal.contains("foo@m"), "{refusal}");
        assert!(check_enable_allowed(&[], "a@m").is_err());
    }

    #[test]
    fn enable_reads_the_listing_first_and_stops_on_an_unlisted_plugin() {
        let listing = r#"[{"id":"a@m","scope":"user","enabled":false}]"#;
        let calls = std::cell::RefCell::new(Vec::<Vec<String>>::new());
        let mut run = |args: &[String]| {
            calls.borrow_mut().push(args.to_vec());
            Ok(listing.to_string())
        };
        assert!(guard_enable(&mut run, "a@m").is_ok());
        assert!(guard_enable(&mut run, "ghost@m").is_err());
        // Seule la lecture est partie, jamais une écriture.
        assert!(
            calls
                .borrow()
                .iter()
                .all(|args| *args == plugin_list_args())
        );

        let mut broken = |_: &[String]| Ok("not json".to_string());
        let error = guard_enable(&mut broken, "a@m").unwrap_err();
        assert!(error.stderr.contains("illisible"), "{}", error.stderr);
        let mut failing = |_: &[String]| {
            Err(CliError {
                code: Some(1),
                stderr: "boom".to_string(),
            })
        };
        assert_eq!(
            guard_enable(&mut failing, "a@m").unwrap_err().stderr,
            "boom"
        );
    }

    #[test]
    fn plugin_ids_are_checked_before_any_call() {
        assert_eq!(check_install_id("x@erom-marketplace"), Ok(()));
        assert!(check_install_id("sans-arobase").is_err());
        assert!(check_install_id("x y@m").is_err());
        // Un identifiant qui commence par un tiret passerait pour une option du CLI.
        assert!(check_install_id("--help@m").is_err());
    }

    #[test]
    fn every_write_carries_the_user_scope_and_a_read_carries_nothing() {
        assert_eq!(PluginOp::Read.args(), None);
        let writes = [
            PluginOp::Toggle {
                id: "a@m".to_string(),
                enable: true,
            },
            PluginOp::Install {
                id: "a@m".to_string(),
            },
            PluginOp::Uninstall {
                id: "a@m".to_string(),
            },
            PluginOp::MarketplaceAdd {
                source: "o/r".to_string(),
            },
            PluginOp::MarketplaceRemove {
                name: "m".to_string(),
            },
        ];
        for op in writes {
            let args = op.args().expect("a write has arguments");
            let scoped = args
                .windows(2)
                .any(|pair| matches!(pair[0].as_str(), "-s" | "--scope") && pair[1] == "user");
            assert!(scoped, "{op:?} without user scope: {args:?}");
        }
        // `update` ne prend pas de portée ; sans nom, il rafraîchit tous les marketplaces.
        assert_eq!(
            PluginOp::MarketplaceUpdate { name: None }.args(),
            Some(vec![
                "plugin".to_string(),
                "marketplace".to_string(),
                "update".to_string()
            ])
        );
    }

    #[test]
    fn listing_keeps_each_failure_apart() {
        let mut run = |args: &[String]| {
            if args.iter().any(|arg| arg == "marketplace") {
                Err(CliError {
                    code: Some(1),
                    stderr: "boom\n".to_string(),
                })
            } else {
                Ok(r#"[{"id":"a@m","scope":"user","enabled":true}]"#.to_string())
            }
        };
        let listing = read_listing(&mut run, Path::new("/nonexistent/settings.json"));
        assert_eq!(listing.plugins.unwrap().len(), 1);
        assert_eq!(listing.marketplaces.unwrap_err(), "boom");
    }
}
