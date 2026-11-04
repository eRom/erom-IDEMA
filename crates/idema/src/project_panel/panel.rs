//! Le volet Projet : une entité qui montre, sous une rangée d'onglets, ce que le projet de contexte porte (instructions,
//! compétences, connecteurs). Elle ne porte que la colle GPUI (focus, éditeur d'`AGENTS.md`, gestes) ; le calcul de la
//! notice est dans `instructions_view.rs`, la fusion dans `instructions.rs`. Le rendu se vérifie dans la VM.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton, MouseDownEvent,
    Pixels, Point, PromptButton, PromptLevel, SharedString, Subscription, Window, div, prelude::*,
};

use super::banner::{banner_button, error_banner, warning_banner_with_reason};
use super::instructions::{self, MergeError};
use super::instructions_view::{Notice, NoticeAction, instructions_notice};
use super::mcp_view::ConnectorsTab;
use super::skills_view::SkillsTab;
use crate::assets::Icon;
use crate::files::document::{Opened, Saved, read_document, save_unless_changed};
use crate::files::gestures::EditorGesture;
use crate::files::panel::home_dir;
use crate::files::tabs::fingerprint;
use crate::hud;
use crate::scratchpad::panel::{CopyMode, NoteOptions, ScratchpadEvent, ScratchpadPanel};
use crate::settings::controls::{ButtonKind, action_button};
use crate::terminal_panel::{BODY_MARGIN_SIDES, panel_header, panel_shell};
use crate::theme::{
    HOVER, PROJECT_TAB_ROW, SELECTED, SEPARATOR, TEXT_FULL, TEXT_SECONDARY, Theme, white,
};
use crate::workspace::Workspace;

const AGENTS_FILE: &str = "AGENTS.md";
const LOCAL_FILE: &str = "CLAUDE.local.md";
const MISSING_PROJECT: &str = "Le dossier du projet n'existe plus.";
const NO_AGENTS: &str = "Ce projet n'a pas encore d'AGENTS.md.";
const CREATE_AGENTS: &str = "Créer AGENTS.md";
const MERGE_CONFIRM: &str = "Les règles de CLAUDE.md qui manquent à AGENTS.md y seront ajoutées, puis CLAUDE.md partira à la Corbeille.";
/// La fusion refusée sur le dossier personnel (à ratifier à la porte : texte posé par la revue finale).
const HOME_NOT_MERGED: &str = "Le dossier personnel n'est pas un projet : rien n'a été fusionné.";
const MERGE_CHANGED: &str = "AGENTS.md ou CLAUDE.md a changé sur le disque, rien n'a été fusionné.";
const MERGE_NOT_SAVED: &str = "AGENTS.md n'a pas pu être enregistré : rien n'a été fusionné.";
const LOCAL_CHANGED: &str = "CLAUDE.local.md a changé sur le disque, rien n'a été modifié.";

/// Les onglets du volet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectTab {
    Instructions,
    Skills,
    Connectors,
}

impl ProjectTab {
    /// Le nom du journal (`project tab=<instructions|skills|connectors>`).
    pub fn log_name(self) -> &'static str {
        match self {
            ProjectTab::Instructions => "instructions",
            ProjectTab::Skills => "skills",
            ProjectTab::Connectors => "connectors",
        }
    }
}

/// Les onglets, dans l'ordre de la rangée, avec leur libellé (spec §3.1).
pub const TABS: [(ProjectTab, &str); 3] = [
    (ProjectTab::Instructions, "Instructions"),
    (ProjectTab::Skills, "Compétences"),
    (ProjectTab::Connectors, "Connecteurs"),
];

/// Ce que le volet apprend à `Workspace`.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectEvent {
    /// Le volet (ou un de ses champs, éditeur compris) a gagné ou perdu le focus : le clavier change peut-être de côté.
    FocusChanged,
    /// « Copier vers l'agent » depuis l'éditeur d'`AGENTS.md` : la référence du fichier, avec son espace de fin.
    CopyToAgent(String),
    /// Un clic droit dans l'éditeur : `Workspace` ouvre le menu.
    EditorMenu {
        position: Point<Pixels>,
        has_selection: bool,
    },
}

