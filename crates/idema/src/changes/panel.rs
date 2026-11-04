//! Le volet Changes (spec P3 §4.1 à §4.5) : le volet droit qui prend la place du volet Terminal ou du volet Fichiers.
//! Il lit le dépôt du projet de contexte, hors du fil d'interface, à l'ouverture puis à chaque lot de la surveillance
//! (qui voit aussi l'index et les références de Git), et montre le diff du fichier choisi. Une entité `ChangesPanel` par
//! projet, gardée par `Workspace` tant qu'IdemA tourne, sur le modèle de `FilesPanel`. Le modèle sans fenêtre est dans
//! `model.rs`, le diff dans `panel/diff_view.rs`.

use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use futures::StreamExt;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    MouseDownEvent, Pixels, Point, ScrollStrategy, Subscription, Task, UniformListScrollHandle,
    Window, actions, div, prelude::*, svg,
};
use gpui_component::input::{InputEvent, TextareaState};

use super::model::{self, LoadError};
use super::rows::{self, Row, RowKey};
use super::worktrees::{Target, TargetLine, list_targets, target_gone};
use super::{Change, Check, Snapshot, ops};
use crate::assets::Icon;
use crate::column::{self, ColumnDrag};
use crate::commit_message::CommitMessageSettings;
use crate::commit_message::client::CancelHandle;
use crate::commit_message::prompt::PromptStats;
use crate::errors::{Explained, alert, explain_git};
use crate::files::panel::home_dir;
use crate::files::watch::{GitDirs, ProjectWatch};
use crate::hud;
use crate::terminal_panel::{HEADER_HEIGHT, header_button, panel_shell};
use crate::theme::{
    CONTROL, FOCUS_TEXT, HOVER_STRONG, SEPARATOR, TEXT_HOVER, TEXT_LABEL, TEXT_MIN, TOGGLE_ACTIVE,
    Theme, color, focus, white,
};
use crate::workspace::{Workspace, drag_window};

mod diff_view;
mod footer;
mod list_view;
mod menu;

use diff_view::DiffView;
pub use menu::{ChangeTarget, TargetItem, changes_menu, diff_menu, target_menu};

actions!(
    changes,
    [
        ListUp,
        ListDown,
        ToggleCheck,
        Commit,
        GenerateMessage,
        CopyToAgent
    ]
);

/// Le contexte de touches du volet.
pub const KEY_CONTEXT: &str = "Changes";
/// Le contexte des lignes de la liste (pas du pied, qui est dessous dans la colonne, mais à côté) : `↑` `↓` et `espace` n'y
/// valent que là, jamais quand le focus est dans le diff ou dans le champ du message.
pub const LIST_KEY_CONTEXT: &str = "ChangesList";
/// Le contexte du diff du kit, sous celui du volet : `⌘⌥C` n'y vaut que là, jamais dans un pane (l'agent le garde).
const DIFF_KEY_CONTEXT: &str = "Changes > Diff";
/// Le contexte de la boîte du message de commit : `⌘↩` y commite, `espace` n'y coche rien.
pub const MESSAGE_KEY_CONTEXT: &str = "ChangesMessage";

/// Largeur par défaut de la colonne de la liste, son plancher, et celui du diff.
const LIST_WIDTH: f32 = 236.;
const LIST_MIN_WIDTH: f32 = 180.;
const DIFF_MIN_WIDTH: f32 = 200.;
/// L'indication du champ du message de commit.
const MESSAGE_PLACEHOLDER: &str = "Message de commit";

/// Le champ du kit posé sous la boîte du message : plus profond que la boîte seule, à la même profondeur que le contexte
/// `Input` du kit.
const MESSAGE_INPUT_CONTEXT: &str = "ChangesMessage > Input";

/// Les liaisons du volet : `↑` et `↓` déplacent la sélection de la liste quand elle a le focus, `espace` coche ou décoche
/// la ligne choisie. `⌘↩` commite depuis le champ du message : le kit y va à la ligne (`secondary-enter`, lié à `Input`
/// et pris avant tout contexte plus haut), la liaison du volet doit donc être au contexte `ChangesMessage > Input`, lié
/// après celles du kit (où la dernière liée gagne à profondeur égale, `main.rs`). `⌥⇥` génère le message depuis le champ ou
/// depuis la liste, liée de la même façon.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("up", ListUp, Some(LIST_KEY_CONTEXT)),
        KeyBinding::new("down", ListDown, Some(LIST_KEY_CONTEXT)),
        KeyBinding::new("space", ToggleCheck, Some(LIST_KEY_CONTEXT)),
        KeyBinding::new("cmd-enter", Commit, Some(MESSAGE_INPUT_CONTEXT)),
        KeyBinding::new("alt-tab", GenerateMessage, Some(MESSAGE_INPUT_CONTEXT)),
        KeyBinding::new("alt-tab", GenerateMessage, Some(LIST_KEY_CONTEXT)),
        KeyBinding::new("cmd-alt-c", CopyToAgent, Some(DIFF_KEY_CONTEXT)),
    ]
}

/// Ce que le volet apprend à `Workspace`.
#[derive(Debug, Clone, PartialEq)]
pub enum ChangesEvent {
    /// Le volet (ou l'un de ses descendants) a gagné ou perdu le focus : le clavier change peut-être de côté.
    FocusChanged,
    /// La poignée de la liste a été relâchée ou remise à zéro : la largeur voulue (`None` : celle par défaut) s'écrit
    /// dans `state.json`, préférence globale.
    ListWidthChanged(Option<f32>),
    /// Le bouton « liste » a masqué ou rendu la liste : l'état s'écrit dans `state.json`, préférence globale.
    ListHiddenChanged(bool),
    /// Double-clic sur un fichier de la liste, ou « Ouvrir dans Fichiers » : Fichiers l'ouvre (un fichier) ou le révèle (un
    /// dossier) à la place de Changes (chemin absolu).
    OpenInFiles(PathBuf),
    /// Clic droit sur une ligne de la liste : `Workspace` ouvre le menu à `position` (coordonnées de la fenêtre). Le
    /// chemin de la cible est relatif à la racine du dépôt.
    ListMenu {
        target: ChangeTarget,
        position: Point<Pixels>,
    },
    /// Un appui sur la puce du sélecteur : `Workspace` ouvre le menu des cibles à `position` (coordonnées de la fenêtre).
    /// `current` est la racine du worktree montré, `None` pour le dossier du projet.
    TargetMenu {
        lines: Vec<TargetLine>,
        current: Option<PathBuf>,
        position: Point<Pixels>,
    },
    /// `⌘⌥C` dans le diff, ou l'entrée de son menu : la référence des lignes choisies part vers le pane de l'agent.
    CopyToAgent(String),
    /// Clic droit dans le diff : `Workspace` ouvre le menu à `position` (coordonnées de la fenêtre). `reference` est celle
    /// des lignes choisies, `None` sans sélection de lignes ou avec des lignes retirées seules.
    DiffMenu {
        position: Point<Pixels>,
        reference: Option<String>,
    },
}

