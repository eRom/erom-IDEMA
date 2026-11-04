//! L'onglet Connecteurs du volet Projet (spec §3.4, maquette `maquette/volet-projet-connecteurs.html`) : les serveurs
//! MCP du projet, une ligne par nom sur les cinq fichiers des harnais. Ce fichier porte les décisions pures (textes des
//! bandeaux, validation du nom, erreurs à montrer) et, plus bas, la colle GPUI. Le modèle (lecture, écriture) est dans
//! `mcp_model.rs`, les formats dans `mcp_files.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, Context, Div, Entity, FocusHandle, FontWeight, MouseButton, MouseDownEvent,
    PromptButton, PromptLevel, SharedString, Stateful, Subscription, Task, Window, div, prelude::*,
    svg,
};
use gpui_component::input::{EditorState, InputEvent, InputState};
use serde_json::Value;

use super::banner::{banner_button, error_banner, info_banner, warning_banner};
use super::mcp_files::{Harness, validate_core};
use super::mcp_model::{McpOp, ProjectServers, RowState, ServerRow, apply, load};
use super::skills_view::tilde_path;
use crate::assets::Icon;
use crate::files::panel::home_dir;
use crate::mcp_widgets::{
    BADGE_COLUMN, CHEVRON_COLUMN, busy_text, code_box, json_editor, type_badge,
};
use crate::settings::claude_cli::mcp_summary;
use crate::settings::controls::{ButtonKind, action_button, error_line, field_box};
use crate::settings::mcp_section::{
    NEW_SERVER_TEMPLATE, SaveState, is_remote, pretty_server, save_enabled,
};
use crate::terminal_panel::BODY_MARGIN_SIDES;
use crate::theme::{ICON_SECONDARY, OPEN_ROW, SEPARATOR, TEXT_MIN, TEXT_PRIMARY, Theme, white};

/// Le texte d'un nom refusé : ni vide, ni hors de `[A-Za-z0-9_-]` (règle de Claude Code 2.1.317).
const EMPTY_NAME: &str = "Un nom est requis.";
const BAD_NAME: &str = "Lettres, chiffres, tirets et soulignés seulement.";
const TAKEN_NAME: &str = "Ce nom est déjà pris.";
/// Un enregistrement refusé : les fichiers ne disent plus ce que la ligne montrait (à ratifier à la porte : texte posé
/// par la revue finale).
pub const FILES_CHANGED: &str =
    "Les fichiers ont changé depuis l'ouverture : rien n'a été écrit. Relis la ligne.";

/// L'ordre des cinq harnais pour les textes (maquette) : celui de la forme commune d'abord, puis les autres.
const WRITE_ORDER: [Harness; 5] = [
    Harness::Claude,
    Harness::OpenCode,
    Harness::Codex,
    Harness::Grok,
    Harness::Antigravity,
];

/// Une liste en français : « a », « a et b », « a, b et c ».
pub fn join_and(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_string(),
        [head @ .., last] => format!("{} et {last}", head.join(", ")),
    }
}

/// Les fichiers que porte `harnesses`, dans l'ordre reçu.
fn files_of(harnesses: &[Harness]) -> Vec<&'static str> {
    harnesses.iter().map(|harness| harness.mcp_file()).collect()
}

/// Le bandeau orange d'une ligne : « Seulement dans <fichiers> » (serveur incomplet) ou « Différent selon les
/// harnais ». Une ligne complète n'en a pas.
pub fn row_banner_text(state: &RowState) -> Option<String> {
    match state {
        RowState::Complete => None,
        RowState::Incomplete { present } => {
            Some(format!("Seulement dans {}", join_and(&files_of(present))))
        }
        RowState::Divergent { .. } => Some("Différent selon les harnais".to_string()),
    }
}

/// La ligne grise d'une ligne dépliée : ce qu'un fichier porte en plus ou éteint, « Désactivé dans OpenCode ·
/// startup_timeout_sec dans Codex ». Le modèle donne des faits bruts `(harnais, note)` ; la phrase est d'ici.
pub fn notes_line(notes: &[(Harness, String)]) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    let parts: Vec<String> = notes
        .iter()
        .map(|(harness, note)| format!("{note} dans {}", harness.name()))
        .collect();
    Some(parts.join(" · "))
}

/// « Enregistrer » n'existe pas sur une ligne aux définitions différentes : « Garder celle-ci » le remplace, rien ne
/// s'écrase sans ce choix (spec §3.4).
pub fn offers_save(state: &RowState) -> bool {
    !matches!(state, RowState::Divergent { .. })
}

/// Le nom d'un serveur neuf : non vide, de lettres, chiffres, tirets et soulignés seulement (règle de Claude Code
/// 2.1.317), et absent des cinq fichiers (`existing` est l'union de leurs noms).
pub fn validate_project_server_name(name: &str, existing: &[String]) -> Result<(), &'static str> {
    let name = name.trim();
    if name.is_empty() {
        Err(EMPTY_NAME)
    } else if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Err(BAD_NAME)
    } else if existing.iter().any(|known| known == name) {
        Err(TAKEN_NAME)
    } else {
        Ok(())
    }
}

/// Le nom vide n'est pas une faute à montrer : « Enregistrer » reste simplement inactif.
pub fn name_error_to_show(check: Result<(), &'static str>) -> Option<&'static str> {
    match check {
        Err(EMPTY_NAME) | Ok(()) => None,
        Err(message) => Some(message),
    }
}

/// Le bandeau rouge d'un fichier illisible : « <fichier> illisible : <erreur> ». La raison du modèle commence par le
/// nom du fichier concerné (« .codex/config.toml : TOML invalide… », ou « opencode.jsonc : format non géré »).
pub fn unreadable_text(harness: Harness, reason: &str) -> String {
    match reason.split_once(" : ") {
        Some((file, error)) => format!("{file} illisible : {error}"),
        None => format!("{} illisible : {reason}", harness.mcp_file()),
    }
}

/// « N serveur(s) » de l'en-tête.
pub fn count_text(count: usize) -> String {
    if count > 1 {
        format!("{count} serveurs")
    } else {
        format!("{count} serveur")
    }
}

/// La confirmation d'une suppression : son titre et, quand des fichiers sont illisibles, la précision qui les nomme. Le
/// retrait ne touche que les fichiers lisibles : « des cinq harnais » ne se dit que si les cinq le sont.
pub fn removal_prompt(name: &str, unreadable: &[(Harness, String)]) -> (String, Option<String>) {
    if unreadable.is_empty() {
        return (format!("Supprimer {name} des cinq harnais ?"), None);
    }
    let readable = WRITE_ORDER.len().saturating_sub(unreadable.len());
    let title = match readable {
        0 => format!("Supprimer {name} ?"),
        1 => format!("Supprimer {name} du seul fichier lisible ?"),
        count => format!("Supprimer {name} des {count} fichiers lisibles ?"),
    };
    let files: Vec<&str> = unreadable
        .iter()
        .map(|(harness, _)| harness.mcp_file())
        .collect();
    let verb = if files.len() > 1 {
        "sont illisibles"
    } else {
        "est illisible"
    };
    (
        title,
        Some(format!(
            "{} {verb} : le serveur n'y sera pas touché.",
            join_and(&files)
        )),
    )
}

/// Les harnais dont un fichier lisible porte la ligne.
pub fn carriers(row: &ServerRow, unreadable: &[(Harness, String)]) -> Vec<Harness> {
    match &row.state {
        RowState::Complete => Harness::ALL
            .into_iter()
            .filter(|harness| !unreadable.iter().any(|(known, _)| known == harness))
            .collect(),
        RowState::Incomplete { present } => present.clone(),
        RowState::Divergent { versions } => Harness::ALL
            .into_iter()
            .filter(|harness| {
                versions
                    .iter()
                    .any(|version| version.harnesses.contains(harness))
            })
            .collect(),
    }
}

/// Le texte d'une ligne que la forme commune ne sait pas écrire : elle reste dans les fichiers qui la portent.
pub fn unsupported_text(carriers: &[Harness]) -> String {
    format!(
        "Type non géré : ce serveur reste dans {}.",
        join_and(&files_of(carriers))
    )
}

/// Les erreurs d'un geste qui valent d'être montrées sous la ligne : `apply` en rend une pour chaque fichier illisible
/// à chaque geste, et ce fichier a déjà son bandeau rouge. Une erreur n'est écartée que si elle est, mot pour mot, celle
/// du bandeau : une erreur de geste attribuée au premier harnais, fichier illisible ou non, reste montrée.
pub fn visible_errors(
    errors: &[(Harness, String)],
    unreadable: &[(Harness, String)],
) -> Vec<String> {
    errors
        .iter()
        .filter(|error| !unreadable.contains(error))
        .map(|(_, message)| message.clone())
        .collect()
}

/// Ce avec quoi une ligne a été dépliée : le bloc de son serveur, ou celui de chaque version quand les définitions
/// divergent (dans l'ordre des boîtes montrées).
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    pub cores: Vec<Value>,
    pub divergent: bool,
}