/// Là où la saisie va quand un onglet est choisi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusTarget {
    /// L'éditeur d'`AGENTS.md`.
    Editor,
    SkillsField,
    ConnectorsBody,
    /// La racine du volet : Instructions sans `AGENTS.md` (il n'y a pas d'éditeur à servir).
    Root,
}

/// Le focus de `tab` : l'éditeur d'`AGENTS.md` seulement sur Instructions et quand il existe, le champ sur Compétences,
/// le corps de l'onglet sur Connecteurs (qui n'a pas d'éditeur d'`AGENTS.md`).
fn focus_target(tab: ProjectTab, has_editor: bool) -> FocusTarget {
    match tab {
        ProjectTab::Instructions if has_editor => FocusTarget::Editor,
        ProjectTab::Instructions => FocusTarget::Root,
        ProjectTab::Skills => FocusTarget::SkillsField,
        ProjectTab::Connectors => FocusTarget::ConnectorsBody,
    }
}

/// L'état pur du volet : le projet suivi et l'onglet choisi. L'onglet survit au changement de projet (spec §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabState {
    project: Option<PathBuf>,
    tab: ProjectTab,
}

impl TabState {
    pub fn new() -> Self {
        Self {
            project: None,
            tab: ProjectTab::Instructions,
        }
    }

    /// Suit `project` ; l'onglet ne bouge pas. Rend vrai quand le projet a changé.
    pub fn show_project(&mut self, project: &Path) -> bool {
        if self.project.as_deref() == Some(project) {
            return false;
        }
        self.project = Some(project.to_path_buf());
        true
    }

    /// Choisit un onglet. Rend vrai quand il change.
    pub fn select(&mut self, tab: ProjectTab) -> bool {
        let changed = self.tab != tab;
        self.tab = tab;
        changed
    }
}

impl Default for TabState {
    fn default() -> Self {
        Self::new()
    }
}

/// L'éditeur d'`AGENTS.md` d'un projet, et l'abonnement qui relaie ses événements.
struct AgentsEditor {
    panel: Entity<ScratchpadPanel>,
    _subscription: Subscription,
}

/// Ce que le disque dit du projet affiché, recalculé par `refresh`.
#[derive(Default)]
struct Snapshot {
    /// Le dossier du projet n'existe plus.
    missing: bool,
    /// Pourquoi les fichiers d'instructions n'ont pas pu être lus.
    read_error: Option<String>,
    notice: Option<Notice>,
}

