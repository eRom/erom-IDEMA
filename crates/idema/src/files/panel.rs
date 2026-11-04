//! Le volet Fichiers (spec P2, §4.1 et §4.2) : le volet droit qui prend la place du volet Terminal. Il montre l'arbre
//! du projet, qui suit le disque, les onglets et leurs éditeurs (sauvegarde automatique, rechargement depuis le
//! disque), et fixe la place dans la fenêtre, les largeurs, le clavier et le cycle de vie : une entité `FilesPanel` par
//! projet, gardée par `Workspace` tant qu'IdemA tourne. Les règles sans fenêtre des onglets sont dans `tabs.rs`.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::oneshot::Receiver;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    MouseDownEvent, Pixels, Point, ScrollHandle, ScrollStrategy, Subscription, Task,
    UniformListScrollHandle, Window, actions, div, prelude::*, px, svg,
};
use gpui_component::input::{Editor, EditorState, InputEvent, Position, Replace, TabSize};

use super::document::{
    ExternalAction, Indent, LineEnding, LoadedText, Opened, Saved, external_action, language_for,
    open_with_default_app, read_document, reveal_in_finder, save_unless_changed,
};
use super::gestures::{EditorGesture, EntryGit, TreeTarget};
use super::ignored::{IgnoredSet, git_ignored};
use super::line_ops::{Direction, LineEdit, duplicate_lines, move_lines};
use super::reference::{agent_reference, selection_lines};
use super::tabs::{DiskVerdict, Fingerprint, SaveCheck, SaveState, TabKey, TabStrip, fingerprint};
use super::tracked::{TrackedSet, git_tracked};
use super::tree::{FileTree, Row, behind_link};
use super::tree_edit::{ListItem, list_items};
use super::watch::{ProjectWatch, WatchBatch};
use crate::assets::Icon;
use crate::column::{self, ColumnDrag, HandleActions};
use crate::errors::{Explained, alert, explain_write};
use crate::files::filter::{TabSave, idle_save};
use crate::hud;
use crate::rename_field;
use crate::terminal_panel::{panel_header, panel_shell};
use crate::theme::{
    HOVER_STRONG, ICON_SECONDARY, SEPARATOR, TEXT_FULL, TEXT_HOVER, TEXT_MIN, TEXT_SECONDARY,
    TREE_DROP, Theme, focus, white,
};
use crate::workspace::Workspace;

mod filter_view;
mod tree_ops;
mod tree_view;

pub(crate) use filter_view::{FIELD_HEIGHT, FIELD_PADDING, FIELD_RADIUS, field_input};
pub use filter_view::{
    FILTER_KEY_CONTEXT, FieldGuard, FilterClear, FilterDown, FilterNextField, FilterOpen, FilterUp,
    IgnoreEnter, REPLACE_KEY_CONTEXT,
};
use filter_view::{ProjectFilter, batch_changes_listing};
use tree_ops::TreeEdit;
pub(crate) use tree_view::{GLYPH_SLOT, row_lead};

actions!(
    files,
    [
        ToggleTree,
        CloseTab,
        SaveNow,
        ProjectSearch,
        CopyToAgent,
        RenameSelected,
        TrashSelected,
        MoveLineUp,
        MoveLineDown,
        DuplicateLineUp,
        DuplicateLineDown
    ]
);

/// Largeur de l'arbre dans le volet, et de la bande où il se replie (maquette à l'échelle ; valeurs à ratifier).
pub const TREE_WIDTH: f32 = 180.;
pub const TREE_COLLAPSED_WIDTH: f32 = 22.;
/// Ce que l'éditeur garde au moins de largeur quand l'arbre s'élargit (maquette à l'échelle).
pub const EDITOR_MIN_WIDTH: f32 = 200.;

/// La largeur que l'arbre montre : la préférence (ou `TREE_WIDTH`), rognée pour laisser `EDITOR_MIN_WIDTH` à
/// l'éditeur dans un corps de `body_width`, sans jamais passer sous `TREE_WIDTH`. La préférence n'est pas touchée :
/// un volet rétréci puis rélargi rend la colonne à la largeur voulue.
pub(crate) fn tree_width_shown(wanted: Option<f32>, body_width: f32) -> f32 {
    column::width_shown(wanted, body_width, TREE_WIDTH, TREE_WIDTH, EDITOR_MIN_WIDTH)
}

/// La largeur que le glisser de la poignée écrit : celle de départ plus le déplacement (unités de la maquette), telle que
/// l'arbre la montre, donc bornée par les deux planchers. C'est la valeur gardée, jamais celle de la souris.
pub(crate) fn dragged_tree_width(start_width: f32, delta: f32, body_width: f32) -> f32 {
    column::dragged_width(
        start_width,
        delta,
        body_width,
        TREE_WIDTH,
        TREE_WIDTH,
        EDITOR_MIN_WIDTH,
    )
}

/// En-tête de l'arbre : la boîte du champ du filtre (28 de haut, dans un retrait de 8) et le bouton de repli, qui tient
/// seul dans la bande repliée de 22. La barre d'onglets garde ses 30.
const TREE_HEADER_HEIGHT: f32 = 44.;
/// Hauteur de la barre d'onglets de l'éditeur.
const TAB_BAR_HEIGHT: f32 = 30.;
/// Délai entre la dernière frappe et la sauvegarde automatique (spec §4.6, valeur à ratifier).
pub(crate) const AUTOSAVE_DELAY: Duration = Duration::from_secs(1);
/// Un onglet reprend la pastille de l'en-tête des volets droits (`terminal_panel.rs`, cotes de maquette) : 28 de haut,
/// rayon 6, retrait gauche 10, écart 7. Le titre est borné à 110 : l'éditeur n'a que 240 px par défaut (volet de 420,
/// arbre de 180), trois onglets doivent tenir.
const TAB_HEIGHT: f32 = 28.;
const TAB_RADIUS: f32 = 6.;
const TAB_PADDING_LEFT: f32 = 10.;
const TAB_GAP: f32 = 7.;
const TAB_TITLE_MAX_WIDTH: f32 = 110.;
/// Largeur d'un titre hors projet (`…/dossier/fichier.ext`) : plus long qu'un nom de fichier seul.
const OUTSIDE_TAB_TITLE_MAX_WIDTH: f32 = 190.;
/// Largeur d'affichage d'une tabulation dure, pour un fichier indenté par tabulations : 4, le `tab_size` par défaut
/// de Zed.
const HARD_TAB_WIDTH: usize = 4;

/// Contexte de touches du volet : l'arbre et l'éditeur en descendent. Ses liaisons passent avant celles de `Workspace`
/// parce que `Files` est plus profond dans la pile de contextes ; à profondeur égale, la dernière liée gagnerait.
pub const KEY_CONTEXT: &str = "Files";

/// Contexte de touches de la colonne de l'arbre : `↩` et `⌘⌫` n'y valent que là. L'éditeur et le champ de renommage ne
/// sont pas des descendants de la colonne (le champ en est un, mais son contexte est plus profond et l'emporte) : leurs
/// touches restent celles du texte.
pub const TREE_KEY_CONTEXT: &str = "FilesTree";

/// Les liaisons du volet. `⌘W` y ferme l'onglet au lieu de la session (le contexte `Files`, plus profond que
/// `Workspace`, l'emporte ; sans onglet ouvert l'action se propage et la session se ferme), `⌘S` sauvegarde tout de
/// suite. `⌘⇧F` ouvre la recherche dans le projet de n'importe où (`ProjectSearch`, liée au contexte de `Workspace`,
/// dont le gestionnaire ouvre le volet au besoin, comme `⌘P`). Dans le contexte `Input` de l'éditeur, `⌘⌥F` ouvre le
/// remplacement (le kit le lie à `⌘⇧F`) et `⌘⇧F` est repris par la même action, qui remonte jusqu'à `Workspace` : le
/// bouclier empêche `⌘⇧F` d'ouvrir le remplacement du kit. À profondeur égale la dernière liée gagne : ces liaisons se
/// posent après `gpui_component::init`. `⌘⌥C` envoie la
/// sélection à l'agent, et seulement dans l'éditeur : un pane au focus garde `⌘⌥C` pour l'agent. `↩` renomme et `⌘⌫`
/// met à la Corbeille la ligne sélectionnée, quand l'arbre a le focus. Les champs du filtre et de « Remplacer par » sont
/// des `Input` du kit sous une boîte de contexte `FilesFilter` ou `FilesReplace`, dans la colonne de l'arbre : le kit
/// prend ses touches d'abord, puis la boîte (contexte moins profond) attrape ce qu'il propage ou ne prend pas (`↩`,
/// Échap, `⌫` en tête de texte, `↑` `↓` `⇥` sur une ligne). Chaque touche de la boîte vise une action réellement prise,
/// même pour ne rien faire (`FieldGuard`) : GPUI passerait sinon à `RenameSelected` ou `TrashSelected`.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-w", CloseTab, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-s", SaveNow, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-alt-f", Replace, Some(EDITOR_INPUT_CONTEXT)),
        KeyBinding::new("cmd-shift-f", ProjectSearch, Some(EDITOR_INPUT_CONTEXT)),
        KeyBinding::new(
            "cmd-shift-f",
            ProjectSearch,
            Some(crate::workspace::KEY_CONTEXT),
        ),
        KeyBinding::new("cmd-alt-c", CopyToAgent, Some(EDITOR_INPUT_CONTEXT)),
        // Les gestes de lignes de Zed et VS Code : le kit ne lie ni `⌥↑` `⌥↓` ni, sur macOS, `⇧⌥↑` `⇧⌥↓`. Seul
        // l'éditeur les prend : un champ du kit (filtre, « Remplacer par », recherche `⌘F` de l'éditeur) est aussi un
        // `Input`, et le geste y modifierait le document derrière.
        KeyBinding::new("alt-up", MoveLineUp, Some(LINE_EDITOR_CONTEXT)),
        KeyBinding::new("alt-down", MoveLineDown, Some(LINE_EDITOR_CONTEXT)),
        KeyBinding::new("shift-alt-up", DuplicateLineUp, Some(LINE_EDITOR_CONTEXT)),
        KeyBinding::new(
            "shift-alt-down",
            DuplicateLineDown,
            Some(LINE_EDITOR_CONTEXT),
        ),
        KeyBinding::new("enter", RenameSelected, Some(TREE_KEY_CONTEXT)),
        KeyBinding::new("cmd-backspace", TrashSelected, Some(TREE_KEY_CONTEXT)),
        KeyBinding::new("up", FilterUp, Some(FILTER_KEY_CONTEXT)),
        KeyBinding::new("down", FilterDown, Some(FILTER_KEY_CONTEXT)),
        KeyBinding::new("enter", FilterOpen, Some(FILTER_KEY_CONTEXT)),
        KeyBinding::new("escape", FilterClear, Some(FILTER_KEY_CONTEXT)),
        KeyBinding::new("tab", FilterNextField, Some(FILTER_KEY_CONTEXT)),
        KeyBinding::new("shift-tab", FieldGuard, Some(FILTER_KEY_CONTEXT)),
        KeyBinding::new("backspace", FieldGuard, Some(FILTER_KEY_CONTEXT)),
        KeyBinding::new("cmd-backspace", FieldGuard, Some(FILTER_KEY_CONTEXT)),
        // Le champ « Remplacer par » : `↩` y est pris sans effet, un remplacement ne part jamais au clavier. `↑` `↓`
        // n'ont rien à y faire : la boîte les prend sans effet, l'arbre ne les voit pas.
        KeyBinding::new("enter", IgnoreEnter, Some(REPLACE_KEY_CONTEXT)),
        KeyBinding::new("escape", FilterClear, Some(REPLACE_KEY_CONTEXT)),
        KeyBinding::new("tab", FilterNextField, Some(REPLACE_KEY_CONTEXT)),
        KeyBinding::new("shift-tab", FilterNextField, Some(REPLACE_KEY_CONTEXT)),
        KeyBinding::new("up", FieldGuard, Some(REPLACE_KEY_CONTEXT)),
        KeyBinding::new("down", FieldGuard, Some(REPLACE_KEY_CONTEXT)),
        KeyBinding::new("backspace", FieldGuard, Some(REPLACE_KEY_CONTEXT)),
        KeyBinding::new("cmd-backspace", FieldGuard, Some(REPLACE_KEY_CONTEXT)),
        // `⌘⌥C` est lié plus haut au contexte `Input` pour l'éditeur : un champ du kit en est un aussi. Le contexte
        // `FilesFilter > Input`, à la même profondeur et lié après, le lui reprend : le geste n'envoie rien à l'agent.
        KeyBinding::new("cmd-alt-c", FieldGuard, Some(FILTER_INPUT_CONTEXT)),
        KeyBinding::new("cmd-alt-c", FieldGuard, Some(REPLACE_INPUT_CONTEXT)),
    ]
}

/// Le champ du kit posé sous la boîte du filtre, ou sous celle de « Remplacer par » : plus profond que la boîte seule.
const FILTER_INPUT_CONTEXT: &str = "FilesFilter > Input";
const REPLACE_INPUT_CONTEXT: &str = "FilesReplace > Input";

/// Le contexte de touches de l'éditeur de gpui-component (`gpui-base/src/input/base/state.rs`, `CONTEXT`).
const EDITOR_INPUT_CONTEXT: &str = "Input";
/// L'éditeur seul : il porte `Input mode=editor` (`gpui-base/src/input/editor/mod.rs`), un champ du kit non.
const LINE_EDITOR_CONTEXT: &str = "Input && mode == editor";