impl Opened {
    pub fn of(row: &ServerRow) -> Self {
        match &row.state {
            RowState::Divergent { versions } => Self {
                cores: versions
                    .iter()
                    .map(|version| version.core.clone())
                    .collect(),
                divergent: true,
            },
            _ => Self {
                cores: vec![row.core.clone()],
                divergent: false,
            },
        }
    }
}

/// Vrai quand une ligne dépliée survit à une relecture. Elle survit si son serveur est toujours listé, réécrivable, de
/// même forme, et soit identique à ce avec quoi elle a été dépliée, soit éditée par l'utilisateur (sa saisie reste ;
/// « Enregistrer » se compare au bloc relu). Un bloc changé sous une ligne intacte la replie : elle montrerait une
/// définition périmée, et « Enregistrer » ou « Garder celle-ci » l'écrirait dans les cinq fichiers. Les boîtes d'une
/// ligne divergente ne s'éditent pas : changées, elles se replient toujours.
pub fn keeps_open_row(opened: &Opened, fresh: Option<&ServerRow>, edited: bool) -> bool {
    let Some(row) = fresh.filter(|row| row.supported) else {
        return false;
    };
    let now = Opened::of(row);
    if now.divergent != opened.divergent {
        return false;
    }
    now == *opened || (!opened.divergent && edited)
}

/// L'état de « Enregistrer » côté projet : `save_enabled` de Personnaliser (JSON valide et changé), puis `validate_core`
/// (un serveur que les cinq fichiers savent écrire : type géré, `command` ou `url` non vide). Un seul message.
pub fn project_save_state(original: Option<&Value>, draft: &str) -> SaveState {
    match save_enabled(original, draft) {
        SaveState::Ready(core) => match validate_core(&core) {
            Ok(core) => SaveState::Ready(core),
            Err(message) => SaveState::Invalid(message),
        },
        other => other,
    }
}

/// Un geste, joué dans le projet : rend les erreurs à montrer sous la ligne, vides si tout est écrit. Un serveur neuf
/// (`must_be_new`) est refusé si un agent a pris son nom depuis l'affichage : `Save` écraserait ce serveur. `shown` est
/// ce que la ligne montrait à l'ouverture : « Enregistrer » écrit le texte de l'éditeur et « Garder celle-ci » le bloc
/// d'une boîte, tous deux nés de cet affichage ; si les fichiers ne le disent plus, rien n'est écrit (`FILES_CHANGED`).
pub fn run_op(
    project: &Path,
    op: &McpOp,
    must_be_new: bool,
    shown: Option<&Opened>,
) -> Vec<String> {
    let before = load(project);
    if must_be_new
        && let McpOp::Save { name, .. } = op
        && before.rows.iter().any(|row| &row.name == name)
    {
        return vec![TAKEN_NAME.to_string()];
    }
    if let (Some(shown), McpOp::Save { name, .. }) = (shown, op) {
        let fresh = before
            .rows
            .iter()
            .find(|row| &row.name == name)
            .map(Opened::of);
        if fresh.as_ref() != Some(shown) {
            return vec![FILES_CHANGED.to_string()];
        }
    }
    visible_errors(&apply(project, op), &before.unreadable)
}

// ---------------------------------------------------------------------------------------------------------------------
// La colle GPUI. Le rendu se vérifie dans la VM ; les décisions sont ci-dessus.
// ---------------------------------------------------------------------------------------------------------------------

const NO_SERVER: &str = "Aucun serveur MCP.";
const NAME_PLACEHOLDER: &str = "Nom du serveur";
/// Le texte d'un geste en cours.
const BUSY: &str = "En cours…";
/// La hauteur minimale d'une ligne (maquette `.row` : 46, deux lignes de texte l'une sous l'autre).
const ROW_MIN_HEIGHT: f32 = 46.;

/// Qui un geste ou une erreur concerne : une ligne de la liste, ou la ligne « Ajouter ».
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Target {
    Server(String),
    New,
}

/// Une ligne dépliée : un éditeur JSON (ou un par version, quand les définitions divergent).
struct OpenRow {
    editors: Vec<Entity<EditorState>>,
    /// Ce avec quoi la ligne a été dépliée : une relecture qui le change la replie (`keeps_open_row`).
    opened: Opened,
    _subscriptions: Vec<Subscription>,
}

/// La ligne « Ajouter » pas encore écrite : le nom et le JSON.
struct NewServer {
    name: Entity<InputState>,
    editor: Entity<EditorState>,
    _subscriptions: Vec<Subscription>,
}

/// Le geste en cours, avec le projet pour lequel il tourne : un autre projet n'en montre rien.
struct McpCall {
    project: PathBuf,
    target: Target,
}

/// L'onglet Connecteurs : la dernière lecture des cinq fichiers, les lignes dépliées, le brouillon « Ajouter » et le
/// geste en cours (un seul à la fois : `apply` écrit par un fichier temporaire au nom fixe).
pub struct ConnectorsTab {
    project: Option<PathBuf>,
    /// Là où la saisie va quand l'onglet est choisi (`ProjectPanel::body_focus`).
    focus_handle: FocusHandle,
    /// `None` tant que la première lecture du projet n'est pas revenue.
    loaded: Option<ProjectServers>,
    /// Une lecture est en route ; son numéro écarte la réponse d'une lecture plus ancienne.
    loading: bool,
    generation: u64,
    load_task: Option<Task<()>>,
    open: HashMap<String, OpenRow>,
    draft: Option<NewServer>,
    call: Option<McpCall>,
    call_task: Option<Task<()>>,
    /// Les erreurs du dernier geste, sous leur ligne.
    errors: HashMap<Target, Vec<String>>,
}