/// Ce que dit la ligne d'état quand la surveillance du disque n'a pas démarré.
const WATCH_FAILED: &str = "La liste ne suit plus le disque.";
/// La ligne de notice quand la liste des worktrees n'a pas pu être lue (comme « Le worktree … n'existe plus. »).
const TARGETS_FAILED: &str = "La liste des worktrees n'a pas pu être lue.";

/// Une génération du message en cours (spec B-11 §4.4). Elle vit de la demande du geste jusqu'à `Done`, `Failed` ou un arrêt ;
/// les lectures de l'index précèdent le processus, d'où un `cancel` vide pendant ce temps.
struct RunningGeneration {
    /// Numéro de cette génération (`generation_counter`) : un fragment d'une génération qui n'est plus la courante est jeté.
    id: u64,
    /// Le champ où le texte arrive : celui de la cible où la génération a commencé, jamais le champ alors montré.
    key: PathBuf,
    /// Lâcher la poignée annule la génération : elle reste ici tant qu'elle court. Vide jusqu'au retour des lectures Git.
    cancel: Option<CancelHandle>,
    started: Instant,
    /// Le modèle demandé, pour le journal et le texte d'une erreur.
    model: String,
    /// Ce que le prompt a retenu du diff, pour le journal ; vide jusqu'au retour des lectures Git.
    stats: Option<PromptStats>,
    /// Un fragment de texte est déjà arrivé : le saut de ligne qui sépare du texte présent n'est écrit qu'avant le premier
    /// (une réponse vide n'écrit rien).
    received: bool,
    _task: Task<()>,
}

/// La dernière lecture du dépôt. Une seule vit à la fois dans le volet : l'écart de taille entre variantes ne coûte rien.
#[allow(clippy::large_enum_variant)]
enum Load {
    /// Rien n'est encore revenu.
    Pending,
    Loaded(Snapshot),
    Failed(LoadError),
}

pub struct ChangesPanel {
    /// Le dossier du projet, canonisé une fois ; le dépôt qui le contient est lu par Git (P3-11).
    project: PathBuf,
    /// Ce que le volet montre : le dossier du projet ou l'un des worktrees de son dépôt. Gardé tant qu'IdemA tourne, jamais
    /// écrit dans `state.json`.
    target: Target,
    focus_handle: FocusHandle,
    /// Le focus de la colonne de la liste : `↑` et `↓` n'y valent que là (`LIST_KEY_CONTEXT`). Il descend du corps du
    /// volet, dont `contains_focused` reste la mesure. Liste masquée, le corps garde le focus.
    list_focus: FocusHandle,
    /// La largeur voulue de la liste, préférence globale ; `None` : `LIST_WIDTH`. La largeur montrée passe par
    /// `column::width_shown`.
    list_width: Option<f32>,
    list_hidden: bool,
    /// La largeur du corps du volet (liste et diff), en unités de la maquette, mesurée à la peinture : sans elle, le
    /// plancher du diff ne peut pas rogner la liste. `None` jusqu'à la première peinture.
    body_width: Option<f32>,
    /// Le glisser de la poignée de la liste en cours, s'il y en a un.
    list_drag: Option<ColumnDrag>,
    /// Les groupes et dossiers repliés, par clé de ligne.
    collapsed: HashSet<RowKey>,
    /// Les lignes de la liste, relues de la dernière lecture du dépôt et des replis ; la liste n'en peint que la fenêtre.
    rows: Vec<Row>,
    scroll: UniformListScrollHandle,
    /// Le volet est celui que l'écran montre. Un volet qui n'est pas affiché est inerte : pas de surveillance.
    shown: bool,
    load: Load,
    /// Le fichier choisi, par son chemin relatif à la racine du dépôt.
    selected: Option<PathBuf>,
    diff: DiffView,
    /// Les champs du message de commit (le champ multi-ligne du kit), un par dossier de cible (`target.dir`), gardés avec
    /// le volet : un message tient tant qu'IdemA tourne, même volet fermé ou cible quittée. Celui du dossier du projet
    /// naît avec le volet, celui d'un worktree à son premier choix.
    messages: HashMap<PathBuf, Entity<TextareaState>>,
    /// Ce que dit la ligne d'état quand la cible a disparu ; effacée au prochain choix de cible et à la fermeture du volet.
    target_notice: Option<String>,
    /// Compte les changements de cible : une lecture rendue par une génération plus ancienne est jetée.
    load_generation: u64,
    /// La liste des worktrees se lit : un second appui sur le sélecteur n'en lance pas une autre.
    listing_targets: bool,
    _targets_task: Option<Task<()>>,
    /// Les cases cliquées dont l'écriture n'est pas finie, par chemin : la liste les montre à la place de la dernière
    /// lecture (la case répond au clic, pas à Git).
    pending_checks: HashMap<PathBuf, Check>,
    /// Les écritures de cases en attente, jouées une à une : deux `git add` à la fois se disputeraient `index.lock`.
    writes: VecDeque<list_view::CheckWrite>,
    writing: bool,
    /// La file vient de se vider : les cases cliquées s'effacent à la fin de la première lecture lancée après (une lecture
    /// plus ancienne montrerait un instant l'état d'avant le geste).
    settling: bool,
    /// La lecture en cours a été lancée une fois la file vide.
    settled_load: bool,
    /// Un commit ou un push est en cours : son bouton dit « Commit… » ou « Push… » et ne répond plus.
    committing: bool,
    pushing: bool,
    /// La génération du message en cours, s'il y en a une : Commit est éteint tant qu'elle court.
    generation: Option<RunningGeneration>,
    /// Compte les générations lancées : le numéro de la suivante.
    generation_counter: u64,
    /// La surveillance et les dossiers qu'elle couvre.
    watch: Option<(WatchedDirs, ProjectWatch)>,
    /// Le dernier démarrage de la surveillance a échoué : la liste ne suit plus le disque, et le volet le dit à l'écran.
    watch_failed: bool,
    _watch_task: Option<Task<()>>,
    /// Jamais deux lectures à la fois : une demande qui arrive pendant une lecture n'en lance pas une seconde, elle note
    /// qu'il faudra en refaire une à sa fin.
    _load_task: Option<Task<()>>,
    running: bool,
    redo: bool,
    /// La dernière lecture a échoué sur une erreur de Git : l'échec n'est journalisé qu'une fois de suite.
    failing: bool,
    _subscriptions: Vec<Subscription>,
}

