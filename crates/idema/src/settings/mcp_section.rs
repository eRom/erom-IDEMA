//! La section « Serveurs MCP » de « Personnaliser » : la liste des serveurs de portée utilisateur lus dans
//! `~/.claude.json` (lecture seule), un éditeur JSON par serveur, et l'ajout, la modification et la suppression par le CLI
//! `claude` (portée `user`). Un seul appel CLI à la fois dans la section, hors du fil d'interface ; après chaque appel,
//! le fichier est relu pour que la liste montre l'état réel.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use gpui::{
    AnyElement, Context, Entity, FontWeight, MouseButton, PromptButton, PromptLevel, SharedString,
    Subscription, Task, Window, div, prelude::*, svg,
};
use gpui_component::input::{EditorState, InputEvent, InputState};
use serde_json::Value;

use super::claude_cli::{
    CliArgument, CliError, describe_failure, edit_mcp_server, mcp_add_args, mcp_remove_args,
    mcp_summary, read_mcp_servers, reject_leading_dash, resolve_claude, run_claude,
};
use super::controls::{ButtonKind, action_button, error_line, field_box};
use super::nav::Section;
use super::view::SettingsView;
use crate::assets::Icon;
use crate::files::panel::home_dir;
use crate::mcp_widgets::{
    BADGE_COLUMN, CHEVRON_COLUMN, ROW_HEIGHT, busy_text, code_box, json_editor, type_badge,
};
use crate::theme::{
    ERROR_TEXT, ICON_SECONDARY, NOTICE, OPEN_ROW, SEPARATOR, TEXT_MIN, TEXT_PRIMARY, Theme, color,
    white,
};

const BANNER: &str = "Les changements s'appliquent aux prochaines sessions Claude.";
const NO_CLI: &str = "CLI Claude introuvable";
const NO_CLI_HELP: &str = "Chemins essayés :";
/// Le résumé d'une ligne que le fichier ne porte plus.
const ABSENT: &str = "Absent du fichier";
const NO_SERVER: &str = "Aucun serveur MCP.";
const HTTP_NOTE: &str = "Enregistrer peut demander de se reconnecter à ce serveur.";
const BUSY: &str = "En cours...";
const NAME_PLACEHOLDER: &str = "Nom du serveur";
const EMPTY_NAME: &str = "Un nom est requis.";
const TAKEN_NAME: &str = "Ce nom est déjà pris.";

/// Le modèle d'un serveur neuf (spec §3.3), tel qu'il remplit l'éditeur de la ligne « Ajouter ».
pub const NEW_SERVER_TEMPLATE: &str =
    "{\n  \"type\": \"stdio\",\n  \"command\": \"\",\n  \"args\": [],\n  \"env\": {}\n}";

/// L'ordre des clés d'un bloc affiché : celui de la maquette (`type` d'abord), puis les autres par ordre alphabétique.
const KEY_ORDER: [&str; 6] = ["type", "command", "args", "env", "url", "headers"];

/// Ce que vaut le texte de l'éditeur face au bloc lu dans `~/.claude.json`.
#[derive(Debug, Clone, PartialEq)]
pub enum SaveState {
    /// Le texte n'est pas l'objet JSON d'un serveur : « Enregistrer » reste inactif, le message dit pourquoi.
    Invalid(String),
    /// Le texte analysé vaut le bloc lu : rien à écrire, pour ne jamais retirer un serveur pour rien.
    Unchanged,
    /// Le bloc à écrire.
    Ready(Value),
}

/// Compare `draft` à `original` après analyse : espaces et ordre des clés ne comptent pas. Sans `original` (un serveur
/// neuf), tout objet valide est prêt.
pub fn save_enabled(original: Option<&Value>, draft: &str) -> SaveState {
    let parsed: Value = match serde_json::from_str(draft) {
        Ok(parsed) => parsed,
        Err(error) => {
            return SaveState::Invalid(format!(
                "JSON invalide (ligne {}, colonne {}).",
                error.line(),
                error.column()
            ));
        }
    };
    if !parsed.is_object() {
        return SaveState::Invalid("Le bloc d'un serveur est un objet JSON.".to_string());
    }
    if original == Some(&parsed) {
        SaveState::Unchanged
    } else {
        SaveState::Ready(parsed)
    }
}