/// Ce que le volet apprend à `Workspace`.
#[derive(Debug, Clone, PartialEq)]
pub enum FilesEvent {
    /// Le volet (ou l'un de ses descendants) a gagné ou perdu le focus : le clavier change peut-être de côté.
    FocusChanged,
    /// L'arbre vient d'être replié ou déplié par un geste : la préférence est globale, `Workspace` l'écrit dans
    /// `state.json` et la rend aux autres volets.
    TreeCollapsedChanged(bool),
    /// La largeur de l'arbre vient d'être changée par un geste (`None` : retour au défaut) : `Workspace` l'écrit dans
    /// `state.json` et la rend aux autres volets, comme le repli.
    TreeWidthChanged(Option<f32>),
    /// « Copier vers l'agent » : la référence de l'onglet actif (sans l'espace qui la suit au collage).
    CopyToAgent(String),
    /// Un clic droit dans l'arbre (sur une ligne, ou dans le vide : la racine) : `Workspace` ouvre le menu.
    TreeMenu {
        target: TreeTarget,
        position: Point<Pixels>,
    },
    /// Un clic droit dans l'éditeur : `Workspace` ouvre le menu. Le curseur et la sélection n'ont pas bougé (E44) ;
    /// `has_selection` dit si Couper et Copier ont de quoi travailler.
    EditorMenu {
        position: Point<Pixels>,
        has_selection: bool,
    },
}

/// Le chemin est-il une règle d'ignorance de Git, dont un changement redéfinit ce qui est atténué ? Un `.gitignore`
/// n'importe où dans le projet, ou `.git/info/exclude` (le seul fichier de `.git/` que la surveillance laisse passer).
fn is_ignore_rules(root: &Path, path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == ".gitignore")
        || path == root.join(".git").join("info").join("exclude")
}

/// Le dossier personnel. Un `HOME` absent ou vide n'est pas un dossier : les chemins restent absolus.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// `path` sous la forme canonique de `root` (liens résolus), pour le comparer aux chemins de l'arbre (arbitrage 5).
/// Un fichier qui n'existe pas encore garde son nom, derrière le parent canonisé ; un chemin introuvable reste tel quel.
fn canonical_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|parent| parent.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

/// La clé d'onglet d'une entrée de l'arbre : son dossier canonisé, son nom tel quel. Sous un dossier-lien, la ligne porte
/// le chemin du lien et l'onglet celui de la cible ; le lien lui-même garde son chemin, si bien que supprimer ou renommer
/// un lien ne touche pas aux onglets de sa cible. Vaut aussi pour un chemin qui n'existe plus (après un renommage).
fn tab_key(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|parent| parent.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

/// Les onglets qu'un lot de la surveillance touche : ceux dont le chemin est dans le lot, et, quand le watcher a
/// débordé sur un dossier, tous ceux qui sont dessous (le lot ne dit plus lesquels ont changé).
fn touched_tabs<'a>(open: impl IntoIterator<Item = &'a Path>, batch: &WatchBatch) -> Vec<PathBuf> {
    open.into_iter()
        .filter(|path| {
            batch.paths.iter().any(|changed| changed == path)
                || batch.rescan.iter().any(|dir| path.starts_with(dir))
        })
        .map(Path::to_path_buf)
        .collect()
}

/// La fin de ligne que l'onglet garde après une lecture du disque : celle du disque. Une conversion faite hors
/// d'IdemA (`dos2unix`) laisse le texte normalisé identique, donc sans rechargement : sans cette reprise, la
/// sauvegarde suivante remettrait l'ancienne fin de ligne.
fn line_ending_after_read(current: LineEnding, disk: Option<&LoadedText>) -> LineEnding {
    disk.map_or(current, |loaded| loaded.line_ending)
}

/// Le second appui d'un double-clic ne refait pas ce que le premier a fait : si le fichier n'est pas ouvert en
/// onglet, le premier appui l'a confié à l'app par défaut (ou au Finder), un seul `open` par geste.
fn is_repeat_of_external_open(click_count: usize, already_open: bool) -> bool {
    click_count >= 2 && !already_open
}

/// Un fichier ouvert : son éditeur, ce que le disque sait de lui et ce qu'il faut pour le réécrire à l'identique.
struct OpenTab {
    editor: Entity<EditorState>,
    save: SaveState,
    line_ending: LineEnding,
    /// Le titre de la barre : le nom du fichier, ou `…/dossier/fichier` hors du projet.
    title: String,
    /// La minuterie de la sauvegarde d'une seconde ; la remplacer à chaque frappe annule la précédente.
    _save_timer: Option<Task<()>>,
    /// Une lecture du disque est en cours ; un lot qui arrive pendant ce temps note qu'il faudra la refaire.
    reloading: bool,
    reload_redo: bool,
    /// Une sauvegarde a été refusée parce que le disque avait changé : si le rechargement n'y trouve rien à reprendre
    /// (même contenu), la sauvegarde repart.
    resave: bool,
    _reload_task: Option<Task<()>>,
    _subscription: Subscription,
}

/// L'indentation du fichier, dans les termes de l'éditeur : ses espaces, ou des tabulations dures.
fn tab_size_for(indent: Indent) -> TabSize {
    match indent {
        Indent::Spaces(width) => TabSize {
            tab_size: usize::from(width),
            hard_tabs: false,
        },
        Indent::Tabs => TabSize {
            tab_size: HARD_TAB_WIDTH,
            hard_tabs: true,
        },
    }
}