/// Ce que la surveillance couvre : la racine de la cible et ses dossiers Git.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WatchedDirs {
    root: PathBuf,
    git: GitDirs,
}

impl WatchedDirs {
    /// Avant toute lecture : le dossier du projet et son `.git` ordinaire.
    fn of_project(project: &Path) -> WatchedDirs {
        WatchedDirs {
            root: project.to_path_buf(),
            git: GitDirs::of_root(project),
        }
    }

    /// Ce que la lecture a trouvé : la racine du dépôt, le dossier Git de la cible et le dossier commun.
    fn of_snapshot(snapshot: &Snapshot) -> WatchedDirs {
        WatchedDirs {
            root: snapshot.root.clone(),
            git: GitDirs {
                git_dir: snapshot.git_dir.clone(),
                common_dir: snapshot.common_dir.clone(),
            },
        }
    }
}

impl EventEmitter<ChangesEvent> for ChangesPanel {}

impl ChangesPanel {
    /// Le volet du dossier `project`, tel que `state.json` le garde. Rien n'est lu ni surveillé avant `set_shown(true)`.
    pub fn new(
        project: PathBuf,
        list_width: Option<f32>,
        list_hidden: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = std::fs::canonicalize(&project).unwrap_or(project);
        let focus_handle = cx.focus_handle();
        let list_focus = cx.focus_handle();
        let (message, message_subscription) = Self::new_message(window, cx);
        let subscriptions = vec![
            cx.on_focus_in(&focus_handle, window, |_, _, cx| {
                cx.emit(ChangesEvent::FocusChanged);
            }),
            cx.on_focus_out(&focus_handle, window, |_, _, _, cx| {
                cx.emit(ChangesEvent::FocusChanged);
            }),
            message_subscription,
            // Un rechargement de `config.toml` qui retire la section éteint la fonction : la génération en cours s'arrête.
            cx.observe_global::<CommitMessageSettings>(|panel, cx| {
                if panel.generation.is_some() && !panel.generation_allowed(cx) {
                    panel.stop_generation("stopped", cx);
                }
            }),
        ];
        let messages = HashMap::from([(project.clone(), message)]);
        Self {
            project,
            target: Target::WorkingTree,
            focus_handle,
            list_focus,
            list_width,
            list_hidden,
            body_width: None,
            list_drag: None,
            collapsed: HashSet::new(),
            rows: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            shown: false,
            load: Load::Pending,
            selected: None,
            diff: DiffView::new(window, cx),
            messages,
            target_notice: None,
            load_generation: 0,
            listing_targets: false,
            _targets_task: None,
            pending_checks: HashMap::new(),
            writes: VecDeque::new(),
            writing: false,
            settling: false,
            settled_load: false,
            committing: false,
            pushing: false,
            generation: None,
            generation_counter: 0,
            watch: None,
            watch_failed: false,
            _watch_task: None,
            _load_task: None,
            running: false,
            redo: false,
            failing: false,
            _subscriptions: subscriptions,
        }
    }