/// Le nom d'un serveur neuf : non vide, et libre.
pub fn validate_new_name(
    name: &str,
    existing: &BTreeMap<String, Value>,
) -> Result<(), &'static str> {
    let name = name.trim();
    if name.is_empty() {
        Err(EMPTY_NAME)
    } else if existing.contains_key(name) {
        Err(TAKEN_NAME)
    } else {
        reject_leading_dash(name, CliArgument::Name)
    }
}

/// Vrai pour un serveur distant (`http` ou `sse`, ou qui porte une `url`) : badge HTTP et mise en garde de reconnexion.
pub fn is_remote(server: &Value) -> bool {
    server["url"].is_string() || matches!(server["type"].as_str(), Some("http" | "sse"))
}

/// Le bloc d'un serveur, mis en forme pour l'éditeur : les clés de la maquette d'abord, les valeurs imbriquées sur
/// plusieurs lignes.
pub fn pretty_server(server: &Value) -> String {
    let Some(map) = server.as_object().filter(|map| !map.is_empty()) else {
        return serde_json::to_string_pretty(server).unwrap_or_default();
    };
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort_by_key(|key| {
        let rank = KEY_ORDER
            .iter()
            .position(|known| *known == key.as_str())
            .unwrap_or(KEY_ORDER.len());
        (rank, key.as_str())
    });
    let fields: Vec<String> = keys
        .into_iter()
        .map(|key| {
            let value = serde_json::to_string_pretty(&map[key.as_str()])
                .unwrap_or_default()
                .replace('\n', "\n  ");
            format!("  {}: {value}", Value::String(key.clone()))
        })
        .collect();
    format!("{{\n{}\n}}", fields.join(",\n"))
}

/// Une écriture que la section demande au CLI.
#[derive(Debug, Clone)]
pub enum McpOp {
    Add {
        name: String,
        server: Value,
    },
    Edit {
        name: String,
        old: Value,
        new: Value,
    },
    Remove {
        name: String,
    },
}

/// Joue `op` avec `run` : un `add-json`, un `remove`, ou la séquence d'édition (avec restauration de l'ancien bloc).
pub fn apply_op(
    run: &mut dyn FnMut(&[String]) -> Result<String, CliError>,
    op: &McpOp,
) -> Result<(), CliError> {
    match op {
        McpOp::Add { name, server } => run(&mcp_add_args(name, &server.to_string())).map(|_| ()),
        McpOp::Remove { name } => run(&mcp_remove_args(name)).map(|_| ()),
        McpOp::Edit { name, old, new } => edit_mcp_server(run, name, old, new),
    }
}

/// Vrai si la ligne dépliée de `name` survit à une relecture de `~/.claude.json` : le serveur y figure encore, ou la
/// ligne porte l'erreur d'un appel de la section (un serveur perdu par un échec ne doit pas emporter sa saisie ni son
/// message). Seule une ligne intacte dont le serveur a été retiré ailleurs disparaît.
pub fn keeps_row(listed: bool, has_error: bool) -> bool {
    listed || has_error
}

/// Qui un appel concerne : un serveur de la liste, ou la ligne « Ajouter ».
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Server(String),
    New,
}

/// Une ligne dépliée : son éditeur JSON et l'erreur du dernier appel.
struct OpenRow {
    editor: Entity<EditorState>,
    error: Option<String>,
    _subscription: Subscription,
}

/// La ligne « Ajouter » pas encore écrite : le nom, le JSON et l'erreur du dernier appel.
struct NewServer {
    name: Entity<InputState>,
    editor: Entity<EditorState>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// L'état de la section, tenu par `SettingsView`.
pub(super) struct McpState {
    claude_json: Option<PathBuf>,
    home: PathBuf,
    /// Le binaire `claude`, ou les chemins essayés.
    claude: Result<PathBuf, Vec<PathBuf>>,
    /// La dernière lecture de `~/.claude.json`.
    servers: Result<BTreeMap<String, Value>, String>,
    open: HashMap<String, OpenRow>,
    draft: Option<NewServer>,
    /// L'appel CLI en cours : une seule à la fois dans la section.
    busy: Option<Target>,
    _call: Option<Task<()>>,
}

impl McpState {
    pub(super) fn new() -> Self {
        let home = home_dir();
        Self {
            claude_json: home.as_ref().map(|home| home.join(".claude.json")),
            home: home.unwrap_or_default(),
            claude: Err(Vec::new()),
            servers: Ok(BTreeMap::new()),
            open: HashMap::new(),
            draft: None,
            busy: None,
            _call: None,
        }
    }