/// Le titre d'un onglet : le nom du fichier dans le projet. Hors du projet, le chemin court (`~/…` sous le dossier
/// personnel) tant qu'il tient en deux niveaux, sinon `…/dossier/fichier` : la coupe se fait par le début, le nom du
/// fichier se lit toujours.
fn tab_title(path: &Path, root: &Path, home: Option<&Path>) -> String {
    if path.starts_with(root) {
        return path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    }
    let under_home = home.and_then(|home| path.strip_prefix(home).ok());
    let names: Vec<_> = under_home
        .unwrap_or(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy()),
            _ => None,
        })
        .collect();
    if names.len() > 2 {
        return format!("…/{}/{}", names[names.len() - 2], names[names.len() - 1]);
    }
    match under_home {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Le volet d'un projet. Sa racine est canonique (liens résolus) : tout l'état interne s'indexe dessus, de sorte que
/// deux écritures du même chemin ne donnent pas deux fichiers ouverts.
pub struct FilesPanel {
    root: PathBuf,
    focus_handle: FocusHandle,
    /// Le focus de la colonne de l'arbre : `↩` et `⌘⌫` n'y valent que là (`TREE_KEY_CONTEXT`). Il descend du corps du
    /// volet, dont `contains_focused` reste la mesure. Replié, l'arbre n'est plus peint : le corps garde le focus.
    tree_focus: FocusHandle,
    tree: FileTree,
    /// Les lignes visibles de l'arbre, relues du modèle à chaque changement : la liste n'en peint que la fenêtre.
    rows: Vec<Row>,
    /// Ce que la liste dessine : les lignes, plus le champ ouvert et son message de refus (`list_items`).
    items: Vec<ListItem>,
    /// Le champ de création ou de renommage ouvert dans l'arbre, s'il y en a un.
    edit: Option<TreeEdit>,
    /// Le champ a disparu avec sa place (dossier replié ou supprimé sous lui) : le prochain rendu rend le focus à l'arbre.
    refocus_tree: bool,
    /// Le dossier que le glisser en cours éclaire (E47) : les lignes dessous prennent `TREE_DROP`. `drop_candidate` est
    /// l'avis de la capture du dernier mouvement (la racine si la souris est dans la colonne, puis chaque ligne
    /// survolée l'écrase) ; la colonne le retient dans `drop_target` après coup, quand toutes les lignes ont parlé.
    drop_target: Option<PathBuf>,
    drop_candidate: Option<PathBuf>,
    /// La souris est sur la zone des lignes (dans la colonne, sous l'en-tête et au-dessus du bas de la liste) : seules
    /// les lignes visibles parlent alors. Une ligne rognée par le défilement garde sa boîte entière sous l'en-tête, qui
    /// ne doit pas lui donner le dépôt.
    pointer_over_list: bool,
    selected: Option<PathBuf>,
    scroll: UniformListScrollHandle,
    /// Le champ en tête de la colonne et le filtre des fichiers qu'il pilote ; son texte décide de ce que la colonne
    /// montre (l'arbre, la liste du filtre ou la zone de la recherche).
    filter: ProjectFilter,
    /// Le dossier du projet n'existe plus (ou n'est plus un dossier) : la zone le dit à la place de l'arbre.
    root_missing: bool,
    tree_collapsed: bool,
    /// La largeur voulue de l'arbre, préférence globale ; `None` : `TREE_WIDTH`. La largeur montrée passe par
    /// `tree_width_shown`.
    tree_width: Option<f32>,
    /// La largeur du corps du volet (l'arbre et l'éditeur), en unités de la maquette, mesurée à la peinture : sans elle,
    /// le plancher de l'éditeur ne peut pas rogner l'arbre. `None` jusqu'à la première peinture.
    body_width: Option<f32>,
    /// Le glisser de la poignée de l'arbre en cours, s'il y en a un.
    tree_drag: Option<ColumnDrag>,
    /// Le volet est celui que l'écran montre. Un volet qui n'est pas affiché est inerte : pas de surveillance.
    shown: bool,
    /// Les fichiers ouverts : l'ordre et l'onglet actif dans `tabs`, le reste dans `open`, indexé par chemin canonique.
    tabs: TabStrip,
    open: HashMap<TabKey, OpenTab>,
    /// Le défilement horizontal de la barre d'onglets : l'onglet actif y reste visible.
    tabs_scroll: ScrollHandle,
    /// Le numéro de la dernière frappe, tous onglets confondus : la sauvegarde d'une frappe n'est due que si aucune
    /// autre ne l'a suivie.
    edit_generation: u64,
    watch: Option<ProjectWatch>,
    _watch_task: Option<Task<()>>,
    /// Le calcul des fichiers ignorés en cours. Jamais deux `git ls-files` à la fois : une demande qui arrive pendant
    /// un calcul n'en lance pas un second, elle note qu'il faudra en refaire un à sa fin.
    _ignored_task: Option<Task<()>>,
    ignored_running: bool,
    ignored_redo: bool,
    /// Le dernier calcul a échoué (hors dépôt, `git` absent) : l'échec n'est journalisé qu'une fois de suite.
    ignored_failing: bool,
    /// Ce que Git suit, relu avec les fichiers ignorés (mêmes déclencheurs). `None` tant que rien n'est revenu, ou si la
    /// lecture a échoué : on ne sait pas ce qui est suivi, « Ajouter au .gitignore » reste éteint.
    tracked: Option<TrackedSet>,
    /// Les dossiers illisibles déjà journalisés : un dossier n'est signalé qu'une fois, jusqu'à ce qu'il se relise.
    unreadable: HashSet<PathBuf>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FilesEvent> for FilesPanel {}

impl FilesPanel {
    /// Le volet du dossier `project`, tel que `state.json` le garde. La racine est canonisée une fois ici ; si le
    /// dossier n'est pas lisible (disparu), le chemin brut reste la racine. `tree_collapsed` et `tree_width` sont
    /// les préférences globales du repli et de la largeur de l'arbre. Seule la racine est lue ; la surveillance démarre à `set_shown(true)`.
    pub fn new(
        project: PathBuf,
        tree_collapsed: bool,
        tree_width: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let root = std::fs::canonicalize(&project).unwrap_or(project);
        let focus_handle = cx.focus_handle();
        let tree_focus = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus_in(&focus_handle, window, |_, _, cx| {
                cx.emit(FilesEvent::FocusChanged);
            }),
            cx.on_focus_out(&focus_handle, window, |_, _, _, cx| {
                cx.emit(FilesEvent::FocusChanged);
            }),
        ];
        let tree = FileTree::new(root.clone());
        let rows = tree.rows();
        let items = (0..rows.len()).map(ListItem::Row).collect();
        let filter = ProjectFilter::new(window, cx);
        Self {
            root_missing: !root.is_dir(),
            root,
            focus_handle,
            tree_focus,
            tree,
            rows,
            items,
            edit: None,
            refocus_tree: false,
            drop_target: None,
            drop_candidate: None,
            pointer_over_list: false,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            filter,
            tree_collapsed,
            tree_width,
            body_width: None,
            tree_drag: None,
            shown: false,
            tabs: TabStrip::default(),
            open: HashMap::new(),
            tabs_scroll: ScrollHandle::new(),
            edit_generation: 0,
            watch: None,
            _watch_task: None,
            _ignored_task: None,
            ignored_running: false,
            ignored_redo: false,
            ignored_failing: false,
            tracked: None,
            unreadable: HashSet::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Le focus est-il dans le volet (lui-même ou un descendant, l'éditeur plus tard) ?
    pub fn contains_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    /// `Workspace` dit si le volet est celui que l'écran montre. Affiché, il surveille le disque, relit ce qu'il a
    /// pu rater (l'arbre, les onglets) et recalcule les fichiers ignorés ; masqué, il sauvegarde ses onglets et ne
    /// garde la surveillance que s'il en a d'ouverts, pour qu'ils se rechargent encore (spec §4.5, cycle de vie).
    pub fn set_shown(&mut self, shown: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.shown == shown {
            return;
        }
        self.shown = shown;
        if !shown {
            // Les écouteurs du glisser ne vivent que tant que la poignée est peinte : masqué, le glisser s'arrête.
            self.cancel_tree_drag(cx);
        }
        if shown {
            self.start_watch(window, cx);
            self.root_missing = !self.root.is_dir();
            if !self.root_missing {
                let outcome = self.tree.reload();
                self.report_read_errors(outcome.errors);
                self.refresh_ignored(cx);
            }
            // Le disque a pu changer pendant que le volet était masqué : la liste des fichiers du filtre se relit.
            self.forget_project_files(cx);
            // Un fichier hors du projet n'est pas surveillé : on le relit à chaque affichage.
            let paths: Vec<PathBuf> = self.open.keys().map(|key| key.0.clone()).collect();
            for path in paths {
                self.reload_tab(&path, window, cx);
            }
            self.rebuild_rows();
            cx.notify();
        } else {
            let _ = self.save_all("hide", cx);
            self.sync_watch(window, cx);
        }
    }

    /// La surveillance sert tant que le volet est affiché ou qu'un onglet est ouvert (spec §4.5).
    fn watch_wanted(&self) -> bool {
        self.shown || !self.tabs.is_empty()
    }

    fn sync_watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.watch_wanted() {
            self.start_watch(window, cx);
        } else {
            self.stop_watch(cx);
        }
    }

    /// Déplie les ancêtres de `path`, le sélectionne et le fait défiler à l'écran. Un chemin hors du projet ne
    /// change rien, mais le journal le dit. Le chemin est comparé sous la forme canonique de la racine.
    pub fn reveal(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path = canonical_path(path);
        if !path.starts_with(&self.root) {
            hud::log_event(&format!("files reveal ignored path={}", path.display()));
            return;
        }
        if let Err(failure) = self.tree.reveal(&path) {
            self.report_read_errors(vec![failure]);
        }
        self.selected = Some(path.clone());
        self.rebuild_rows();
        self.scroll_to_row(&path);
        cx.notify();
    }

    /// Fait défiler la liste jusqu'à la ligne de `path`, au centre. La liste peut porter un champ : l'indice à faire
    /// défiler est celui de la liste, pas celui des lignes.
    fn scroll_to_row(&self, path: &Path) {
        let Some(row) = self.rows.iter().position(|row| row.path == path) else {
            return;
        };
        if let Some(index) = self
            .items
            .iter()
            .position(|item| *item == ListItem::Row(row))
        {
            self.scroll.scroll_to_item(index, ScrollStrategy::Center);
        }
    }

    /// Replie ou déplie l'arbre par un geste (bouton, `⌘⇧E`) : `Workspace` garde la préférence.
    pub fn toggle_tree(&mut self, cx: &mut Context<Self>) {
        self.tree_collapsed = !self.tree_collapsed;
        self.cancel_tree_drag(cx);
        cx.emit(FilesEvent::TreeCollapsedChanged(self.tree_collapsed));
        cx.notify();
    }

    /// La préférence de repli, changée depuis un autre volet : on la suit sans la renvoyer.
    pub fn apply_tree_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if self.tree_collapsed != collapsed {
            self.tree_collapsed = collapsed;
            self.cancel_tree_drag(cx);
            cx.notify();
        }
    }

    /// La préférence de largeur, changée depuis un autre volet : on la suit sans la renvoyer.
    pub fn apply_tree_width(&mut self, width: Option<f32>, cx: &mut Context<Self>) {
        if self.tree_width != width {
            self.tree_width = width;
            cx.notify();
        }
    }

    /// La largeur que la colonne montre : sa bande repliée, ou la largeur voulue rognée au corps mesuré.
    fn shown_tree_width(&self) -> f32 {
        if self.tree_collapsed {
            TREE_COLLAPSED_WIDTH
        } else {
            tree_width_shown(self.tree_width, self.body_width.unwrap_or(f32::MAX))
        }
    }

    /// La peinture a mesuré le corps du volet. Rend `true` si la valeur a changé : à l'appelant de demander une image,
    /// car GPUI ignore les `notify` faits pendant le dessin.
    fn note_body_width(&mut self, width: f32) -> bool {
        let changed = self
            .body_width
            .is_none_or(|known| (known - width).abs() > 0.01);
        if changed {
            self.body_width = Some(width);
        }
        changed
    }

    /// L'appui sur la poignée : le glisser part de la largeur montrée (pas de saut).
    fn begin_tree_drag(&mut self, x: Pixels, cx: &mut Context<Self>) {
        self.tree_drag = Some(ColumnDrag {
            start_x: x,
            start_width: self.shown_tree_width(),
            start_wanted: self.tree_width,
            moved: false,
        });
        cx.notify();
    }

    /// Le mouvement : la largeur voulue devient la largeur montrée, bornée comme au rendu. Rien n'est émis avant le
    /// relâché.
    fn drag_tree_to(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some(drag) = self.tree_drag.as_mut() else {
            return;
        };
        drag.moved = true;
        let scale = cx.global::<Theme>().scale;
        let delta = f32::from(x - drag.start_x) / scale;
        self.tree_width = Some(dragged_tree_width(
            drag.start_width,
            delta,
            self.body_width.unwrap_or(f32::MAX),
        ));
        cx.notify();
    }

    /// Le relâché (ou le relâché perdu) : s'il y a eu mouvement, la largeur est écrite par `Workspace`.
    fn end_tree_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(drag) = self.tree_drag.take() {
            if drag.moved {
                self.announce_tree_width(cx);
            }
            cx.notify();
        }
    }

    /// Le glisser s'arrête sans relâché : le volet est masqué ou l'arbre se replie en plein glisser, et la poignée
    /// n'est plus peinte pour recevoir le relâché. La largeur voulue d'avant l'appui revient et rien n'est écrit,
    /// comme pour les bords de `Workspace`.
    fn cancel_tree_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(drag) = self.tree_drag.take() {
            self.tree_width = drag.start_wanted;
            cx.notify();
        }
    }

    /// Le double-clic sur la poignée : la largeur de la maquette revient, et s'écrit.
    fn reset_tree_width(&mut self, cx: &mut Context<Self>) {
        self.tree_drag = None;
        self.tree_width = None;
        self.announce_tree_width(cx);
        cx.notify();
    }

    /// `Workspace` écrit la largeur dans `state.json` ; le journal la donne aux scripts d'acceptation.
    fn announce_tree_width(&self, cx: &mut Context<Self>) {
        let wanted = self
            .tree_width
            .map_or_else(|| "none".to_string(), |width| format!("{}", width.round()));
        hud::log_event(&format!(
            "files tree width shown={} wanted={wanted} body={}",
            self.shown_tree_width().round(),
            self.body_width.unwrap_or_default().round()
        ));
        cx.emit(FilesEvent::TreeWidthChanged(self.tree_width));
    }

    fn start_watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.watch.is_some() {
            return;
        }
        match ProjectWatch::start(&self.root) {
            Ok((watch, mut batches)) => {
                self.watch = Some(watch);
                self._watch_task = Some(cx.spawn_in(window, async move |this, cx| {
                    while let Some(batch) = batches.next().await {
                        if this
                            .update_in(cx, |panel, window, cx| panel.on_batch(&batch, window, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
            }
            Err(error) => hud::log_event(&format!(
                "files watch failed root={}: {error}",
                self.root.display()
            )),
        }
    }

    fn stop_watch(&mut self, cx: &mut Context<Self>) {
        self._watch_task = None;
        // Lâcher la surveillance bloque un instant (le fil de regroupement s'arrête) : hors du fil d'interface.
        if let Some(watch) = self.watch.take() {
            cx.background_spawn(async move { drop(watch) }).detach();
        }
    }

    /// Un lot de la surveillance : les onglets qu'il touche se relisent (une recherche par chemin dans la table des
    /// onglets, le disque se lit en arrière-plan ; un débordement du watcher recharge tous ceux qui sont sous le dossier
    /// nommé). Pour l'arbre, seuls les dossiers déjà lus qu'il touche sont relus
    /// (un lot sous un dossier replié, `target/` par exemple, ne coûte donc qu'un balayage de ses chemins). Git n'est
    /// relancé que si une règle d'ignorance a changé ou si un dossier lu a gagné une entrée (elle peut être ignorée).
    /// L'écran n'est redessiné que si une ligne a changé. Un volet masqué ne s'occupe que de ses onglets : l'arbre se
    /// relit à son retour.
    fn on_batch(&mut self, batch: &WatchBatch, window: &mut Window, cx: &mut Context<Self>) {
        let paths = &batch.paths;
        if !self.open.is_empty() {
            let touched = touched_tabs(self.open.keys().map(|key| key.0.as_path()), batch);
            for path in touched {
                self.reload_tab(&path, window, cx);
            }
        }
        if !self.shown {
            return;
        }
        let was_missing = self.root_missing;
        self.root_missing = !self.root.is_dir();
        let mut dirty = was_missing != self.root_missing;
        if !self.root_missing {
            if was_missing {
                // Le dossier est revenu : les lectures gardées datent d'avant sa disparition.
                let outcome = self.tree.reload();
                self.report_read_errors(outcome.errors);
                self.refresh_ignored(cx);
                dirty = true;
            } else {
                let mut outcome = self.tree.refresh(paths);
                if !batch.rescan.is_empty() {
                    // Le watcher a débordé : les dossiers nommés se relisent en entier, pas seulement leur parent.
                    let rescanned = self.tree.rescan(&batch.rescan);
                    outcome.changed |= rescanned.changed;
                    outcome.gained_entries |= rescanned.gained_entries;
                    outcome.errors.extend(rescanned.errors);
                }
                dirty |= outcome.changed;
                self.report_read_errors(outcome.errors);
                if outcome.gained_entries
                    || paths.iter().any(|path| is_ignore_rules(&self.root, path))
                {
                    self.refresh_ignored(cx);
                }
            }
        }
        if dirty {
            self.rebuild_rows();
            cx.notify();
        }
        // Un chemin que l'arbre n'ignore pas change la liste des fichiers du filtre ; un lot sous `target/` n'y coûte rien.
        if batch_changes_listing(batch, |path| self.tree.is_ignored(path)) {
            self.forget_project_files(cx);
        }
    }

    /// Journalise les dossiers que l'arbre n'a pas pu lire, un par un et une seule fois chacun, puis oublie ceux qui
    /// se relisent de nouveau (ils seront signalés encore s'ils retombent).
    fn report_read_errors(&mut self, errors: Vec<(PathBuf, io::Error)>) {
        for (dir, error) in &errors {
            if self.unreadable.insert(dir.clone()) {
                hud::log_event(&format!(
                    "files tree read failed path={} error={error}",
                    dir.display()
                ));
            }
        }
        self.unreadable.retain(|dir| {
            errors.iter().any(|(failed, _)| failed == dir)
                || (dir.is_dir() && std::fs::read_dir(dir).is_err())
        });
    }

    /// Recalcule les fichiers ignorés par Git, et ceux qu'il suit (pour éteindre « Ajouter au .gitignore »), hors du fil
    /// d'interface, un calcul à la fois.
    fn refresh_ignored(&mut self, cx: &mut Context<Self>) {
        if self.ignored_running {
            self.ignored_redo = true;
            return;
        }
        self.ignored_running = true;
        let root = self.root.clone();
        self._ignored_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { (git_ignored(&root), git_tracked(&root)) })
                .await;
            let _ = this.update(cx, |panel, cx| panel.finish_ignored(result, cx));
        }));
    }

    /// Ce que Git dit de l'entrée `path`, d'après les derniers calculs : suivie, ignorée. Tant que ce que Git suit n'est
    /// pas connu, l'entrée compte pour suivie (rien n'est proposé au hasard).
    pub fn entry_git(&self, path: &Path) -> EntryGit {
        EntryGit {
            tracked: self
                .tracked
                .as_ref()
                .is_none_or(|tracked| tracked.contains(path)),
            // Git ne regarde pas derrière un lien : une entrée sous un dossier-lien n'a rien à ignorer.
            ignored: self.tree.is_ignored(path) || behind_link(&self.root, path),
        }
    }

    /// Le calcul des ignorés et des suivis est fini : on garde l'ensemble neuf, ou l'ancien si `git` a échoué (journalisé
    /// une fois). Les suivis, eux, tombent à `None` sur un échec : mieux vaut ne rien proposer que proposer au hasard. Une
    /// demande arrivée entre-temps relance un calcul.
    fn finish_ignored(
        &mut self,
        (result, tracked): (io::Result<IgnoredSet>, io::Result<TrackedSet>),
        cx: &mut Context<Self>,
    ) {
        self.ignored_running = false;
        self.tracked = tracked.ok();
        match result {
            Ok(ignored) => {
                self.ignored_failing = false;
                self.tree.set_ignored(ignored);
                self.rebuild_rows();
                cx.notify();
            }
            Err(error) => {
                if !self.ignored_failing {
                    self.ignored_failing = true;
                    hud::log_event(&format!(
                        "files ignored failed root={} error={error}",
                        self.root.display()
                    ));
                }
            }
        }
        if self.ignored_redo {
            self.ignored_redo = false;
            self.refresh_ignored(cx);
        }
    }

    /// Relit les lignes du modèle et recompose la liste dessinée. Un champ qui n'a plus de place (son dossier est replié
    /// ou a disparu) est fermé : le prochain rendu rend le focus à l'arbre.
    fn rebuild_rows(&mut self) {
        self.rows = self.tree.rows();
        let slot = self.edit.as_ref().map(|edit| &edit.slot);
        self.items = match list_items(&self.rows, &self.root, slot) {
            Some(items) => items,
            None => {
                self.edit = None;
                self.refocus_tree = true;
                (0..self.rows.len()).map(ListItem::Row).collect()
            }
        };
    }

    /// Le focus de la colonne de l'arbre ; replié, l'arbre n'est plus peint et le corps du volet le garde.
    fn tree_focus_handle(&self) -> &FocusHandle {
        if self.tree_collapsed {
            &self.focus_handle
        } else {
            &self.tree_focus
        }
    }

    /// Un appui droit dans l'éditeur : `Workspace` ouvre le menu. Rien ne bouge (ni curseur, ni sélection, ni focus)
    /// avant le choix d'une entrée.
    fn on_editor_menu_requested(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.active_path() else {
            return;
        };
        let Some(tab) = self.open.get(path.as_path()) else {
            return;
        };
        let selection = tab.editor.read(cx).selected_range();
        window.prevent_default();
        cx.emit(FilesEvent::EditorMenu {
            position,
            has_selection: selection.start != selection.end,
        });
    }

    /// Exécute un geste du menu de l'éditeur. Le menu a pris le focus : l'éditeur le reprend d'abord, puis l'action du
    /// kit part vers lui, comme celles du menu « Édition ».
    pub fn run_editor_gesture(
        &mut self,
        gesture: EditorGesture,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_active_editor(window, cx);
        match gesture {
            EditorGesture::CopyToAgent => {
                if let Some(reference) = self.active_reference(cx) {
                    cx.emit(FilesEvent::CopyToAgent(reference));
                }
            }
            EditorGesture::Cut => window.dispatch_action(Box::new(gpui_component::input::Cut), cx),
            EditorGesture::Copy => {
                window.dispatch_action(Box::new(gpui_component::input::Copy), cx);
            }
            EditorGesture::Paste => {
                window.dispatch_action(Box::new(gpui_component::input::Paste), cx);
            }
            EditorGesture::SelectAll => {
                window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx);
            }
        }
    }

    /// Ouvre `path` dans un onglet (aperçu, remplacé par l'ouverture suivante, sauf si `pin` le rend permanent) ou
    /// active son onglet s'il est déjà ouvert, et donne le focus à son éditeur : c'est l'entrée des gestes qui viennent
    /// d'ailleurs que de l'arbre (⌘-clic, dialogue, Personnaliser). Un onglet déjà permanent le reste, que `pin` soit
    /// vrai ou non. `position` est une ligne et une colonne à partir de 1 ; un fichier hors du
    /// projet s'ouvre quand même (spec §4.5). Un fichier qui n'est pas du texte UTF-8, trop gros ou illisible part
    /// dans l'app par défaut, sans onglet (spec §4.2 et §5). Le chemin est canonisé ici (arbitrage 5).
    pub fn open_path(
        &mut self,
        path: &Path,
        position: Option<(u32, u32)>,
        pin: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file(path, position, true, window, cx);
        // Un fichier confié à l'app par défaut n'a pas d'onglet : rien n'est épinglé alors.
        if pin && let Some(index) = self.tabs.index_of(&canonical_path(path)) {
            self.tabs.pin(index);
            cx.notify();
        }
    }

    /// Le fichier `path` a-t-il un onglet dans ce volet ? Le chemin est canonisé comme à l'ouverture.
    pub fn has_open(&self, path: &Path) -> bool {
        self.tabs.index_of(&canonical_path(path)).is_some()
    }

    /// Ferme l'onglet de `path` pour qu'il s'ouvre dans le volet d'un autre projet (un fichier n'a jamais qu'un onglet,
    /// tous volets confondus). Le texte en attente part sur le disque d'abord, avant que le minuteur de frappe ne
    /// disparaisse avec l'onglet. Rend `Some(épinglé)` quand l'onglet est fermé, `None` quand il n'y en avait pas ou
    /// qu'il reste faute d'avoir pu écrire (comme `close_tab`, un texte non sauvegardé n'est jamais perdu). Le volet
    /// n'est pas affiché : le focus n'est pas touché.
    pub fn release_file(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        let path = canonical_path(path);
        let index = self.tabs.index_of(&path)?;
        let pinned = !self.tabs.is_preview(index);
        let _ = self.save_tab_with(&path, Some(&mut *window), true, cx);
        if self.keeps_unsaved_text(&path, cx) {
            hud::log_event(&format!(
                "files tab move refused path={}: unsaved text",
                path.display()
            ));
            cx.notify();
            return None;
        }
        self.tabs.close(index);
        self.open.remove(path.as_path());
        self.reveal_active_tab();
        self.sync_watch(window, cx);
        cx.notify();
        Some(pinned)
    }

    /// `open_path`, avec le choix du focus : un clic simple dans l'arbre ouvre l'aperçu sans quitter l'arbre.
    fn open_file(
        &mut self,
        path: &Path,
        position: Option<(u32, u32)>,
        focus_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = canonical_path(path);
        if let Some(index) = self.tabs.index_of(&path) {
            self.activate_tab(index, focus_editor, window, cx);
        } else {
            // L'empreinte avant la lecture : une écriture entre les deux est vue à la sauvegarde.
            let opened = fingerprint(&path).ok().flatten();
            let loaded = match read_document(&path) {
                Ok(Opened::Text(loaded)) => loaded,
                Ok(Opened::External) => {
                    self.open_externally(&path, window, cx);
                    return;
                }
                Err(error) => {
                    hud::log_event(&format!(
                        "file read failed path={}: {error}",
                        path.display()
                    ));
                    self.open_externally(&path, window, cx);
                    return;
                }
            };
            self.add_tab(&path, loaded, opened, focus_editor, window, cx);
        }
        if let (Some((line, column)), Some(tab)) = (position, self.open.get(path.as_path())) {
            let target = Position {
                line: line.saturating_sub(1),
                character: column.saturating_sub(1),
            };
            // Placer le curseur donne aussi le focus à l'éditeur.
            tab.editor.update(cx, |state, cx| {
                state.set_cursor_position(target, window, cx)
            });
            hud::log_event(&format!(
                "files opened path={} line={line} column={column}",
                path.display()
            ));
        }
    }

    /// Confie le fichier à l'app par défaut ; si `open` échoue, la bulle donne le message brut (spec §5). Un exécutable
    /// ou une archive n'est jamais passé à `open` (il serait lancé ou extrait dans le projet) : le Finder le révèle.
    fn open_externally(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let outcome = match external_action(path) {
            ExternalAction::Open => open_with_default_app(path),
            ExternalAction::Reveal => reveal_in_finder(path),
        };
        self.alert_on_failure(path, outcome, "Impossible d'ouvrir", window, cx);
    }

    /// Attend le résultat d'un `open` et, s'il échoue, ouvre la bulle (`failure` suivi du nom, puis le brut en détail).
    fn alert_on_failure(
        &mut self,
        path: &Path,
        outcome: Receiver<Result<(), String>>,
        failure: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        cx.spawn_in(window, async move |_, cx| {
            if let Ok(Err(raw)) = outcome.await {
                let _ = cx.update(|window, cx| {
                    alert(window, cx, &format!("{failure} {name}."), Some(&raw));
                });
            }
        })
        .detach();
    }

    /// Un onglet de plus pour `path` (l'aperçu en place est remplacé), avec son éditeur : langue de coloration,
    /// retour à la ligne, indentation du fichier, menu natif coupé. L'onglet qu'on quitte part sur le disque d'abord.
    fn add_tab(
        &mut self,
        path: &Path,
        loaded: LoadedText,
        opened: Option<Fingerprint>,
        focus_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_active(window, cx);
        let LoadedText {
            text,
            line_ending,
            indent,
        } = loaded;
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language(language_for(path).unwrap_or("text"))
                .soft_wrap(true)
                .tab_size(tab_size_for(indent))
                .context_menu(false);
            state.set_value(text.clone(), window, cx);
            state
        });
        let subscription = Self::subscribe_editor(&editor, path, window, cx);
        self.tabs.open(path.to_path_buf(), false);
        // L'onglet d'aperçu remplacé disparaît avec son éditeur.
        let kept: HashSet<&Path> = self.tabs.paths().into_iter().collect();
        self.open.retain(|key, _| kept.contains(key.0.as_path()));
        let title = tab_title(path, &self.root, home_dir().as_deref());
        if focus_editor {
            editor.update(cx, |state, cx| state.focus(window, cx));
        }
        let mut save = SaveState::new(text);
        save.set_fingerprint(opened);
        self.open.insert(
            TabKey(path.to_path_buf()),
            OpenTab {
                editor,
                save,
                line_ending,
                title,
                _save_timer: None,
                reloading: false,
                reload_redo: false,
                resave: false,
                _reload_task: None,
                _subscription: subscription,
            },
        );
        hud::log_event(&format!("files opened path={}", path.display()));
        self.reveal_active_tab();
        self.sync_watch(window, cx);
        cx.notify();
    }

    /// L'abonnement aux événements de l'éditeur de `path`. La fermeture porte le chemin : un onglet qui change de chemin
    /// (renommage, déplacement) refait son abonnement.
    fn subscribe_editor(
        editor: &Entity<EditorState>,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let tab_path = path.to_path_buf();
        cx.subscribe_in(
            editor,
            window,
            move |this, _, event: &InputEvent, window, cx| {
                this.on_editor_event(&tab_path, event, window, cx);
            },
        )
    }

    /// Rend actif l'onglet `index` (celui qu'on quitte est sauvegardé) et, si `focus_editor`, donne le focus à son
    /// éditeur.
    fn activate_tab(
        &mut self,
        index: usize,
        focus_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.active() != Some(index) {
            self.save_active(window, cx);
        }
        self.tabs.activate(index);
        self.reveal_active_tab();
        if focus_editor {
            self.focus_active_editor(window, cx);
        }
        cx.notify();
    }

    /// Fait défiler la barre jusqu'à l'onglet actif.
    fn reveal_active_tab(&self) {
        if let Some(index) = self.tabs.active() {
            self.tabs_scroll.scroll_to_item(index);
        }
    }

    /// Donne le focus à l'arbre, pas à l'éditeur : là où un dossier vient d'être révélé, ou un champ fermé.
    pub fn focus_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(self.tree_focus_handle(), cx);
    }

    fn focus_active_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self
            .active_path()
            .and_then(|path| self.open.get(path.as_path()))
        {
            Some(tab) => tab.editor.update(cx, |state, cx| state.focus(window, cx)),
            None => self.focus_tree(window, cx),
        }
    }

    fn active_path(&self) -> Option<PathBuf> {
        let index = self.tabs.active()?;
        self.tabs.paths().get(index).map(|path| path.to_path_buf())
    }

    /// La référence de l'onglet actif pour l'agent : le chemin et les lignes de la sélection, ou la ligne du curseur
    /// quand rien n'est sélectionné. `None` sans onglet ouvert.
    pub fn active_reference(&self, cx: &App) -> Option<String> {
        let path = self.active_path()?;
        let tab = self.open.get(path.as_path())?;
        let (text, selection) = tab
            .editor
            .read_with(cx, |state, _| (state.value(), state.selected_range()));
        // Le sens de la sélection ne compte pas : `selection_lines` veut le début avant la fin.
        let ordered = selection.start.min(selection.end)..selection.start.max(selection.end);
        let lines = selection_lines(&text, ordered);
        Some(agent_reference(&path, home_dir().as_deref(), Some(lines)))
    }

    /// Applique à l'éditeur actif l'édition de lignes que rend `edit` : un remplacement, donc un seul pas d'annulation,
    /// et l'onglet se sauvegarde comme après une frappe.
    fn edit_active_lines(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&str, std::ops::Range<usize>) -> Option<LineEdit>,
    ) {
        let Some(path) = self.active_path() else {
            return;
        };
        let Some(tab) = self.open.get(path.as_path()) else {
            return;
        };
        tab.editor.update(cx, |state, cx| {
            let Some(edit) = edit(&state.value(), state.selected_range()) else {
                return;
            };
            state.set_selected_range(edit.range, cx);
            state.replace(edit.text, window, cx);
            state.set_selected_range(edit.selection, cx);
        });
    }

    /// Rend permanent l'onglet actif (double-clic).
    fn pin_active(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.active() {
            self.tabs.pin(index);
            cx.notify();
        }
    }

    /// Ferme l'onglet `index` après l'avoir sauvegardé ; le voisin devient actif. Le dernier onglet fermé rend la
    /// surveillance au seul affichage du volet. Un onglet dont le texte n'a pas pu être écrit ne se ferme pas (ruling
    /// 21) : il reste, marqué « non sauvegardé », et la bulle de la panne s'affiche de nouveau pour ce geste.
    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.tabs.paths().get(index).map(|path| path.to_path_buf()) else {
            return;
        };
        let _ = self.save_tab_with(&path, Some(&mut *window), true, cx);
        if self.keeps_unsaved_text(&path, cx) {
            hud::log_event(&format!(
                "file close refused path={}: unsaved text",
                path.display()
            ));
            cx.notify();
            return;
        }
        self.tabs.close(index);
        self.open.remove(path.as_path());
        self.reveal_active_tab();
        self.focus_active_editor(window, cx);
        self.sync_watch(window, cx);
        cx.notify();
    }

    /// `⌘S` : sauvegarde l'onglet actif tout de suite.
    fn save_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_active(window, cx);
    }

    fn save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.active_path() {
            self.save_tab(&path, Some(window), cx);
        }
    }

    /// L'onglet de `path` a-t-il du texte que le disque n'a pas (après l'essai de sauvegarde) ? Le tampon de
    /// l'éditeur est comparé à ce que le disque a : un `⌘Z` revenu à l'original ne retient rien (ruling 22).
    fn keeps_unsaved_text(&self, path: &Path, cx: &Context<Self>) -> bool {
        self.open
            .get(path)
            .is_some_and(|tab| tab.save.blocks_close(&tab.editor.read(cx).value()))
    }

    /// Sauvegarde tous les onglets qui ont du texte à écrire, tout de suite et sans fenêtre : fermeture du volet,
    /// changement de projet de contexte, `⌘Q`, retrait d'un projet. Un échec reste marqué dans l'onglet (pas de bulle
    /// sans fenêtre) et le journal dit ce qui n'a pas pu partir (`on` : « quit », « hide », « remove »). Rend les
    /// titres des onglets dont le texte n'a pas pu partir (l'appelant avec une fenêtre peut le dire).
    pub fn save_all(&mut self, on: &str, cx: &mut Context<Self>) -> Vec<String> {
        let paths: Vec<PathBuf> = self.open.keys().map(|key| key.0.clone()).collect();
        let mut unsaved = Vec::new();
        for path in paths {
            self.save_tab(&path, None, cx);
            if self.keeps_unsaved_text(&path, cx) {
                hud::log_event(&format!("file save failed on {on} path={}", path.display()));
                if let Some(tab) = self.open.get(path.as_path()) {
                    unsaved.push(tab.title.clone());
                }
            }
        }
        unsaved
    }

    fn on_editor_event(
        &mut self,
        path: &Path,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => self.on_edit(path, window, cx),
            InputEvent::Blur => self.save_tab(path, Some(window), cx),
            InputEvent::Focus | InputEvent::PressEnter { .. } => {}
        }
    }

    /// Une frappe : l'onglet devient permanent et la sauvegarde d'une seconde repart de zéro.
    fn on_edit(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_generation += 1;
        let generation = self.edit_generation;
        if let Some(index) = self.tabs.index_of(path) {
            self.tabs.pin(index);
        }
        let Some(tab) = self.open.get_mut(path) else {
            return;
        };
        tab.save.on_edit(generation);
        let path = path.to_path_buf();
        let background = cx.background_executor().clone();
        tab._save_timer = Some(cx.spawn_in(window, async move |this, cx| {
            background.timer(AUTOSAVE_DELAY).await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.autosave(&path, generation, window, cx);
            });
        }));
        cx.notify();
    }

    /// Le minuteur de la frappe `generation` a sonné : on écrit si c'est encore la dernière et si le disque ne l'a
    /// pas annulée.
    fn autosave(
        &mut self,
        path: &Path,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .open
            .get(path)
            .is_some_and(|tab| tab.save.due(generation))
        {
            self.save_tab(path, Some(window), cx);
        }
    }

    /// Écrit le texte de l'onglet s'il a des frappes en attente ou une écriture refusée à rejouer, mais jamais par-dessus
    /// un disque qui a changé : l'empreinte du fichier est comparée d'abord, et un fichier modifié ou disparu depuis
    /// la dernière lecture ou écriture d'IdemA passe par le rechargement (le disque gagne, P2-7, sans attendre la
    /// surveillance, qui met 100 ms à parler et ne voit pas les fichiers hors projet). Sans fenêtre (`⌘Q`, fermeture du
    /// volet), le rechargement est impossible : l'écriture est sautée, ce que la surveillance ou le prochain affichage
    /// reprendra. Un refus d'écriture laisse le texte dans l'onglet, le marque « non sauvegardé » et, la première fois,
    /// ouvre une bulle (avec une fenêtre).
    fn save_tab(&mut self, path: &Path, window: Option<&mut Window>, cx: &mut Context<Self>) {
        let _ = self.save_tab_with(path, window, false, cx);
    }

    /// `save_tab`, avec le choix de la bulle : `alert_every_failure` la montre à chaque refus et non à la première
    /// panne de la série seulement (la fermeture d'un onglet, qui reste ouvert faute d'avoir pu écrire). Rend ce qui s'est
    /// passé : le remplacement dans le projet en tire son bilan. Un onglet qui n'a rien en attente rend `Unchanged` si
    /// son texte est celui du disque, `DiskChanged` sinon (rien n'a été écrit) ; un onglet absent rend `DiskChanged`.
    fn save_tab_with(
        &mut self,
        path: &Path,
        window: Option<&mut Window>,
        alert_every_failure: bool,
        cx: &mut Context<Self>,
    ) -> TabSave {
        let Some(tab) = self.open.get_mut(path) else {
            return TabSave::DiskChanged;
        };
        if !tab.save.needs_save() {
            return idle_save(tab.save.holds(&tab.editor.read(cx).value()));
        }
        // Une métadonnée illisible n'est pas un changement : l'écriture dira la vraie panne.
        if let Ok(disk) = fingerprint(path)
            && tab.save.check_write(disk) == SaveCheck::DiskChanged
        {
            self.route_to_reload(path, window, cx);
            return TabSave::DiskChanged;
        }
        let text = tab.editor.read(cx).value().to_string();
        // L'empreinte est relue une dernière fois juste avant le `rename` : un agent qui écrit entre la vérification
        // ci-dessus et le remplacement n'est pas écrasé.
        let saved = match save_unless_changed(path, &text, tab.line_ending, tab.save.fingerprint())
        {
            Ok(Saved::Written) => {
                tab.save.on_saved(text);
                tab.save.set_fingerprint(fingerprint(path).ok().flatten());
                hud::log_event(&format!("file saved path={}", path.display()));
                TabSave::Written
            }
            Ok(Saved::DiskChanged) => {
                self.route_to_reload(path, window, cx);
                return TabSave::DiskChanged;
            }
            Err(error) => {
                let first = tab.save.on_save_failed(error.to_string());
                hud::log_event(&format!(
                    "file save failed path={}: {error}",
                    path.display()
                ));
                if let (true, Some(window)) = (first || alert_every_failure, window) {
                    // Le texte du catalogue en titre, le fichier et le brut en détail (spec §5).
                    let Explained { text, detail } = explain_write(&error);
                    let raw = detail.unwrap_or_default();
                    alert(window, cx, &text, Some(&format!("{}\n{raw}", tab.title)));
                }
                TabSave::Failed(error)
            }
        };
        cx.notify();
        saved
    }

    /// Le disque a changé sous une sauvegarde : le disque gagne. Avec une fenêtre l'onglet se recharge, et la
    /// sauvegarde repart si le rechargement n'y trouve rien de neuf ; sans fenêtre (`⌘Q`, fermeture du volet) le
    /// rechargement est impossible et l'écriture est sautée.
    fn route_to_reload(
        &mut self,
        path: &Path,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        match window {
            Some(window) => {
                if let Some(tab) = self.open.get_mut(path) {
                    tab.resave = true;
                }
                self.reload_tab(path, window, cx);
            }
            None => hud::log_event(&format!(
                "file save skipped path={}: changed on disk",
                path.display()
            )),
        }
    }

    /// Relit le fichier de l'onglet en arrière-plan puis applique le verdict de `SaveState::on_disk_change`. Une
    /// lecture déjà en cours en note une seconde, qui part à sa fin.
    fn reload_tab(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.open.get_mut(path) else {
            return;
        };
        if tab.reloading {
            tab.reload_redo = true;
            return;
        }
        tab.reloading = true;
        let path = path.to_path_buf();
        let background = cx.background_executor().clone();
        tab._reload_task = Some(cx.spawn_in(window, async move |this, cx| {
            let read_path = path.clone();
            // L'empreinte avant le contenu : une écriture entre les deux est vue à la sauvegarde suivante.
            let (disk, result) = background
                .spawn(async move {
                    (
                        fingerprint(&read_path).ok().flatten(),
                        read_document(&read_path),
                    )
                })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_reload(&path, disk, result, window, cx);
            });
        }));
    }

    /// Le disque est lu : le disque gagne toujours. Le texte reçu remplace celui de l'éditeur, qui garde sa sélection
    /// et son défilement bornés à la nouvelle longueur ; un fichier disparu est marqué « supprimé » ; un fichier qui
    /// n'est plus du texte lisible marque l'onglet « illisible » et le laisse comme il est.
    fn finish_reload(
        &mut self,
        path: &Path,
        disk_print: Option<Fingerprint>,
        result: io::Result<Opened>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.open.get_mut(path) else {
            return;
        };
        tab.reloading = false;
        let disk = match result {
            Ok(Opened::Text(loaded)) => Some(loaded),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Ok(Opened::External) => {
                let error = "not editable text (binary, not UTF-8 or over 5 MB)".to_owned();
                self.mark_unreadable(path, error, window, cx);
                return;
            }
            Err(error) => {
                self.mark_unreadable(path, error.to_string(), window, cx);
                return;
            }
        };
        tab.line_ending = line_ending_after_read(tab.line_ending, disk.as_ref());
        let buffer = tab.editor.read(cx).value();
        let verdict = tab
            .save
            .on_disk_change(disk.as_ref().map(|loaded| loaded.text.as_str()), &buffer);
        // Ce que le disque a maintenant est ce que l'onglet sait de lui (un fichier supprimé n'a pas d'empreinte).
        if verdict != DiskVerdict::Deleted {
            tab.save.set_fingerprint(disk_print);
        }
        let resave = std::mem::take(&mut tab.resave);
        match verdict {
            DiskVerdict::Ignore => {}
            DiskVerdict::Reload { pending_edit } => {
                if let Some(loaded) = disk {
                    tab.editor.update(cx, |state, cx| {
                        let selection = state.selected_range();
                        let scroll = state.scroll_offset();
                        state.set_tab_size(tab_size_for(loaded.indent), cx);
                        state.set_value(loaded.text, window, cx);
                        let length = state.text().len();
                        state.set_selected_range(
                            selection.start.min(length)..selection.end.min(length),
                            cx,
                        );
                        state.set_scroll_offset(scroll, cx);
                    });
                    hud::log_event(&format!(
                        "file reloaded path={} pending_edit={pending_edit}",
                        path.display()
                    ));
                }
            }
            DiskVerdict::Deleted => {
                hud::log_event(&format!("file deleted path={}", path.display()));
            }
        }
        cx.notify();
        // Une sauvegarde refusée pour un disque qui a changé, et le disque n'a rien de neuf à reprendre (même
        // contenu qu'avant) : les frappes en attente partent maintenant.
        if resave && verdict == DiskVerdict::Ignore {
            self.save_tab(path, Some(window), cx);
        }
        self.redo_reload(path, window, cx);
    }

    /// Le disque porte un fichier que l'éditeur ne lit plus (binaire, non UTF-8, trop gros, refusé) : l'onglet garde son
    /// texte, prend la marque « illisible », ne sauvegarde plus par-dessus et le journal le dit une fois par série.
    fn mark_unreadable(
        &mut self,
        path: &Path,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.open.get_mut(path) {
            tab.resave = false;
            if tab.save.on_unreadable(error.clone()) {
                hud::log_event(&format!(
                    "file reload failed path={} error={error}",
                    path.display()
                ));
            }
        }
        cx.notify();
        self.redo_reload(path, window, cx);
    }

    /// Un lot est arrivé pendant la lecture : on relit une fois de plus.
    fn redo_reload(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.open.get_mut(path)
            && std::mem::take(&mut tab.reload_redo)
        {
            self.reload_tab(path, window, cx);
        }
    }

    /// Une tab de la barre : titre (italique en aperçu), marque « non sauvegardé » ou « supprimé », croix au survol.
    fn render_tab(
        &self,
        index: usize,
        path: &Path,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let tab = self.open.get(path)?;
        let active = self.tabs.active() == Some(index);
        let group = format!("files-tab-{index}");
        let title = div()
            .min_w_0()
            .max_w(theme.px(if tab.title.contains('/') {
                OUTSIDE_TAB_TITLE_MAX_WIDTH
            } else {
                TAB_TITLE_MAX_WIDTH
            }))
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .when(self.tabs.is_preview(index), |title| title.italic())
            .child(tab.title.clone());
        let close = div()
            .id(("files-tab-close", index))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(theme.px(16.))
            .rounded(theme.px(4.))
            .cursor_pointer()
            .opacity(0.)
            .group_hover(group.clone(), |style| style.opacity(1.))
            .hover(|style| style.bg(white(HOVER_STRONG)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.prevent_default();
                    this.close_tab(index, window, cx);
                }),
            )
            .child(
                svg()
                    .path(Icon::Close.path())
                    .size(theme.px(12.))
                    .text_color(white(ICON_SECONDARY)),
            );
        Some(
            div()
                .id(("files-tab", index))
                .group(group)
                .flex()
                .flex_none()
                .items_center()
                .gap(theme.px(TAB_GAP))
                .h(theme.px(TAB_HEIGHT))
                .pl(theme.px(TAB_PADDING_LEFT))
                .pr(theme.px(5.))
                .rounded(theme.px(TAB_RADIUS))
                .cursor_pointer()
                .text_size(theme.text(12.5))
                .text_color(white(if active { TEXT_FULL } else { TEXT_SECONDARY }))
                .when(active, |tab| tab.bg(white(HOVER_STRONG)))
                .when(!active, |tab| {
                    tab.hover(|style| style.text_color(white(TEXT_HOVER)))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        window.prevent_default();
                        this.activate_tab(index, true, window, cx);
                        if event.click_count >= 2 {
                            this.pin_active(cx);
                        }
                    }),
                )
                .child(title)
                .when_some(tab.save.mark(), |tab, mark| {
                    tab.child(
                        div()
                            .flex_none()
                            .text_size(theme.text(10.5))
                            .text_color(white(TEXT_SECONDARY))
                            .child(mark),
                    )
                })
                .child(close),
        )
    }

    /// La zone de l'éditeur : la barre d'onglets, puis l'éditeur de l'onglet actif. Sans onglet, elle reste vide.
    fn render_editor_area(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        // L'éditeur garde `EDITOR_MIN_WIDTH` tant que l'arbre est au-dessus de son plancher ; à son plancher, l'arbre ne cède
        // plus et l'éditeur prend ce qui reste (volet de 364 : le comportement du jalon 3).
        let area = div().flex_1().h_full().flex().flex_col();
        let area = if self.shown_tree_width() > TREE_WIDTH {
            area.min_w(theme.px(EDITOR_MIN_WIDTH))
        } else {
            area.min_w_0()
        };
        let active = self
            .active_path()
            .and_then(|path| self.open.get(path.as_path()));
        let Some(active) = active else {
            return area;
        };
        let paths: Vec<PathBuf> = self
            .tabs
            .paths()
            .into_iter()
            .map(Path::to_path_buf)
            .collect();
        let bar = div()
            .id("files-tabs")
            .flex_none()
            .h(theme.px(TAB_BAR_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .gap(theme.px(2.))
            .px(theme.px(6.))
            .overflow_x_scroll()
            .track_scroll(&self.tabs_scroll)
            .border_b_1()
            .border_color(white(SEPARATOR))
            .children(
                paths
                    .iter()
                    .enumerate()
                    .filter_map(|(index, path)| self.render_tab(index, path, theme, cx)),
            );
        area.child(bar).child(
            div()
                .flex_1()
                .min_h_0()
                // L'éditeur ignore le bouton droit (`context_menu(false)`) : le clic arrive ici, curseur intact.
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        this.on_editor_menu_requested(event.position, window, cx);
                    }),
                )
                .child(Editor::new(&active.editor).bordered(false).size_full()),
        )
    }
}