    /// Un champ de message neuf, avec son indication et son abonnement : chaque frappe peut allumer ou éteindre Commit.
    /// `PressEnter` n'est pas écouté : `↩` va à la ligne dans le kit, et `⌘↩` est une action du volet.
    fn new_message(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<TextareaState>, Subscription) {
        let message = cx.new(|cx| TextareaState::new(window, cx).placeholder(MESSAGE_PLACEHOLDER));
        let subscription = cx.subscribe_in(&message, window, |_, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        (message, subscription)
    }

    /// Le champ du message de la cible montrée.
    fn message(&self) -> &Entity<TextareaState> {
        &self.messages[self.target.dir(&self.project)]
    }

    /// Le focus est-il dans le volet (lui-même ou un descendant, le diff) ?
    pub fn contains_focused(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    /// `Workspace` dit si le volet est celui que l'écran montre. Affiché, il surveille le dépôt et le relit ; masqué, il
    /// lâche la surveillance (le disque se relit à son retour).
    pub fn set_shown(&mut self, shown: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if self.shown == shown {
            return;
        }
        self.shown = shown;
        if shown {
            // Avant la première lecture, on ne connaît que le dossier : son `.git` ordinaire. La lecture dira le reste.
            let watched = WatchedDirs::of_project(self.target.dir(&self.project));
            self.start_watch(watched, cx);
            self.refresh(cx);
        } else {
            // Le volet n'est plus peint : la poignée ne recevrait plus le relâché.
            self.cancel_list_drag(cx);
            self.stop_generation("stopped", cx);
            self.stop_watch(cx);
            self.target_notice = None;
        }
    }

    /// Un appui sur la puce du sélecteur : lit les worktrees du dépôt hors du fil d'interface, puis `Workspace` ouvre le
    /// menu au point de l'appui. Rien ne se passe pendant un commit, un push, une écriture de case, ou une lecture des
    /// worktrees déjà en cours.
    pub fn request_target_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.committing
            || self.pushing
            || self.writing
            || !self.writes.is_empty()
            || self.listing_targets
        {
            return;
        }
        self.listing_targets = true;
        let project = self.project.clone();
        let home = home_dir();
        self._targets_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { list_targets(&project, home.as_deref()) })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.listing_targets = false;
                match result {
                    Ok(lines) => {
                        panel.target_notice = None;
                        let current = match &panel.target {
                            Target::WorkingTree => None,
                            Target::Worktree { root, .. } => Some(root.clone()),
                        };
                        cx.emit(ChangesEvent::TargetMenu {
                            lines,
                            current,
                            position,
                        });
                    }
                    Err(error) => {
                        hud::log_event(&format!("changes targets failed error={error:?}"));
                        panel.target_notice = Some(TARGETS_FAILED.to_string());
                        cx.notify();
                    }
                }
            });
        }));
    }

    /// Un worktree (ou le dossier du projet) choisi dans le menu. Rien ne se passe si la cible ne change pas. Le champ du
    /// message d'un worktree naît à son premier choix ; celui d'une cible quittée reste en mémoire.
    pub fn choose_target(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if target == self.target {
            return;
        }
        let dir = target.dir(&self.project).to_path_buf();
        if let Entry::Vacant(slot) = self.messages.entry(dir) {
            let (message, subscription) = Self::new_message(window, cx);
            slot.insert(message);
            self._subscriptions.push(subscription);
        }
        self.apply_target(target, cx);
    }

    /// Passe à `target` (dont le champ du message existe déjà) : tout ce que la lecture précédente avait posé s'efface, la
    /// génération avance (une lecture encore en vol pour l'ancienne cible sera jetée), la surveillance suit le nouveau
    /// dossier et une lecture part.
    fn apply_target(&mut self, target: Target, cx: &mut Context<Self>) {
        // Le texte reçu reste dans le message gardé pour la cible quittée.
        self.stop_generation("stopped", cx);
        self.target = target;
        self.target_notice = None;
        self.load = Load::Pending;
        self.rows.clear();
        self.selected = None;
        self.diff.clear(cx);
        self.pending_checks.clear();
        self.settling = false;
        self.settled_load = false;
        self.failing = false;
        self.cancel_list_drag(cx);
        self.load_generation += 1;
        hud::log_event(&format!("changes target name={}", self.target.name()));
        if self.shown {
            self.stop_watch(cx);
            let watched = WatchedDirs::of_project(self.target.dir(&self.project));
            self.start_watch(watched, cx);
        }
        self.refresh(cx);
        cx.notify();
    }

    /// Le texte du sélecteur de l'en-tête.
    pub fn selector_label(&self) -> String {
        match &self.load {
            Load::Loaded(snapshot) => selector_of(
                snapshot.base.as_deref(),
                snapshot.branch.head.as_deref(),
                &target_label(&self.target, snapshot.branch.head.as_deref()),
            ),
            _ => selector_of(None, None, self.target.name()),
        }
    }

    /// La liste est-elle visible ?
    pub fn list_visible(&self) -> bool {
        !self.list_hidden
    }

    /// Le bouton « liste » de l'en-tête : masque la liste (le diff prend toute la largeur) ou la rend, et `Workspace`
    /// écrit l'état dans `state.json`.
    pub fn toggle_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list_hidden = !self.list_hidden;
        self.cancel_list_drag(cx);
        window.focus(&self.focus_handle, cx);
        cx.emit(ChangesEvent::ListHiddenChanged(self.list_hidden));
        cx.notify();
    }

    /// La préférence « liste masquée », changée depuis le volet d'un autre projet : on la suit sans la renvoyer.
    pub fn apply_list_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        if self.list_hidden != hidden {
            self.list_hidden = hidden;
            self.cancel_list_drag(cx);
            cx.notify();
        }
    }

    fn start_watch(&mut self, watched: WatchedDirs, cx: &mut Context<Self>) {
        match ProjectWatch::start_with_git(&watched.root, &watched.git) {
            Ok((watch, mut batches)) => {
                self.watch_failed = false;
                self.watch = Some((watched, watch));
                self._watch_task = Some(cx.spawn(async move |this, cx| {
                    while batches.next().await.is_some() {
                        if this.update(cx, |panel, cx| panel.refresh(cx)).is_err() {
                            break;
                        }
                    }
                }));
            }
            Err(error) => {
                hud::log_event(&format!(
                    "changes watch failed root={}: {error}",
                    watched.root.display()
                ));
                self.watch_failed = true;
                cx.notify();
            }
        }
    }

    fn stop_watch(&mut self, cx: &mut Context<Self>) {
        self._watch_task = None;
        // Lâcher la surveillance bloque un instant (le fil de regroupement s'arrête) : hors du fil d'interface.
        if let Some((_, watch)) = self.watch.take() {
            cx.background_spawn(async move { drop(watch) }).detach();
        }
    }

    /// Relit le dépôt hors du fil d'interface, une lecture à la fois.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.shown {
            return;
        }
        if self.running {
            self.redo = true;
            return;
        }
        self.running = true;
        self.settled_load = self.settling;
        // La lecture emporte la génération de la cible qu'elle lit : si la cible change avant son retour, `finish_load` la
        // jette.
        let dir = self.target.dir(&self.project).to_path_buf();
        let generation = self.load_generation;
        let started = Instant::now();
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { model::load(&dir) }).await;
            let _ = this.update(cx, |panel, cx| {
                panel.finish_load(result, generation, started, cx)
            });
        }));
    }

    /// Une lecture est revenue : on garde le dépôt neuf (ou l'erreur), le fichier choisi suit son chemin, et le diff se relit.
    /// Une demande arrivée entre-temps relance une lecture. Une lecture d'une génération plus ancienne que la cible
    /// courante est jetée sans rien toucher, et la cible courante se relit aussitôt. Sur un worktree disparu, le volet
    /// revient au dossier du projet.
    fn finish_load(
        &mut self,
        result: Result<Snapshot, LoadError>,
        generation: u64,
        started: Instant,
        cx: &mut Context<Self>,
    ) {
        self.running = false;
        // `took_ms` est le temps de la lecture ; `build_ms`, le travail synchrone de cette fonction sur le fil d'interface.
        let took_ms = started.elapsed().as_millis();
        let build_started = Instant::now();
        if generation != self.load_generation {
            self.redo = false;
            self.refresh(cx);
            return;
        }
        if let Target::Worktree { root, name } = &self.target
            && target_gone(root, &result)
        {
            let name = name.clone();
            hud::log_event(&format!("changes target gone name={name}"));
            self.redo = false;
            self.apply_target(Target::WorkingTree, cx);
            self.target_notice = Some(format!("Le worktree {name} n'existe plus."));
            return;
        }
        // Cette lecture a été lancée une fois les écritures de cases finies : elle dit l'index tel qu'il est.
        if self.settled_load {
            self.pending_checks.clear();
            self.settling = false;
            self.settled_load = false;
        }
        match result {
            Ok(snapshot) => {
                self.failing = false;
                let entries = snapshot.changes.len();
                // Un projet ouvert sur un sous-dossier ou sur un worktree lié : Git ne bouge qu'à la racine du dépôt, et
                // l'index d'un worktree vit dans son propre dossier Git. La lecture dit lesquels surveiller.
                let read = WatchedDirs::of_snapshot(&snapshot);
                if self.shown
                    && self
                        .watch
                        .as_ref()
                        .is_some_and(|(watched, _)| *watched != read)
                {
                    self.stop_watch(cx);
                    self.start_watch(read, cx);
                    // Un `git add` qui tombe pendant la lecture, avant que la surveillance ne soit (ré)armée, ne
                    // serait jamais vu : une lecture de plus suit.
                    if self.watch.is_some() {
                        self.redo = true;
                    }
                }
                self.load = Load::Loaded(snapshot);
                self.rebuild_rows();
                if let Load::Loaded(snapshot) = &self.load {
                    self.selected =
                        next_selection(self.selected.take(), Some((&snapshot.changes, &self.rows)));
                }
                // Plus de ligne à peindre (dépôt vide) : la colonne disparaît, et un glisser de sa poignée ne recevrait plus
                // le relâché.
                if self.rows.is_empty() {
                    self.cancel_list_drag(cx);
                }
                self.read_selected_diff(cx);
                hud::log_event(&format!(
                    "changes refreshed entries={entries} took_ms={took_ms} build_ms={}",
                    build_started.elapsed().as_millis()
                ));
            }
            Err(error) => {
                if let LoadError::Git(message) = &error
                    && !self.failing
                {
                    self.failing = true;
                    hud::log_event(&format!(
                        "changes refresh failed project={} error={message}",
                        self.project.display()
                    ));
                }
                // Le fichier choisi reste noté (`next_selection` sans lecture). La colonne disparaît : un glisser de sa
                // poignée en cours est annulé, sans rien écrire.
                self.selected = next_selection(self.selected.take(), None);
                // Le pied n'est peint que dans l'état `Loaded` : un Stop caché laisserait la génération courir sans recours.
                self.stop_generation("stopped", cx);
                self.load = Load::Failed(error);
                self.rows.clear();
                self.cancel_list_drag(cx);
                self.diff.clear(cx);
            }
        }
        cx.notify();
        if self.redo {
            self.redo = false;
            self.refresh(cx);
        }
    }

    /// Un clic sur une ligne de la liste.
    fn select(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&path) {
            return;
        }
        self.selected = Some(path);
        self.read_selected_diff(cx);
        cx.notify();
    }

    /// Relit le patch du fichier choisi (ou vide la zone s'il n'y en a pas).
    fn read_selected_diff(&mut self, cx: &mut Context<Self>) {
        let Load::Loaded(snapshot) = &self.load else {
            return;
        };
        let change = self
            .selected
            .as_ref()
            .and_then(|path| snapshot.changes.iter().find(|change| &change.path == path));
        match change {
            Some(change) => self.diff.read(snapshot, change, cx),
            None => self.diff.clear(cx),
        }
    }

    /// Les lignes de la liste, relues de la dernière lecture du dépôt, des cases cliquées et des replis.
    fn rebuild_rows(&mut self) {
        self.rows = match &self.load {
            Load::Loaded(snapshot) => rows::build(
                &list_view::with_pending(&snapshot.changes, &self.pending_checks),
                &self.collapsed,
                snapshot.branch.head.as_deref(),
            ),
            _ => Vec::new(),
        };
    }

    /// Replie ou déplie le groupe ou le dossier `key`.
    fn toggle_row(&mut self, key: RowKey, cx: &mut Context<Self>) {
        toggle_collapsed(&mut self.collapsed, key);
        self.rebuild_rows();
        cx.notify();
    }
}