pub struct ProjectPanel {
    state: TabState,
    /// Un éditeur d'`AGENTS.md` par projet qui en a un : un projet quitté garde son texte et sa sauvegarde en attente.
    editors: HashMap<PathBuf, AgentsEditor>,
    snapshot: Snapshot,
    /// L'onglet Compétences : une seule entité, qui suit le projet affiché.
    skills: Entity<SkillsTab>,
    /// L'onglet Connecteurs : une seule entité, qui suit le projet affiché.
    connectors: Entity<ConnectorsTab>,
    /// L'erreur du dernier geste (fusion, import, création) ; elle tombe au geste suivant ou au changement de projet.
    message: Option<String>,
    /// La racine du volet : `contains_focused` en est la mesure, tout champ du volet en descend.
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ProjectEvent> for ProjectPanel {}

impl ProjectPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus_in(&focus_handle, window, |_, _, cx| {
                cx.emit(ProjectEvent::FocusChanged);
            }),
            cx.on_focus_out(&focus_handle, window, |_, _, _, cx| {
                cx.emit(ProjectEvent::FocusChanged);
            }),
        ];
        let skills = cx.new(|cx| SkillsTab::new(window, cx));
        let connectors = cx.new(ConnectorsTab::new);
        Self {
            state: TabState::new(),
            editors: HashMap::new(),
            snapshot: Snapshot::default(),
            skills,
            connectors,
            message: None,
            focus_handle,
            _subscriptions: subscriptions,
        }
    }

    /// Le focus est-il dans le volet (lui-même ou un de ses champs) ?
    pub fn contains_focused(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    /// Le volet suit `project` (le projet de contexte). L'onglet choisi reste choisi ; la notice est recalculée.
    pub fn show_project(&mut self, project: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let changed = self.state.show_project(project);
        if changed {
            self.message = None;
        }
        self.refresh(window, cx);
        // Un changement de session dans le même projet ne relance pas le CLI : la liste se relit à l'ouverture du volet
        // (`opened`), à l'ouverture de l'onglet et après chaque appel.
        if changed {
            self.refresh_skills(cx);
            self.refresh_connectors(cx);
        }
        cx.notify();
    }

    /// Le volet vient de s'ouvrir : les onglets Compétences et Connecteurs relisent le disque, comme la notice des Instructions (spec §3.3,
    /// « lecture à l'ouverture du volet »). Appelée après `show_project`, qui a déjà lancé la lecture d'un projet nouveau.
    pub fn opened(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh(window, cx);
        self.refresh_skills(cx);
        self.refresh_connectors(cx);
        cx.notify();
    }

    /// Écrit ce qui attend dans les éditeurs (`⌘Q`, fermeture de la fenêtre).
    pub fn flush(&mut self, reason: &str, cx: &mut App) {
        for entry in self.editors.values() {
            entry.panel.update(cx, |panel, cx| {
                panel.flush(reason, cx);
            });
        }
    }

    /// Exécute un geste du menu de l'éditeur d'`AGENTS.md` : l'éditeur le reçoit.
    pub fn run_editor_gesture(
        &mut self,
        gesture: EditorGesture,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.visible_editor() {
            panel.update(cx, |panel, cx| {
                panel.run_editor_gesture(gesture, window, cx)
            });
        }
    }

    /// L'éditeur d'`AGENTS.md` du projet affiché, quand il y en a un.
    fn current_editor(&self) -> Option<Entity<ScratchpadPanel>> {
        let project = self.state.project.as_ref()?;
        Some(self.editors.get(project)?.panel.clone())
    }

    /// L'éditeur d'`AGENTS.md` quand l'onglet Instructions est celui qu'on voit : sur un autre onglet, il n'est pas dessiné
    /// et ne doit ni prendre le focus ni recevoir un geste de menu.
    fn visible_editor(&self) -> Option<Entity<ScratchpadPanel>> {
        let editor = self.current_editor();
        (focus_target(self.state.tab, editor.is_some()) == FocusTarget::Editor)
            .then_some(editor)
            .flatten()
    }

    /// Là où la saisie va, selon l'onglet (voir `focus_target`).
    fn body_focus(&self, cx: &App) -> FocusHandle {
        match focus_target(self.state.tab, self.current_editor().is_some()) {
            FocusTarget::Editor => match self.visible_editor() {
                Some(panel) => panel.read(cx).focus_handle(cx),
                None => self.focus_handle.clone(),
            },
            FocusTarget::SkillsField => self.skills.read(cx).field_focus(cx),
            FocusTarget::ConnectorsBody => self.connectors.read(cx).body_focus(),
            FocusTarget::Root => self.focus_handle.clone(),
        }
    }

    /// Donne le focus au corps de l'onglet choisi.
    pub fn focus_body(&self, window: &mut Window, cx: &mut App) {
        let handle = self.body_focus(cx);
        window.focus(&handle, cx);
    }

    /// Relit le disque : le dossier du projet, les fichiers d'instructions, la notice, et crée l'éditeur d'`AGENTS.md`
    /// quand le fichier existe. Appelée à l'ouverture de l'onglet, au changement de projet et après chaque geste.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.state.project.clone() else {
            return;
        };
        self.snapshot = Snapshot::default();
        if !project.is_dir() {
            self.snapshot.missing = true;
            return;
        }
        match instructions::read(&project) {
            Ok(files) => {
                self.snapshot.notice = instructions_notice(&files, &project, home_dir().as_deref());
            }
            Err(error) => {
                hud::log_event(&format!("instructions read failed reason={error}"));
                self.snapshot.read_error = Some(format!(
                    "Lecture impossible : {}",
                    instructions::io_error_text(&error)
                ));
            }
        }
        if !self.editors.contains_key(&project) && project.join(AGENTS_FILE).exists() {
            self.open_editor(&project, window, cx);
        }
    }

    /// Relit les skills du projet affiché quand l'onglet Compétences est celui qu'on voit (le CLI tourne hors du fil
    /// d'interface). Rien si le dossier du projet n'existe plus.
    fn refresh_skills(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.state.project.clone() else {
            return;
        };
        if self.state.tab == ProjectTab::Skills && project.is_dir() {
            self.skills
                .update(cx, |skills, cx| skills.refresh(&project, cx));
        }
    }

    /// Relit les serveurs MCP du projet affiché quand l'onglet Connecteurs est celui qu'on voit (la lecture se fait
    /// hors du fil d'interface). Rien si le dossier du projet n'existe plus.
    fn refresh_connectors(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.state.project.clone() else {
            return;
        };
        if self.state.tab == ProjectTab::Connectors && project.is_dir() {
            self.connectors
                .update(cx, |connectors, cx| connectors.refresh(&project, cx));
        }
    }

    /// Crée l'éditeur d'`AGENTS.md` de `project` (le fichier existe) et relaie ses événements.
    fn open_editor(&mut self, project: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let options = NoteOptions {
            label: AGENTS_FILE,
            log_tag: "instructions",
            create_parent: false,
            copy: CopyMode::Reference {
                root: project.to_path_buf(),
            },
        };
        let path = project.join(AGENTS_FILE);
        let panel = cx.new(|cx| ScratchpadPanel::with_options(path, options, window, cx));
        let subscription = cx.subscribe_in(
            &panel,
            window,
            |_, _, event: &ScratchpadEvent, _, cx| match event {
                // La racine du volet voit déjà le focus de l'éditeur, qui en descend.
                ScratchpadEvent::FocusChanged => {}
                ScratchpadEvent::CopyToAgent(text) => {
                    cx.emit(ProjectEvent::CopyToAgent(text.clone()));
                }
                ScratchpadEvent::EditorMenu {
                    position,
                    has_selection,
                } => cx.emit(ProjectEvent::EditorMenu {
                    position: *position,
                    has_selection: *has_selection,
                }),
            },
        );
        self.editors.insert(
            project.to_path_buf(),
            AgentsEditor {
                panel,
                _subscription: subscription,
            },
        );
    }

    /// Choisit un onglet (un clic sur la rangée).
    fn select_tab(&mut self, tab: ProjectTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.select(tab) {
            hud::log_event(&format!("project tab={}", tab.log_name()));
        }
        self.message = None;
        // La notice se recalcule à chaque ouverture de l'onglet, même celui qui était déjà choisi.
        self.refresh(window, cx);
        self.refresh_skills(cx);
        self.refresh_connectors(cx);
        // Le clavier suit l'onglet : l'éditeur disparu ne le garde pas.
        self.focus_body(window, cx);
        cx.notify();
    }

    /// « Créer AGENTS.md » : crée le fichier vide (jamais par-dessus un fichier qui existe), ouvre l'éditeur et lui
    /// donne le focus.
    fn create_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.state.project.clone() else {
            return;
        };
        self.message = None;
        // Jamais de création du dossier du projet : s'il manque, le geste le dit.
        let created = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(project.join(AGENTS_FILE));
        match created {
            Ok(_) => hud::log_event("instructions created"),
            // Un agent l'a créé entre-temps : on ouvre le sien, sans y toucher.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                hud::log_event(&format!("instructions create failed reason={error}"));
                self.message = Some(if error.kind() == io::ErrorKind::NotFound {
                    MISSING_PROJECT.to_owned()
                } else {
                    format!("{AGENTS_FILE} n'a pas pu être créé.")
                });
            }
        }
        self.refresh(window, cx);
        self.focus_body(window, cx);
        cx.notify();
    }

    /// « Fusionner dans AGENTS.md » : la confirmation, puis la fusion.
    fn confirm_merge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Le projet visé est celui de la notice cliquée : la confirmation native laisse le temps de changer de session.
        let Some(project) = self.state.project.clone() else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            MERGE_CONFIRM,
            None,
            &[
                PromptButton::new("Fusionner"),
                PromptButton::cancel("Annuler"),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, window, cx| this.run_merge(&project, window, cx));
            }
        })
        .detach();
    }

    /// La fusion de `project` : l'éditeur écrit d'abord ce qu'il porte (un refus arrête tout), puis `apply_merge` met
    /// `CLAUDE.md` à la Corbeille, et l'éditeur relit le disque. Si le volet montre un autre projet que celui qu'on a
    /// confirmé (changement de session pendant la confirmation), rien ne part.
    fn run_merge(&mut self, project: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.project.as_deref() != Some(project) {
            hud::log_event("instructions merge skipped reason=project-changed");
            return;
        }
        let project = project.to_path_buf();
        self.message = None;
        if let Some(panel) = self.current_editor() {
            let saved = panel.update(cx, |panel, cx| panel.flush("merge", cx));
            if !saved {
                // Le bandeau du Scratchpad dit la cause quand il y en a une (droits, disque plein).
                self.message = Some(MERGE_NOT_SAVED.to_owned());
                cx.notify();
                return;
            }
        }
        let merged =
            instructions::apply_merge(&project, &mut |path| crate::files::ops::move_to_trash(path));
        match merged {
            Ok(trashed) => hud::log_event(&format!("instructions merged trashed={trashed}")),
            Err(error) => {
                hud::log_event(&format!("instructions merge failed reason={error}"));
                self.message = Some(merge_error_message(&error));
            }
        }
        self.refresh(window, cx);
        // L'éditeur reprend le disque tout de suite : `AGENTS.md` vient peut-être d'être complété ou créé.
        if let Some(panel) = self.current_editor() {
            panel.update(cx, |panel, cx| panel.reload_from_disk(window, cx));
        }
        cx.notify();
    }

    /// « Importer AGENTS.md dedans » : `@AGENTS.md` en tête de `CLAUDE.local.md`, sans écraser un fichier qui a changé.
    fn import_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.state.project.clone() else {
            return;
        };
        self.message = None;
        match import_agents_into_local(&project) {
            Ok(Saved::Written) => hud::log_event("instructions imported file=CLAUDE.local.md"),
            Ok(Saved::DiskChanged) => self.message = Some(LOCAL_CHANGED.to_owned()),
            Err(error) => {
                hud::log_event(&format!("instructions import failed reason={error}"));
                self.message = Some(format!("{LOCAL_FILE} n'a pas pu être modifié."));
            }
        }
        self.refresh(window, cx);
        cx.notify();
    }

    /// La rangée d'onglets sous l'en-tête : haute de `PROJECT_TAB_ROW`, retrait de 10, séparateur bas ; onglets de 28,
    /// rayon 6, 12,5 px, retrait 10, écart 2 (maquette `settings-window.html`, variante B).
    fn render_tab_row(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(theme.px(PROJECT_TAB_ROW))
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.px(2.))
            .px(theme.px(10.))
            .border_b_1()
            .border_color(white(SEPARATOR))
            .children(TABS.iter().map(|(tab, label)| {
                let tab = *tab;
                let active = self.state.tab == tab;
                div()
                    .id(("project-tab", tab as usize))
                    .h(theme.px(28.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(theme.px(10.))
                    .rounded(theme.px(6.))
                    .text_size(theme.text(12.5))
                    .cursor_pointer()
                    // Le survol d'un onglet inactif est celui de la rangée d'onglets du volet Browser (`chrome.rs`, `HOVER`).
                    .when(active, |item| {
                        item.bg(white(SELECTED)).text_color(white(TEXT_FULL))
                    })
                    .when(!active, |item| {
                        item.text_color(white(TEXT_SECONDARY))
                            .hover(|style| style.bg(white(HOVER)))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            // La racine du volet reprendrait le focus à la remontée de l'appui : `select_tab` le
                            // donne au corps de l'onglet.
                            window.prevent_default();
                            this.select_tab(tab, window, cx);
                        }),
                    )
                    .child(*label)
            }))
    }

    /// Le texte centré d'un état sans contenu (projet absent).
    fn render_centered(&self, theme: &Theme, text: &'static str) -> gpui::Div {
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_size(theme.text(13.))
            .text_color(white(TEXT_SECONDARY))
            .child(text)
    }

    /// L'onglet Instructions : l'erreur du dernier geste, la notice, puis l'éditeur d'`AGENTS.md` ou l'état vide.
    fn render_instructions(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(theme.px(8.))
            .px(theme.px(BODY_MARGIN_SIDES))
            .pt(theme.px(8.))
            .pb(theme.px(12.));
        if self.snapshot.missing {
            return self.render_centered(theme, MISSING_PROJECT);
        }
        let error = self
            .message
            .clone()
            .or_else(|| self.snapshot.read_error.clone())
            .map(|text| error_banner(SharedString::from(text), None, Vec::new(), theme));
        let notice = self
            .snapshot
            .notice
            .map(|notice| self.render_notice(&notice, theme, cx));
        let content = match self.current_editor() {
            Some(panel) => div().flex_1().min_h_0().child(panel),
            None => self.render_no_agents(theme, cx),
        };
        body.children(error).children(notice).child(content)
    }

    /// La notice orange : son texte, la raison quand le bouton est grisé, et le bouton.
    fn render_notice(&self, notice: &Notice, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let id = match notice.action {
            NoticeAction::Merge => "instructions-merge",
            NoticeAction::ImportAgents => "instructions-import",
        };
        let action = notice.action;
        let button = banner_button(id, notice.button, notice.enabled, theme).when(
            notice.enabled,
            |button| {
                button.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        match action {
                            NoticeAction::Merge => this.confirm_merge(window, cx),
                            NoticeAction::ImportAgents => this.import_agents(window, cx),
                        }
                    }),
                )
            },
        );
        warning_banner_with_reason(
            SharedString::from(notice.text),
            notice.reason.map(SharedString::from),
            vec![button.into_any_element()],
            theme,
        )
    }

    /// L'état vide sans `AGENTS.md` : un texte et le bouton qui crée le fichier.
    fn render_no_agents(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let create = action_button(
            "instructions-create",
            CREATE_AGENTS,
            None,
            ButtonKind::Primary,
            true,
            theme,
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.create_agents(window, cx);
            }),
        );
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(theme.px(12.))
            .child(
                div()
                    .text_size(theme.text(13.))
                    .text_color(white(TEXT_SECONDARY))
                    .child(NO_AGENTS),
            )
            .child(create)
    }
}