impl Focusable for FilesPanel {
    /// Là où la saisie doit aller quand `Workspace` rend le focus au volet : l'éditeur de l'onglet actif, sinon le corps
    /// du volet.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_path()
            .and_then(|path| self.open.get(path.as_path()))
            .map_or_else(
                || self.tree_focus_handle().clone(),
                |tab| tab.editor.read(cx).focus_handle(cx),
            )
    }
}

impl Render for FilesPanel {
    /// La zone sous l'en-tête : l'arbre à gauche, les onglets et l'éditeur à droite. Elle porte le focus du volet (un
    /// appui y reste, `root_focus` de `Workspace` ne le renvoie pas à la session), le contexte de touches et les
    /// actions `⌘W` et `⌘S`. Sans dossier de projet, elle ne montre que le message.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        // Le champ a perdu sa place pendant qu'il avait le focus : l'arbre le reprend.
        if std::mem::take(&mut self.refocus_tree) {
            self.focus_tree(window, cx);
        }
        // L'arbre vient de se replier (⌘⇧E, le bouton, ou l'autre volet) : sa colonne n'est plus peinte. Un champ ouvert
        // est annulé, et le focus qu'il ou l'arbre avait retombe sur le corps du volet (sinon `⌘⇧E` ne trouverait plus de
        // contexte pour déplier).
        if self.tree_collapsed {
            let field_focused = self
                .edit
                .as_ref()
                .is_some_and(|edit| edit.field.focus_handle(cx).is_focused(window));
            if self.edit.take().is_some() {
                self.rebuild_rows();
            }
            // Le champ du filtre est un descendant de la colonne : lui aussi rend son focus au corps du volet.
            if field_focused || self.tree_focus.contains_focused(window, cx) {
                window.focus(&self.focus_handle, cx);
            }
        }
        let body = div()
            .id("files-body")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .flex();
        if self.root_missing {
            // Aucun gestionnaire de `CloseTab` ici : `⌘W` se propage à `CloseSession`, comme sans onglet.
            return body.child(
                div()
                    .p(theme.px(16.))
                    .text_size(theme.text(12.5))
                    .text_color(white(TEXT_MIN))
                    .child("Le dossier du projet n'existe plus."),
            );
        }
        body.on_action(cx.listener(|this, _: &CloseTab, window, cx| {
            match this.tabs.active() {
                Some(index) => this.close_tab(index, window, cx),
                // Sans onglet, `⌘W` reste celui de `Workspace` : l'action se propage jusqu'à `CloseSession`.
                None => cx.propagate(),
            }
        }))
        .on_action(cx.listener(|this, _: &SaveNow, window, cx| {
            this.save_now(window, cx);
        }))
        // Le menu « Édition » envoie les actions d'IdemA (`menus::Copy`…), que l'éditeur ne connaît pas : elles remontent
        // jusqu'ici et repartent vers l'éditeur en actions du kit. Sans éditeur au focus, elles ne font rien.
        .on_action(cx.listener(|_, _: &crate::menus::Cut, window, cx| {
            window.dispatch_action(Box::new(gpui_component::input::Cut), cx);
        }))
        .on_action(cx.listener(|_, _: &crate::menus::Copy, window, cx| {
            window.dispatch_action(Box::new(gpui_component::input::Copy), cx);
        }))
        .on_action(cx.listener(|_, _: &crate::menus::Paste, window, cx| {
            window.dispatch_action(Box::new(gpui_component::input::Paste), cx);
        }))
        .on_action(cx.listener(|_, _: &crate::menus::SelectAll, window, cx| {
            window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx);
        }))
        .on_action(cx.listener(|this, _: &MoveLineUp, window, cx| {
            this.edit_active_lines(window, cx, |text, selection| {
                move_lines(text, selection, Direction::Up)
            });
        }))
        .on_action(cx.listener(|this, _: &MoveLineDown, window, cx| {
            this.edit_active_lines(window, cx, |text, selection| {
                move_lines(text, selection, Direction::Down)
            });
        }))
        .on_action(cx.listener(|this, _: &DuplicateLineUp, window, cx| {
            this.edit_active_lines(window, cx, |text, selection| {
                Some(duplicate_lines(text, selection, Direction::Up))
            });
        }))
        .on_action(cx.listener(|this, _: &DuplicateLineDown, window, cx| {
            this.edit_active_lines(window, cx, |text, selection| {
                Some(duplicate_lines(text, selection, Direction::Down))
            });
        }))
        .on_action(cx.listener(|this, _: &CopyToAgent, _, cx| {
            if let Some(reference) = this.active_reference(cx) {
                cx.emit(FilesEvent::CopyToAgent(reference));
            }
        }))
        // Derrière tout : la mesure du corps, qui décide de ce que le plancher de l'éditeur laisse à l'arbre.
        .child(column::measure_body(cx.entity(), Self::note_body_width))
        .child(self.render_tree(&theme, cx))
        .child(self.render_editor_area(&theme, cx))
        // Par-dessus : la poignée du bord droit de la colonne (replié, il n'y a rien à tirer).
        .when(!self.tree_collapsed, |body| {
            body.child(self.render_tree_handle(&theme, cx))
        })
    }
}