/// Replie la ligne `key` si elle est dépliée, la déplie sinon. Rend `true` quand elle est repliée ensuite.
fn toggle_collapsed(collapsed: &mut HashSet<RowKey>, key: RowKey) -> bool {
    if collapsed.remove(&key) {
        false
    } else {
        collapsed.insert(key);
        true
    }
}

impl Focusable for ChangesPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ChangesPanel {
    /// La zone sous l'en-tête : la liste à gauche, le diff à droite, ou un écran plein quand il n'y a rien à lister. Elle
    /// porte le focus du volet (un appui y reste, `root_focus` de `Workspace` ne le renvoie pas à la session) et son
    /// contexte de touches.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        // Le focus du corps (à l'ouverture, ou après un clic dans l'en-tête) passe à la colonne de la liste dès qu'elle est
        // peinte : `↑` et `↓` y valent aussitôt. Quand la colonne disparaît (liste masquée, dépôt vide ou illisible), le
        // corps reprend le focus : sans cela il resterait sur une colonne qui n'est plus peinte.
        let list_painted = self.list_painted();
        if list_painted {
            if self.focus_handle.is_focused(window) {
                window.focus(&self.list_focus, cx);
            }
        } else if self.list_focus.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        // Le corps porte le focus et le contexte de touches ; il empile la zone de la liste et du diff, puis le pied quand
        // la liste ne le porte pas (liste masquée, ou rien à lister : un Push peut rester à faire).
        let body = div()
            .id("changes-body")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(|this, _: &CopyToAgent, _, cx| {
                this.copy_selection_to_agent(cx);
            }));
        let area = div().flex_1().min_h_0().flex();
        // Sans surveillance, la liste ne bouge plus quand le disque change : une ligne le dit, au-dessus de tout.
        let body = body.children(
            self.watch_failed
                .then(|| notice_line(&theme, "changes-watch-notice", WATCH_FAILED)),
        );
        let body = body.children(
            self.target_notice
                .as_deref()
                .map(|text| notice_line(&theme, "changes-target-notice", text)),
        );
        match &self.load {
            Load::Pending => body,
            Load::Failed(error) => {
                let (text, detail) = match error {
                    LoadError::NotARepository => ("Ce projet n'est pas un dépôt Git.", None),
                    LoadError::Missing => ("Git est introuvable.", None),
                    LoadError::Git(message) => {
                        ("Le dépôt n'a pas pu être lu.", Some(message.as_str()))
                    }
                };
                body.child(area.child(full_message(&theme, text, detail)))
            }
            Load::Loaded(snapshot) if snapshot.changes.is_empty() => body
                .child(area.child(full_message(&theme, "Aucun changement.", None)))
                .child(self.render_footer(&theme, true, cx)),
            Load::Loaded(_) => body
                .child(
                    area
                        // Derrière tout : la mesure du corps, qui décide de ce que le plancher du diff laisse à la liste.
                        .child(column::measure_body(cx.entity(), Self::note_body_width))
                        .when(list_painted, |area| {
                            area.child(self.render_list(&theme, cx))
                        })
                        .child(self.diff.render(&theme, cx))
                        // Par-dessus : la poignée du bord droit de la liste.
                        .when(list_painted, |area| {
                            area.child(self.render_list_handle(&theme, cx))
                        }),
                )
                .when(!list_painted, |body| {
                    body.child(self.render_footer(&theme, true, cx))
                }),
        }
    }
}

