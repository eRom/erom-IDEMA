//! Le champ en tête de l'arbre, le filtre des fichiers par nom, la recherche dans leur contenu et le remplacement
//! (P2 jalon 3) : l'état du champ, le listage des fichiers du projet, leur classement et la recherche hors du fil
//! d'interface, le clavier des champs, la liste des résultats, qui prend la place de l'arbre tant que le texte n'est pas
//! vide, et le remplacement des lignes cochées.

use super::*;

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use gpui::{AnyElement, ClickEvent, FontWeight, HighlightStyle, Rgba, StyledText, uniform_list};
use gpui_component::input::{Input, InputState};
use regex::Regex;

use super::tree_view::{GLYPH_FONT, GLYPH_SLOT, ROW_HEIGHT, row_lead};
use crate::files::filter::{
    FieldMode, FileOutcome, FileTargets, FilterHit, GIT_FAILED, HitParts, INVALID_REGEX, NO_FILE,
    REPLACING, ResultItem, SEARCHING, SNIPPET_LEAD, SingleFlight, SpentReplace, Unchecked,
    field_mode, file_outcome, filter_hits, first_selectable, is_replace_press, kept_hit, kept_line,
    move_selection, open_position, replace_enabled, replace_summary, result_items, search_status,
    snippet, split_hit, tab_outcome, totals,
};
use crate::files::project_search::{
    FileReplace, MAX_RESULT_LINES, QueryError, ReplaceError, SearchOptions, SearchOutcome, compile,
    project_files, replace_in_file, replace_lines, search_files,
};
use crate::theme::{
    ACTION_HOVER, CONTROL, ERROR_TEXT, FIELD_BACKGROUND, FIELD_RING, FOCUS, FOCUS_TEXT, ICON_MUTED,
    SEARCH_MATCH, TEXT_DISABLED, TEXT_PRIMARY, TOGGLE_ACTIVE, TREE_SELECTED, TREE_SELECTED_BORDER,
    TREE_TEXT, WAITING, black, color, focus,
};

actions!(
    files,
    [
        FilterUp,
        FilterDown,
        FilterOpen,
        FilterClear,
        FilterNextField,
        IgnoreEnter,
        FieldGuard
    ]
);

/// Contexte de touches du champ du filtre, dans la colonne de l'arbre : ses touches y sont prises par le champ ou par
/// un gestionnaire de la boîte, jamais par `RenameSelected` ni `TrashSelected` (GPUI passerait à la liaison de l'arbre
/// si l'action de celle du champ n'était pas prise).
pub const FILTER_KEY_CONTEXT: &str = "FilesFilter";

/// Contexte de touches du champ « Remplacer par » : ses touches sont prises par le champ ou par la boîte, `↩` y est
/// pris sans effet (un remplacement ne part jamais au clavier), pour les mêmes raisons que `FILTER_KEY_CONTEXT`.
pub const REPLACE_KEY_CONTEXT: &str = "FilesReplace";

/// Les textes indicatifs des deux champs, et le corps de leur texte : celui des lignes de l'arbre.
const FILTER_PLACEHOLDER: &str = "Filtrer, ou ?texte";
const REPLACE_PLACEHOLDER: &str = "Remplacer par";
const FIELD_TEXT: f32 = 12.5;

/// La boîte du champ (maquette) : 28 de haut, rayon 7, retrait de 8 plus le contour de 1 (le `0 9` de la maquette), écart
/// de 7 entre la loupe de 15 et le champ.
pub(crate) const FIELD_HEIGHT: f32 = 28.;
pub(crate) const FIELD_RADIUS: f32 = 7.;
pub(crate) const FIELD_PADDING: f32 = 8.;
const FIELD_GAP: f32 = 7.;
const FIELD_ICON: f32 = 15.;
/// Le contour de la boîte (`border_1`) : l'intérieur du champ fait la hauteur de la boîte moins deux contours.
const FIELD_BORDER: f32 = 1.;
/// La hauteur de ligne du champ du kit : 1,25 rem, soit 20 px au corps de base de GPUI.
const KIT_LINE_HEIGHT: f32 = 20.;

/// Les bascules de la recherche dans le projet : 22 de côté, rayon 5, écart 4 (maquette), icône de 16.
const TOGGLE_SIZE: f32 = 22.;
const TOGGLE_RADIUS: f32 = 5.;
const TOGGLE_GAP: f32 = 4.;
const TOGGLE_ICON: f32 = 16.;
/// Le bouton « Remplacer » : 22 de haut, retrait `0 8`, rayon 6, texte de 12. La case d'une ligne trouvée : 14 de côté,
/// rayon 3,5, coche de 12.
const BUTTON_HEIGHT: f32 = 22.;
const BUTTON_PADDING: f32 = 8.;
const BUTTON_RADIUS: f32 = 6.;
const BUTTON_TEXT: f32 = 12.;
const CHECKBOX_SIZE: f32 = 14.;
const CHECKBOX_RADIUS: f32 = 3.5;
const CHECKBOX_ICON: f32 = 12.;
/// La liste des échecs d'un remplacement défile au-delà de cette hauteur : elle ne mange pas les résultats.
const FAILURES_MAX_HEIGHT: f32 = 120.;
/// Au plus ce nombre de caractères d'une ligne trouvée est posé à l'écran : une ligne d'un fichier minifié ne doit pas
/// faire composer des centaines de milliers de caractères à chaque image.
const MAX_SNIPPET_CHARS: usize = 240;

/// Une ligne de statut sous les bascules : « Recherche… », le décompte, ou une erreur (en rouge).
pub(super) struct Status {
    pub(super) text: String,
    pub(super) error: bool,
}

impl Status {
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: false,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: true,
        }
    }
}

/// La recherche affichée avec ce qui l'a produite : le texte, les bascules et l'expression compilée. Le remplacement
/// agit sur ces lignes avec cette expression, pas avec ce que le champ dit maintenant.
pub(super) struct SearchResult {
    pub(super) query: String,
    pub(super) options: SearchOptions,
    pub(super) regex: Regex,
    pub(super) outcome: SearchOutcome,
}

/// Ce qui fait un remplacement : l'expression qui a produit le résultat affiché, le texte de « Remplacer par » et
/// `expand` (`$1` développé en mode expression régulière seulement).
struct ReplaceSpec {
    regex: Regex,
    replacement: String,
    expand: bool,
}

/// Ce qu'un remplacement en cours porte d'une étape à l'autre : les onglets remplacés attendent leur sauvegarde, les
/// fichiers sans onglet leur écriture, et le bilan est fait une fois, quand tout est résolu.
struct ReplaceRun {
    generation: u64,
    spec: ReplaceSpec,
    outcomes: Vec<FileOutcome>,
    on_disk: Vec<FileTargets>,
    saving: Vec<(PathBuf, FileReplace)>,
}

/// Le remplacement d'un onglet : décidé d'emblée (échec, rien à remplacer), ou en attente de sa sauvegarde.
enum TabReplace {
    Done(FileOutcome),
    Saving(FileReplace),
}

/// Une bascule de la recherche.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Toggle {
    MatchCase,
    WholeWord,
    Regex,
}

/// La hauteur de ligne du champ pour une boîte de `box_height` px : celle du kit, ou un pixel de plus quand l'intérieur
/// de la boîte et la ligne n'ont pas la même parité. Le kit centre la ligne dans l'intérieur ; avec une différence
/// impaire, la ligne tombe à une demi-case et GPUI ramène ce demi-pixel vers zéro selon l'origine absolue de la boîte :
/// la boîte du filtre (centrée dans l'en-tête, origine à demi-pixel) et celle de « Remplacer par » (origine entière)
/// plaçaient alors leur texte à un pixel d'écart. Une différence paire donne une ligne posée au pixel près, quelle que
/// soit l'origine de la boîte, et la ligne reste centrée.
fn field_line_height(box_height: f32) -> f32 {
    let inner = box_height - 2. * FIELD_BORDER;
    if (inner - KIT_LINE_HEIGHT) % 2. == 0. {
        KIT_LINE_HEIGHT
    } else {
        KIT_LINE_HEIGHT + 1.
    }
}

/// Le champ de saisie d'une boîte : un `Input` du kit sans son habillage (la boîte porte le fond, le contour et le
/// rayon), sans retrait ni hauteur propres, au corps et à la couleur de l'arbre. Le curseur, la sélection et le texte
/// indicatif prennent les couleurs du thème du kit, déjà réglé sur les jetons d'IdemA pour l'éditeur.
pub(crate) fn field_input(state: &Entity<InputState>, theme: &Theme) -> impl IntoElement {
    let line_height = field_line_height(f32::from(theme.px(FIELD_HEIGHT)));
    // `Styled::h_full` en toutes lettres : `Input::h_full` (multiligne) porte le même nom.
    Styled::h_full(
        Input::new(state)
            .appearance(false)
            .bordered(false)
            .focus_bordered(false)
            .px_0()
            .py_0()
            .text_size(theme.text(FIELD_TEXT))
            .line_height(px(line_height))
            .text_color(white(TEXT_FULL)),
    )
}