    fn set_error(&mut self, target: &Target, text: Option<String>) {
        match target {
            Target::Server(name) => {
                if let Some(row) = self.open.get_mut(name) {
                    row.error = text;
                }
            }
            Target::New => {
                if let Some(draft) = self.draft.as_mut() {
                    draft.error = text;
                }
            }
        }
    }

    fn known(&self) -> BTreeMap<String, Value> {
        self.servers.clone().unwrap_or_default()
    }
}

impl SettingsView {
    // ---- Lecture ----------------------------------------------------------------------------------------------------

    /// La première lecture : le compteur de la barre latérale.
    pub(super) fn init_mcp(&mut self, cx: &mut Context<Self>) {
        self.refresh_mcp(cx);
    }

    /// Relit `~/.claude.json` et cherche `claude` de nouveau : à l'ouverture de la section et après chaque appel.
    pub(super) fn refresh_mcp(&mut self, cx: &mut Context<Self>) {
        // Pendant un appel, le fichier est à moitié édité (un serveur manque entre `remove` et `add-json`) : la relecture
        // de fin d'appel (`finish_mcp_call`) montre l'état réel.
        if self.mcp.busy.is_some() {
            return;
        }
        let state = &mut self.mcp;
        state.claude = resolve_claude(&state.home, std::env::var_os("IDEMA_CLAUDE_BIN").as_deref());
        state.servers = match &state.claude_json {
            Some(path) => read_mcp_servers(path),
            None => Err("HOME introuvable.".to_string()),
        };
        if let Ok(servers) = &state.servers {
            // Un serveur retiré ailleurs emporte sa ligne dépliée, sauf si elle porte l'erreur d'un appel de la section.
            state
                .open
                .retain(|name, row| keeps_row(servers.contains_key(name), row.error.is_some()));
        }
        let count = match (&state.claude, &state.servers) {
            (Ok(_), Ok(servers)) => Some(servers.len().to_string()),
            _ => None,
        };
        self.set_count(Section::McpServers, count, cx);
        cx.notify();
    }

    // ---- Gestes -----------------------------------------------------------------------------------------------------

    /// Un clic sur une ligne la déplie, ou la replie (la saisie non enregistrée est perdue). Celle dont l'appel court
    /// reste en place.
    fn toggle_mcp_row(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.mcp.busy == Some(Target::Server(name.to_string())) {
            return;
        }
        if self.mcp.open.remove(name).is_some() {
            cx.notify();
            return;
        }
        let Some(server) = self.mcp.known().get(name).cloned() else {
            return;
        };
        let editor = json_editor(&pretty_server(&server), window, cx);
        let subscription = cx.subscribe_in(&editor, window, |_, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        self.mcp.open.insert(
            name.to_string(),
            OpenRow {
                editor,
                error: None,
                _subscription: subscription,
            },
        );
        cx.notify();
    }

    /// « Ajouter » : une ligne neuve dépliée en tête de liste, le focus dans le champ du nom.
    pub(super) fn add_mcp_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mcp.draft.is_none() {
            let name = cx.new(|cx| InputState::new(window, cx).placeholder(NAME_PLACEHOLDER));
            let editor = json_editor(NEW_SERVER_TEMPLATE, window, cx);
            let subscriptions = vec![
                cx.subscribe_in(&name, window, |_, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }),
                cx.subscribe_in(&editor, window, |_, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }),
            ];
            self.mcp.draft = Some(NewServer {
                name,
                editor,
                error: None,
                _subscriptions: subscriptions,
            });
        }
        if let Some(draft) = &self.mcp.draft {
            draft.name.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    /// « Annuler » : la ligne se replie, sa saisie part.
    fn cancel_mcp(&mut self, target: &Target, cx: &mut Context<Self>) {
        if self.mcp.busy.is_some() {
            return;
        }
        match target {
            Target::Server(name) => {
                self.mcp.open.remove(name);
            }
            Target::New => self.mcp.draft = None,
        }
        cx.notify();
    }

    /// « Enregistrer » d'un serveur de la liste : `remove` puis `add-json` du bloc édité.
    fn save_mcp_row(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let old = self.mcp.known().get(name).cloned();
        let Some(row) = self.mcp.open.get(name) else {
            return;
        };
        let draft = row.editor.read(cx).value().to_string();
        if let SaveState::Ready(new) = save_enabled(old.as_ref(), &draft) {
            // Un serveur absent du fichier (perdu par un échec) se remet par un simple `add-json`.
            let op = match old {
                Some(old) => McpOp::Edit {
                    name: name.to_string(),
                    old,
                    new,
                },
                None => McpOp::Add {
                    name: name.to_string(),
                    server: new,
                },
            };
            self.start_mcp_call(Target::Server(name.to_string()), op, window, cx);
        }
    }

    /// « Enregistrer » de la ligne neuve : `add-json`.
    fn save_mcp_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.mcp.draft else {
            return;
        };
        let name = draft.name.read(cx).value().trim().to_string();
        if validate_new_name(&name, &self.mcp.known()).is_err() {
            return;
        }
        let text = draft.editor.read(cx).value().to_string();
        if let SaveState::Ready(server) = save_enabled(None, &text) {
            self.start_mcp_call(Target::New, McpOp::Add { name, server }, window, cx);
        }
    }