/// Une ligne d'état en haut du volet : la surveillance du disque n'a pas pu démarrer, ou le worktree montré a disparu.
fn notice_line(theme: &Theme, id: &'static str, text: &str) -> impl IntoElement + use<> {
    div()
        .id(id)
        .flex_none()
        .w_full()
        .px(theme.px(10.))
        .py(theme.px(5.))
        .border_b_1()
        .border_color(white(SEPARATOR))
        .text_size(theme.text(11.5))
        .text_color(white(TEXT_MIN))
        .child(text.to_string())
}

/// Un écran plein : une phrase centrée, et le message brut de Git dessous quand il y en a un.
fn full_message(theme: &Theme, text: &str, detail: Option<&str>) -> impl IntoElement + use<> {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(theme.px(6.))
        .p(theme.px(24.))
        .text_size(theme.text(13.))
        .text_color(white(TEXT_MIN))
        .child(text.to_string())
        .children(detail.map(|detail| div().text_size(theme.text(11.5)).child(detail.to_string())))
}

/// Le nom que la puce donne à la cible chargée. Un worktree se nomme par sa branche (§4.2), celle que la lecture vient de
/// voir : si l'agent change de branche dans son worktree, la puce suit. En HEAD détachée, c'est le nom du dossier. Le
/// nom de `Target` (figé au choix du menu) ne sert que tant qu'aucune lecture n'a rendu de branche.
fn target_label(target: &Target, head: Option<&str>) -> String {
    match target {
        Target::WorkingTree => target.name().to_string(),
        Target::Worktree { root, name } => head.map(str::to_string).unwrap_or_else(|| {
            root.file_name()
                .map(|folder| folder.to_string_lossy().into_owned())
                .unwrap_or_else(|| name.clone())
        }),
    }
}

/// Le texte de la puce de l'en-tête : la base, une flèche, puis la cible (`working tree` ou le nom du worktree). Sans
/// commit il n'y a pas de base : la branche courante la remplace ; sans branche non plus (HEAD détachée sans base), il
/// reste la cible.
fn selector_of(base: Option<&str>, head: Option<&str>, target: &str) -> String {
    match base.or(head) {
        Some(name) => format!("{name} \u{2192} {target}"),
        None => target.to_string(),
    }
}

/// Le fichier choisi après une lecture. `read` est ce que la lecture a rendu : les changements et les lignes de la liste qui
/// en viennent, ou `None` quand elle a échoué. Réussie : le même chemin s'il est toujours dans la liste, sinon le premier
/// fichier affiché (l'ordre des lignes de l'arbre, pas celui des octets des chemins). Échouée (un verrou pris un instant) :
/// le fichier choisi reste noté, la lecture suivante le retrouve.
fn next_selection(selected: Option<PathBuf>, read: Option<(&[Change], &[Row])>) -> Option<PathBuf> {
    let Some((changes, rows)) = read else {
        return selected;
    };
    selected
        .filter(|path| changes.iter().any(|change| &change.path == path))
        .or_else(|| {
            rows.iter()
                .find_map(|row| row.change)
                .map(|index| changes[index].path.clone())
        })
}