/// Le champ du filtre et ce qu'il pilote. Le mode se déduit du seul texte (`field_mode`).
pub(super) struct ProjectFilter {
    /// Le champ de saisie, dans l'en-tête de l'arbre.
    pub(super) query: Entity<InputState>,
    /// Le texte du champ à la dernière frappe.
    pub(super) text: String,
    /// Les fichiers du projet, listés par Git ; `None` tant qu'ils ne le sont pas ou après un changement du disque. Le
    /// tableau est partagé avec la tâche de classement : il ne se copie pas à chaque frappe.
    files: Option<Arc<Vec<PathBuf>>>,
    /// Le listage : un seul `git ls-files` à la fois, une demande pendant qu'il tourne le fait refaire à sa fin (un agent
    /// qui écrit sans cesse n'empile pas les processus). `failed` : le dernier a échoué (Git absent ou hors dépôt).
    listing: SingleFlight,
    failed: bool,
    /// Change quand le champ revient à l'arbre : le listage en cours est abandonné, son résultat jeté.
    epoch: u64,
    /// Les fichiers retenus, du meilleur au moins bon, et la liste que l'écran dessine (celle du filtre ou celle de la
    /// recherche, selon le mode).
    pub(super) hits: Vec<FilterHit>,
    pub(super) items: Vec<ResultItem>,
    /// L'indice, dans `items`, de la ligne sélectionnée.
    pub(super) selected: Option<usize>,
    /// Le texte du filtre dont `hits` est le classement : un classement refait pour le même texte (le disque a changé)
    /// garde la sélection sur son fichier, un autre texte la renvoie à la première ligne.
    shown_query: Option<String>,
    /// Les bascules de la recherche : elles restent celles du volet tant qu'IdemA tourne.
    pub(super) options: SearchOptions,
    /// La recherche affichée, la ligne de statut, l'arrêt de la recherche en cours et son numéro : un résultat qui n'est
    /// plus le dernier demandé est jeté. La recherche a ses numéros et sa tâche à elle : un changement du disque, qui
    /// oublie le classement du filtre, ne l'interrompt pas.
    pub(super) search: Option<SearchResult>,
    pub(super) status: Option<Status>,
    search_stop: Option<Arc<AtomicBool>>,
    search_generation: u64,
    search_task: Option<Task<()>>,
    /// Une recherche est demandée et son résultat n'est pas arrivé : « Remplacer » attend.
    searching: bool,
    /// Le champ « Remplacer par » : son texte est lu au clic sur « Remplacer ».
    pub(super) replace: Entity<InputState>,
    /// Les lignes décochées du résultat affiché ; toutes les lignes sont cochées à chaque nouvelle recherche.
    pub(super) unchecked: Unchecked,
    /// Le remplacement que le bouton vient de faire : « Remplacer » reste inactif jusqu'à un changement de la requête,
    /// d'une option ou du texte de « Remplacer par ».
    spent: SpentReplace,
    /// Les échecs du dernier remplacement (le fichier et son message), affichés au-dessus de la liste.
    pub(super) failures: Vec<(PathBuf, String)>,
    /// Un remplacement est en cours (les fichiers sans onglet s'écrivent hors du fil d'interface).
    pub(super) replacing: bool,
    /// Le bilan du dernier remplacement : il tient lieu de statut jusqu'au prochain changement du texte ou d'une option.
    summary: Option<String>,
    /// Change à chaque nouvelle recherche (texte, option) : le bilan d'un remplacement d'avant n'est plus montré.
    replace_generation: u64,
    replace_task: Option<Task<()>>,
    /// Le classement du texte courant est arrivé : « Aucun fichier. » ne s'affiche qu'alors, pas pendant l'attente.
    settled: bool,
    /// Numéro du dernier classement demandé : un résultat qui n'est plus le dernier est jeté.
    generation: u64,
    listing_task: Option<Task<()>>,
    ranking_task: Option<Task<()>>,
    pub(super) scroll: UniformListScrollHandle,
    _subscription: Subscription,
    _replace_subscription: Subscription,
}

impl ProjectFilter {
    pub(super) fn new(window: &mut Window, cx: &mut Context<FilesPanel>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(FILTER_PLACEHOLDER));
        let replace = cx.new(|cx| InputState::new(window, cx).placeholder(REPLACE_PLACEHOLDER));
        // `Change` vient d'une frappe, d'un collage, d'une composition, d'un annuler ; `set_value` n'en émet pas, ceux
        // qui l'appellent (`⌘⇧F`, Échap) relancent donc la suite eux-mêmes. `PressEnter` n'est pas écouté : `↩` se
        // propage à la boîte (`FilterOpen`, `IgnoreEnter`).
        let subscription = cx.subscribe_in(
            &query,
            window,
            |this, state, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = state.read(cx).value();
                    this.on_filter_changed(&text, cx);
                }
            },
        );
        let replace_subscription = cx.subscribe_in(
            &replace,
            window,
            |this, state, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = state.read(cx).value();
                    this.filter.spent.replacement_edited(&text);
                    cx.notify();
                }
            },
        );
        Self {
            query,
            text: String::new(),
            files: None,
            listing: SingleFlight::default(),
            failed: false,
            epoch: 0,
            hits: Vec::new(),
            items: Vec::new(),
            selected: None,
            shown_query: None,
            options: SearchOptions::default(),
            search: None,
            status: None,
            search_stop: None,
            search_generation: 0,
            search_task: None,
            searching: false,
            replace,
            unchecked: Unchecked::default(),
            spent: SpentReplace::default(),
            failures: Vec::new(),
            replacing: false,
            summary: None,
            replace_generation: 0,
            replace_task: None,
            settled: false,
            generation: 0,
            listing_task: None,
            ranking_task: None,
            scroll: UniformListScrollHandle::new(),
            _subscription: subscription,
            _replace_subscription: replace_subscription,
        }
    }

    /// Oublie ce qui a été classé et rend périmé tout classement en vol.
    fn clear_results(&mut self) {
        self.hits.clear();
        self.items.clear();
        self.selected = None;
        self.shown_query = None;
        self.settled = false;
        self.generation += 1;
        self.ranking_task = None;
    }

    /// Oublie la liste des fichiers : la prochaine demande la relit. Un classement en vol devient périmé ; un listage en
    /// cours continue (un seul à la fois) et sera refait à sa fin. La liste affichée reste jusqu'à l'arrivée de la
    /// suivante, elle ne clignote pas.
    fn drop_cache(&mut self) {
        self.files = None;
        self.failed = false;
        if self.listing.is_running() {
            self.listing.request();
        }
        self.generation += 1;
        self.ranking_task = None;
    }

    /// Oublie la liste des fichiers et ce qui en dépend : le champ est revenu à l'arbre. Le listage en cours est
    /// abandonné, son résultat jeté.
    fn forget_files(&mut self) {
        self.drop_cache();
        self.listing.cancel();
        self.epoch += 1;
        self.listing_task = None;
        self.clear_results();
        self.clear_search();
        self.reset_replacement();
    }

    /// Une nouvelle recherche (texte ou bascule changés) : toutes les lignes sont cochées, et le bilan et les échecs
    /// du remplacement d'avant disparaissent. Un remplacement encore en cours ne montrera pas son bilan.
    fn reset_replacement(&mut self) {
        self.unchecked = Unchecked::default();
        self.spent.rearm();
        self.failures.clear();
        self.summary = None;
        self.replace_generation += 1;
    }

    /// Arrête la recherche en cours (elle lit son drapeau avant chaque fichier) et rend périmé ce qu'elle rendrait.
    fn stop_search(&mut self) {
        if let Some(stop) = self.search_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.search_task = None;
        self.search_generation += 1;
        self.searching = false;
    }

    /// Oublie la recherche : son résultat, son statut et la liste qu'ils nourrissaient.
    fn clear_search(&mut self) {
        self.stop_search();
        self.search = None;
        self.status = None;
        if self.hits.is_empty() {
            // La liste affichée était celle de la recherche : le filtre n'a rien classé.
            self.items.clear();
            self.selected = None;
        }
    }

    /// La ligne sélectionnée de la recherche : son fichier et son numéro de ligne.
    fn selected_line_key(&self) -> Option<(PathBuf, usize)> {
        let search = self.search.as_ref()?;
        match self.items.get(self.selected?)? {
            ResultItem::Line { file, line } => {
                let found = search.outcome.files.get(*file)?;
                Some((found.path.clone(), found.lines.get(*line)?.line))
            }
            _ => None,
        }
    }
}

/// Un lot de la surveillance change-t-il la liste des fichiers du projet ? Oui dès qu'il nomme un chemin que l'arbre
/// n'ignore pas, ou un dossier que le watcher a débordé sur : un lot sous `target/` ne coûte rien au filtre.
pub(super) fn batch_changes_listing(
    batch: &WatchBatch,
    is_ignored: impl Fn(&Path) -> bool,
) -> bool {
    batch
        .paths
        .iter()
        .chain(batch.rescan.iter())
        .any(|path| !is_ignored(path))
}

/// L'extrait prêt à poser : au plus `MAX_SNIPPET_CHARS` caractères, les tabulations en espaces (même longueur en
/// octets, donc les plages restent justes) ; une plage coupée est raccourcie, une plage entièrement coupée disparaît.
fn fit_snippet((text, ranges): (String, Vec<Range<usize>>)) -> (String, Vec<Range<usize>>) {
    let text = text.replace('\t', " ");
    let Some((cut, _)) = text.char_indices().nth(MAX_SNIPPET_CHARS) else {
        return (text, ranges);
    };
    let ranges = ranges
        .into_iter()
        .filter(|range| range.start < cut)
        .map(|range| range.start..range.end.min(cut))
        .collect();
    (text[..cut].to_string(), ranges)
}