impl ConnectorsTab {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            project: None,
            focus_handle: cx.focus_handle(),
            loaded: None,
            loading: false,
            generation: 0,
            load_task: None,
            open: HashMap::new(),
            draft: None,
            call: None,
            call_task: None,
            errors: HashMap::new(),
        }
    }

    /// Le focus du corps de l'onglet.
    pub fn body_focus(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Relit les cinq fichiers de `project` (ouverture de l'onglet, changement de projet). Une lecture déjà en route
    /// pour ce projet n'est pas doublée.
    pub fn refresh(&mut self, project: &Path, cx: &mut Context<Self>) {
        self.load(project, false, cx);
    }

    fn load(&mut self, project: &Path, force: bool, cx: &mut Context<Self>) {
        if self.project.as_deref() != Some(project) {
            self.project = Some(project.to_path_buf());
            self.loaded = None;
            self.open.clear();
            self.draft = None;
            self.errors.clear();
        } else if self.loading && !force {
            return;
        }
        self.loading = true;
        self.generation += 1;
        let generation = self.generation;
        let project = project.to_path_buf();
        let work = {
            let project = project.clone();
            cx.background_spawn(async move { load(&project) })
        };
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = work.await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.generation {
                    return;
                }
                this.loading = false;
                if this.project.as_deref() != Some(project.as_path()) {
                    return;
                }
                // Pendant un geste, les fichiers sont à moitié écrits : la relecture de fin de geste montre l'état réel.
                if this
                    .call
                    .as_ref()
                    .is_some_and(|call| call.project == project)
                {
                    return;
                }
                this.accept(loaded, cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Retient une lecture : une ligne dépliée dont le serveur a disparu, changé de forme ou de définition sous une
    /// saisie intacte se replie (voir `keeps_open_row`).
    fn accept(&mut self, loaded: ProjectServers, cx: &App) {
        let rows = &loaded.rows;
        self.open.retain(|name, open| {
            let edited = !open.opened.divergent
                && open.editors.first().is_some_and(|editor| {
                    let text = editor.read(cx).value().to_string();
                    !matches!(
                        save_enabled(open.opened.cores.first(), &text),
                        SaveState::Unchanged
                    )
                });
            keeps_open_row(
                &open.opened,
                rows.iter().find(|row| row.name == *name),
                edited,
            )
        });
        self.errors.retain(|target, _| match target {
            Target::Server(name) => rows.iter().any(|row| row.name == *name),
            Target::New => true,
        });
        self.loaded = Some(loaded);
    }

    fn rows(&self) -> &[ServerRow] {
        self.loaded
            .as_ref()
            .map_or(&[], |loaded| loaded.rows.as_slice())
    }

    fn row(&self, name: &str) -> Option<&ServerRow> {
        self.rows().iter().find(|row| row.name == name)
    }

    /// Les fichiers illisibles de la dernière lecture.
    fn unreadable(&self) -> &[(Harness, String)] {
        self.loaded
            .as_ref()
            .map_or(&[], |loaded| loaded.unreadable.as_slice())
    }

    fn call_on(&self, target: &Target) -> bool {
        self.call.as_ref().is_some_and(|call| {
            self.project.as_ref() == Some(&call.project) && call.target == *target
        })
    }

    // ---- Gestes -----------------------------------------------------------------------------------------------------

    /// Un clic sur une ligne la déplie, ou la replie (la saisie non enregistrée est perdue). Celle dont le geste court
    /// reste en place.
    fn toggle_row(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let target = Target::Server(name.to_string());
        if self.call_on(&target) {
            return;
        }
        if self.open.remove(name).is_some() {
            self.errors.remove(&target);
            cx.notify();
            return;
        }
        let Some(row) = self.row(name).filter(|row| row.supported) else {
            return;
        };
        let opened = Opened::of(row);
        let mut editors = Vec::new();
        let mut subscriptions = Vec::new();
        for core in &opened.cores {
            let editor = json_editor(&pretty_server(core), window, cx);
            subscriptions.push(cx.subscribe_in(
                &editor,
                window,
                |_, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ));
            editors.push(editor);
        }
        self.errors.remove(&target);
        self.open.insert(
            name.to_string(),
            OpenRow {
                editors,
                opened,
                _subscriptions: subscriptions,
            },
        );
        cx.notify();
    }

    /// « Ajouter » : une ligne neuve dépliée en tête de liste, le focus dans le champ du nom.
    fn add_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loaded.is_none() {
            return;
        }
        if self.draft.is_none() {
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
            self.errors.remove(&Target::New);
            self.draft = Some(NewServer {
                name,
                editor,
                _subscriptions: subscriptions,
            });
        }
        if let Some(draft) = &self.draft {
            draft.name.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    /// « Annuler » : la ligne se replie, sa saisie part.
    fn cancel(&mut self, target: &Target, cx: &mut Context<Self>) {
        if self.call.is_some() {
            return;
        }
        match target {
            Target::Server(name) => {
                self.open.remove(name);
            }
            Target::New => self.draft = None,
        }
        self.errors.remove(target);
        cx.notify();
    }

    /// « Enregistrer » d'un serveur de la liste : le bloc édité dans les cinq fichiers.
    fn save_row(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self
            .row(name)
            .filter(|row| row.supported && offers_save(&row.state))
        else {
            return;
        };
        let Some(open) = self.open.get(name) else {
            return;
        };
        let Some(editor) = open.editors.first() else {
            return;
        };
        let text = editor.read(cx).value().to_string();
        if let SaveState::Ready(core) = project_save_state(Some(&row.core), &text) {
            let op = McpOp::Save {
                name: name.to_string(),
                core,
            };
            let shown = Some(open.opened.clone());
            self.start_call(
                Target::Server(name.to_string()),
                op,
                false,
                shown,
                window,
                cx,
            );
        }
    }

    /// « Enregistrer » de la ligne neuve.
    fn save_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.draft else {
            return;
        };
        let name = draft.name.read(cx).value().trim().to_string();
        let existing: Vec<String> = self.rows().iter().map(|row| row.name.clone()).collect();
        if validate_project_server_name(&name, &existing).is_err() {
            return;
        }
        let text = draft.editor.read(cx).value().to_string();
        if let SaveState::Ready(core) = project_save_state(None, &text) {
            let op = McpOp::Save { name, core };
            self.start_call(Target::New, op, true, None, window, cx);
        }
    }

    /// « Garder celle-ci » : le bloc de la boîte montrée remplace les autres versions dans les cinq fichiers. C'est le bloc
    /// avec lequel la ligne a été dépliée, jamais celui d'une relecture : une version qui a bougé a replié la ligne.
    fn keep_version(
        &mut self,
        name: &str,
        position: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(core) = self
            .open
            .get(name)
            .filter(|open| open.opened.divergent)
            .and_then(|open| open.opened.cores.get(position))
            .cloned()
        else {
            return;
        };
        let op = McpOp::Save {
            name: name.to_string(),
            core,
        };
        let shown = self.open.get(name).map(|open| open.opened.clone());
        self.start_call(
            Target::Server(name.to_string()),
            op,
            false,
            shown,
            window,
            cx,
        );
    }

    /// « Propager » : le serveur s'écrit dans les fichiers où il manque.
    fn propagate(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let op = McpOp::Propagate {
            name: name.to_string(),
        };
        self.start_call(
            Target::Server(name.to_string()),
            op,
            false,
            None,
            window,
            cx,
        );
    }

    /// « Supprimer » : la confirmation native, puis le retrait des cinq fichiers. Le projet visé est celui de la ligne
    /// cliquée : la confirmation laisse le temps de changer de session.
    fn confirm_removal(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        if self.call.is_some() {
            return;
        }
        let (title, detail) = removal_prompt(name, self.unreadable());
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            detail.as_deref(),
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
                    if this.project.as_deref() != Some(project.as_path()) {
                        crate::hud::log_event("mcp removal skipped reason=project-changed");
                        return;
                    }
                    let op = McpOp::Remove { name: name.clone() };
                    this.start_call(Target::Server(name), op, false, None, window, cx);
                });
            }
        })
        .detach();
    }

    /// Lance `op` hors du fil d'interface, dans le projet affiché. Un seul geste à la fois : un second est refusé. À la
    /// fin les erreurs se posent sous la ligne, la ligne se replie si tout est écrit, et les fichiers sont relus.
    fn start_call(
        &mut self,
        target: Target,
        op: McpOp,
        must_be_new: bool,
        shown: Option<Opened>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project.clone() else {
            return;
        };
        if self.call.is_some() {
            return;
        }
        self.errors.remove(&target);
        self.call = Some(McpCall {
            project: project.clone(),
            target: target.clone(),
        });
        let work = {
            let project = project.clone();
            cx.background_spawn(async move { run_op(&project, &op, must_be_new, shown.as_ref()) })
        };
        self.call_task = Some(cx.spawn(async move |this, cx| {
            let errors = work.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_call(&project, target, errors, cx)
            });
        }));
        cx.notify();
    }

    fn finish_call(
        &mut self,
        project: &Path,
        target: Target,
        errors: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.call = None;
        self.call_task = None;
        if self.project.as_deref() == Some(project) {
            if errors.is_empty() {
                match &target {
                    Target::Server(name) => {
                        self.open.remove(name);
                    }
                    Target::New => self.draft = None,
                }
            } else {
                // Les fichiers ont bougé sous une ligne dépliée : elle se replie, le message se pose dessous, et la
                // relecture qui suit montre ce que les fichiers disent maintenant.
                if errors.iter().any(|error| error == FILES_CHANGED)
                    && let Target::Server(name) = &target
                {
                    self.open.remove(name);
                }
                self.errors.insert(target, errors);
            }
            self.load(project, true, cx);
        }
        cx.notify();
    }
}