/// Le volet : en-tête commun aux volets droits (pastille « Fichiers » et projet, agrandir, fermer) puis le corps, qui
/// est l'entité `FilesPanel`. `project` est le nom affiché du projet ; `header_inset` est le retrait gauche de l'en-tête.
pub fn render_panel(
    project: String,
    panel: Entity<FilesPanel>,
    maximized: bool,
    header_inset: Pixels,
    width: Pixels,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let header = panel_header(
        theme,
        Icon::Folder,
        "Fichiers",
        project,
        ("files-maximize", "files-close"),
        maximized,
        header_inset,
        |this, window, cx| this.focus_files(window, cx),
        |this, window, cx| this.toggle_files_maximized(window, cx),
        |this, window, cx| this.close_files(window, cx),
        cx,
    );
    panel_shell("files-panel", maximized, width)
        .child(header)
        .child(div().flex_1().min_h_0().child(panel))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("idema-panel-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn tree_width_shown_defaults_to_the_mockup_width() {
        assert_eq!(tree_width_shown(None, 420.), 180.);
    }

    #[test]
    fn tree_width_shown_leaves_the_editor_its_floor() {
        assert_eq!(tree_width_shown(Some(400.), 420.), 220.);
        assert_eq!(tree_width_shown(Some(260.), 600.), 260.);
    }

    #[test]
    fn tree_width_shown_never_goes_under_the_mockup_width() {
        assert_eq!(tree_width_shown(Some(100.), 420.), 180.);
        // Au volet le plus étroit (364), le plancher de l'éditeur ne rogne pas l'arbre sous sa largeur de base.
        assert_eq!(tree_width_shown(Some(260.), 364.), 180.);
    }

    #[test]
    fn a_tree_drag_writes_the_shown_width() {
        // Tirée loin à droite, la colonne s'arrête au plancher de l'éditeur : c'est cette largeur qui est gardée.
        assert_eq!(dragged_tree_width(180., 500., 420.), 220.);
        // Tirée vers la gauche, elle suit la souris, puis butte sur sa largeur de base.
        assert_eq!(dragged_tree_width(220., -100., 420.), 180.);
        assert_eq!(dragged_tree_width(260., -30., 600.), 230.);
    }

    #[test]
    fn only_git_ignore_rules_trigger_a_new_ignore_listing() {
        let root = Path::new("/project");
        assert!(is_ignore_rules(root, Path::new("/project/.gitignore")));
        assert!(is_ignore_rules(root, Path::new("/project/sub/.gitignore")));
        assert!(is_ignore_rules(
            root,
            Path::new("/project/.git/info/exclude")
        ));
        assert!(!is_ignore_rules(root, Path::new("/project/.git/HEAD")));
        assert!(!is_ignore_rules(root, Path::new("/project/target/debug/x")));
        assert!(!is_ignore_rules(
            root,
            Path::new("/project/sub/.git/info/exclude")
        ));
    }

    #[test]
    fn close_tab_is_bound_in_the_files_context() {
        use crate::workspace::{CloseSession, KEY_CONTEXT as WORKSPACE_CONTEXT};
        use gpui::{KeyContext, Keymap, Keystroke};

        // Le keymap de l'app. Ce qui décide est la profondeur du contexte (`Files` est sous `Workspace`) ; l'ordre de
        // pose ne compterait qu'à profondeur égale (la dernière liée gagne).
        let mut bindings = vec![KeyBinding::new(
            "cmd-w",
            CloseSession,
            Some(WORKSPACE_CONTEXT),
        )];
        bindings.extend(key_bindings());
        let keymap = Keymap::new(bindings);
        let stack = |names: &[&str]| -> Vec<KeyContext> {
            names
                .iter()
                .map(|name| KeyContext::parse(name).unwrap())
                .collect()
        };
        let cmd_w = [Keystroke::parse("cmd-w").unwrap()];

        // L'éditeur au focus : Workspace, Files, puis le contexte de l'éditeur lui-même.
        let (found, _) = keymap.bindings_for_input(
            &cmd_w,
            &stack(&[WORKSPACE_CONTEXT, KEY_CONTEXT, "Input mode=editor"]),
        );
        assert!(found[0].action().partial_eq(&CloseTab));
        // Hors du volet, ⌘W ferme toujours la session.
        let (found, _) = keymap.bindings_for_input(&cmd_w, &stack(&[WORKSPACE_CONTEXT]));
        assert!(found[0].action().partial_eq(&CloseSession));
        // ⌘S ne vaut que dans le volet.
        let cmd_s = [Keystroke::parse("cmd-s").unwrap()];
        let (found, _) = keymap.bindings_for_input(&cmd_s, &stack(&[WORKSPACE_CONTEXT]));
        assert!(found.is_empty());
        let (found, _) = keymap.bindings_for_input(
            &cmd_s,
            &stack(&[WORKSPACE_CONTEXT, KEY_CONTEXT, "Input mode=editor"]),
        );
        assert!(found[0].action().partial_eq(&SaveNow));
    }

    #[test]
    fn project_search_opens_from_anywhere_and_still_shields_replace() {
        use crate::workspace::KEY_CONTEXT as WORKSPACE_CONTEXT;
        use gpui::{KeyContext, Keymap, Keystroke};

        // Le keymap de l'app : le kit lie d'abord `⌘⇧F` au remplacement (`gpui_component::init`), puis IdemA pose les
        // siennes (`main`).
        let mut bindings = vec![KeyBinding::new("cmd-shift-f", Replace, Some("Input"))];
        bindings.extend(key_bindings());
        let keymap = Keymap::new(bindings);
        let resolve = |names: &[&str], keystroke: &str| {
            let stack: Vec<KeyContext> = names
                .iter()
                .map(|name| KeyContext::parse(name).unwrap())
                .collect();
            let keystrokes = [Keystroke::parse(keystroke).unwrap()];
            let (found, _) = keymap.bindings_for_input(&keystrokes, &stack);
            found
        };

        // Un pane au focus, l'éditeur, l'arbre, le champ du filtre : `⌘⇧F` vaut la recherche dans le projet partout, et
        // dans l'éditeur le bouclier prend le pas sur le remplacement du kit.
        let stacks: [&[&str]; 4] = [
            &[WORKSPACE_CONTEXT],
            &[WORKSPACE_CONTEXT, KEY_CONTEXT, "Input mode=editor"],
            &[WORKSPACE_CONTEXT, KEY_CONTEXT, TREE_KEY_CONTEXT],
            &[
                WORKSPACE_CONTEXT,
                KEY_CONTEXT,
                TREE_KEY_CONTEXT,
                FILTER_KEY_CONTEXT,
            ],
        ];
        for names in stacks {
            let found = resolve(names, "cmd-shift-f");
            let first = found.first().unwrap_or_else(|| panic!("{names:?} unbound"));
            assert!(first.action().partial_eq(&ProjectSearch), "{names:?}");
            assert!(!first.action().partial_eq(&Replace), "{names:?}");
        }
        // `⌘⌥F` ouvre toujours le remplacement dans l'éditeur.
        let alt = resolve(
            &[WORKSPACE_CONTEXT, KEY_CONTEXT, "Input mode=editor"],
            "cmd-alt-f",
        );
        assert!(alt[0].action().partial_eq(&Replace));
    }

    #[test]
    fn copy_to_agent_is_bound_only_in_the_editor() {
        use crate::workspace::KEY_CONTEXT as WORKSPACE_CONTEXT;
        use gpui::{KeyContext, Keymap, Keystroke};

        let keymap = Keymap::new(key_bindings());
        let resolve = |names: &[&str]| {
            let stack: Vec<KeyContext> = names
                .iter()
                .map(|name| KeyContext::parse(name).unwrap())
                .collect();
            let keystrokes = [Keystroke::parse("cmd-alt-c").unwrap()];
            let (found, _) = keymap.bindings_for_input(&keystrokes, &stack);
            found
        };

        // L'éditeur au focus : le geste est le nôtre.
        let in_editor = resolve(&[WORKSPACE_CONTEXT, KEY_CONTEXT, "Input mode=editor"]);
        assert!(in_editor[0].action().partial_eq(&CopyToAgent));
        // Un pane au focus (le contexte `Workspace` seul) : aucune de nos liaisons, l'agent garde `⌘⌥C`.
        assert!(resolve(&[WORKSPACE_CONTEXT]).is_empty());
    }

    #[test]
    fn line_keys_edit_the_editor_and_never_the_filter_field() {
        use crate::workspace::KEY_CONTEXT as WORKSPACE_CONTEXT;
        use gpui::{Action, KeyContext, Keymap, Keystroke};

        let keymap = Keymap::new(key_bindings());
        let resolve = |names: &[&str], key: &str| {
            let stack: Vec<KeyContext> = names
                .iter()
                .map(|name| KeyContext::parse(name).unwrap())
                .collect();
            let (found, _) = keymap.bindings_for_input(&[Keystroke::parse(key).unwrap()], &stack);
            found
        };
        let first = |names: &[&str], key: &str| resolve(names, key)[0].action().boxed_clone();
        let editor = [WORKSPACE_CONTEXT, KEY_CONTEXT, "Input mode=editor"];
        let fields = [
            [WORKSPACE_CONTEXT, KEY_CONTEXT, "FilesFilter", "Input"],
            [WORKSPACE_CONTEXT, KEY_CONTEXT, "FilesReplace", "Input"],
            // La recherche `⌘F` de l'éditeur : ses champs sont des frères de l'éditeur, sous `SearchPanel`.
            [WORKSPACE_CONTEXT, KEY_CONTEXT, "SearchPanel", "Input"],
        ];
        let cases: [(&str, Box<dyn Action>); 4] = [
            ("alt-up", Box::new(MoveLineUp)),
            ("alt-down", Box::new(MoveLineDown)),
            ("shift-alt-up", Box::new(DuplicateLineUp)),
            ("shift-alt-down", Box::new(DuplicateLineDown)),
        ];
        for (key, action) in cases {
            assert!(first(&editor, key).partial_eq(action.as_ref()), "{key}");
            for field in &fields {
                assert!(resolve(field, key).is_empty(), "{key} in {field:?}");
            }
        }
    }

    #[test]
    fn tree_keys_stay_in_the_tree() {
        use crate::rename_field::{self, RenameConfirm, RenameDeleteToStart};
        use crate::workspace::KEY_CONTEXT as WORKSPACE_CONTEXT;
        use gpui::{KeyContext, Keymap, Keystroke};
        use gpui_component::input::{DeleteToBeginningOfLine, Enter};

        // Le keymap de l'app : les liaisons du kit qui comptent (contexte `Input`), celles du champ de renommage
        // (`main`), puis celles du volet.
        let mut bindings = vec![
            KeyBinding::new("cmd-backspace", DeleteToBeginningOfLine, Some("Input")),
            KeyBinding::new(
                "enter",
                Enter {
                    secondary: false,
                    shift: false,
                },
                Some("Input"),
            ),
            KeyBinding::new("enter", RenameConfirm, Some(rename_field::KEY_CONTEXT)),
            KeyBinding::new(
                "cmd-backspace",
                RenameDeleteToStart,
                Some(rename_field::KEY_CONTEXT),
            ),
        ];
        bindings.extend(key_bindings());
        let keymap = Keymap::new(bindings);
        let resolve = |names: &[&str], keystroke: &str| {
            let stack: Vec<KeyContext> = names
                .iter()
                .map(|name| KeyContext::parse(name).unwrap())
                .collect();
            let keystrokes = [Keystroke::parse(keystroke).unwrap()];
            let (found, _) = keymap.bindings_for_input(&keystrokes, &stack);
            found
        };

        // L'arbre au focus : `↩` renomme, `⌘⌫` met à la Corbeille.
        let tree = [WORKSPACE_CONTEXT, KEY_CONTEXT, TREE_KEY_CONTEXT];
        assert!(
            resolve(&tree, "enter")[0]
                .action()
                .partial_eq(&RenameSelected)
        );
        assert!(
            resolve(&tree, "cmd-backspace")[0]
                .action()
                .partial_eq(&TrashSelected)
        );

        // L'éditeur au focus : les touches restent celles du texte, et aucune ne vise l'arbre.
        let editor = [WORKSPACE_CONTEXT, KEY_CONTEXT, "Input mode=editor"];
        let enter = resolve(&editor, "enter");
        assert!(enter[0].action().partial_eq(&Enter {
            secondary: false,
            shift: false
        }));
        let delete = resolve(&editor, "cmd-backspace");
        assert!(delete[0].action().partial_eq(&DeleteToBeginningOfLine));
        for found in [&enter, &delete] {
            assert!(
                found
                    .iter()
                    .all(|binding| !binding.action().partial_eq(&RenameSelected)
                        && !binding.action().partial_eq(&TrashSelected))
            );
        }

        // Le champ ouvert dans l'arbre (sous `FilesTree`) : ses touches passent avant celles de l'arbre.
        let field = [
            WORKSPACE_CONTEXT,
            KEY_CONTEXT,
            TREE_KEY_CONTEXT,
            rename_field::KEY_CONTEXT,
        ];
        assert!(
            resolve(&field, "enter")[0]
                .action()
                .partial_eq(&RenameConfirm)
        );
        assert!(
            resolve(&field, "cmd-backspace")[0]
                .action()
                .partial_eq(&RenameDeleteToStart)
        );
    }

    /// Les liaisons du kit au contexte `Input`, telles que `gpui_component::init` les pose sur macOS. Le kit n'expose pas
    /// la liste (`gpui-base-0.7.3/src/input/base/state.rs:140-306`, `init` est `pub(crate)`) et le dépôt n'a pas de
    /// `TestAppContext` : la recopie est complète pour toutes les actions que le kit exporte. Y manquent seulement celles
    /// qu'il n'exporte pas (`SelectLeft`, `SelectRight`, `SelectUp`, `SelectDown`, `AddCursorAbove`, `AddCursorBelow` :
    /// `shift-left` `shift-right` `shift-up` `shift-down` `cmd-alt-up` `cmd-alt-down`), qu'aucune liaison de l'arbre ne
    /// suit. Version du kit épinglée (`=0.7.3`) : relire ces lignes à chaque montée.
    pub(crate) fn kit_input_bindings() -> Vec<KeyBinding> {
        use gpui_component::input::{
            Backspace, Copy, Cut, Delete, DeleteToBeginningOfLine, DeleteToEndOfLine,
            DeleteToNextWordEnd, DeleteToPreviousWordStart, Enter, Escape, Indent, IndentInline,
            MoveDown, MoveEnd, MoveHome, MoveLeft, MovePageDown, MovePageUp, MoveRight, MoveToEnd,
            MoveToNextWord, MoveToPreviousWord, MoveToStart, MoveUp, Outdent, OutdentInline, Paste,
            Redo, Search, SelectAll, SelectToEnd, SelectToEndOfLine, SelectToNextWordEnd,
            SelectToPreviousWordStart, SelectToStart, SelectToStartOfLine, ShowCharacterPalette,
            ToggleCodeActions, Undo,
        };
        let ctx = Some("Input");
        let enter = |secondary, shift| Enter { secondary, shift };
        vec![
            KeyBinding::new("backspace", Backspace, ctx),
            KeyBinding::new("shift-backspace", Backspace, ctx),
            KeyBinding::new("ctrl-backspace", Backspace, ctx),
            KeyBinding::new("delete", Delete, ctx),
            KeyBinding::new("shift-delete", Delete, ctx),
            KeyBinding::new("cmd-backspace", DeleteToBeginningOfLine, ctx),
            KeyBinding::new("cmd-delete", DeleteToEndOfLine, ctx),
            KeyBinding::new("alt-backspace", DeleteToPreviousWordStart, ctx),
            KeyBinding::new("alt-delete", DeleteToNextWordEnd, ctx),
            KeyBinding::new("enter", enter(false, false), ctx),
            KeyBinding::new("shift-enter", enter(false, true), ctx),
            KeyBinding::new("secondary-enter", enter(true, false), ctx),
            KeyBinding::new("escape", Escape, ctx),
            KeyBinding::new("up", MoveUp, ctx),
            KeyBinding::new("down", MoveDown, ctx),
            KeyBinding::new("left", MoveLeft, ctx),
            KeyBinding::new("right", MoveRight, ctx),
            KeyBinding::new("pageup", MovePageUp, ctx),
            KeyBinding::new("pagedown", MovePageDown, ctx),
            KeyBinding::new("tab", IndentInline, ctx),
            KeyBinding::new("shift-tab", OutdentInline, ctx),
            KeyBinding::new("cmd-]", Indent, ctx),
            KeyBinding::new("cmd-[", Outdent, ctx),
            KeyBinding::new("home", MoveHome, ctx),
            KeyBinding::new("end", MoveEnd, ctx),
            KeyBinding::new("shift-home", SelectToStartOfLine, ctx),
            KeyBinding::new("shift-end", SelectToEndOfLine, ctx),
            KeyBinding::new("ctrl-shift-a", SelectToStartOfLine, ctx),
            KeyBinding::new("ctrl-shift-e", SelectToEndOfLine, ctx),
            KeyBinding::new("shift-cmd-left", SelectToStartOfLine, ctx),
            KeyBinding::new("shift-cmd-right", SelectToEndOfLine, ctx),
            KeyBinding::new("alt-shift-left", SelectToPreviousWordStart, ctx),
            KeyBinding::new("alt-shift-right", SelectToNextWordEnd, ctx),
            KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, ctx),
            KeyBinding::new("cmd-a", SelectAll, ctx),
            KeyBinding::new("cmd-c", Copy, ctx),
            KeyBinding::new("cmd-x", Cut, ctx),
            KeyBinding::new("cmd-v", Paste, ctx),
            KeyBinding::new("ctrl-a", MoveHome, ctx),
            KeyBinding::new("cmd-left", MoveHome, ctx),
            KeyBinding::new("ctrl-e", MoveEnd, ctx),
            KeyBinding::new("cmd-right", MoveEnd, ctx),
            KeyBinding::new("cmd-z", Undo, ctx),
            KeyBinding::new("cmd-shift-z", Redo, ctx),
            KeyBinding::new("cmd-up", MoveToStart, ctx),
            KeyBinding::new("cmd-down", MoveToEnd, ctx),
            KeyBinding::new("alt-left", MoveToPreviousWord, ctx),
            KeyBinding::new("alt-right", MoveToNextWord, ctx),
            KeyBinding::new("cmd-shift-up", SelectToStart, ctx),
            KeyBinding::new("cmd-shift-down", SelectToEnd, ctx),
            KeyBinding::new("cmd-.", ToggleCodeActions, ctx),
            KeyBinding::new("cmd-f", Search, ctx),
            KeyBinding::new("cmd-shift-f", gpui_component::input::Replace, ctx),
        ]
    }

    /// Aucune touche tapée dans l'un des deux champs ne peut atteindre `RenameSelected` (`↩`) ni `TrashSelected` (`⌘⌫`)
    /// de l'arbre. GPUI essaie la liaison suivante, d'un contexte moins profond, quand l'action de la première n'est pas
    /// prise ou que son gestionnaire propage : le kit propage `↩`, Échap et `⌫` en tête de texte, et ne prend pas `↑` `↓`
    /// `⇥` `⇧⇥` sur une ligne. Le test rejoue les liaisons du kit puis celles du volet (`key_bindings`, dans l'ordre de
    /// `main`) sur la pile du champ au focus : pour chaque touche, la première liaison hors du contexte `Input` est une
    /// action de la boîte, et aucune liaison de l'arbre ne vient avant une liaison de la boîte. Les liaisons de `main.rs`
    /// (contexte `Workspace`, popover, barre de recherche) et de `menus.rs` (sans contexte) n'y sont pas : elles sont
    /// d'un contexte moins profond que la boîte du champ, donc elles ne passent jamais devant ses liaisons ni devant
    /// celles de l'arbre. Seules comptent les liaisons de la boîte ou plus profondes, que le test couvre.
    #[test]
    fn keys_typed_in_the_fields_never_reach_the_tree() {
        use crate::workspace::KEY_CONTEXT as WORKSPACE_CONTEXT;
        use gpui::{Action, KeyContext, Keymap, Keystroke};

        // Le kit d'abord (`gpui_component::init`), puis les liaisons du volet (`files::panel::key_bindings`).
        let mut bindings = kit_input_bindings();
        let kit_actions: HashSet<&'static str> =
            bindings.iter().map(|b| b.action().name()).collect();
        bindings.extend(key_bindings());
        let keymap = Keymap::new(bindings);

        // Les touches liées dans l'arbre, celles du §4.2 de la spec du jalon 4 (sélection, mots, ligne, presse-papiers,
        // annuler), puis celles que le kit propage ou ne prend pas, et `⌘⌥C` (envoi à l'agent, lié au contexte `Input`).
        let keys = [
            "enter",
            "cmd-backspace",
            "backspace",
            "escape",
            "tab",
            "shift-tab",
            "up",
            "down",
            "left",
            "right",
            "cmd-left",
            "cmd-right",
            "shift-cmd-left",
            "shift-cmd-right",
            "alt-left",
            "alt-right",
            "alt-shift-left",
            "alt-shift-right",
            "shift-left",
            "shift-right",
            "cmd-a",
            "cmd-c",
            "cmd-x",
            "cmd-v",
            "alt-backspace",
            "cmd-z",
            "cmd-shift-z",
            "cmd-alt-c",
        ];
        // Le contexte de la boîte, puis ce qu'elle prend en premier pour les touches qu'elle attrape.
        type Taken = Vec<(&'static str, Box<dyn Action>)>;
        let fields: [(&str, Taken); 2] = [
            (
                FILTER_KEY_CONTEXT,
                vec![
                    ("enter", Box::new(FilterOpen)),
                    ("cmd-backspace", Box::new(FieldGuard)),
                    ("backspace", Box::new(FieldGuard)),
                    ("escape", Box::new(FilterClear)),
                    ("tab", Box::new(FilterNextField)),
                    ("shift-tab", Box::new(FieldGuard)),
                    ("up", Box::new(FilterUp)),
                    ("down", Box::new(FilterDown)),
                    ("cmd-alt-c", Box::new(FieldGuard)),
                ],
            ),
            (
                REPLACE_KEY_CONTEXT,
                vec![
                    // Un remplacement ne part jamais au clavier : `↩` est pris sans effet.
                    ("enter", Box::new(IgnoreEnter)),
                    ("cmd-backspace", Box::new(FieldGuard)),
                    ("backspace", Box::new(FieldGuard)),
                    ("escape", Box::new(FilterClear)),
                    ("tab", Box::new(FilterNextField)),
                    ("shift-tab", Box::new(FilterNextField)),
                    ("up", Box::new(FieldGuard)),
                    ("down", Box::new(FieldGuard)),
                    ("cmd-alt-c", Box::new(FieldGuard)),
                ],
            ),
        ];
        let is_tree = |action: &dyn Action| {
            action.partial_eq(&RenameSelected) || action.partial_eq(&TrashSelected)
        };

        for (box_context, taken) in &fields {
            let stack: Vec<KeyContext> = [
                WORKSPACE_CONTEXT,
                KEY_CONTEXT,
                TREE_KEY_CONTEXT,
                box_context,
                "Input",
            ]
            .iter()
            .map(|name| KeyContext::parse(name).unwrap())
            .collect();
            for key in keys {
                let keystrokes = [Keystroke::parse(key).unwrap()];
                let (found, _) = keymap.bindings_for_input(&keystrokes, &stack);
                // Une liaison est « du kit » quand son contexte est `Input` seul et son action une action du kit : celles
                // que le volet lie plus profond (`FilesFilter > Input`) comptent comme celles de la boîte, et `⌘⌥C`
                // (`CopyToAgent`, que le volet lie à `Input` pour l'éditeur) n'est pas au kit.
                let in_input = |binding: &KeyBinding| {
                    binding.predicate().map(|p| p.to_string()).as_deref() == Some("Input")
                        && kit_actions.contains(binding.action().name())
                };
                let in_box = |binding: &KeyBinding| {
                    !in_input(binding) && !is_tree(binding.action()) && {
                        let context = binding.predicate().map(|p| p.to_string());
                        context.as_deref().is_some_and(|c| {
                            c == *box_context || c.starts_with(&format!("{box_context} > "))
                        })
                    }
                };
                let tree_at = found.iter().position(|b| is_tree(b.action()));
                let box_at = found.iter().position(&in_box);
                if let Some(tree_at) = tree_at {
                    assert!(
                        box_at.is_some_and(|box_at| box_at < tree_at),
                        "{box_context}: {key} reaches the tree before any binding of the box"
                    );
                }
                if let Some(first) = found.iter().find(|b| !in_input(b)) {
                    assert!(
                        in_box(first),
                        "{box_context}: the first binding of {key} outside Input is not the box's"
                    );
                }
            }
            // `↩` et `⌘⌫` : la boîte les lie, c'est ce qui arrête la chute vers l'arbre quand le kit propage.
            for key in ["enter", "cmd-backspace"] {
                let keystrokes = [Keystroke::parse(key).unwrap()];
                let (found, _) = keymap.bindings_for_input(&keystrokes, &stack);
                assert!(
                    found
                        .iter()
                        .any(|b| b.predicate().is_some_and(|p| p.to_string() == *box_context)),
                    "{box_context}: {key} has no binding in the box"
                );
            }
            // Ce que la boîte prend en premier, hors du kit.
            for (key, action) in taken {
                let keystrokes = [Keystroke::parse(key).unwrap()];
                let (found, _) = keymap.bindings_for_input(&keystrokes, &stack);
                let first = found
                    .iter()
                    .find(|b| !kit_actions.contains(b.action().name()))
                    .unwrap_or_else(|| panic!("{box_context}: {key} has no binding outside Input"));
                assert!(
                    first.action().partial_eq(action.as_ref()),
                    "{box_context}: {key} falls on {} instead of {}",
                    first.action().name(),
                    action.name()
                );
            }
        }
    }

    #[test]
    fn a_tab_follows_the_files_indentation() {
        let spaces = tab_size_for(Indent::Spaces(4));
        assert_eq!((spaces.tab_size, spaces.hard_tabs), (4, false));
        let tabs = tab_size_for(Indent::Tabs);
        assert!(tabs.hard_tabs);
    }

    #[test]
    fn a_file_outside_the_project_is_titled_by_its_short_path() {
        let root = Path::new("/work/project");
        let home = Path::new("/Users/me");
        let title = |path: &str, home: Option<&Path>| tab_title(Path::new(path), root, home);
        assert_eq!(title("/work/project/docs/spec.md", Some(home)), "spec.md");
        assert_eq!(title("/Users/me/notes.md", Some(home)), "~/notes.md");
        assert_eq!(title("/etc/hosts", Some(home)), "/etc/hosts");
        assert_eq!(title("/etc/hosts", None), "/etc/hosts");
    }

    #[test]
    fn long_outside_paths_keep_their_file_name_and_stay_distinct() {
        let root = Path::new("/work/project");
        let home = Some(Path::new("/Users/me"));
        let config = tab_title(Path::new("/Users/me/.config/idema/config.toml"), root, home);
        let employees = tab_title(
            Path::new("/Users/me/dev/erom-tool-idema/outside/caserne/employees.yml"),
            root,
            home,
        );
        assert_eq!(config, "…/idema/config.toml");
        assert_eq!(employees, "…/caserne/employees.yml");
        assert_ne!(config, employees);
        // Hors du dossier personnel, même règle.
        assert_eq!(
            tab_title(Path::new("/var/log/system/boot.log"), root, home),
            "…/system/boot.log"
        );
    }

    #[test]
    fn a_path_through_a_symlink_is_compared_in_the_roots_canonical_form() {
        let dir = temp_dir("canonical");
        let real = dir.join("real");
        fs::create_dir_all(real.join("docs")).unwrap();
        fs::write(real.join("docs/a.md"), b"").unwrap();
        std::os::unix::fs::symlink(&real, dir.join("link")).unwrap();
        let root = fs::canonicalize(&real).unwrap();

        let through_link = canonical_path(&dir.join("link/docs/a.md"));
        assert_eq!(through_link, root.join("docs/a.md"));
        // Un fichier qui n'existe pas encore garde son nom derrière un parent canonique.
        let not_yet = canonical_path(&dir.join("link/docs/new.md"));
        assert_eq!(not_yet, root.join("docs/new.md"));
        assert!(through_link.starts_with(&root));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tree_entry_under_a_folder_link_keys_the_tab_of_its_target() {
        let dir = temp_dir("tab-key");
        let real = dir.join("real");
        fs::create_dir_all(real.join("docs")).unwrap();
        fs::write(real.join("docs/a.md"), b"").unwrap();
        std::os::unix::fs::symlink(&real, dir.join("link")).unwrap();
        let target = fs::canonicalize(&real).unwrap();

        // Une ligne sous le lien retrouve l'onglet ouvert sur la cible, même une fois le fichier renommé.
        assert_eq!(
            tab_key(&dir.join("link/docs/a.md")),
            target.join("docs/a.md")
        );
        assert_eq!(
            tab_key(&dir.join("link/docs/gone.md")),
            target.join("docs/gone.md")
        );
        assert_eq!(tab_key(&dir.join("link/a.md")), target.join("a.md"));
        // Le lien lui-même n'est pas sa cible : ses gestes ne visent pas les onglets de la cible.
        let link = tab_key(&dir.join("link"));
        assert_eq!(link.file_name(), Some(std::ffi::OsStr::new("link")));
        assert!(!target.starts_with(&link));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rescan_reloads_every_open_tab_below_the_folder_a_plain_batch_only_the_named_ones() {
        let open = [
            Path::new("/p/src/a.rs"),
            Path::new("/p/src/deep/b.rs"),
            Path::new("/p/docs/c.md"),
        ];
        let plain = WatchBatch {
            paths: vec![PathBuf::from("/p/src/a.rs"), PathBuf::from("/p/src")],
            rescan: Vec::new(),
        };
        assert_eq!(
            touched_tabs(open, &plain),
            vec![PathBuf::from("/p/src/a.rs")]
        );
        let overflow = WatchBatch {
            paths: vec![PathBuf::from("/p/src")],
            rescan: vec![PathBuf::from("/p/src")],
        };
        assert_eq!(
            touched_tabs(open, &overflow),
            vec![
                PathBuf::from("/p/src/a.rs"),
                PathBuf::from("/p/src/deep/b.rs")
            ]
        );
        // Un dossier qui ne fait que commencer pareil n'est pas un parent.
        let sibling = WatchBatch {
            paths: Vec::new(),
            rescan: vec![PathBuf::from("/p/sr")],
        };
        assert!(touched_tabs(open, &sibling).is_empty());
    }

    #[test]
    fn a_line_ending_conversion_made_by_an_agent_is_kept_even_with_identical_text() {
        use crate::files::document::{read_document, save_atomically};
        use crate::files::tabs::{DiskVerdict, SaveState};

        let dir = temp_dir("dos2unix");
        let file = dir.join("a.yml");
        fs::write(&file, b"a: 1\r\nb: 2\r\n").unwrap();
        let Opened::Text(opened) = read_document(&file).unwrap() else {
            panic!("a.yml should open as text");
        };
        assert_eq!(opened.line_ending, LineEnding::Crlf);
        let mut state = SaveState::new(opened.text.clone());

        // `dos2unix` : le texte normalisé est le même, seules les fins de ligne changent.
        fs::write(&file, b"a: 1\nb: 2\n").unwrap();
        let Opened::Text(reloaded) = read_document(&file).unwrap() else {
            panic!("a.yml should still open as text");
        };
        assert_eq!(
            state.on_disk_change(Some(&reloaded.text), &opened.text),
            DiskVerdict::Ignore
        );
        let kept = line_ending_after_read(opened.line_ending, Some(&reloaded));
        assert_eq!(kept, LineEnding::Lf);

        // La sauvegarde suivante ne remet pas les `\r\n`.
        save_atomically(&file, "a: 1\nb: 2\nc: 3\n", kept).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"a: 1\nb: 2\nc: 3\n");
        // Un fichier disparu garde la fin de ligne qu'il avait.
        assert_eq!(
            line_ending_after_read(LineEnding::Crlf, None),
            LineEnding::Crlf
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_second_press_of_a_double_click_does_not_open_a_file_externally_again() {
        // Premier appui : rien d'ouvert, le fichier part dehors (ou s'ouvre en onglet).
        assert!(!is_repeat_of_external_open(1, false));
        // Second appui : un onglet existe, l'épinglage se fait ; sinon le premier appui a déjà tout fait.
        assert!(!is_repeat_of_external_open(2, true));
        assert!(is_repeat_of_external_open(2, false));
        assert!(is_repeat_of_external_open(3, false));
    }
}