    /// « Supprimer » : la confirmation, puis `remove`.
    fn confirm_mcp_removal(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.mcp.busy.is_some() {
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Supprimer « {name} » ?"),
            Some("Le serveur est retiré de la portée utilisateur de Claude Code (~/.claude.json)."),
            &[
                PromptButton::new("Supprimer"),
                PromptButton::cancel("Annuler"),
            ],
            cx,
        );
        let name = name.to_string();
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    let op = McpOp::Remove { name: name.clone() };
                    this.start_mcp_call(Target::Server(name), op, window, cx);
                });
            }
        })
        .detach();
    }

    /// Lance `op` hors du fil d'interface. Un seul appel à la fois : un second geste pendant le premier ne fait rien.
    fn start_mcp_call(
        &mut self,
        target: Target,
        op: McpOp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mcp.busy.is_some() {
            return;
        }
        let Ok(bin) = self.mcp.claude.clone() else {
            return;
        };
        self.mcp.busy = Some(target.clone());
        self.mcp.set_error(&target, None);
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { apply_op(&mut |args| run_claude(&bin, args), &op) })
                .await;
            let _ = this.update_in(cx, |this, _, cx| this.finish_mcp_call(target, result, cx));
        });
        self.mcp._call = Some(task);
        cx.notify();
    }

    /// La fin d'un appel : le fichier est relu dans tous les cas ; en cas de succès la ligne se replie, sinon elle reste
    /// ouverte avec le stderr du CLI dessous.
    fn finish_mcp_call(
        &mut self,
        target: Target,
        result: Result<(), CliError>,
        cx: &mut Context<Self>,
    ) {
        self.mcp.busy = None;
        self.mcp._call = None;
        match result {
            Ok(()) => match &target {
                Target::Server(name) => {
                    self.mcp.open.remove(name);
                }
                Target::New => self.mcp.draft = None,
            },
            // L'erreur se pose avant la relecture : une ligne qui la porte reste affichée même si l'échec a fait
            // disparaître le serveur du fichier (ajout et restauration ratés).
            Err(error) => self.mcp.set_error(&target, Some(describe_failure(&error))),
        }
        self.refresh_mcp(cx);
        cx.notify();
    }

    // ---- Rendu ------------------------------------------------------------------------------------------------------

    /// Le bouton « Ajouter » de l'en-tête ; sans `claude` ou sans fichier lisible, il n'y a rien à ajouter.
    pub(super) fn mcp_actions(
        &mut self,
        section: Section,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if section != Section::McpServers || self.mcp.claude.is_err() || self.mcp.servers.is_err() {
            return None;
        }
        let theme = *cx.global::<Theme>();
        let enabled = self.mcp.busy.is_none();
        let mut add = action_button(
            "settings-mcp-add",
            "Ajouter",
            Some(Icon::Add),
            ButtonKind::Primary,
            enabled,
            &theme,
        )
        // L'en-tête déplace la fenêtre sur un appui : le bouton garde le sien.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        if enabled {
            add = add.on_click(cx.listener(|this, _, window, cx| this.add_mcp_server(window, cx)));
        }
        Some(add.into_any_element())
    }

    /// Le corps de la section : le bandeau et la liste, ou l'état vide quand `claude` manque, ou l'erreur de lecture.
    pub(super) fn render_mcp_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match (self.mcp.claude.clone(), self.mcp.servers.clone()) {
            (Err(tried), _) => self.render_cli_missing(&tried, theme).into_any_element(),
            (Ok(_), Err(message)) => div()
                .pt(theme.px(8.))
                .text_color(color(ERROR_TEXT))
                .child(SharedString::from(message))
                .into_any_element(),
            (Ok(_), Ok(servers)) => self.render_mcp_list(&servers, theme, cx).into_any_element(),
        };
        div()
            .id(("settings-body", Section::McpServers as usize))
            .size_full()
            .overflow_y_scroll()
            .px(theme.px(20.))
            .pb(theme.px(20.))
            .child(content)
            .into_any_element()
    }

    /// L'état vide « CLI Claude introuvable » suivi des chemins essayés.
    pub(super) fn render_cli_missing(&self, tried: &[PathBuf], theme: &Theme) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap(theme.px(6.))
            .pt(theme.px(8.))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(white(TEXT_PRIMARY))
                    .child(NO_CLI),
            )
            .child(div().text_color(white(TEXT_MIN)).child(NO_CLI_HELP))
            .children(tried.iter().map(|path| {
                div()
                    .text_size(theme.text(12.))
                    .text_color(white(TEXT_MIN))
                    .child(SharedString::from(path.display().to_string()))
            }))
    }

    fn render_mcp_list(
        &mut self,
        servers: &BTreeMap<String, Value>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let banner = notice_banner(theme);
        let head = div()
            .flex()
            .items_center()
            .h(theme.px(28.))
            .border_b_1()
            .border_color(white(SEPARATOR))
            .text_size(theme.text(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(white(TEXT_MIN))
            .child(div().flex_1().child("Serveur"))
            .child(div().w(theme.px(BADGE_COLUMN)).flex_none().child("Type"))
            .child(div().w(theme.px(CHEVRON_COLUMN)).flex_none());

        let mut list = div().flex().flex_col().child(head);
        if self.mcp.draft.is_some() {
            list = list.child(self.render_new_server(servers, theme, cx));
        }
        // Les lignes dont l'appel a échoué et que le fichier ne porte plus, en tête : elles ont besoin d'attention.
        let mut absent: Vec<String> = self
            .mcp
            .open
            .keys()
            .filter(|name| !servers.contains_key(*name))
            .cloned()
            .collect();
        absent.sort();
        for name in &absent {
            list = list.child(self.render_mcp_row(name, None, theme, cx));
        }
        for (name, server) in servers {
            list = list.child(self.render_mcp_row(name, Some(server), theme, cx));
        }
        let mut body = div().flex().flex_col().child(banner).child(list);
        if servers.is_empty() && self.mcp.draft.is_none() {
            body = body.child(
                div()
                    .py(theme.px(12.))
                    .text_color(white(TEXT_MIN))
                    .child(NO_SERVER),
            );
        }
        body
    }

    /// Une ligne de la liste : nom, résumé, badge, chevron ; dépliée, son éditeur et ses actions.
    fn render_mcp_row(
        &mut self,
        name: &str,
        server: Option<&Value>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let open = self.mcp.open.get(name);
        // Absent du fichier : le bloc affiché est la saisie, faute d'un bloc lu.
        let typed: Value = open
            .and_then(|row| serde_json::from_str(&row.editor.read(cx).value()).ok())
            .unwrap_or(Value::Null);
        let listed = server.is_some();
        let server = server.unwrap_or(&typed);
        let is_open = open.is_some();
        let target = Target::Server(name.to_string());
        let busy_here = self.mcp.busy.as_ref() == Some(&target);
        let remote = is_remote(server);
        let key = name.to_string();
        let header = div()
            .id(SharedString::from(format!("settings-mcp-row-{name}")))
            .flex()
            .items_center()
            .h(theme.px(ROW_HEIGHT))
            .cursor_pointer()
            .when(!is_open, |row| {
                row.border_b_1().border_color(white(SEPARATOR))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_mcp_row(&key, window, cx);
            }))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(theme.px(10.))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(white(TEXT_PRIMARY))
                            .child(SharedString::from(name.to_string())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(
                                cx.global::<gpui_component::Theme>()
                                    .mono_font_family
                                    .clone(),
                            )
                            .text_size(theme.text(11.))
                            .text_color(white(TEXT_MIN))
                            .child(SharedString::from(if listed {
                                mcp_summary(server)
                            } else {
                                ABSENT.to_string()
                            })),
                    ),
            )
            .child(type_badge(
                remote,
                cx.global::<gpui_component::Theme>()
                    .mono_font_family
                    .clone(),
                theme,
            ))
            .child(
                div().w(theme.px(CHEVRON_COLUMN)).flex_none().child(
                    svg()
                        .path(
                            if is_open {
                                Icon::ExpandMore
                            } else {
                                Icon::ChevronRight
                            }
                            .path(),
                        )
                        .size(theme.px(16.))
                        .text_color(white(ICON_SECONDARY)),
                ),
            );

        let Some(open) = open else {
            return div().child(header);
        };
        let draft = open.editor.read(cx).value().to_string();
        let state = save_enabled(listed.then_some(server), &draft);
        let idle = self.mcp.busy.is_none();
        let ready = idle && matches!(state, SaveState::Ready(_));
        let save_key = name.to_string();
        let cancel_target = target.clone();
        let delete_key = name.to_string();

        let mut actions = div()
            .flex()
            .items_center()
            .gap(theme.px(6.))
            .mt(theme.px(10.));
        let mut delete = action_button(
            SharedString::from(format!("settings-mcp-delete-{name}")),
            "Supprimer",
            Some(Icon::Delete),
            ButtonKind::Danger,
            idle && listed,
            theme,
        );
        if idle && listed {
            delete = delete.on_click(cx.listener(move |this, _, window, cx| {
                this.confirm_mcp_removal(&delete_key, window, cx);
            }));
        }
        let mut cancel = action_button(
            SharedString::from(format!("settings-mcp-cancel-{name}")),
            "Annuler",
            None,
            ButtonKind::Ghost,
            idle,
            theme,
        );
        if idle {
            cancel = cancel.on_click(cx.listener(move |this, _, _, cx| {
                this.cancel_mcp(&cancel_target, cx);
            }));
        }
        let mut save = action_button(
            SharedString::from(format!("settings-mcp-save-{name}")),
            "Enregistrer",
            None,
            ButtonKind::Primary,
            ready,
            theme,
        );
        if ready {
            save = save.on_click(cx.listener(move |this, _, window, cx| {
                this.save_mcp_row(&save_key, window, cx);
            }));
        }
        actions = actions
            .child(delete)
            .child(div().flex_1())
            .children(busy_here.then(|| busy_label(theme)))
            .child(cancel)
            .child(save);

        let mut editor_block = div().flex().flex_col().pb(theme.px(12.)).child(code_box(
            &open.editor,
            &draft,
            !idle,
            theme,
        ));
        if let SaveState::Invalid(message) = &state {
            editor_block = editor_block.child(error_line(message.clone(), theme));
        }
        if remote {
            editor_block = editor_block.child(
                div()
                    .mt(theme.px(8.))
                    .text_size(theme.text(12.))
                    .text_color(white(TEXT_MIN))
                    .child(HTTP_NOTE),
            );
        }
        if let Some(text) = &open.error {
            editor_block = editor_block.child(error_line(text.clone(), theme));
        }
        editor_block = editor_block.child(actions);

        div()
            .mx(theme.px(-8.))
            .px(theme.px(8.))
            .rounded(theme.px(7.))
            .bg(white(OPEN_ROW))
            .border_b_1()
            .border_color(white(SEPARATOR))
            .child(header)
            .child(editor_block)
    }

    /// La ligne « Ajouter » : le champ du nom, l'éditeur du modèle, les actions.
    fn render_new_server(
        &mut self,
        servers: &BTreeMap<String, Value>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let Some(draft) = &self.mcp.draft else {
            return div();
        };
        let name = draft.name.read(cx).value().to_string();
        let text = draft.editor.read(cx).value().to_string();
        let name_check = validate_new_name(&name, servers);
        // Un nom vide n'est pas une faute à montrer : « Enregistrer » reste simplement inactif.
        let name_error = match name_check {
            Err(EMPTY_NAME) | Ok(()) => None,
            Err(message) => Some(message.to_string()),
        };
        let state = save_enabled(None, &text);
        let idle = self.mcp.busy.is_none();
        let ready = idle && name_check.is_ok() && matches!(state, SaveState::Ready(_));
        let busy_here = self.mcp.busy == Some(Target::New);
        let name_field = field_box(
            "settings-mcp-new-name",
            &draft.name,
            Some(260.),
            name_error.is_some(),
            theme,
            cx,
        );
        let editor = draft.editor.clone();
        let error = draft.error.clone();

        let mut cancel = action_button(
            "settings-mcp-new-cancel",
            "Annuler",
            None,
            ButtonKind::Ghost,
            idle,
            theme,
        );
        if idle {
            cancel =
                cancel.on_click(cx.listener(|this, _, _, cx| this.cancel_mcp(&Target::New, cx)));
        }
        let mut save = action_button(
            "settings-mcp-new-save",
            "Enregistrer",
            None,
            ButtonKind::Primary,
            ready,
            theme,
        );
        if ready {
            save =
                save.on_click(cx.listener(|this, _, window, cx| this.save_mcp_draft(window, cx)));
        }
        let actions = div()
            .flex()
            .items_center()
            .gap(theme.px(6.))
            .mt(theme.px(10.))
            .child(div().flex_1())
            .children(busy_here.then(|| busy_label(theme)))
            .child(cancel)
            .child(save);

        let mut block = div()
            .flex()
            .flex_col()
            .pb(theme.px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(theme.px(ROW_HEIGHT))
                    .child(name_field),
            )
            .children(name_error.map(|text| error_line(text, theme)))
            .child(div().h(theme.px(8.)))
            .child(code_box(&editor, &text, !idle, theme));
        if let SaveState::Invalid(message) = &state {
            block = block.child(error_line(message.clone(), theme));
        }
        if let Some(text) = error {
            block = block.child(error_line(text, theme));
        }
        div()
            .mx(theme.px(-8.))
            .px(theme.px(8.))
            .rounded(theme.px(7.))
            .bg(white(OPEN_ROW))
            .border_b_1()
            .border_color(white(SEPARATOR))
            .child(block.child(actions))
    }
}