/// Ce que le bandeau d'erreur dit d'une fusion refusée ou interrompue. Une erreur d'entrée-sortie peut tomber après
/// l'écriture d'`AGENTS.md` et après la mise à la Corbeille d'un premier `CLAUDE.md` : le texte ne prétend pas que rien
/// n'a bougé.
fn merge_error_message(error: &MergeError) -> String {
    match error {
        MergeError::Changed => MERGE_CHANGED.to_owned(),
        MergeError::Blocked(blocked) => blocked.message().to_owned(),
        MergeError::UserMemory => {
            "Le CLAUDE.md de ~/.claude est votre mémoire globale : il n'est jamais fusionné."
                .to_owned()
        }
        MergeError::Home => HOME_NOT_MERGED.to_owned(),
        MergeError::GlobalConfig(file) => super::global_config_text(file),
        MergeError::Io(error) => format!(
            "La fusion s'est arrêtée en route : vérifiez AGENTS.md et CLAUDE.md. ({})",
            instructions::io_error_text(error)
        ),
    }
}

/// Écrit `@AGENTS.md` en tête de `<projet>/CLAUDE.local.md`, d'après le disque tel qu'il est maintenant : l'écriture
/// refuse de remplacer un fichier qui a changé depuis cette lecture. Un fichier qui importe déjà `AGENTS.md` n'est pas
/// touché.
fn import_agents_into_local(project: &Path) -> io::Result<Saved> {
    let path = project.join(LOCAL_FILE);
    // L'empreinte avant le contenu : un changement entre les deux est refusé à l'écriture.
    let print = fingerprint(&path)?;
    let Opened::Text(loaded) = read_document(&path)? else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CLAUDE.local.md is not editable text",
        ));
    };
    if !instructions::local_needs_import(&loaded.text) {
        return Ok(Saved::Written);
    }
    let text = instructions::with_agents_import(&loaded.text);
    save_unless_changed(&path, &text, loaded.line_ending, print)
}

