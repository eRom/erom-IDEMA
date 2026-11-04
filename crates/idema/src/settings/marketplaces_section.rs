//! La section « Marketplaces » de « Personnaliser » : les marketplaces que connaît `claude plugin marketplace list --json`,
//! l'ajout d'une source, « Rafraîchir » par ligne et pour tous, et la suppression des seuls marketplaces que l'utilisateur
//! a déclarés (portée `user`). L'état et la mécanique d'appel sont ceux de la section Plugins (`plugins_section.rs`).

use gpui::{
    AnyElement, Context, FontWeight, MouseButton, PromptButton, PromptLevel, SharedString, Window,
    div, prelude::*,
};

use super::claude_cli::{CliArgument, MarketplaceEntry, PluginEntry, reject_leading_dash};
use super::controls::{ButtonKind, action_button, error_line, field_box, icon_button};
use super::mcp_section::busy_label;
use super::nav::Section;
use super::plugins_section::{Failure, PluginOp, Row};
use super::view::SettingsView;
use crate::assets::Icon;
use crate::theme::{ERROR_TEXT, SEPARATOR, TEXT_MIN, TEXT_PRIMARY, Theme, color, white};

const NO_MARKETPLACE: &str = "Aucun marketplace.";
const READING: &str = "Lecture de la liste...";
const SOURCE_PLACEHOLDER_ADD: &str = "Ajouter";
/// Le marketplace intégré à Claude Code, que l'utilisateur ne déclare pas.
const BUILTIN: &str = "claude-plugins-official";
const ROW_HEIGHT: f32 = 40.;
const NAME_COLUMN: f32 = 190.;
const SOURCE_COLUMN: f32 = 80.;
/// La colonne des actions de ligne : « Supprimer » (ou la mention) et le bouton « Rafraîchir ».
const ACTIONS_COLUMN: f32 = 150.;

/// Le nombre de plugins installés depuis le marketplace `name`.
pub fn plugins_from(name: &str, plugins: &[PluginEntry]) -> usize {
    plugins
        .iter()
        .filter(|plugin| plugin.marketplace == name)
        .count()
}

/// Ce que la confirmation de suppression annonce : `claude plugin marketplace remove` désinstalle aussi les plugins
/// installés depuis ce marketplace (constaté avec le CLI 2.1.310 : « Also uninstalled 2 plugins from this marketplace »).
pub fn removal_detail(installed: usize) -> String {
    match installed {
        0 => "Aucun plugin n'est installé depuis ce marketplace.".to_string(),
        1 => "1 plugin installé depuis ce marketplace sera désinstallé.".to_string(),
        n => format!("{n} plugins installés depuis ce marketplace seront désinstallés."),
    }
}

/// La source saisie : non vide, et pas prise pour une option du CLI. Rend la source rognée.
pub fn check_source(source: &str) -> Result<(), &'static str> {
    let source = source.trim();
    if source.is_empty() {
        Err("Une source est requise.")
    } else {
        reject_leading_dash(source, CliArgument::Source)
    }
}

/// La mention d'un marketplace que la fenêtre ne retire pas : « Intégré » pour celui de Claude Code, sinon il est déclaré
/// ailleurs que dans la portée utilisateur.
pub fn not_removable_note(name: &str) -> &'static str {
    if name == BUILTIN {
        "Intégré"
    } else {
        "Déclaré ailleurs"
    }
}

impl SettingsView {
    // ---- Gestes -----------------------------------------------------------------------------------------------------

    /// « Ajouter » : la source refusée se lit sous le champ, sans appel.
    pub(super) fn submit_source(&mut self, cx: &mut Context<Self>) {
        if self.plugins.busy.is_some() || self.plugins.claude.is_err() {
            return;
        }
        let raw = self.plugins.source.read(cx).value().to_string();
        if raw.trim().is_empty() {
            return;
        }
        match check_source(&raw) {
            Err(message) => {
                self.plugins.failure = Some(Failure {
                    row: Row::NewMarketplace,
                    text: message.to_string(),
                    link: false,
                });
                cx.notify();
            }
            Ok(()) => {
                let source = raw.trim().to_string();
                self.start_plugin_call(
                    Row::NewMarketplace,
                    PluginOp::MarketplaceAdd { source },
                    cx,
                );
            }
        }
    }

    /// « Rafraîchir » d'une ligne, seulement pour un nom de la dernière liste.
    fn refresh_marketplace(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self
            .plugins
            .marketplace_list()
            .iter()
            .any(|entry| entry.name == name)
        {
            return;
        }
        let op = PluginOp::MarketplaceUpdate {
            name: Some(name.to_string()),
        };
        self.start_plugin_call(Row::Marketplace(name.to_string()), op, cx);
    }

    /// « Tout rafraîchir » de l'en-tête.
    fn refresh_all_marketplaces(&mut self, cx: &mut Context<Self>) {
        let op = PluginOp::MarketplaceUpdate { name: None };
        self.start_plugin_call(Row::AllMarketplaces, op, cx);
    }