/// Le libellé d'un fichier de la recherche : le nom puis le dossier, le tout en retrait de texte (`TEXT_MIN`), coupé à
/// droite par l'élément qui le porte.
fn folder_label(parts: &HitParts) -> StyledText {
    let (label, runs) = label_runs(parts);
    let folder = HighlightStyle {
        color: Some(white(TEXT_MIN).into()),
        ..Default::default()
    };
    StyledText::new(label).with_highlights(runs.into_iter().map(|(range, _)| (range, folder)))
}

/// Ce qui distingue une plage du libellé d'un résultat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Emphasis {
    /// Un caractère trouvé, dans le nom ou le dossier.
    Found,
    /// Le dossier, en retrait.
    Folder,
}

/// L'écart entre le nom et le dossier, dans un seul texte coupé à droite.
const NAME_FOLDER_GAP: &str = "  ";

/// Le libellé d'un résultat : le nom, puis le dossier en retrait. Les plages (en octets, croissantes, sans
/// chevauchement, sur des limites de caractère) disent ce qui est trouvé et ce qui est le dossier ; le reste est le nom.
fn label_runs(parts: &HitParts) -> (String, Vec<(Range<usize>, Emphasis)>) {
    let mut runs = Vec::new();
    let found_len = |text: &str, at: usize| text[at..].chars().next().map_or(0, char::len_utf8);
    for &at in &parts.name_positions {
        runs.push((at..at + found_len(&parts.name, at), Emphasis::Found));
    }
    let mut label = parts.name.clone();
    if !parts.dir.is_empty() {
        label.push_str(NAME_FOLDER_GAP);
        let base = label.len();
        let mut done = base;
        for &at in &parts.dir_positions {
            let start = base + at;
            if done < start {
                runs.push((done..start, Emphasis::Folder));
            }
            let end = start + found_len(&parts.dir, at);
            runs.push((start..end, Emphasis::Found));
            done = end;
        }
        label.push_str(&parts.dir);
        if done < label.len() {
            runs.push((done..label.len(), Emphasis::Folder));
        }
    }
    (label, runs)
}