impl Focusable for ProjectPanel {
    /// Là où la saisie doit aller quand `Workspace` rend le focus au volet : le corps de l'onglet choisi.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.body_focus(cx)
    }
}

impl Render for ProjectPanel {
    /// Le corps sous l'en-tête : la rangée d'onglets, puis l'onglet choisi. La racine porte le focus du volet.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        let body = match self.state.tab {
            ProjectTab::Instructions => self.render_instructions(&theme, cx),
            ProjectTab::Skills if self.snapshot.missing => {
                self.render_centered(&theme, MISSING_PROJECT)
            }
            ProjectTab::Skills => div().flex_1().min_h_0().child(self.skills.clone()),
            ProjectTab::Connectors if self.snapshot.missing => {
                self.render_centered(&theme, MISSING_PROJECT)
            }
            ProjectTab::Connectors => div().flex_1().min_h_0().child(self.connectors.clone()),
        };
        div()
            .id("project-body")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_tab_row(&theme, cx))
            .child(body)
    }
}

/// Le volet : en-tête commun aux volets droits (pastille « Projet » et nom du projet, agrandir, fermer) puis le corps, qui
/// est l'entité `ProjectPanel`. `project` est le nom affiché du projet ; `header_inset` est le retrait gauche de l'en-tête.
#[allow(clippy::too_many_arguments)]
pub fn render_panel(
    project: String,
    panel: Entity<ProjectPanel>,
    maximized: bool,
    header_inset: Pixels,
    width: Pixels,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> gpui::Stateful<gpui::Div> {
    let header = panel_header(
        theme,
        Icon::Settings,
        "Projet",
        project,
        ("project-maximize", "project-close"),
        maximized,
        header_inset,
        |this, window, cx| this.focus_project(window, cx),
        |this, window, cx| this.toggle_project_maximized(window, cx),
        |this, window, cx| this.close_project(window, cx),
        cx,
    );
    panel_shell("project-panel", maximized, width)
        .child(header)
        .child(div().flex_1().min_h_0().child(panel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chosen_tab_survives_a_project_change() {
        let mut state = TabState::new();
        assert_eq!(state.tab, ProjectTab::Instructions);
        assert!(state.select(ProjectTab::Skills));
        assert!(state.show_project(Path::new("/work/a")));
        assert_eq!(state.tab, ProjectTab::Skills);
        assert!(state.show_project(Path::new("/work/b")));
        assert_eq!(state.tab, ProjectTab::Skills);
        // Le même projet n'est pas un changement.
        assert!(!state.show_project(Path::new("/work/b")));
    }

    #[test]
    fn the_body_focus_agrees_with_the_visible_editor() {
        // Seul l'onglet Instructions, avec un éditeur, envoie le focus à l'éditeur d'`AGENTS.md`.
        for (tab, _) in TABS {
            assert_eq!(
                focus_target(tab, true) == FocusTarget::Editor,
                tab == ProjectTab::Instructions,
                "{tab:?}"
            );
        }
        assert_eq!(
            focus_target(ProjectTab::Instructions, false),
            FocusTarget::Root
        );
        assert_eq!(
            focus_target(ProjectTab::Skills, true),
            FocusTarget::SkillsField
        );
        // Connecteurs : le corps de l'onglet, éditeur d'`AGENTS.md` ouvert ou non.
        assert_eq!(
            focus_target(ProjectTab::Connectors, true),
            FocusTarget::ConnectorsBody
        );
        assert_eq!(
            focus_target(ProjectTab::Connectors, false),
            FocusTarget::ConnectorsBody
        );
    }

    #[test]
    fn an_interrupted_merge_never_claims_that_nothing_moved() {
        let error = MergeError::Io(io::Error::other("trash refused"));
        let message = merge_error_message(&error);
        assert!(
            message.starts_with("La fusion s'est arrêtée en route"),
            "{message}"
        );
        assert!(
            message.contains("vérifiez AGENTS.md et CLAUDE.md"),
            "{message}"
        );
        // Le texte brut reste dans le journal : le volet ne montre jamais un message anglais.
        assert!(!message.contains("trash refused"), "{message}");
        assert!(message.ends_with("(erreur du système)"), "{message}");
        assert!(!message.contains("rien n'a"), "{message}");
        // Un fichier qui a bougé avant toute écriture garde, lui, le « rien n'a été fusionné » de la spec.
        assert_eq!(
            merge_error_message(&MergeError::Changed),
            "AGENTS.md ou CLAUDE.md a changé sur le disque, rien n'a été fusionné."
        );
    }

    #[test]
    fn io_errors_reach_the_ui_in_french_only() {
        let not_text = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                instructions::NotEditableText(PathBuf::from("/work/app/CLAUDE.md")),
            )
        };
        assert_eq!(
            instructions::io_error_text(&not_text()),
            "CLAUDE.md n'est pas un texte modifiable"
        );
        let cases = [
            (io::ErrorKind::NotFound, "fichier introuvable"),
            (io::ErrorKind::PermissionDenied, "accès refusé"),
            (io::ErrorKind::Other, "erreur du système"),
        ];
        for (kind, expected) in cases {
            let error = io::Error::new(kind, "an english message");
            assert_eq!(instructions::io_error_text(&error), expected);
        }
        // Un fichier qui n'est pas du texte, lu par le volet : aucune phrase anglaise n'arrive à l'écran.
        let dir = std::env::temp_dir().join(format!("idema-io-text-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), [0u8, 159, 146, 150]).unwrap();
        let error = instructions::read(&dir).unwrap_err();
        let message = format!(
            "Lecture impossible : {}",
            instructions::io_error_text(&error)
        );
        assert_eq!(
            message,
            "Lecture impossible : AGENTS.md n'est pas un texte modifiable"
        );
        let merge = merge_error_message(&MergeError::Io(not_text()));
        assert!(
            merge.contains("CLAUDE.md n'est pas un texte modifiable"),
            "{merge}"
        );
        assert!(!merge.contains("editable"), "{merge}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merging_the_home_folder_is_refused_in_french() {
        assert_eq!(
            merge_error_message(&MergeError::Home),
            "Le dossier personnel n'est pas un projet : rien n'a été fusionné."
        );
    }

    #[test]
    fn every_listed_tab_has_a_distinct_label_and_log_name() {
        let labels: std::collections::HashSet<_> = TABS.iter().map(|(_, label)| *label).collect();
        let names: std::collections::HashSet<_> =
            TABS.iter().map(|(tab, _)| tab.log_name()).collect();
        assert_eq!(labels.len(), TABS.len());
        assert_eq!(names.len(), TABS.len());
        assert_eq!(TABS[0], (ProjectTab::Instructions, "Instructions"));
        // « Compétences » vient juste après « Instructions », « Connecteurs » en dernier (spec §3.1).
        assert_eq!(TABS[1], (ProjectTab::Skills, "Compétences"));
        assert_eq!(TABS[2], (ProjectTab::Connectors, "Connecteurs"));
    }

    #[test]
    fn importing_agents_md_prepends_the_import_and_keeps_the_rest() {
        let dir = std::env::temp_dir().join(format!("idema-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let local = dir.join(LOCAL_FILE);
        std::fs::write(&local, "my notes\r\nmore\r\n").unwrap();
        assert!(matches!(import_agents_into_local(&dir), Ok(Saved::Written)));
        // L'import est en tête et la fin de ligne du fichier est gardée.
        assert_eq!(
            std::fs::read_to_string(&local).unwrap(),
            "@AGENTS.md\r\nmy notes\r\nmore\r\n"
        );
        // Un second import ne duplique pas la ligne.
        assert!(matches!(import_agents_into_local(&dir), Ok(Saved::Written)));
        assert_eq!(
            std::fs::read_to_string(&local).unwrap(),
            "@AGENTS.md\r\nmy notes\r\nmore\r\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn importing_agents_md_into_a_missing_file_is_an_error_not_a_creation() {
        let dir = std::env::temp_dir().join(format!("idema-import-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(import_agents_into_local(&dir).is_err());
        assert!(!dir.join(LOCAL_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