/// Un geste sur un élément : l'appui retient la propagation (la racine du volet reprendrait le focus).
fn on_press(
    element: Stateful<Div>,
    cx: &mut Context<ConnectorsTab>,
    handler: impl Fn(&mut ConnectorsTab, &mut Window, &mut Context<ConnectorsTab>) + 'static,
) -> Stateful<Div> {
    element.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            handler(this, window, cx);
        }),
    )
}

/// Les puces des cinq harnais : vertes pour ceux de `present`.
fn chips_for(present: &[Harness]) -> Vec<(Harness, bool)> {
    Harness::ALL
        .into_iter()
        .map(|harness| (harness, present.contains(&harness)))
        .collect()
}

/// La ligne grise des notes d'un harnais, sous l'éditeur.
fn grey_line(text: String, theme: &Theme) -> Div {
    div()
        .mt(theme.px(8.))
        .text_size(theme.text(11.))
        .text_color(white(TEXT_MIN))
        .child(SharedString::from(text))
}

/// Le fond d'une ligne dépliée (maquette `.row.open`) : blanc à 3 %, rayon 7, débordant de 8 de chaque côté.
fn open_shell(theme: &Theme) -> Div {
    div()
        .flex_none()
        .flex()
        .flex_col()
        .mx(theme.px(-8.))
        .px(theme.px(8.))
        .rounded(theme.px(7.))
        .bg(white(OPEN_ROW))
        .border_b_1()
        .border_color(white(SEPARATOR))
}