/// Le bandeau « prochaines sessions » sous l'en-tête des sections Serveurs MCP et Plugins.
pub(super) fn notice_banner(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(theme.px(8.))
        .mb(theme.px(12.))
        .px(theme.px(10.))
        .py(theme.px(8.))
        .rounded(theme.px(7.))
        .bg(white(NOTICE))
        .text_size(theme.text(12.))
        .text_color(white(TEXT_MIN))
        .child(
            svg()
                .path(Icon::Info.path())
                .flex_none()
                .size(theme.px(15.))
                .text_color(white(ICON_SECONDARY)),
        )
        .child(BANNER)
}

/// L'indicateur d'un appel en cours, à gauche des boutons de sa ligne.
pub(crate) fn busy_label(theme: &Theme) -> gpui::Div {
    busy_text(BUSY, theme)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn save_is_disabled_until_the_json_changes() {
        let original =
            json!({"type": "stdio", "command": "bun", "args": ["run", "a.ts"], "env": {}});
        // Les mêmes valeurs, d'autres espaces et un autre ordre de clés : rien à écrire.
        let reshuffled = r#"{ "env": {},
            "args": ["run",   "a.ts"], "command": "bun",
            "type": "stdio" }"#;
        assert_eq!(
            save_enabled(Some(&original), reshuffled),
            SaveState::Unchanged
        );
        assert!(matches!(
            save_enabled(Some(&original), "{"),
            SaveState::Invalid(_)
        ));
        let changed = r#"{"type":"stdio","command":"bun","args":["run","b.ts"],"env":{}}"#;
        assert_eq!(
            save_enabled(Some(&original), changed),
            SaveState::Ready(
                json!({"type": "stdio", "command": "bun", "args": ["run", "b.ts"], "env": {}})
            )
        );
        // Un JSON valide qui n'est pas un objet n'est pas un serveur.
        assert!(matches!(
            save_enabled(Some(&original), "[]"),
            SaveState::Invalid(_)
        ));
        assert!(matches!(save_enabled(None, ""), SaveState::Invalid(_)));
        // Sans bloc d'origine, tout objet valide est prêt.
        assert_eq!(save_enabled(None, "{}"), SaveState::Ready(json!({})));
    }

    #[test]
    fn a_new_server_needs_a_free_name() {
        let mut existing = BTreeMap::new();
        existing.insert("erom-memory".to_string(), json!({"command": "bun"}));
        assert!(validate_new_name("", &existing).is_err());
        assert!(validate_new_name("   ", &existing).is_err());
        assert!(validate_new_name("erom-memory", &existing).is_err());
        assert!(validate_new_name(" erom-memory ", &existing).is_err());
        assert_eq!(validate_new_name("image-crop", &existing), Ok(()));
        // Un nom qui commence par un tiret passerait pour une option du CLI.
        assert!(validate_new_name("--help", &existing).is_err());
        assert!(validate_new_name(" -x", &existing).is_err());
    }

    #[test]
    fn new_server_template_is_the_spec_template() {
        let template: Value = serde_json::from_str(NEW_SERVER_TEMPLATE).unwrap();
        assert_eq!(
            template,
            json!({"type": "stdio", "command": "", "args": [], "env": {}})
        );
    }

    #[test]
    fn a_block_is_shown_type_first_and_reads_back_unchanged() {
        let server = json!({"env": {"K": "v"}, "args": ["run", "/a/b.ts"], "command": "bun", "type": "stdio", "zeta": 1});
        let shown = pretty_server(&server);
        let order: Vec<&str> =
            ["\"type\"", "\"command\"", "\"args\"", "\"env\"", "\"zeta\""].to_vec();
        let positions: Vec<usize> = order.iter().map(|key| shown.find(key).unwrap()).collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{shown}"
        );
        assert_eq!(serde_json::from_str::<Value>(&shown).unwrap(), server);
        // Ouvrir un bloc sans le toucher ne donne jamais un « Enregistrer » allumé.
        assert_eq!(save_enabled(Some(&server), &shown), SaveState::Unchanged);
        assert_eq!(pretty_server(&json!({})), "{}");
    }

    #[test]
    fn remote_servers_are_the_ones_with_a_url_or_an_http_type() {
        assert!(is_remote(&json!({"type": "http", "url": "https://x/mcp"})));
        assert!(is_remote(&json!({"type": "sse"})));
        assert!(is_remote(&json!({"url": "https://x/mcp"})));
        assert!(!is_remote(&json!({"type": "stdio", "command": "bun"})));
        assert!(!is_remote(&json!({"command": "bun"})));
    }

    #[test]
    fn a_row_that_holds_a_call_error_survives_a_reread_that_lost_its_server() {
        // Le serveur est dans le fichier : la ligne reste, avec ou sans erreur.
        assert!(keeps_row(true, false));
        // Ajout et restauration ratés : le serveur a quitté le fichier, la ligne garde sa saisie et son message.
        assert!(keeps_row(false, true));
        // Retiré ailleurs, ligne intacte : elle disparaît.
        assert!(!keeps_row(false, false));
    }

    type Calls = Rc<RefCell<Vec<Vec<String>>>>;

    fn recorder(calls: &Calls) -> impl FnMut(&[String]) -> Result<String, CliError> {
        let calls = Rc::clone(calls);
        move |args: &[String]| {
            calls.borrow_mut().push(args.to_vec());
            Ok(String::new())
        }
    }

    #[test]
    fn each_operation_sends_its_cli_calls_in_user_scope() {
        let old = json!({"command": "a"});
        let new = json!({"command": "b"});

        let calls = Calls::default();
        apply_op(
            &mut recorder(&calls),
            &McpOp::Add {
                name: "n".into(),
                server: new.clone(),
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), vec![mcp_add_args("n", &new.to_string())]);

        let calls = Calls::default();
        apply_op(&mut recorder(&calls), &McpOp::Remove { name: "n".into() }).unwrap();
        assert_eq!(*calls.borrow(), vec![mcp_remove_args("n")]);

        let calls = Calls::default();
        apply_op(
            &mut recorder(&calls),
            &McpOp::Edit {
                name: "n".into(),
                old,
                new: new.clone(),
            },
        )
        .unwrap();
        assert_eq!(
            *calls.borrow(),
            vec![mcp_remove_args("n"), mcp_add_args("n", &new.to_string())]
        );
    }

    #[test]
    fn a_failure_without_stderr_still_says_what_happened() {
        let silent = CliError {
            code: Some(2),
            stderr: String::new(),
        };
        assert_eq!(describe_failure(&silent), "claude a échoué (code 2).");
        let killed = CliError {
            code: None,
            stderr: "\n".into(),
        };
        assert!(describe_failure(&killed).contains("sans code"));
        let loud = CliError {
            code: Some(1),
            stderr: "Invalid name\n".into(),
        };
        assert_eq!(describe_failure(&loud), "Invalid name");
    }
}