/// Le volet : l'en-tête de 48 que dessine le volet (le bouton « liste » et le sélecteur à gauche, agrandir et fermer à
/// droite), puis le corps, qui est l'entité `ChangesPanel`. `header_inset` est le retrait gauche de l'en-tête.
pub fn render_panel(
    panel: Entity<ChangesPanel>,
    maximized: bool,
    header_inset: Pixels,
    width: Pixels,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let (selector, list_visible) = {
        let state = panel.read(cx);
        (state.selector_label(), state.list_visible())
    };
    let (maximize_icon, maximize_size) = if maximized {
        (Icon::CloseFullscreen, 16.)
    } else {
        (Icon::OpenInFull, 16.)
    };
    let list_button = {
        let panel = panel.clone();
        let icon = if list_visible {
            Icon::LeftPanelClose
        } else {
            Icon::LeftPanelOpen
        };
        div()
            .id("changes-list-toggle")
            .group("changes-list-toggle")
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(theme.px(28.))
            .rounded(theme.px(6.))
            .occlude()
            .cursor_pointer()
            .when(list_visible, |button| button.bg(focus(TOGGLE_ACTIVE)))
            .when(!list_visible, |button| {
                button.hover(|style| style.bg(white(HOVER_STRONG)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    panel.update(cx, |panel, cx| panel.toggle_list(window, cx));
                }),
            )
            .child(
                svg()
                    .path(icon.path())
                    .size(theme.px(18.))
                    .text_color(if list_visible {
                        color(FOCUS_TEXT)
                    } else {
                        white(TEXT_MIN)
                    })
                    .when(!list_visible, |icon| {
                        icon.group_hover("changes-list-toggle", |style| {
                            style.text_color(white(TEXT_HOVER))
                        })
                    }),
            )
    };
    let header = div()
        .h(theme.px(HEADER_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.px(4.))
        .pl(header_inset)
        .pr(theme.px(8.))
        // L'en-tête déplace la fenêtre, et un appui y rend la saisie au volet.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.focus_changes(window, cx);
                drag_window(event, window, cx);
            }),
        )
        .child(list_button)
        .child(
            // La puce du sélecteur : un appui ouvre le menu des cibles. Elle s'éclaircit au survol, comme les boutons.
            div()
                .id("changes-target")
                .group("changes-target")
                .min_w_0()
                .h(theme.px(28.))
                .flex()
                .items_center()
                .gap(theme.px(2.))
                .pl(theme.px(10.))
                .pr(theme.px(6.))
                .rounded(theme.px(6.))
                .bg(white(HOVER_STRONG))
                .text_color(white(TEXT_LABEL))
                .occlude()
                .cursor_pointer()
                .hover(|style| style.bg(white(CONTROL)))
                .on_mouse_down(MouseButton::Left, {
                    let panel = panel.clone();
                    cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        let position = event.position;
                        panel.update(cx, |panel, cx| panel.request_target_menu(position, cx));
                    })
                })
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(selector),
                )
                .child(
                    svg()
                        .path(Icon::ExpandMore.path())
                        .size(theme.px(16.))
                        .flex_none()
                        .text_color(white(TEXT_MIN))
                        .group_hover("changes-target", |style| {
                            style.text_color(white(TEXT_HOVER))
                        }),
                ),
        )
        .child(div().flex_1())
        .child(header_button(
            theme,
            "changes-maximize",
            maximize_icon,
            maximize_size,
            |this, window, cx| this.toggle_changes_maximized(window, cx),
            cx,
        ))
        .child(header_button(
            theme,
            "changes-close",
            Icon::Close,
            18.,
            |this, window, cx| this.close_changes(window, cx),
            cx,
        ));
    panel_shell("changes-panel", maximized, width)
        .child(header)
        .child(div().flex_1().min_h_0().child(panel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{Check, Group, Letter};

    fn change(path: &str, group: Group) -> Change {
        Change {
            path: PathBuf::from(path),
            orig_path: None,
            group,
            letter: Letter::Modified,
            check: Check::Unchecked,
        }
    }

    #[test]
    fn selector_names_the_base_then_the_target() {
        assert_eq!(
            selector_of(Some("main"), Some("demo"), "demo"),
            "main \u{2192} demo"
        );
        assert_eq!(
            selector_of(Some("main"), Some("main"), "working tree"),
            "main \u{2192} working tree"
        );
        // Sans commit il n'y a pas de base : la branche courante la remplace.
        assert_eq!(
            selector_of(None, Some("main"), "working tree"),
            "main \u{2192} working tree"
        );
        assert_eq!(selector_of(None, None, "working tree"), "working tree");
    }

    #[test]
    fn a_worktree_is_named_by_its_current_branch() {
        let demo = Target::Worktree {
            root: PathBuf::from("/repo/.worktrees/demo"),
            name: "demo".to_string(),
        };
        // L'agent a changé de branche dans le worktree : la puce suit.
        assert_eq!(target_label(&demo, Some("other")), "other");
        // HEAD détachée : le nom du dossier.
        assert_eq!(target_label(&demo, None), "demo");
        let renamed_folder = Target::Worktree {
            root: PathBuf::from("/repo/.worktrees/folder"),
            name: "stale".to_string(),
        };
        assert_eq!(target_label(&renamed_folder, None), "folder");
        // Le dossier du projet garde son nom, quelle que soit la branche.
        assert_eq!(
            target_label(&Target::WorkingTree, Some("main")),
            "working tree"
        );
    }

    #[test]
    fn list_rows_follow_collapse() {
        let changes = [
            change("d/a.md", Group::Modified),
            change("d/b.md", Group::Modified),
            change("z.md", Group::Untracked),
        ];
        let labels = |collapsed: &HashSet<RowKey>| -> Vec<String> {
            rows::build(&changes, collapsed, Some("main"))
                .into_iter()
                .map(|row| row.label)
                .collect()
        };
        let mut collapsed = HashSet::new();
        let group = RowKey::Group(Group::Modified);
        let folder = RowKey::Folder(Group::Modified, PathBuf::from("d"));

        // Replier un dossier cache ses fichiers, sa ligne reste.
        assert!(toggle_collapsed(&mut collapsed, folder.clone()));
        assert_eq!(labels(&collapsed), ["Modifiés", "d", "Non suivis", "z.md"]);
        // Replier le groupe cache aussi le dossier.
        assert!(toggle_collapsed(&mut collapsed, group.clone()));
        assert_eq!(labels(&collapsed), ["Modifiés", "Non suivis", "z.md"]);
        // Déplier rend les lignes (le dossier garde son repli propre).
        assert!(!toggle_collapsed(&mut collapsed, group));
        assert_eq!(labels(&collapsed), ["Modifiés", "d", "Non suivis", "z.md"]);
        assert!(!toggle_collapsed(&mut collapsed, folder));
        assert_eq!(
            labels(&collapsed),
            ["Modifiés", "d", "a.md", "b.md", "Non suivis", "z.md"]
        );
        assert!(collapsed.is_empty());
    }

    /// La sélection après une lecture réussie des `changes`, avec les lignes que la liste en tire.
    fn selection_after_read(selected: Option<&str>, changes: &[Change]) -> Option<PathBuf> {
        let rows = rows::build(changes, &HashSet::new(), Some("main"));
        next_selection(selected.map(PathBuf::from), Some((changes, &rows)))
    }

    #[test]
    fn the_selection_follows_its_path_and_falls_back_to_the_first_displayed_file() {
        // L'ordre de la liste : les dossiers d'abord, puis les noms sans tenir compte de la casse. `docs/x.md` est le premier
        // fichier affiché, bien que `README.md` le précède par octets.
        let changes = [
            change("README.md", Group::Modified),
            change("docs/x.md", Group::Modified),
            change("logo.bin", Group::Modified),
        ];
        // Le fichier choisi est toujours là : il reste choisi, quelle que soit sa place.
        assert_eq!(
            selection_after_read(Some("logo.bin"), &changes),
            Some(PathBuf::from("logo.bin"))
        );
        // Il a disparu (commité, supprimé), ou rien n'était choisi : le premier fichier affiché.
        assert_eq!(
            selection_after_read(Some("gone.md"), &changes),
            Some(PathBuf::from("docs/x.md"))
        );
        assert_eq!(
            selection_after_read(None, &changes),
            Some(PathBuf::from("docs/x.md"))
        );
        // Sans fichier, rien n'est choisi.
        assert_eq!(selection_after_read(Some("a.md"), &[]), None);
    }

    #[test]
    fn the_first_displayed_file_follows_the_group_order() {
        let changes = [
            change("a.md", Group::Untracked),
            change("z.md", Group::Modified),
        ];
        assert_eq!(
            selection_after_read(None, &changes),
            Some(PathBuf::from("z.md"))
        );
    }

    /// Les touches du volet sur le keymap réel : le kit d'abord (`gpui_component::init`), puis `key_bindings`, dans l'ordre
    /// de `main`. Chaque pile est celle du focus : la fenêtre, le corps du volet, puis la colonne de la liste, ou la boîte
    /// du message et le champ du kit au contexte `Input`.
    fn first_action(keys: &str, contexts: &[&str]) -> Option<Box<dyn gpui::Action>> {
        use gpui::{KeyContext, Keymap, Keystroke};
        let mut bindings = crate::files::panel::tests::kit_input_bindings();
        bindings.extend(key_bindings());
        let keymap = Keymap::new(bindings);
        let stack: Vec<KeyContext> = contexts
            .iter()
            .map(|name| KeyContext::parse(name).unwrap())
            .collect();
        let (found, _) = keymap.bindings_for_input(&[Keystroke::parse(keys).unwrap()], &stack);
        found.first().map(|binding| binding.action().boxed_clone())
    }

    // Les piles du focus, telles que `render` les construit : le pied et la zone des lignes sont frères sous la colonne de la
    // liste (`render_list`), donc le contexte `ChangesList` n'est pas au-dessus du champ du message. Un pied posé sous ce
    // contexte ferait prendre `espace` par la liste (il coche le fichier choisi à chaque espace du message).
    const MESSAGE_STACK: [&str; 4] = [
        crate::workspace::KEY_CONTEXT,
        KEY_CONTEXT,
        MESSAGE_KEY_CONTEXT,
        "Input",
    ];
    const LIST_STACK: [&str; 3] = [crate::workspace::KEY_CONTEXT, KEY_CONTEXT, LIST_KEY_CONTEXT];

    #[test]
    fn cmd_enter_in_the_message_commits_and_enter_stays_a_newline() {
        use gpui_component::input::Enter;
        let commit = first_action("cmd-enter", &MESSAGE_STACK).expect("cmd-enter is bound");
        assert!(
            commit.partial_eq(&Commit),
            "cmd-enter falls on {}",
            commit.name()
        );
        // `↩` reste au kit, qui va à la ligne ; `⌘↩` ne passe pas par lui (il irait à la ligne aussi).
        let enter = first_action("enter", &MESSAGE_STACK).expect("enter is bound");
        assert!(
            enter.partial_eq(&Enter {
                secondary: false,
                shift: false
            }),
            "enter falls on {}",
            enter.name()
        );
        // Hors de la boîte, `⌘↩` ne commite pas.
        let in_list = first_action("cmd-enter", &LIST_STACK);
        assert!(!in_list.is_some_and(|action| action.partial_eq(&Commit)));
    }

    #[test]
    fn alt_tab_generates_from_the_message_and_from_the_list_only() {
        // Le keymap de l'app : le kit d'abord, puis Changes, comme `main.rs` les lie : le kit ne mange pas `⌥⇥`.
        for (name, stack) in [("message", &MESSAGE_STACK[..]), ("list", &LIST_STACK[..])] {
            let generate = first_action("alt-tab", stack).expect("alt-tab is bound");
            assert!(
                generate.partial_eq(&GenerateMessage),
                "alt-tab falls on {} in the {name}",
                generate.name()
            );
        }
        // Le diff et un pane au focus n'ont pas cette liaison.
        let in_diff = first_action(
            "alt-tab",
            &[crate::workspace::KEY_CONTEXT, KEY_CONTEXT, "Diff"],
        );
        assert!(!in_diff.is_some_and(|action| action.partial_eq(&GenerateMessage)));
        let in_workspace = first_action("alt-tab", &[crate::workspace::KEY_CONTEXT]);
        assert!(!in_workspace.is_some_and(|action| action.partial_eq(&GenerateMessage)));
    }

    #[test]
    fn space_toggles_in_the_list_but_never_in_the_message() {
        let toggle = first_action("space", &LIST_STACK).expect("space is bound in the list");
        assert!(
            toggle.partial_eq(&ToggleCheck),
            "space falls on {}",
            toggle.name()
        );
        // Dans la boîte du message, la touche est une frappe : aucune liaison ne la prend.
        assert!(first_action("space", &MESSAGE_STACK).is_none());
        // `↑` et `↓` du champ restent ceux du kit, la liste n'est pas dans la pile du message.
        for (keys, list_action) in [("up", &ListUp as &dyn gpui::Action), ("down", &ListDown)] {
            let in_message = first_action(keys, &MESSAGE_STACK).expect("the kit binds it");
            assert!(
                !in_message.partial_eq(list_action),
                "{keys} reaches the list"
            );
            let in_list = first_action(keys, &LIST_STACK).expect("the list binds it");
            assert!(in_list.partial_eq(list_action), "{keys} is not the list's");
        }
    }

    #[test]
    fn copy_to_agent_is_bound_only_in_the_diff() {
        use crate::workspace::KEY_CONTEXT as WORKSPACE_CONTEXT;
        use gpui::{KeyContext, Keymap, Keystroke};

        // Le keymap de l'app : le kit, Fichiers (qui lie aussi `⌘⌥C`, à son éditeur), puis Changes.
        let mut bindings = crate::files::panel::tests::kit_input_bindings();
        bindings.extend(crate::files::panel::key_bindings());
        bindings.extend(key_bindings());
        let keymap = Keymap::new(bindings);
        let resolve = |names: &[&str]| {
            let stack: Vec<KeyContext> = names
                .iter()
                .map(|name| KeyContext::parse(name).unwrap())
                .collect();
            let keystrokes = [Keystroke::parse("cmd-alt-c").unwrap()];
            keymap.bindings_for_input(&keystrokes, &stack).0
        };

        // Le diff au focus : le geste est celui de Changes, et lui seul.
        let in_diff = resolve(&[WORKSPACE_CONTEXT, KEY_CONTEXT, "Diff"]);
        assert_eq!(in_diff.len(), 1, "{in_diff:?}");
        assert!(in_diff[0].action().partial_eq(&CopyToAgent));
        // Un pane au focus (le contexte `Workspace` seul) ou la liste : aucune liaison, l'agent garde `⌘⌥C`.
        assert!(resolve(&[WORKSPACE_CONTEXT]).is_empty());
        assert!(resolve(&LIST_STACK).is_empty());
    }

    #[test]
    fn a_failed_load_keeps_the_current_selection() {
        // Une lecture ratée (verrou pris un instant) n'efface pas le fichier choisi : la lecture suivante le retrouve.
        assert_eq!(
            next_selection(Some(PathBuf::from("b.md")), None),
            Some(PathBuf::from("b.md"))
        );
        assert_eq!(next_selection(None, None), None);
    }
}