impl ConnectorsTab {
    fn render_header(&self, theme: &Theme, mono: SharedString, cx: &mut Context<Self>) -> Div {
        let home = home_dir();
        let path = self
            .project
            .as_deref()
            .map(|project| tilde_path(project, home.as_deref()))
            .unwrap_or_default();
        let enabled = self.call.is_none() && self.loaded.is_some();
        let mut add = action_button(
            "connectors-add",
            "Ajouter",
            Some(Icon::Add),
            ButtonKind::Primary,
            enabled,
            theme,
        );
        if enabled {
            add = on_press(add, cx, |this, window, cx| this.add_draft(window, cx));
        }
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.px(8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(theme.px(6.))
                    .font_family(mono)
                    .text_size(theme.text(11.))
                    .text_color(white(TEXT_MIN))
                    .child(
                        svg()
                            .path(Icon::Folder.path())
                            .flex_none()
                            .size(theme.px(14.))
                            .text_color(white(TEXT_MIN)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_ellipsis_start()
                            .child(SharedString::from(path)),
                    )
                    .children(self.loaded.as_ref().map(|_| {
                        div().flex_none().child(SharedString::from(format!(
                            "· {}",
                            count_text(self.rows().len())
                        )))
                    })),
            )
            .child(add)
    }

    /// Les bandeaux sous l'en-tête : un rouge par fichier illisible, rien d'autre.
    fn render_banners(&self, theme: &Theme) -> Div {
        let mut banners = div();
        let Some(loaded) = &self.loaded else {
            return banners;
        };
        if loaded.unreadable.is_empty() {
            return banners;
        }
        banners = banners
            .flex_none()
            .flex()
            .flex_col()
            .gap(theme.px(8.))
            .pt(theme.px(12.));
        for (harness, reason) in &loaded.unreadable {
            banners = banners.child(error_banner(
                SharedString::from(unreadable_text(*harness, reason)),
                None,
                Vec::new(),
                theme,
            ));
        }
        banners
    }

    /// L'en-tête de la liste (maquette `.thead`) : « Serveur », « Type ».
    fn render_columns(&self, theme: &Theme) -> Div {
        div()
            .flex_none()
            .flex()
            .items_center()
            .h(theme.px(28.))
            .mt(theme.px(10.))
            .border_b_1()
            .border_color(white(SEPARATOR))
            .text_size(theme.text(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(white(TEXT_MIN))
            .child(div().flex_1().child("Serveur"))
            .child(div().w(theme.px(BADGE_COLUMN)).flex_none().child("Type"))
            .child(div().w(theme.px(CHEVRON_COLUMN)).flex_none())
    }

    /// Les boutons d'une ligne dépliée : « Supprimer » (une ligne de la liste seulement), « Annuler » et, sauf sur une
    /// ligne aux définitions divergentes, « Enregistrer » (`save` dit s'il est allumé).
    fn render_actions(
        &self,
        target: &Target,
        save: Option<bool>,
        busy: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let idle = self.call.is_none();
        let id = match target {
            Target::Server(name) => name.clone(),
            Target::New => "new".to_string(),
        };
        let mut actions = div()
            .flex()
            .items_center()
            .gap(theme.px(6.))
            .mt(theme.px(10.));
        if let Target::Server(name) = target {
            let name = name.clone();
            let mut button = action_button(
                SharedString::from(format!("connectors-delete-{id}")),
                "Supprimer",
                Some(Icon::Delete),
                ButtonKind::Danger,
                idle,
                theme,
            );
            if idle {
                button = on_press(button, cx, move |this, window, cx| {
                    this.confirm_removal(&name, window, cx);
                });
            }
            actions = actions.child(button);
        }
        actions = actions
            .child(div().flex_1())
            .children(busy.then(|| busy_text(BUSY, theme)));
        let mut cancel = action_button(
            SharedString::from(format!("connectors-cancel-{id}")),
            "Annuler",
            None,
            ButtonKind::Ghost,
            idle,
            theme,
        );
        if idle {
            let target = target.clone();
            cancel = on_press(cancel, cx, move |this, _, cx| this.cancel(&target, cx));
        }
        actions = actions.child(cancel);
        if let Some(ready) = save {
            let mut button = action_button(
                SharedString::from(format!("connectors-save-{id}")),
                "Enregistrer",
                None,
                ButtonKind::Primary,
                ready,
                theme,
            );
            if ready {
                let target = target.clone();
                button = on_press(button, cx, move |this, window, cx| match &target {
                    Target::Server(name) => this.save_row(name, window, cx),
                    Target::New => this.save_draft(window, cx),
                });
            }
            actions = actions.child(button);
        }
        actions
    }

    /// Le bandeau d'une ligne : « Seulement dans … » avec ses puces et « Propager », ou « Différent selon les
    /// harnais » ; une ligne que la forme commune ne sait pas écrire n'a ni geste ni éditeur.
    fn render_row_banner(
        &self,
        index: usize,
        row: &ServerRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let unreadable = self
            .loaded
            .as_ref()
            .map_or(&[][..], |loaded| loaded.unreadable.as_slice());
        let banner = if !row.supported {
            info_banner(
                SharedString::from(unsupported_text(&carriers(row, unreadable))),
                None,
                Vec::new(),
                theme,
            )
        } else {
            let text = row_banner_text(&row.state)?;
            match &row.state {
                RowState::Incomplete { present } => {
                    let target = Target::Server(row.name.clone());
                    let action: AnyElement = if self.call_on(&target) {
                        busy_text(BUSY, theme).into_any_element()
                    } else {
                        let idle = self.call.is_none();
                        let name = row.name.clone();
                        let mut wrapper = div().id(("connectors-propagate", index));
                        if idle {
                            wrapper = on_press(wrapper, cx, move |this, window, cx| {
                                this.propagate(&name, window, cx);
                            });
                        }
                        wrapper
                            .child(banner_button(
                                "connectors-propagate-button",
                                "Propager",
                                idle,
                                theme,
                            ))
                            .into_any_element()
                    };
                    warning_banner(
                        SharedString::from(text),
                        Some(chips_for(present)),
                        vec![action],
                        theme,
                    )
                }
                _ => warning_banner(SharedString::from(text), None, Vec::new(), theme),
            }
        };
        Some(div().flex_none().mt(theme.px(8.)).child(banner))
    }

    /// Le corps déplié d'une ligne : l'éditeur (ou une boîte par version), les notes, les erreurs et les actions.
    fn render_open(
        &self,
        row: &ServerRow,
        open: &OpenRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let target = Target::Server(row.name.clone());
        let idle = self.call.is_none();
        let busy = self.call_on(&target);
        let mut block = div().flex().flex_col().pb(theme.px(12.));
        let mut save = None;
        match &row.state {
            RowState::Divergent { versions } => {
                for (position, (version, editor)) in versions.iter().zip(&open.editors).enumerate()
                {
                    let text = editor.read(cx).value().to_string();
                    let names: Vec<&str> = version
                        .harnesses
                        .iter()
                        .map(|harness| harness.name())
                        .collect();
                    let name = row.name.clone();
                    let action: AnyElement = if busy {
                        busy_text(BUSY, theme).into_any_element()
                    } else {
                        let mut wrapper = div().id(SharedString::from(format!(
                            "connectors-keep-{}-{position}",
                            row.name
                        )));
                        if idle {
                            wrapper = on_press(wrapper, cx, move |this, window, cx| {
                                this.keep_version(&name, position, window, cx);
                            });
                        }
                        wrapper
                            .child(banner_button(
                                "connectors-keep-button",
                                "Garder celle-ci",
                                idle,
                                theme,
                            ))
                            .into_any_element()
                    };
                    block = block
                        .child(
                            div()
                                .mt(theme.px(8.))
                                .child(code_box(editor, &text, true, theme)),
                        )
                        .child(div().mt(theme.px(6.)).child(info_banner(
                            SharedString::from(join_and(&names)),
                            Some(chips_for(&version.harnesses)),
                            vec![action],
                            theme,
                        )));
                }
            }
            _ => {
                if let Some(editor) = open.editors.first() {
                    let text = editor.read(cx).value().to_string();
                    let state = project_save_state(Some(&row.core), &text);
                    save = Some(idle && matches!(state, SaveState::Ready(_)));
                    block = block.child(code_box(editor, &text, !idle, theme));
                    if let SaveState::Invalid(message) = &state {
                        block = block.child(error_line(message.clone(), theme));
                    }
                }
            }
        }
        block = block.children(notes_line(&row.notes).map(|text| grey_line(text, theme)));
        for message in self.errors.get(&target).into_iter().flatten() {
            block = block.child(error_line(message.clone(), theme));
        }
        block.child(self.render_actions(&target, save, busy, theme, cx))
    }

    /// Une ligne de la liste : nom, résumé, badge, chevron ; son bandeau ; dépliée, son corps.
    fn render_row(
        &self,
        index: usize,
        row: &ServerRow,
        mono: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let open = self.open.get(&row.name);
        let expandable = row.supported;
        let name = row.name.clone();
        let mut header = div()
            .id(("connectors-row", index))
            .flex()
            .items_center()
            .min_h(theme.px(ROW_MIN_HEIGHT))
            .py(theme.px(6.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(theme.px(2.))
                    .child(
                        div()
                            .truncate()
                            .text_size(theme.text(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(white(TEXT_PRIMARY))
                            .child(SharedString::from(row.name.clone())),
                    )
                    .child(
                        div()
                            .truncate()
                            .font_family(mono.clone())
                            .text_size(theme.text(11.))
                            .text_color(white(TEXT_MIN))
                            .child(SharedString::from(mcp_summary(&row.core))),
                    ),
            )
            .child(type_badge(is_remote(&row.core), mono.clone(), theme))
            .child(
                div()
                    .w(theme.px(CHEVRON_COLUMN))
                    .flex_none()
                    .children(expandable.then(|| {
                        svg()
                            .path(
                                if open.is_some() {
                                    Icon::ExpandMore
                                } else {
                                    Icon::ChevronRight
                                }
                                .path(),
                            )
                            .size(theme.px(16.))
                            .text_color(white(ICON_SECONDARY))
                    })),
            );
        if expandable {
            header = on_press(header.cursor_pointer(), cx, move |this, window, cx| {
                this.toggle_row(&name, window, cx);
            });
        }
        let banner = self.render_row_banner(index, row, theme, cx);
        let shell = match open {
            Some(_) => open_shell(theme),
            None => div()
                .flex_none()
                .flex()
                .flex_col()
                .border_b_1()
                .border_color(white(SEPARATOR)),
        };
        let mut body = shell.child(header).children(banner);
        match open {
            Some(open) => body = body.child(self.render_open(row, open, theme, cx)),
            None => {
                for message in self
                    .errors
                    .get(&Target::Server(row.name.clone()))
                    .into_iter()
                    .flatten()
                {
                    body = body.child(error_line(message.clone(), theme).pb(theme.px(8.)));
                }
            }
        }
        body
    }

    /// La ligne « Ajouter » : le champ du nom, l'éditeur du modèle, les actions.
    fn render_draft(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let Some(draft) = &self.draft else {
            return div();
        };
        let name = draft.name.read(cx).value().to_string();
        let text = draft.editor.read(cx).value().to_string();
        let existing: Vec<String> = self.rows().iter().map(|row| row.name.clone()).collect();
        let name_check = validate_project_server_name(&name, &existing);
        let name_error = name_error_to_show(name_check);
        let state = project_save_state(None, &text);
        let idle = self.call.is_none();
        let ready = idle && name_check.is_ok() && matches!(state, SaveState::Ready(_));
        let busy = self.call_on(&Target::New);
        let mut block = div()
            .flex()
            .flex_col()
            .pt(theme.px(8.))
            .pb(theme.px(12.))
            .child(field_box(
                "connectors-new-name",
                &draft.name,
                None,
                name_error.is_some(),
                theme,
                cx,
            ))
            .children(name_error.map(|message| error_line(message.to_string(), theme)))
            .child(div().h(theme.px(8.)))
            .child(code_box(&draft.editor, &text, !idle, theme));
        if let SaveState::Invalid(message) = &state {
            block = block.child(error_line(message.clone(), theme));
        }
        for message in self.errors.get(&Target::New).into_iter().flatten() {
            block = block.child(error_line(message.clone(), theme));
        }
        block = block.child(self.render_actions(&Target::New, Some(ready), busy, theme, cx));
        open_shell(theme).child(block)
    }
}

impl Render for ConnectorsTab {
    /// L'onglet : l'en-tête, puis, dans une zone qui défile, les bandeaux, les colonnes et les lignes.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        let mono = cx
            .global::<gpui_component::Theme>()
            .mono_font_family
            .clone();
        let mut list = div()
            .flex_none()
            .flex()
            .flex_col()
            .child(self.render_columns(&theme));
        if self.draft.is_some() {
            list = list.child(self.render_draft(&theme, cx));
        }
        for (index, row) in self.rows().iter().enumerate() {
            list = list.child(self.render_row(index, row, mono.clone(), &theme, cx));
        }
        if self.loaded.is_some() && self.rows().is_empty() && self.draft.is_none() {
            list = list.child(
                div()
                    .py(theme.px(12.))
                    .text_size(theme.text(12.))
                    .text_color(white(TEXT_MIN))
                    .child(NO_SERVER),
            );
        }
        div()
            .id("connectors-tab")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .px(theme.px(BODY_MARGIN_SIDES))
            .pt(theme.px(8.))
            .pb(theme.px(12.))
            .child(self.render_header(&theme, mono, cx))
            .child(
                div()
                    .id("connectors-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.render_banners(&theme))
                    .child(list),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::super::mcp_model::Version;
    use super::*;
    use serde_json::{Value, json};
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "idema-mcp-view-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn row(name: &str, state: RowState) -> ServerRow {
        ServerRow {
            name: name.to_string(),
            core: json!({"type": "stdio", "command": "bun"}),
            state,
            notes: Vec::new(),
            supported: true,
        }
    }

    #[test]
    fn rows_say_where_an_incomplete_server_lives() {
        assert_eq!(row_banner_text(&RowState::Complete), None);
        assert_eq!(
            row_banner_text(&RowState::Incomplete {
                present: vec![Harness::Claude]
            })
            .as_deref(),
            Some("Seulement dans .mcp.json")
        );
        assert_eq!(
            row_banner_text(&RowState::Incomplete {
                present: vec![Harness::Claude, Harness::OpenCode]
            })
            .as_deref(),
            Some("Seulement dans .mcp.json et opencode.json")
        );
        assert_eq!(
            row_banner_text(&RowState::Incomplete {
                present: vec![Harness::Claude, Harness::Codex, Harness::OpenCode]
            })
            .as_deref(),
            Some("Seulement dans .mcp.json, .codex/config.toml et opencode.json")
        );
        assert_eq!(
            row_banner_text(&RowState::Divergent {
                versions: Vec::new()
            })
            .as_deref(),
            Some("Différent selon les harnais")
        );
    }

    #[test]
    fn notes_are_phrased_per_harness() {
        assert_eq!(notes_line(&[]), None);
        assert_eq!(
            notes_line(&[
                (Harness::OpenCode, "Désactivé".to_string()),
                (Harness::Codex, "startup_timeout_sec".to_string()),
            ])
            .as_deref(),
            Some("Désactivé dans OpenCode · startup_timeout_sec dans Codex")
        );
        assert_eq!(
            notes_line(&[(Harness::Grok, "Désactivé".to_string())]).as_deref(),
            Some("Désactivé dans Grok")
        );
    }

    #[test]
    fn save_waits_for_a_choice_on_a_divergent_row() {
        assert!(offers_save(&RowState::Complete));
        assert!(offers_save(&RowState::Incomplete {
            present: vec![Harness::Claude]
        }));
        let versions = vec![
            Version {
                core: json!({"command": "a"}),
                harnesses: vec![Harness::Claude],
            },
            Version {
                core: json!({"command": "b"}),
                harnesses: vec![Harness::Codex],
            },
        ];
        assert!(!offers_save(&RowState::Divergent { versions }));
    }

    #[test]
    fn new_names_follow_claude() {
        let existing = vec!["taken".to_string(), "my.server".to_string()];
        assert_eq!(
            validate_project_server_name("hello-demo", &existing),
            Ok(())
        );
        assert_eq!(validate_project_server_name("a_b9", &existing), Ok(()));
        for bad in ["my.server", "a b", "é", "a/b", "x:y"] {
            assert_eq!(
                validate_project_server_name(bad, &existing),
                Err("Lettres, chiffres, tirets et soulignés seulement."),
                "{bad}"
            );
        }
        assert_eq!(
            validate_project_server_name("taken", &existing),
            Err("Ce nom est déjà pris.")
        );
        // Une saisie qui ne diffère que par des espaces de bord est le même nom.
        assert_eq!(
            validate_project_server_name(" taken ", &existing),
            Err("Ce nom est déjà pris.")
        );
        assert_eq!(
            validate_project_server_name("  ", &existing),
            Err("Un nom est requis.")
        );
        // Le nom vide ne s'affiche pas comme une faute, les autres refus si.
        assert_eq!(
            name_error_to_show(validate_project_server_name("", &existing)),
            None
        );
        assert_eq!(
            name_error_to_show(validate_project_server_name("taken", &existing)),
            Some("Ce nom est déjà pris.")
        );
    }

    #[test]
    fn an_unreadable_file_is_named_with_its_error() {
        assert_eq!(
            unreadable_text(
                Harness::Codex,
                ".codex/config.toml : TOML invalide (ligne 3, colonne 5)"
            ),
            ".codex/config.toml illisible : TOML invalide (ligne 3, colonne 5)"
        );
        assert_eq!(
            unreadable_text(Harness::OpenCode, "opencode.jsonc : format non géré"),
            "opencode.jsonc illisible : format non géré"
        );
        // Une raison sans nom de fichier prend celui du harnais.
        assert_eq!(
            unreadable_text(Harness::Grok, "cassé"),
            ".grok/config.toml illisible : cassé"
        );
    }

    #[test]
    fn the_header_counts_servers_in_french() {
        assert_eq!(count_text(0), "0 serveur");
        assert_eq!(count_text(1), "1 serveur");
        assert_eq!(count_text(3), "3 serveurs");
    }

    #[test]
    fn the_removal_prompt_only_promises_what_will_be_removed() {
        assert_eq!(
            removal_prompt("hello", &[]),
            ("Supprimer hello des cinq harnais ?".to_string(), None)
        );
        let one = vec![(Harness::Codex, "x".to_string())];
        assert_eq!(
            removal_prompt("hello", &one),
            (
                "Supprimer hello des 4 fichiers lisibles ?".to_string(),
                Some(
                    ".codex/config.toml est illisible : le serveur n'y sera pas touché."
                        .to_string()
                )
            )
        );
        let two = vec![
            (Harness::Codex, "x".to_string()),
            (Harness::Grok, "y".to_string()),
        ];
        let (title, detail) = removal_prompt("hello", &two);
        assert_eq!(title, "Supprimer hello des 3 fichiers lisibles ?");
        assert_eq!(
            detail.as_deref(),
            Some(
                ".codex/config.toml et .grok/config.toml sont illisibles : le serveur n'y sera pas touché."
            )
        );
        let four: Vec<(Harness, String)> = Harness::ALL
            .into_iter()
            .take(4)
            .map(|harness| (harness, "x".to_string()))
            .collect();
        assert_eq!(
            removal_prompt("hello", &four).0,
            "Supprimer hello du seul fichier lisible ?"
        );
        let all: Vec<(Harness, String)> = Harness::ALL
            .into_iter()
            .map(|harness| (harness, "x".to_string()))
            .collect();
        assert_eq!(removal_prompt("hello", &all).0, "Supprimer hello ?");
    }

    #[test]
    fn carriers_follow_the_state_and_skip_unreadable_files() {
        let unreadable = vec![(Harness::Codex, ".codex/config.toml : cassé".to_string())];
        assert_eq!(
            carriers(&row("a", RowState::Complete), &unreadable),
            vec![
                Harness::Claude,
                Harness::Grok,
                Harness::OpenCode,
                Harness::Antigravity
            ]
        );
        assert_eq!(
            carriers(
                &row(
                    "a",
                    RowState::Incomplete {
                        present: vec![Harness::Grok]
                    }
                ),
                &unreadable
            ),
            vec![Harness::Grok]
        );
        let divergent = RowState::Divergent {
            versions: vec![
                Version {
                    core: json!({"command": "a"}),
                    harnesses: vec![Harness::OpenCode],
                },
                Version {
                    core: json!({"command": "b"}),
                    harnesses: vec![Harness::Claude],
                },
            ],
        };
        assert_eq!(
            carriers(&row("a", divergent), &[]),
            vec![Harness::Claude, Harness::OpenCode]
        );
        assert_eq!(
            unsupported_text(&[Harness::Claude, Harness::Grok]),
            "Type non géré : ce serveur reste dans .mcp.json et .grok/config.toml."
        );
    }

    #[test]
    fn the_banner_of_an_unreadable_file_is_not_repeated_under_the_row() {
        let unreadable = vec![(Harness::Codex, ".codex/config.toml : cassé".to_string())];
        let errors = vec![
            (Harness::Codex, ".codex/config.toml : cassé".to_string()),
            (
                Harness::Grok,
                ".grok/config.toml : écriture impossible (refusée)".to_string(),
            ),
        ];
        assert_eq!(
            visible_errors(&errors, &unreadable),
            vec![".grok/config.toml : écriture impossible (refusée)".to_string()]
        );
        // Un refus de geste attribué au premier harnais, lui-même illisible, n'est pas le texte du bandeau : montré.
        let refusal = vec![(
            Harness::Codex,
            "serveur `a` : définitions différentes, choisis une version avant de propager"
                .to_string(),
        )];
        assert_eq!(visible_errors(&refusal, &unreadable).len(), 1);
        assert!(visible_errors(&[], &unreadable).is_empty());
    }

    fn version(command: &str, harnesses: &[Harness]) -> Version {
        Version {
            core: json!({"type": "stdio", "command": command}),
            harnesses: harnesses.to_vec(),
        }
    }

    fn divergent_row(versions: Vec<Version>) -> ServerRow {
        row("a", RowState::Divergent { versions })
    }

    #[test]
    fn an_open_row_survives_a_reread_only_while_it_still_shows_the_truth() {
        let single = row("a", RowState::Complete);
        let opened = Opened::of(&single);
        // Rien n'a bougé : la ligne reste, éditée ou non.
        assert!(keeps_open_row(&opened, Some(&single), false));
        assert!(keeps_open_row(&opened, Some(&single), true));
        // Un agent a changé le bloc : intacte, la ligne se replie (« Enregistrer » le reviendrait en arrière) ;
        // éditée, la saisie reste.
        let mut changed = single.clone();
        changed.core = json!({"type": "stdio", "command": "other"});
        assert!(!keeps_open_row(&opened, Some(&changed), false));
        assert!(keeps_open_row(&opened, Some(&changed), true));
        // Le serveur a disparu ou ne s'écrit plus.
        assert!(!keeps_open_row(&opened, None, true));
        let mut unsupported = single.clone();
        unsupported.supported = false;
        assert!(!keeps_open_row(&opened, Some(&unsupported), true));
        // La forme a changé : les éditeurs ouverts ne correspondent plus.
        let now_divergent = divergent_row(vec![
            version("a", &[Harness::Claude]),
            version("b", &[Harness::Codex]),
        ]);
        assert!(!keeps_open_row(&opened, Some(&now_divergent), true));
    }

    #[test]
    fn a_divergent_row_closes_when_its_boxes_no_longer_match_the_versions() {
        let versions = vec![
            version("a", &[Harness::Claude]),
            version("b", &[Harness::Codex]),
        ];
        let row = divergent_row(versions.clone());
        let opened = Opened::of(&row);
        assert!(opened.divergent);
        assert_eq!(opened.cores.len(), 2);
        assert!(keeps_open_row(&opened, Some(&row), false));
        // Les versions ont changé d'ordre : la boîte n° 0 n'est plus la version n° 0.
        let reordered = divergent_row(vec![versions[1].clone(), versions[0].clone()]);
        assert!(!keeps_open_row(&opened, Some(&reordered), false));
        // Une définition a bougé : fermée, même si l'on y avait cliqué (les boîtes ne s'éditent pas).
        let moved = divergent_row(vec![
            version("a", &[Harness::Claude]),
            version("c", &[Harness::Codex]),
        ]);
        assert!(!keeps_open_row(&opened, Some(&moved), true));
        // Une version de plus : fermée.
        let more = divergent_row(vec![
            versions[0].clone(),
            versions[1].clone(),
            version("z", &[Harness::Grok]),
        ]);
        assert!(!keeps_open_row(&opened, Some(&more), false));
        // Les harnais qui portent une version changent sans que sa définition change : les boîtes restent justes.
        let regrouped = divergent_row(vec![
            version("a", &[Harness::Claude, Harness::Grok]),
            version("b", &[Harness::Codex]),
        ]);
        assert!(keeps_open_row(&opened, Some(&regrouped), false));
    }

    #[test]
    fn the_save_button_needs_a_block_the_five_files_can_hold() {
        let ready = |draft: &str| matches!(project_save_state(None, draft), SaveState::Ready(_));
        // Le modèle intact ne s'écrit pas : `command` est vide.
        assert!(!ready(NEW_SERVER_TEMPLATE));
        let SaveState::Invalid(message) = project_save_state(None, NEW_SERVER_TEMPLATE) else {
            panic!("the template must be invalid");
        };
        assert!(message.contains("command"), "{message}");
        // Un type que la forme commune ne porte pas : un seul message.
        let SaveState::Invalid(message) = project_save_state(None, r#"{"type":"sse","url":"x"}"#)
        else {
            panic!("sse must be invalid");
        };
        assert!(message.contains("sse"), "{message}");
        // Un stdio sans `command`, un objet sans type, un `url` vide.
        assert!(matches!(
            project_save_state(None, r#"{"type":"stdio"}"#),
            SaveState::Invalid(_)
        ));
        assert!(matches!(
            project_save_state(None, "{}"),
            SaveState::Invalid(_)
        ));
        assert!(!ready(r#"{"type":"http","url":"  "}"#));
        assert!(!ready(r#"{"type":"stdio","command":"   "}"#));
        // Un JSON cassé garde son message de Personnaliser.
        assert!(matches!(
            project_save_state(None, "{"),
            SaveState::Invalid(message) if message.starts_with("JSON invalide")
        ));
        // Des blocs valides s'écrivent.
        assert!(ready(r#"{"type":"stdio","command":"bun","args":["a.ts"]}"#));
        assert!(ready(r#"{"type":"http","url":"https://x/mcp"}"#));
        // Un bloc valide identique au bloc lu reste inactif (« Unchanged »).
        let original = json!({"type": "stdio", "command": "bun"});
        assert_eq!(
            project_save_state(Some(&original), r#"{"command":"bun","type":"stdio"}"#),
            SaveState::Unchanged
        );
    }

    #[test]
    fn a_new_server_never_overwrites_a_name_taken_since_the_last_read() {
        let project = temp_dir("taken");
        let mcp = project.join(".mcp.json");
        let original = r#"{"mcpServers":{"hello":{"type":"stdio","command":"bun"}}}"#;
        fs::write(&mcp, original).unwrap();
        let op = McpOp::Save {
            name: "hello".to_string(),
            core: json!({"type": "stdio", "command": "other"}),
        };
        assert_eq!(
            run_op(&project, &op, true, None),
            vec!["Ce nom est déjà pris.".to_string()]
        );
        assert_eq!(fs::read_to_string(&mcp).unwrap(), original);
        // Une modification (pas un serveur neuf) écrit bien.
        assert!(run_op(&project, &op, false, None).is_empty());
        assert!(fs::read_to_string(&mcp).unwrap().contains("other"));
    }

    /// Le texte des cinq fichiers de config, par harnais.
    fn five_files(project: &Path) -> Vec<(Harness, Option<String>)> {
        Harness::ALL
            .iter()
            .map(|&harness| {
                (
                    harness,
                    fs::read_to_string(project.join(harness.mcp_file())).ok(),
                )
            })
            .collect()
    }

    fn stdio_core(command: &str) -> Value {
        json!({"type": "stdio", "command": command, "args": [], "env": {}})
    }

    #[test]
    fn a_save_is_refused_when_the_files_no_longer_say_what_the_row_showed() {
        let project = temp_dir("stale-save");
        let save = |command: &str| McpOp::Save {
            name: "srv".to_string(),
            core: stdio_core(command),
        };
        assert!(apply(&project, &save("shown")).is_empty());
        let opened = Opened::of(&load(&project).rows[0]);

        // Un agent réécrit un des fichiers pendant que la ligne est dépliée.
        let mcp = project.join(".mcp.json");
        fs::write(
            &mcp,
            fs::read_to_string(&mcp).unwrap().replace("shown", "third"),
        )
        .unwrap();
        let before = five_files(&project);

        let errors = run_op(&project, &save("typed"), false, Some(&opened));
        assert_eq!(errors, vec![FILES_CHANGED.to_string()]);
        assert_eq!(five_files(&project), before);

        // Sans changement sous la ligne, l'enregistrement part.
        let fresh = Opened::of(&load(&project).rows[0]);
        // La ligne diverge maintenant : « Garder celle-ci » est la bonne action, sur ce qui était montré.
        assert!(fresh.divergent);
        assert!(run_op(&project, &save("third"), false, Some(&fresh)).is_empty());
        assert!(!Opened::of(&load(&project).rows[0]).divergent);
    }

    #[test]
    fn keeping_a_version_is_refused_when_a_third_value_appeared_under_the_row() {
        let project = temp_dir("stale-keep");
        let save = |command: &str| McpOp::Save {
            name: "srv".to_string(),
            core: stdio_core(command),
        };
        assert!(apply(&project, &save("first")).is_empty());
        let mcp = project.join(".mcp.json");
        fs::write(
            &mcp,
            fs::read_to_string(&mcp).unwrap().replace("first", "second"),
        )
        .unwrap();
        let opened = Opened::of(&load(&project).rows[0]);
        assert!(opened.divergent);

        // Un fichier prend une troisième valeur depuis l'ouverture.
        let grok = project.join(".grok/config.toml");
        fs::write(
            &grok,
            fs::read_to_string(&grok).unwrap().replace("first", "third"),
        )
        .unwrap();
        let before = five_files(&project);

        let keep = save("second");
        assert_eq!(
            run_op(&project, &keep, false, Some(&opened)),
            vec![FILES_CHANGED.to_string()]
        );
        assert_eq!(five_files(&project), before);
    }

    #[test]
    fn a_gesture_leaves_an_unreadable_file_alone_and_does_not_report_it_under_the_row() {
        let project = temp_dir("unreadable");
        fs::create_dir_all(project.join(".codex")).unwrap();
        let broken = "[mcp_servers\ncassé";
        fs::write(project.join(".codex").join("config.toml"), broken).unwrap();
        let op = McpOp::Save {
            name: "fresh".to_string(),
            core: json!({"type": "stdio", "command": "bun", "args": [], "env": {}}),
        };
        let errors = run_op(&project, &op, true, None);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            fs::read_to_string(project.join(".codex").join("config.toml")).unwrap(),
            broken
        );
        assert!(
            fs::read_to_string(project.join(".mcp.json"))
                .unwrap()
                .contains("fresh")
        );
        // Le bandeau rouge, lui, nomme le fichier.
        let unreadable = load(&project).unreadable;
        assert_eq!(unreadable.len(), 1);
        assert!(unreadable_text(unreadable[0].0, &unreadable[0].1).contains("illisible"));
    }
}