    /// « Supprimer » : la confirmation annonce le nombre de plugins désinstallés avec le marketplace, puis `remove`. Un
    /// marketplace que l'utilisateur n'a pas déclaré ne part jamais au CLI.
    fn confirm_marketplace_removal(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.plugins.busy.is_some() {
            return;
        }
        let removable = self
            .plugins
            .marketplace_list()
            .iter()
            .any(|entry| entry.name == name && entry.removable);
        if !removable {
            return;
        }
        let installed = plugins_from(name, &self.plugins.plugin_list());
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Supprimer « {name} » ?"),
            Some(&removal_detail(installed)),
            &[
                PromptButton::new("Supprimer"),
                PromptButton::cancel("Annuler"),
            ],
            cx,
        );
        let name = name.to_string();
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, _, cx| {
                    let op = PluginOp::MarketplaceRemove { name: name.clone() };
                    this.start_plugin_call(Row::Marketplace(name), op, cx);
                });
            }
        })
        .detach();
    }

    // ---- Rendu ------------------------------------------------------------------------------------------------------

    /// « Tout rafraîchir » de l'en-tête ; sans `claude`, il n'y a rien à rafraîchir.
    pub(super) fn marketplaces_actions(
        &mut self,
        section: Section,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if section != Section::Marketplaces || self.plugins.claude.is_err() {
            return None;
        }
        let theme = *cx.global::<Theme>();
        let enabled = self.plugins.busy.is_none();
        let mut all = action_button(
            "settings-marketplaces-refresh-all",
            "Tout rafraîchir",
            Some(Icon::Refresh),
            ButtonKind::Ghost,
            enabled,
            &theme,
        )
        // L'en-tête déplace la fenêtre sur un appui : le bouton garde le sien.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        if enabled {
            all = all.on_click(cx.listener(|this, _, _, cx| this.refresh_all_marketplaces(cx)));
        }
        Some(all.into_any_element())
    }

    /// Le corps de la section : le champ d'ajout et la liste, ou l'état vide quand `claude` manque.
    pub(super) fn render_marketplaces_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match (
            self.plugins.claude.clone(),
            self.plugins.marketplaces.clone(),
        ) {
            (Err(tried), _) => self.render_cli_missing(&tried, theme).into_any_element(),
            (Ok(_), Err(message)) => div()
                .pt(theme.px(8.))
                .text_color(color(ERROR_TEXT))
                .child(SharedString::from(message))
                .into_any_element(),
            (Ok(_), Ok(marketplaces)) => self
                .render_marketplace_list(&marketplaces, theme, cx)
                .into_any_element(),
        };
        div()
            .id(("settings-body", Section::Marketplaces as usize))
            .size_full()
            .overflow_y_scroll()
            .px(theme.px(20.))
            .pb(theme.px(20.))
            .child(content)
            .into_any_element()
    }

    fn render_marketplace_list(
        &mut self,
        marketplaces: &[MarketplaceEntry],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let idle = self.plugins.busy.is_none();
        let typed = !self.plugins.source.read(cx).value().trim().is_empty();
        let refused = self.plugins.failure_of(&Row::NewMarketplace);
        let field = field_box(
            "settings-marketplace-source",
            &self.plugins.source,
            None,
            refused.is_some(),
            theme,
            cx,
        );
        let mut add = action_button(
            "settings-marketplace-add",
            SOURCE_PLACEHOLDER_ADD,
            None,
            ButtonKind::Primary,
            idle && typed,
            theme,
        );
        if idle && typed {
            add = add.on_click(cx.listener(|this, _, _, cx| this.submit_source(cx)));
        }
        let mut add_row = div().flex().flex_col().mb(theme.px(12.)).child(
            div()
                .flex()
                .items_center()
                .gap(theme.px(8.))
                .child(field)
                .children(
                    matches!(
                        self.plugins.busy,
                        Some(Row::NewMarketplace | Row::AllMarketplaces)
                    )
                    .then(|| busy_label(theme)),
                )
                .child(add),
        );
        if let Some(failure) = refused {
            add_row = add_row.child(error_line(failure.text.clone(), theme));
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
            .child(
                div()
                    .w(theme.px(NAME_COLUMN))
                    .flex_none()
                    .child("Marketplace"),
            )
            .child(div().w(theme.px(SOURCE_COLUMN)).flex_none().child("Source"))
            .child(div().flex_1().child("Emplacement"))
            .child(div().w(theme.px(ACTIONS_COLUMN)).flex_none());

        let mut list = div().flex().flex_col().child(head);
        for entry in marketplaces {
            list = list.child(self.render_marketplace_row(entry, theme, cx));
        }
        let mut body = div().flex().flex_col().child(add_row);
        // Un échec sans ligne à porter (« Tout rafraîchir », ou le marketplace a disparu) se lit tout de même.
        if let Some(failure) = self
            .plugins
            .failure
            .as_ref()
            .filter(|failure| match &failure.row {
                Row::AllMarketplaces => true,
                Row::Marketplace(name) => !marketplaces.iter().any(|entry| &entry.name == name),
                _ => false,
            })
        {
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
        } else if marketplaces.is_empty() {
            body = body.child(
                div()
                    .py(theme.px(12.))
                    .text_color(white(TEXT_MIN))
                    .child(NO_MARKETPLACE),
            );
        }
        body
    }

    /// Une ligne : nom, source, emplacement ; « Supprimer » si l'utilisateur l'a déclaré, sinon la mention ; « Rafraîchir ».
    fn render_marketplace_row(
        &mut self,
        entry: &MarketplaceEntry,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let idle = self.plugins.busy.is_none();
        let target = Row::Marketplace(entry.name.clone());
        let busy_here = self.plugins.busy.as_ref() == Some(&target);
        let remove_name = entry.name.clone();
        let refresh_name = entry.name.clone();

        let mut refresh = icon_button(
            SharedString::from(format!("settings-marketplace-refresh-{}", entry.name)),
            Icon::Refresh,
            idle,
            theme,
        );
        if idle {
            refresh = refresh.on_click(cx.listener(move |this, _, _, cx| {
                this.refresh_marketplace(&refresh_name, cx);
            }));
        }
        let removal: AnyElement = if entry.removable {
            let mut remove = action_button(
                SharedString::from(format!("settings-marketplace-remove-{}", entry.name)),
                "Supprimer",
                None,
                ButtonKind::Danger,
                idle,
                theme,
            );
            if idle {
                remove = remove.on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_marketplace_removal(&remove_name, window, cx);
                }));
            }
            remove.into_any_element()
        } else {
            div()
                .text_size(theme.text(11.5))
                .text_color(white(TEXT_MIN))
                .child(not_removable_note(&entry.name))
                .into_any_element()
        };

        let row = div()
            .flex()
            .items_center()
            .h(theme.px(ROW_HEIGHT))
            .border_b_1()
            .border_color(white(SEPARATOR))
            .child(
                div()
                    .w(theme.px(NAME_COLUMN))
                    .flex_none()
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(white(TEXT_PRIMARY))
                    .child(SharedString::from(entry.name.clone())),
            )
            .child(
                div()
                    .w(theme.px(SOURCE_COLUMN))
                    .flex_none()
                    .truncate()
                    .text_size(theme.text(11.5))
                    .text_color(white(TEXT_MIN))
                    .child(SharedString::from(entry.source.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(
                        cx.global::<gpui_component::Theme>()
                            .mono_font_family
                            .clone(),
                    )
                    .text_size(theme.text(11.))
                    .text_color(white(TEXT_MIN))
                    .child(SharedString::from(entry.location.clone())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(theme.px(6.))
                    .w(theme.px(ACTIONS_COLUMN))
                    .flex_none()
                    .children(busy_here.then(|| busy_label(theme)))
                    .child(removal)
                    .child(refresh),
            );
        let failure = self.plugins.failure_of(&target);
        div().flex().flex_col().child(row).children(
            failure.map(|failure| error_line(failure.text.clone(), theme).mb(theme.px(6.))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(id: &str) -> PluginEntry {
        let (name, marketplace) = id.split_once('@').unwrap();
        PluginEntry {
            id: id.to_string(),
            name: name.to_string(),
            marketplace: marketplace.to_string(),
            enabled: true,
        }
    }

    #[test]
    fn installed_count_for_a_marketplace() {
        let plugins = [plugin("a@one"), plugin("b@one"), plugin("c@two")];
        assert_eq!(plugins_from("one", &plugins), 2);
        assert_eq!(plugins_from("two", &plugins), 1);
        assert_eq!(plugins_from("none", &plugins), 0);
    }

    #[test]
    fn removal_text_announces_the_uninstalled_plugins() {
        assert_eq!(
            removal_detail(0),
            "Aucun plugin n'est installé depuis ce marketplace."
        );
        assert_eq!(
            removal_detail(1),
            "1 plugin installé depuis ce marketplace sera désinstallé."
        );
        assert_eq!(
            removal_detail(3),
            "3 plugins installés depuis ce marketplace seront désinstallés."
        );
    }

    #[test]
    fn a_marketplace_source_is_checked_before_any_call() {
        assert_eq!(check_source("eRom/erom-marketplace"), Ok(()));
        assert_eq!(check_source("  https://example.com/x.git  "), Ok(()));
        assert!(check_source("   ").is_err());
        // Une source qui commence par un tiret passerait pour une option du CLI.
        assert!(check_source("--help").is_err());
    }

    #[test]
    fn builtin_and_foreign_marketplaces_say_why_they_cannot_be_removed() {
        assert_eq!(not_removable_note("claude-plugins-official"), "Intégré");
        assert_eq!(not_removable_note("other"), "Déclaré ailleurs");
    }
}