impl FilesPanel {
    /// Le mode du champ, d'après son texte.
    pub(super) fn filter_mode(&self) -> FieldMode<'_> {
        field_mode(&self.filter.text)
    }

    /// Le texte du champ a changé (une frappe, un collage, une composition). Un texte inchangé (une chasse arrière sans
    /// effet émet aussi `Changed`) ne refait rien.
    fn on_filter_changed(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.filter.text == text {
            return;
        }
        let was_search = matches!(field_mode(&self.filter.text), FieldMode::Search(_));
        text.clone_into(&mut self.filter.text);
        self.filter.reset_replacement();
        match self.filter_mode() {
            // L'arbre revient : résultats et liste des fichiers sont oubliés.
            FieldMode::Tree => self.filter.forget_files(),
            FieldMode::Filter(_) => {
                if was_search {
                    self.filter.clear_search();
                }
                self.refresh_filter(cx);
            }
            // La liste des fichiers reste, c'est ce que la recherche lit. Le filtre oublie son classement ; le résultat
            // d'une recherche précédente reste affiché jusqu'à l'arrivée du suivant.
            FieldMode::Search(_) => {
                if !was_search {
                    self.filter.clear_results();
                }
                self.start_search(cx);
            }
        }
        cx.notify();
    }

    /// Classe les fichiers du projet pour le texte courant (ou lance la recherche), en listant d'abord le projet si la
    /// liste est oubliée.
    fn refresh_filter(&mut self, cx: &mut Context<Self>) {
        let query = match self.filter_mode() {
            FieldMode::Filter(query) => query.to_owned(),
            FieldMode::Search(_) => {
                self.start_search(cx);
                return;
            }
            FieldMode::Tree => return,
        };
        if self.filter.failed {
            self.filter.clear_results();
            self.filter.settled = true;
        } else if let Some(files) = self.filter.files.clone() {
            self.rank_files(files, query, cx);
        } else {
            self.list_files(cx);
        }
        // Un listage en cours classera avec le texte courant à son arrivée.
    }

    /// `git ls-files` hors du fil d'interface, un seul à la fois : pendant qu'il tourne, la demande note de le refaire à
    /// sa fin (`finish_listing`). Un échec est dit à l'écran (« Git n'a pas pu lister… ») et au journal.
    fn list_files(&mut self, cx: &mut Context<Self>) {
        if !self.filter.listing.request() {
            return;
        }
        let epoch = self.filter.epoch;
        let root = self.root.clone();
        self.filter.listing_task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let listed = cx
                .background_spawn({
                    let root = root.clone();
                    async move { project_files(&root) }
                })
                .await;
            let took_ms = started.elapsed().as_millis();
            let _ = this.update(cx, |panel, cx| {
                panel.finish_listing(epoch, listed, took_ms, cx);
            });
        }));
    }

    fn finish_listing(
        &mut self,
        epoch: u64,
        listed: io::Result<Vec<PathBuf>>,
        took_ms: u128,
        cx: &mut Context<Self>,
    ) {
        if epoch != self.filter.epoch {
            return;
        }
        let redo = self.filter.listing.finish();
        match listed {
            Ok(files) => {
                hud::log_event(&format!(
                    "project files listed root={} count={} took_ms={took_ms}",
                    self.root.display(),
                    files.len()
                ));
                self.filter.files = Some(Arc::new(files));
                self.refresh_filter(cx);
                if redo {
                    // Le disque a changé pendant le listage : la liste qu'on vient de poser est peut-être périmée, elle
                    // est refaite une fois (les lots qui ont suivi n'ont pas lancé de processus).
                    self.filter.files = None;
                    self.list_files(cx);
                }
            }
            Err(error) => {
                hud::log_event(&format!(
                    "project files failed root={} error={error}",
                    self.root.display()
                ));
                self.filter.failed = true;
                self.filter.clear_results();
                self.filter.settled = true;
                if matches!(self.filter_mode(), FieldMode::Search(_)) {
                    self.filter.clear_search();
                    self.filter.status = Some(Status::error(GIT_FAILED));
                }
                cx.notify();
            }
        }
    }

    /// Le classement flou de `files` pour `query`, hors du fil d'interface. Seul le dernier demandé s'applique.
    fn rank_files(&mut self, files: Arc<Vec<PathBuf>>, query: String, cx: &mut Context<Self>) {
        self.filter.generation += 1;
        let generation = self.filter.generation;
        let root = self.root.clone();
        self.filter.ranking_task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let ranked = query.clone();
            let hits = cx
                .background_spawn(async move { filter_hits(&root, &files, &ranked) })
                .await;
            let took_ms = started.elapsed().as_millis();
            let _ = this.update(cx, |panel, cx| {
                panel.finish_ranking(generation, hits, query, took_ms, cx);
            });
        }));
    }

    /// Un classement est arrivé : s'il est encore le dernier, la liste est refaite. Pour un autre texte, la sélection va
    /// au premier et la liste revient en haut ; pour le même texte (le disque a changé : l'enregistrement d'un fichier
    /// ouvert depuis la liste en est un), la sélection reste sur son fichier tant qu'il est listé, et la liste ne bouge pas.
    fn finish_ranking(
        &mut self,
        generation: u64,
        hits: Vec<FilterHit>,
        query: String,
        took_ms: u128,
        cx: &mut Context<Self>,
    ) {
        if generation != self.filter.generation {
            return;
        }
        hud::log_debug(&format!(
            "files filter done query_len={} results={} took_ms={took_ms}",
            query.chars().count(),
            hits.len()
        ));
        let previous = self.filter.selected.and_then(|index| self.hit_path(index));
        let same_query = self.filter.shown_query.as_deref() == Some(query.as_str());
        let kept = if same_query {
            kept_hit(previous.as_deref(), &hits)
        } else {
            None
        };
        self.filter.items = (0..hits.len()).map(ResultItem::Hit).collect();
        self.filter.hits = hits;
        self.filter.shown_query = Some(query);
        self.filter.selected = kept.or_else(|| first_selectable(&self.filter.items));
        self.filter.settled = true;
        match kept {
            Some(index) => self
                .filter
                .scroll
                .scroll_to_item(index, ScrollStrategy::Nearest),
            None if !self.filter.items.is_empty() => {
                self.filter.scroll.scroll_to_item(0, ScrollStrategy::Top);
            }
            None => {}
        }
        cx.notify();
    }

    /// La recherche dans le contenu pour le texte et les bascules courants : le champ vide ne cherche rien, une
    /// expression invalide le dit, sinon la recherche part hors du fil d'interface (la précédente est arrêtée). Le
    /// résultat d'avant reste affiché, sous « Recherche… », jusqu'à l'arrivée du nouveau. Le texte cherché n'est jamais
    /// au journal, seulement sa longueur.
    fn start_search(&mut self, cx: &mut Context<Self>) {
        let FieldMode::Search(query) = self.filter_mode() else {
            return;
        };
        let query = query.to_owned();
        if query.is_empty() {
            self.filter.clear_search();
            cx.notify();
            return;
        }
        let query_len = query.chars().count();
        let regex = match compile(&query, self.filter.options) {
            Ok(regex) => regex,
            Err(QueryError::InvalidRegex) => {
                self.filter.clear_search();
                self.filter.status = Some(Status::error(INVALID_REGEX));
                hud::log_debug(&format!("project search invalid query_len={query_len}"));
                cx.notify();
                return;
            }
        };
        if self.filter.failed {
            self.filter.clear_search();
            self.filter.status = Some(Status::error(GIT_FAILED));
            cx.notify();
            return;
        }
        self.filter.stop_search();
        self.filter.searching = true;
        // Après un remplacement, la recherche repart sous le bilan, qui reste à l'écran.
        self.filter.status = Some(Status::info(
            self.filter
                .summary
                .clone()
                .unwrap_or_else(|| SEARCHING.to_string()),
        ));
        match self.filter.files.clone() {
            Some(files) => self.spawn_search(files, query, regex, cx),
            // Le listage classera, à son arrivée, avec le texte et les bascules d'alors.
            // Un listage en cours classera, à son arrivée, avec le texte et les bascules d'alors.
            None => self.list_files(cx),
        }
        cx.notify();
    }

    /// Le texte de chaque onglet ouvert : c'est lui qu'on cherche, pas celui du disque (une frappe n'est peut-être pas
    /// encore sauvegardée).
    fn open_tab_texts(&self, cx: &Context<Self>) -> HashMap<PathBuf, String> {
        self.open
            .iter()
            .map(|(key, tab)| (key.0.clone(), tab.editor.read(cx).value().to_string()))
            .collect()
    }

    /// `search_files` hors du fil d'interface, borné à `MAX_RESULT_LINES` lignes. Le drapeau d'arrêt est lu avant chaque
    /// fichier : une recherche remplacée s'arrête sans rien rendre.
    fn spawn_search(
        &mut self,
        files: Arc<Vec<PathBuf>>,
        query: String,
        regex: Regex,
        cx: &mut Context<Self>,
    ) {
        let overrides = self.open_tab_texts(cx);
        let stop = Arc::new(AtomicBool::new(false));
        self.filter.search_stop = Some(stop.clone());
        let generation = self.filter.search_generation;
        let options = self.filter.options;
        self.filter.search_task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let compiled = regex.clone();
            let outcome = cx
                .background_spawn(async move {
                    search_files(&files, &overrides, &compiled, MAX_RESULT_LINES, &stop)
                })
                .await;
            let took_ms = started.elapsed().as_millis();
            let result = SearchResult {
                query,
                options,
                regex,
                outcome,
            };
            let _ = this.update(cx, |panel, cx| {
                panel.finish_search(generation, result, took_ms, cx);
            });
        }));
    }

    /// Une recherche est arrivée : si elle est encore la dernière, la liste est refaite. Pour un autre texte, la sélection
    /// va à la première ligne et la liste revient en haut ; pour le même texte (une bascule, un changement du disque),
    /// la sélection reste sur sa ligne tant que la recherche la trouve encore.
    fn finish_search(
        &mut self,
        generation: u64,
        result: SearchResult,
        took_ms: u128,
        cx: &mut Context<Self>,
    ) {
        if generation != self.filter.search_generation {
            return;
        }
        let outcome = &result.outcome;
        hud::log_debug(&format!(
            "project search done query_len={} files={} lines={} truncated={} took_ms={took_ms}",
            result.query.chars().count(),
            outcome.files.len(),
            outcome.line_count(),
            outcome.truncated
        ));
        let items = result_items(outcome);
        let same_query = self
            .filter
            .search
            .as_ref()
            .is_some_and(|shown| shown.query == result.query);
        let previous = self.filter.selected_line_key();
        let kept = if same_query {
            kept_line(
                previous
                    .as_ref()
                    .map(|(path, line)| (path.as_path(), *line)),
                &items,
                outcome,
            )
        } else {
            None
        };
        self.filter.selected = kept.or_else(|| first_selectable(&items));
        match kept {
            Some(index) => self
                .filter
                .scroll
                .scroll_to_item(index, ScrollStrategy::Nearest),
            None if !items.is_empty() => {
                self.filter.scroll.scroll_to_item(0, ScrollStrategy::Top);
            }
            None => {}
        }
        self.filter.items = items;
        self.filter.status = Some(Status::info(
            self.filter
                .summary
                .clone()
                .unwrap_or_else(|| search_status(outcome, MAX_RESULT_LINES)),
        ));
        // Les cases décochées d'avant ne valent que si les lignes n'ont pas bougé (voir `Unchecked::carry_over`).
        if let Some(shown) = self.filter.search.as_ref() {
            self.filter
                .unchecked
                .carry_over(&shown.outcome, &result.outcome);
        }
        self.filter.search_stop = None;
        self.filter.searching = false;
        self.filter.search = Some(result);
        cx.notify();
    }

    /// Une bascule de la recherche change et relance la recherche si le champ en est une.
    fn toggle_search_option(&mut self, toggle: Toggle, cx: &mut Context<Self>) {
        let options = &mut self.filter.options;
        match toggle {
            Toggle::MatchCase => options.case_sensitive = !options.case_sensitive,
            Toggle::WholeWord => options.whole_word = !options.whole_word,
            Toggle::Regex => options.regex = !options.regex,
        }
        self.filter.reset_replacement();
        self.start_search(cx);
        cx.notify();
    }

    /// Coche ou décoche la ligne `index` de la liste (la case de la ligne). Un en-tête de fichier n'a pas de case.
    fn toggle_line_checked(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(ResultItem::Line { file, line }) = self.filter.items.get(index).copied() else {
            return;
        };
        let Some((path, number)) = self
            .filter
            .search
            .as_ref()
            .and_then(|search| search.outcome.files.get(file))
            .and_then(|found| found.lines.get(line).map(|line| (&found.path, line.line)))
        else {
            return;
        };
        let path = path.clone();
        self.filter.unchecked.toggle(&path, number);
        cx.notify();
    }

    /// Le bouton « Remplacer » est-il actif ? Il faut un résultat affiché, au moins une ligne cochée, ni recherche ni
    /// remplacement en cours, et que le dernier remplacement ne soit pas « consommé » (`SpentReplace`).
    fn can_replace(&self) -> bool {
        let filter = &self.filter;
        let checked = filter
            .search
            .as_ref()
            .map_or(0, |search| filter.unchecked.checked_count(&search.outcome));
        replace_enabled(filter.searching, filter.replacing, &filter.spent, checked)
    }

    /// « Remplacer » : écrit les seules lignes cochées du résultat affiché, avec l'expression qui l'a produit et le texte
    /// de « Remplacer par » (`$1` développé en mode expression régulière seulement). Dans l'ordre : le statut passe à
    /// « Remplacement… » ; un fichier ouvert dans un onglet est remplacé dans le tampon de l'éditeur (un `⌘Z` l'annule),
    /// sauf si le disque a changé sous l'onglet, auquel cas il est en échec et l'onglet n'est pas touché (le disque
    /// gagne, P2-7) ; l'onglet est sauvegardé tout de suite. Les autres fichiers s'écrivent hors du fil d'interface,
    /// par `replace_in_file`. Le bilan arrive dans `finish_replace`, qui relance la recherche. Aucune écriture n'a lieu
    /// sans geste : ni la frappe ni `↩` ne mènent ici.
    pub(super) fn replace_checked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_replace() {
            return;
        }
        let Some(search) = self.filter.search.as_ref() else {
            return;
        };
        let targets = self.filter.unchecked.targets(&search.outcome);
        let spec = ReplaceSpec {
            regex: search.regex.clone(),
            replacement: self.filter.replace.read(cx).value().to_string(),
            expand: search.options.regex,
        };
        self.filter.spent.spend(&spec.replacement);
        self.filter.replacing = true;
        self.filter.failures.clear();
        self.filter.summary = None;
        self.filter.status = Some(Status::info(REPLACING));
        let mut run = ReplaceRun {
            generation: self.filter.replace_generation,
            spec,
            outcomes: Vec::new(),
            on_disk: Vec::new(),
            saving: Vec::new(),
        };
        for file in targets {
            if self.open.contains_key(file.path.as_path()) {
                match self.replace_in_tab(&file, &run.spec, window, cx) {
                    TabReplace::Done(outcome) => run.outcomes.push(outcome),
                    TabReplace::Saving(replaced) => run.saving.push((file.path, replaced)),
                }
            } else {
                run.on_disk.push(file);
            }
        }
        if run.saving.is_empty() {
            self.write_remaining(run, cx);
        } else {
            // L'éditeur annonce ses changements à la fin de cette mise à jour : les sauvegardes passent derrière lui,
            // quand les onglets savent qu'ils ont un texte à écrire. Sans cela elles ne trouveraient rien à sauvegarder.
            cx.defer_in(window, move |this, window, cx| {
                this.save_replaced_tabs(run, window, cx);
            });
        }
        cx.notify();
    }

    /// Sauvegarde les onglets remplacés, tout de suite, et décide de leur bilan d'après ce que chaque sauvegarde a
    /// réellement fait (`tab_outcome`) : un refus d'écriture ou un disque qui a changé en fait un échec, pas un fichier
    /// modifié.
    fn save_replaced_tabs(
        &mut self,
        mut run: ReplaceRun,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (path, replaced) in std::mem::take(&mut run.saving) {
            let saved = self.save_tab_with(&path, Some(&mut *window), false, cx);
            run.outcomes.push(tab_outcome(path, replaced, saved));
        }
        self.write_remaining(run, cx);
    }

    /// Les fichiers sans onglet s'écrivent hors du fil d'interface, par `replace_in_file` ; sans eux, le bilan est fait
    /// tout de suite. Le bilan est unique : il attend les onglets et les autres fichiers.
    fn write_remaining(&mut self, mut run: ReplaceRun, cx: &mut Context<Self>) {
        if run.on_disk.is_empty() {
            self.finish_replace(run.generation, run.outcomes, cx);
            return;
        }
        let on_disk = std::mem::take(&mut run.on_disk);
        self.filter.replace_task = Some(cx.spawn(async move |this, cx| {
            let spec = run.spec;
            let written = cx
                .background_spawn(async move {
                    on_disk
                        .into_iter()
                        .map(|file| {
                            let result = replace_in_file(
                                &file.path,
                                &file.lines,
                                file.line_count,
                                &spec.regex,
                                &spec.replacement,
                                spec.expand,
                            );
                            file_outcome(file.path, result)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let mut outcomes = run.outcomes;
            let _ = this.update(cx, |panel, cx| {
                outcomes.extend(written);
                panel.finish_replace(run.generation, outcomes, cx);
            });
        }));
    }

    /// Le remplacement d'un fichier ouvert, dans le tampon de son éditeur. Le fichier est en échec, sans que l'onglet soit
    /// touché, quand le disque a changé sous lui sans qu'il se soit rechargé (empreinte), quand un rechargement est en
    /// cours, quand le fichier est supprimé (un remplacement ne le recréerait pas) ou n'est plus du texte. Sinon
    /// `replace_all` garde l'historique (un seul `⌘Z`), la sélection et le défilement sont remis, bornés à la nouvelle
    /// longueur comme au rechargement ; sa sauvegarde part tout de suite après (`save_replaced_tabs`), le bilan du fichier
    /// n'est décidé qu'alors.
    fn replace_in_tab(
        &mut self,
        file: &FileTargets,
        spec: &ReplaceSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TabReplace {
        let path = file.path.as_path();
        let failed =
            |error: ReplaceError| TabReplace::Done(file_outcome(path.to_path_buf(), Err(error)));
        let Some(tab) = self.open.get(path) else {
            return failed(ReplaceError::Changed);
        };
        if tab.save.is_unreadable() {
            return failed(ReplaceError::NotText);
        }
        // Une métadonnée illisible n'est pas un changement (comme à la sauvegarde) : l'écriture dira la vraie panne.
        let disk_changed = fingerprint(path)
            .is_ok_and(|disk| tab.save.check_write(disk) == SaveCheck::DiskChanged);
        if disk_changed || tab.reloading || tab.save.is_deleted() {
            return failed(ReplaceError::Changed);
        }
        let editor = tab.editor.clone();
        let text = editor.read(cx).value().to_string();
        let replaced = replace_lines(
            &text,
            &file.lines,
            file.line_count,
            &spec.regex,
            &spec.replacement,
            spec.expand,
        );
        let done = FileReplace {
            occurrences: replaced.occurrences,
            stale_lines: replaced.stale_lines,
        };
        if replaced.occurrences == 0 {
            return TabReplace::Done(file_outcome(path.to_path_buf(), Ok(done)));
        }
        editor.update(cx, |state, cx| {
            let selection = state.selected_range();
            let scroll = state.scroll_offset();
            state.replace_all(replaced.text, window, cx);
            let length = state.text().len();
            state.set_selected_range(selection.start.min(length)..selection.end.min(length), cx);
            state.set_scroll_offset(scroll, cx);
        });
        TabReplace::Saving(done)
    }

    /// Les fichiers sont écrits : le journal, le bilan, les échecs, puis la recherche repart avec la même requête et les
    /// mêmes options (les lignes décochées le restent). Si le texte ou une option ont changé entretemps, le bilan n'a plus
    /// de recherche à qui parler : seul le journal le dit.
    fn finish_replace(
        &mut self,
        generation: u64,
        outcomes: Vec<FileOutcome>,
        cx: &mut Context<Self>,
    ) {
        let (files, occurrences, failed) = totals(&outcomes);
        hud::log_event(&format!(
            "project replace done files={files} occurrences={occurrences} failed={failed}"
        ));
        for outcome in &outcomes {
            if let Some(failure) = &outcome.failure {
                let detail = failure
                    .detail
                    .as_ref()
                    .map_or_else(String::new, |detail| format!(" error={detail}"));
                hud::log_event(&format!(
                    "project replace failed path={} reason={}{detail}",
                    outcome.path.display(),
                    failure.reason
                ));
            }
        }
        self.filter.replacing = false;
        self.filter.replace_task = None;
        if generation != self.filter.replace_generation {
            cx.notify();
            return;
        }
        self.filter.failures = outcomes
            .into_iter()
            .filter_map(|outcome| Some((outcome.path, outcome.failure?.message)))
            .collect();
        let summary = replace_summary(files, occurrences, failed);
        self.filter.status = Some(Status::info(summary.clone()));
        self.filter.summary = Some(summary);
        self.start_search(cx);
        cx.notify();
    }

    /// Le champ prend le focus.
    pub(super) fn focus_filter_field(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.filter.query.focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Le champ « Remplacer par » prend le focus (`Tab` depuis la requête).
    fn focus_replace_field(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.filter.replace.focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// `⌘⇧F` : l'arbre replié se déplie, et le champ prend le focus. Déjà en recherche, la requête (après le `?`) est
    /// sélectionnée, la frappe suivante la remplace ; sinon le champ devient `?`.
    pub fn start_project_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tree_collapsed {
            self.toggle_tree(cx);
        }
        if matches!(self.filter_mode(), FieldMode::Search(_)) {
            let end = self.filter.text.len();
            self.filter
                .query
                .update(cx, |field, cx| field.set_selected_range(1..end, cx));
        } else {
            self.filter
                .query
                .update(cx, |field, cx| field.set_value("?", window, cx));
            self.on_filter_changed("?", cx);
        }
        self.focus_filter_field(window, cx);
        cx.notify();
    }

    /// Le disque a changé sous le projet (un lot de la surveillance, l'affichage du volet) : la liste des fichiers est
    /// oubliée, et relue et reclassée si le filtre est à l'écran ; la liste affichée reste en attendant la nouvelle. Hors
    /// du filtre, rien ne se redessine : un lot ne doit rien coûter à un volet au repos.
    pub(super) fn forget_project_files(&mut self, cx: &mut Context<Self>) {
        let was_listing = self.filter.listing.is_running();
        self.filter.drop_cache();
        match self.filter_mode() {
            FieldMode::Filter(_) => self.refresh_filter(cx),
            // Le résultat de la recherche est un instantané : il reste. Seule une recherche qui attendait le listage,
            // maintenant oublié, repart.
            FieldMode::Search(_) if was_listing => self.start_search(cx),
            _ => {}
        }
    }

    /// `↑` et `↓` : la sélection passe à la ligne suivante ou précédente et reste à l'écran.
    fn move_filter_selection(&mut self, down: bool, cx: &mut Context<Self>) {
        let next = move_selection(&self.filter.items, self.filter.selected, down);
        if next != self.filter.selected {
            self.filter.selected = next;
            if let Some(index) = next {
                self.filter
                    .scroll
                    .scroll_to_item(index, ScrollStrategy::Nearest);
            }
            cx.notify();
        }
    }

    /// Le fichier de la ligne `index` de la liste, si c'est un résultat du filtre.
    fn hit_path(&self, index: usize) -> Option<PathBuf> {
        match self.filter.items.get(index)? {
            ResultItem::Hit(hit) => self.filter.hits.get(*hit).map(|hit| hit.path.clone()),
            _ => None,
        }
    }

    /// Ouvre `path` dans un onglet épinglé, le focus à l'éditeur. Un fichier confié à l'app par défaut n'a pas d'onglet :
    /// rien n'est épinglé alors.
    fn open_pinned(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path(path, None, true, window, cx);
    }

    /// `↩` : le fichier sélectionné du filtre s'ouvre épinglé ; la ligne sélectionnée d'une recherche s'ouvre à son
    /// occurrence. Sans sélection (arbre, aucun résultat), rien.
    fn open_filter_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.filter.selected else {
            return;
        };
        match self.filter_mode() {
            FieldMode::Filter(_) => {
                if let Some(path) = self.hit_path(index) {
                    self.open_pinned(&path, window, cx);
                }
            }
            FieldMode::Search(_) => self.open_search_item(index, false, window, cx),
            FieldMode::Tree => {}
        }
    }

    /// Le fichier d'une ligne de la recherche, avec la ligne et la colonne où l'ouvrir : celles de la ligne, ou de la
    /// première ligne trouvée pour l'en-tête d'un fichier.
    fn search_target(&self, index: usize) -> Option<(PathBuf, (u32, u32))> {
        let search = self.filter.search.as_ref()?;
        let (file, line) = match *self.filter.items.get(index)? {
            ResultItem::Line { file, line } => (file, line),
            ResultItem::File(file) => (file, 0),
            ResultItem::Hit(_) => return None,
        };
        let found = search.outcome.files.get(file)?;
        Some((found.path.clone(), open_position(found.lines.get(line)?)))
    }

    /// Ouvre la ligne `index` de la recherche à son occurrence, le focus à l'éditeur ; `pin` épingle aussi l'onglet.
    fn open_search_item(
        &mut self,
        index: usize,
        pin: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, position)) = self.search_target(index) else {
            return;
        };
        self.open_file(&path, Some(position), true, window, cx);
        if pin && let Some(tab) = self.tabs.index_of(&canonical_path(&path)) {
            self.tabs.pin(tab);
            cx.notify();
        }
    }

    /// Échap : le champ est vidé, l'arbre revient et prend le focus. Le texte vidé n'émet rien : le panneau applique la
    /// suite lui-même.
    fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter
            .query
            .update(cx, |field, cx| field.set_value("", window, cx));
        self.filter.text.clear();
        self.filter.forget_files();
        self.focus_tree(window, cx);
        cx.notify();
    }

    /// Un appui sur une ligne la sélectionne sans prendre le focus : le clavier reste au champ. Un en-tête de fichier
    /// n'est pas sélectionnable.
    fn on_hit_pressed(&mut self, index: usize, cx: &mut Context<Self>) {
        if self
            .filter
            .items
            .get(index)
            .is_some_and(|item| item.is_selectable())
        {
            self.filter.selected = Some(index);
            cx.notify();
        }
    }

    /// Un clic sur une ligne de la recherche, ou sur l'en-tête d'un fichier : le fichier s'ouvre à l'occurrence et
    /// l'éditeur prend le focus ; un double-clic épingle aussi l'onglet.
    fn on_search_clicked(
        &mut self,
        index: usize,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, _)) = self.search_target(index) else {
            return;
        };
        let already_open = self.tabs.index_of(&canonical_path(&path)).is_some();
        if !is_repeat_of_external_open(click_count, already_open) {
            self.open_search_item(index, click_count >= 2, window, cx);
        }
    }

    /// Un clic au relâché : l'aperçu s'ouvre et le focus reste au champ ; un double-clic épingle le fichier et donne le
    /// focus à l'éditeur.
    fn on_hit_clicked(
        &mut self,
        index: usize,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.hit_path(index) else {
            return;
        };
        self.filter.selected = Some(index);
        let already_open = self.tabs.index_of(&canonical_path(&path)).is_some();
        if !is_repeat_of_external_open(click_count, already_open) {
            if click_count >= 2 {
                self.open_pinned(&path, window, cx);
            } else {
                self.open_file(&path, None, false, window, cx);
            }
        }
        cx.notify();
    }

    /// La boîte du champ : loupe, puis le champ. Un appui gauche y donne le focus au champ ; un appui droit y est arrêté
    /// (pas de menu de la racine). Les touches `↑` `↓` `↩` Échap `Tab` du champ sont prises ici, même quand elles ne
    /// font rien : sans gestionnaire, GPUI passerait à `RenameSelected` ou `TrashSelected` de l'arbre.
    pub(super) fn render_filter_box(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let field_focus = self.filter.query.focus_handle(cx);
        div()
            .id("files-filter-box")
            .key_context(FILTER_KEY_CONTEXT)
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(theme.px(FIELD_GAP))
            .h(theme.px(FIELD_HEIGHT))
            .px(theme.px(FIELD_PADDING))
            .rounded(theme.px(FIELD_RADIUS))
            .border_1()
            .border_color(white(FIELD_RING))
            .bg(black(FIELD_BACKGROUND))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _: &MouseDownEvent, window, cx| {
                    window.focus(&field_focus, cx);
                    // Le corps du volet reprendrait le focus à la remontée de l'appui.
                    window.prevent_default();
                }),
            )
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_action(cx.listener(|_, _: &FieldGuard, _, _| {}))
            .on_action(cx.listener(|this, _: &FilterUp, _, cx| {
                this.move_filter_selection(false, cx);
            }))
            .on_action(cx.listener(|this, _: &FilterDown, _, cx| {
                this.move_filter_selection(true, cx);
            }))
            .on_action(cx.listener(|this, _: &FilterOpen, window, cx| {
                this.open_filter_selection(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FilterClear, window, cx| {
                this.clear_filter(window, cx);
            }))
            // `Tab` passe au champ « Remplacer par » en recherche ; en filtre il n'y a pas d'autre champ, la touche est
            // prise sans effet.
            .on_action(cx.listener(|this, _: &FilterNextField, window, cx| {
                if matches!(this.filter_mode(), FieldMode::Search(_)) {
                    this.focus_replace_field(window, cx);
                }
            }))
            .child(
                svg()
                    .path(Icon::Search.path())
                    .size(theme.px(FIELD_ICON))
                    .flex_none()
                    .text_color(white(TEXT_MIN)),
            )
            .child(field_input(&self.filter.query, theme))
    }

    /// La zone sous l'en-tête quand le champ n'est pas vide : la liste du filtre, ou les bascules, le statut et les
    /// résultats de la recherche.
    pub(super) fn render_filter_results(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.filter_mode() {
            FieldMode::Tree => div().size_full().into_any_element(),
            FieldMode::Filter(_) if self.filter.items.is_empty() => {
                self.render_filter_status(theme)
            }
            FieldMode::Filter(_) => self.render_result_list(cx),
            FieldMode::Search(_) => self.render_search_area(theme, cx),
        }
    }

    /// La liste défilante des lignes : résultats du filtre, ou en-têtes de fichier et lignes de la recherche.
    fn render_result_list(&self, cx: &mut Context<Self>) -> AnyElement {
        uniform_list(
            "files-filter-rows",
            self.filter.items.len(),
            cx.processor(
                |this, range: Range<usize>, _window, cx: &mut Context<Self>| {
                    let theme = *cx.global::<Theme>();
                    range
                        .map(|index| this.render_result_row(index, &theme, cx))
                        .collect::<Vec<_>>()
                },
            ),
        )
        .track_scroll(&self.filter.scroll)
        .size_full()
        .into_any_element()
    }

    fn render_result_row(&self, index: usize, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        match self.filter.items.get(index) {
            Some(ResultItem::Hit(_)) => self.render_hit(index, theme, cx),
            Some(ResultItem::File(file)) => self.render_search_file(index, *file, theme, cx),
            Some(ResultItem::Line { file, line }) => {
                self.render_search_line(index, *file, *line, theme, cx)
            }
            None => div().into_any_element(),
        }
    }

    /// Le mode recherche : le champ « Remplacer par », la ligne des trois bascules et du bouton « Remplacer », le statut
    /// (sur plusieurs lignes au besoin), les échecs du dernier remplacement, puis les résultats.
    fn render_search_area(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let options = self.filter.options;
        let switches = div()
            .flex()
            .items_center()
            .gap(theme.px(TOGGLE_GAP))
            .child(self.render_toggle(
                "files-toggle-case",
                Icon::MatchCase,
                options.case_sensitive,
                Toggle::MatchCase,
                theme,
                cx,
            ))
            .child(self.render_toggle(
                "files-toggle-word",
                Icon::MatchWord,
                options.whole_word,
                Toggle::WholeWord,
                theme,
                cx,
            ))
            .child(self.render_toggle(
                "files-toggle-regex",
                Icon::RegularExpression,
                options.regex,
                Toggle::Regex,
                theme,
                cx,
            ));
        let toggles = div()
            .id("files-search-toggles")
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .px(theme.px(2.))
            .pb(theme.px(4.))
            .child(switches)
            .child(self.render_replace_button(theme, cx));
        let status = self.filter.status.as_ref().map(|status| {
            div()
                .id("files-search-status")
                .flex_none()
                .w_full()
                .px(theme.px(9.))
                .pb(theme.px(6.))
                .text_size(theme.text(11.))
                .text_color(if status.error {
                    color(ERROR_TEXT)
                } else {
                    white(TEXT_MIN)
                })
                .child(status.text.clone())
        });
        let list = if self.filter.items.is_empty() {
            div().into_any_element()
        } else {
            self.render_result_list(cx)
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_replace_box(theme, cx))
            .child(toggles)
            .children(status)
            .children(self.render_failures(theme))
            .child(div().flex_1().min_h_0().child(list))
            .into_any_element()
    }

    /// Le champ « Remplacer par » : la boîte du champ de la requête, sans icône. Ses touches sont prises ici même quand
    /// elles ne font rien (`↩` en tête) : sans gestionnaire, GPUI passerait à `RenameSelected` ou `TrashSelected` de
    /// l'arbre. Un appui gauche y donne le focus au champ ; un appui droit y est arrêté (pas de menu de la racine).
    fn render_replace_box(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let field_focus = self.filter.replace.focus_handle(cx);
        div().flex_none().px(theme.px(2.)).pb(theme.px(6.)).child(
            div()
                .id("files-replace-box")
                .key_context(REPLACE_KEY_CONTEXT)
                .flex()
                .items_center()
                .w_full()
                .min_w_0()
                .h(theme.px(FIELD_HEIGHT))
                .px(theme.px(FIELD_PADDING))
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
                        // La colonne de l'arbre rendrait sinon le focus au champ de la requête.
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                // Un remplacement ne part jamais au clavier : `↩` est pris, sans effet.
                .on_action(cx.listener(|_, _: &IgnoreEnter, _, _| {}))
                .on_action(cx.listener(|_, _: &FieldGuard, _, _| {}))
                .on_action(cx.listener(|this, _: &FilterClear, window, cx| {
                    this.clear_filter(window, cx);
                }))
                .on_action(cx.listener(|this, _: &FilterNextField, window, cx| {
                    this.focus_filter_field(window, cx);
                }))
                .child(field_input(&self.filter.replace, theme)),
        )
    }

    /// Le bouton « Remplacer », secondaire : actif quand `can_replace`, sinon son texte est estompé et le survol absent.
    fn render_replace_button(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = self.can_replace();
        div()
            .id("files-replace-button")
            .flex()
            .flex_none()
            .items_center()
            .h(theme.px(BUTTON_HEIGHT))
            .px(theme.px(BUTTON_PADDING))
            .rounded(theme.px(BUTTON_RADIUS))
            .bg(white(HOVER_STRONG))
            .text_size(theme.text(BUTTON_TEXT))
            .text_color(white(if enabled { TEXT_PRIMARY } else { TEXT_DISABLED }))
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(|style| style.bg(white(CONTROL)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if enabled && is_replace_press(event.click_count) {
                        this.replace_checked(window, cx);
                    }
                    // Le champ garde le focus, comme pour une bascule.
                    window.prevent_default();
                }),
            )
            .child("Remplacer")
    }

    /// Les échecs du dernier remplacement, au-dessus de la liste : le glyphe et le nom du fichier, puis son message en
    /// rouge sur plusieurs lignes au besoin. La liste défile au-delà de `FAILURES_MAX_HEIGHT`.
    fn render_failures(&self, theme: &Theme) -> Option<AnyElement> {
        if self.filter.failures.is_empty() {
            return None;
        }
        let rows = self
            .filter
            .failures
            .iter()
            .enumerate()
            .map(|(index, (path, message))| {
                let parts = self.path_parts(path);
                div()
                    .id(("files-replace-failure", index))
                    .flex()
                    .flex_col()
                    .w_full()
                    .px(theme.px(8.))
                    .pb(theme.px(4.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(theme.px(7.))
                            .h(theme.px(ROW_HEIGHT))
                            .text_color(white(TREE_TEXT))
                            .text_size(theme.text(12.5))
                            .child(
                                div()
                                    .flex_none()
                                    .w(theme.px(GLYPH_SLOT))
                                    .flex()
                                    .justify_center()
                                    .child(row_lead(theme, &parts.name, false, false)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(folder_label(&parts)),
                            ),
                    )
                    .child(
                        div()
                            .pl(theme.px(GLYPH_SLOT + 7.))
                            .text_size(theme.text(11.))
                            .text_color(color(ERROR_TEXT))
                            .child(message.clone()),
                    )
            });
        Some(
            div()
                .id("files-replace-failures")
                .flex_none()
                .w_full()
                .max_h(theme.px(FAILURES_MAX_HEIGHT))
                .overflow_y_scroll()
                .children(rows)
                .into_any_element(),
        )
    }

    /// Le nom et le dossier d'un fichier du projet, pour un libellé.
    fn path_parts(&self, path: &Path) -> HitParts {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        split_hit(&FilterHit {
            path: path.to_path_buf(),
            relative: relative.to_string_lossy().into_owned(),
            positions: Vec::new(),
        })
    }

    /// Une bascule de 22 : active, `FOCUS` à 18 % en fond et l'icône en bleu clair ; inactive, l'icône en retrait et un
    /// fond au survol. Le champ garde le focus.
    fn render_toggle(
        &self,
        id: &'static str,
        icon: Icon,
        active: bool,
        toggle: Toggle,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(theme.px(TOGGLE_SIZE))
            .rounded(theme.px(TOGGLE_RADIUS))
            .cursor_pointer()
            .when(active, |toggle| toggle.bg(focus(TOGGLE_ACTIVE)))
            .when(!active, |toggle| {
                toggle.hover(|style| style.bg(white(HOVER_STRONG)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.toggle_search_option(toggle, cx);
                    window.prevent_default();
                }),
            )
            .child(
                svg()
                    .path(icon.path())
                    .size(theme.px(TOGGLE_ICON))
                    .text_color(if active {
                        color(FOCUS_TEXT)
                    } else {
                        white(ICON_SECONDARY)
                    }),
            )
    }

    /// L'en-tête d'un fichier de la recherche : glyphe, nom, dossier en retrait, et à droite le nombre de ses lignes.
    /// Il n'est pas sélectionnable ; un clic ouvre le fichier à sa première occurrence.
    fn render_search_file(
        &self,
        index: usize,
        file: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(found) = self
            .filter
            .search
            .as_ref()
            .and_then(|search| search.outcome.files.get(file))
        else {
            return div().into_any_element();
        };
        let parts = self.path_parts(&found.path);
        div()
            .id(("files-search-file", index))
            .flex()
            .items_center()
            .gap(theme.px(7.))
            .h(theme.px(ROW_HEIGHT))
            .w_full()
            .px(theme.px(8.))
            .rounded(theme.px(5.))
            .border_1()
            .border_color(black(0.))
            .text_color(white(TREE_TEXT))
            .text_size(theme.text(12.5))
            .cursor_pointer()
            .hover(|style| style.bg(white(ACTION_HOVER)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, window, _| {
                    window.prevent_default();
                }),
            )
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.on_search_clicked(index, event.click_count(), window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .w(theme.px(GLYPH_SLOT))
                    .flex()
                    .justify_center()
                    .child(row_lead(theme, &parts.name, false, false)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(folder_label(&parts)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(theme.text(11.))
                    .text_color(white(TEXT_MIN))
                    .child(found.lines.len().to_string()),
            )
            .into_any_element()
    }

    /// Une ligne trouvée : un retrait de 8, la case du remplacement, le numéro de ligne en mono aligné à droite sur
    /// 4 chiffres, puis l'extrait où les occurrences sont sur le fond de la recherche `⌘F`.
    fn render_search_line(
        &self,
        index: usize,
        file: usize,
        line: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((path, found)) = self
            .filter
            .search
            .as_ref()
            .and_then(|search| search.outcome.files.get(file))
            .and_then(|file| file.lines.get(line).map(|found| (&file.path, found)))
        else {
            return div().into_any_element();
        };
        let checked = self.filter.unchecked.is_checked(path, found.line);
        let selected = self.filter.selected == Some(index);
        let (text, ranges) = fit_snippet(snippet(&found.text, &found.ranges, SNIPPET_LEAD));
        let occurrence = HighlightStyle {
            background_color: Some(
                Rgba {
                    a: SEARCH_MATCH,
                    ..color(WAITING)
                }
                .into(),
            ),
            ..Default::default()
        };
        div()
            .id(("files-search-line", index))
            .flex()
            .items_center()
            .h(theme.px(ROW_HEIGHT))
            .w_full()
            .pl(theme.px(8.))
            .pr(theme.px(8.))
            .rounded(theme.px(5.))
            .border_1()
            .border_color(black(0.))
            .text_color(white(if selected { TEXT_FULL } else { TREE_TEXT }))
            .text_size(theme.text(12.5))
            .cursor_pointer()
            .when(selected, |row| {
                row.bg(focus(TREE_SELECTED))
                    .border_color(focus(TREE_SELECTED_BORDER))
            })
            .when(!selected, |row| {
                row.hover(|style| style.bg(white(ACTION_HOVER)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.on_hit_pressed(index, cx);
                    // Le champ garde le focus jusqu'au clic : ni l'arbre ni le corps du volet ne le reprennent.
                    window.prevent_default();
                }),
            )
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.on_search_clicked(index, event.click_count(), window, cx);
            }))
            .child(self.render_checkbox(index, checked, theme, cx))
            .child(
                div()
                    .flex_none()
                    .ml(theme.px(7.))
                    .font_family(GLYPH_FONT)
                    .text_size(theme.text(11.))
                    .text_color(white(TEXT_MIN))
                    .whitespace_nowrap()
                    .child(format!("{:>4}", found.line)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .ml(theme.px(6.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(
                        StyledText::new(text)
                            .with_highlights(ranges.into_iter().map(|range| (range, occurrence))),
                    ),
            )
            .into_any_element()
    }

    /// La case d'une ligne trouvée : 14 de côté, cochée en `FOCUS` avec une coche blanche, décochée en contour d'un
    /// point. Un appui gauche la bascule sans ouvrir le fichier ni donner le focus à la ligne.
    fn render_checkbox(
        &self,
        index: usize,
        checked: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex_none()
            .w(theme.px(GLYPH_SLOT))
            .flex()
            .justify_center()
            .child(
                div()
                    .id(("files-check", index))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(theme.px(CHECKBOX_SIZE))
                    .rounded(theme.text(CHECKBOX_RADIUS))
                    .cursor_pointer()
                    .when(checked, |case| case.bg(color(FOCUS)))
                    .when(!checked, |case| {
                        case.border_1().border_color(white(ICON_MUTED))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            this.toggle_line_checked(index, cx);
                            // Ni la ligne (sélection, ouverture) ni le corps du volet ne reprennent cet appui.
                            cx.stop_propagation();
                            window.prevent_default();
                        }),
                    )
                    .when(checked, |case| {
                        case.child(
                            svg()
                                .path(Icon::Check.path())
                                .size(theme.px(CHECKBOX_ICON))
                                .text_color(white(TEXT_FULL)),
                        )
                    }),
            )
    }

    /// Aucun résultat : « Aucun fichier. » (ou l'échec de Git) à la place de la liste. Pendant l'attente du classement,
    /// rien : la phrase ne doit pas clignoter à chaque frappe.
    fn render_filter_status(&self, theme: &Theme) -> AnyElement {
        let (text, text_color) = if self.filter.failed {
            (GIT_FAILED, color(ERROR_TEXT))
        } else if self.filter.settled {
            (NO_FILE, white(TEXT_MIN))
        } else {
            return div().size_full().into_any_element();
        };
        div()
            .id("files-filter-status")
            .w_full()
            .min_h(theme.px(ROW_HEIGHT))
            .py(theme.px(4.))
            .px(theme.px(9.))
            .text_size(theme.text(12.5))
            .text_color(text_color)
            .child(text)
            .into_any_element()
    }

    /// Une ligne du filtre : glyphe de type, nom, puis dossier en retrait, coupés à droite ; les lettres trouvées en
    /// blanc plein demi-gras. Ni glisser ni menu du clic droit.
    fn render_hit(&self, index: usize, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let ResultItem::Hit(hit) = self.filter.items[index] else {
            return div().into_any_element();
        };
        let parts = split_hit(&self.filter.hits[hit]);
        let selected = self.filter.selected == Some(index);
        let (label, runs) = label_runs(&parts);
        let found = HighlightStyle {
            color: Some(white(TEXT_FULL).into()),
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        };
        let folder = HighlightStyle {
            color: Some(white(TEXT_MIN).into()),
            ..Default::default()
        };
        let highlights = runs.into_iter().map(|(range, emphasis)| match emphasis {
            Emphasis::Found => (range, found),
            Emphasis::Folder => (range, folder),
        });
        div()
            .id(("files-hit", index))
            .flex()
            .items_center()
            .gap(theme.px(7.))
            .h(theme.px(ROW_HEIGHT))
            .w_full()
            .px(theme.px(8.))
            .rounded(theme.px(5.))
            // Le contour est toujours là, transparent hors sélection : la ligne ne change pas de taille.
            .border_1()
            .border_color(black(0.))
            .text_color(white(if selected { TEXT_FULL } else { TREE_TEXT }))
            .text_size(theme.text(12.5))
            .cursor_pointer()
            .when(selected, |row| {
                row.bg(focus(TREE_SELECTED))
                    .border_color(focus(TREE_SELECTED_BORDER))
            })
            .when(!selected, |row| {
                row.hover(|style| style.bg(white(ACTION_HOVER)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.on_hit_pressed(index, cx);
                    // Le champ garde le focus : ni l'arbre ni le corps du volet ne le reprennent.
                    window.prevent_default();
                }),
            )
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.on_hit_clicked(index, event.click_count(), window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .w(theme.px(GLYPH_SLOT))
                    .flex()
                    .justify_center()
                    .child(row_lead(theme, &parts.name, false, false)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(StyledText::new(label).with_highlights(highlights)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// À toute hauteur de boîte, la ligne du champ se pose au pixel près dans l'intérieur (différence paire, sans quoi
    /// le texte dépend de l'origine fractionnaire de la boîte) et ne s'écarte du kit que d'un pixel.
    #[test]
    fn the_field_line_sits_on_whole_pixels_at_every_box_height() {
        for box_height in 24..=120 {
            let line = field_line_height(box_height as f32);
            let inner = box_height as f32 - 2. * FIELD_BORDER;
            assert_eq!((inner - line) % 2., 0., "boîte de {box_height}");
            assert!(
                (KIT_LINE_HEIGHT..=KIT_LINE_HEIGHT + 1.).contains(&line),
                "boîte de {box_height}"
            );
        }
    }

    fn parts(name: &str, name_positions: &[usize], dir: &str, dir_positions: &[usize]) -> HitParts {
        HitParts {
            name: name.to_owned(),
            name_positions: name_positions.to_vec(),
            dir: dir.to_owned(),
            dir_positions: dir_positions.to_vec(),
        }
    }

    /// Des plages d'octets écrites en paires (début, fin).
    fn spans(pairs: &[(usize, usize)]) -> Vec<Range<usize>> {
        pairs.iter().map(|&(start, end)| start..end).collect()
    }

    /// Les morceaux du libellé qui portent `emphasis`, dans l'ordre.
    fn pieces(label: &str, runs: &[(Range<usize>, Emphasis)], emphasis: Emphasis) -> Vec<String> {
        runs.iter()
            .filter(|(_, kind)| *kind == emphasis)
            .map(|(range, _)| label[range.clone()].to_owned())
            .collect()
    }

    #[test]
    fn found_letters_are_marked_in_the_name_and_the_folder_stays_in_retreat() {
        // `docs/guide.md` cherché par `gmd` : `g` au début du nom, `m` et `d` dans l'extension.
        let hit = parts("guide.md", &[0, 6, 7], "docs", &[]);
        let (label, runs) = label_runs(&hit);
        assert_eq!(label, "guide.md  docs");
        assert_eq!(pieces(&label, &runs, Emphasis::Found), ["g", "m", "d"]);
        assert_eq!(pieces(&label, &runs, Emphasis::Folder), ["docs"]);
    }

    #[test]
    fn found_letters_of_the_folder_split_it_into_ordered_runs() {
        let hit = parts("a.md", &[], "notes/sub", &[0, 6]);
        let (label, runs) = label_runs(&hit);
        assert_eq!(pieces(&label, &runs, Emphasis::Found), ["n", "s"]);
        assert_eq!(pieces(&label, &runs, Emphasis::Folder), ["otes/", "ub"]);
        // Croissantes et sans chevauchement : c'est ce que `StyledText` exige.
        assert!(runs.windows(2).all(|pair| pair[0].0.end <= pair[1].0.start));
    }

    #[test]
    fn a_file_at_the_root_has_no_folder_run() {
        let (label, runs) = label_runs(&parts("README.md", &[0], "", &[]));
        assert_eq!(label, "README.md");
        assert!(pieces(&label, &runs, Emphasis::Folder).is_empty());
    }

    #[test]
    fn runs_fall_on_character_boundaries_for_accented_names() {
        // `é` fait deux octets : la plage trouvée le couvre en entier.
        let hit = parts("été.md", &[0, 3], "dossier-é", &[8]);
        let (label, runs) = label_runs(&hit);
        for (range, _) in &runs {
            assert!(label.is_char_boundary(range.start) && label.is_char_boundary(range.end));
        }
        assert_eq!(pieces(&label, &runs, Emphasis::Found), ["é", "é", "é"]);
    }

    #[test]
    fn a_long_snippet_is_cut_on_a_character_boundary_and_keeps_its_ranges_inside() {
        // Une ligne minifiée : l'occurrence est au début, une autre bien après la coupe.
        let line = format!("foo{}foo", "é".repeat(MAX_SNIPPET_CHARS * 2));
        let tail = line.len() - 3;
        let (text, ranges) = fit_snippet((line, spans(&[(0, 3), (tail, tail + 3)])));
        assert_eq!(text.chars().count(), MAX_SNIPPET_CHARS);
        assert_eq!(ranges, spans(&[(0, 3)]));
        // Une occurrence à cheval sur la coupe est raccourcie, jamais au-delà du texte.
        let (text, ranges) = fit_snippet((
            "a".repeat(MAX_SNIPPET_CHARS - 1) + "foo",
            spans(&[(MAX_SNIPPET_CHARS - 1, MAX_SNIPPET_CHARS + 2)]),
        ));
        assert_eq!(ranges, spans(&[(MAX_SNIPPET_CHARS - 1, MAX_SNIPPET_CHARS)]));
        assert_eq!(&text[ranges[0].clone()], "f");
    }

    #[test]
    fn tabs_become_spaces_without_moving_the_ranges() {
        let (text, ranges) = fit_snippet(("a\tfoo".to_string(), spans(&[(2, 5)])));
        assert_eq!(text, "a foo");
        assert_eq!(&text[ranges[0].clone()], "foo");
    }

    #[test]
    fn a_batch_under_an_ignored_folder_leaves_the_file_list_alone() {
        let ignored = |path: &Path| path.starts_with("/p/target");
        let batch = |paths: &[&str], rescan: &[&str]| WatchBatch {
            paths: paths.iter().map(PathBuf::from).collect(),
            rescan: rescan.iter().map(PathBuf::from).collect(),
        };
        assert!(!batch_changes_listing(
            &batch(&["/p/target/debug/x", "/p/target"], &[]),
            ignored
        ));
        assert!(batch_changes_listing(
            &batch(&["/p/target/debug/x", "/p/new.txt"], &[]),
            ignored
        ));
        // Un débordement du watcher sur un dossier du projet : on ne sait plus ce qui a changé.
        assert!(batch_changes_listing(&batch(&[], &["/p/src"]), ignored));
        assert!(!batch_changes_listing(&batch(&[], &[]), ignored));
    }
}
