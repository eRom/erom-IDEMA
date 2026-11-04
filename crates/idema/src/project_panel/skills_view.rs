//! L'onglet Compétences du volet Projet (spec §3.3, variante A de `maquette/volet-projet-competences.html`) : un seul
//! champ. Vide, il liste les skills du projet ; on tape, il cherche sur skills.sh ; un chemin, il propose d'ajouter un
//! dossier. Ce fichier porte les décisions pures (que montre le champ, quelle ligne est « Installée », quel texte de
//! retrait) et, plus bas, la colle GPUI. Le modèle est dans `skills.rs`, le CLI dans `skills_cli.rs`.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Div, Entity, FocusHandle, Focusable, FontWeight, KeyBinding,
    MouseButton, MouseDownEvent, PathPromptOptions, PromptButton, PromptLevel, Rgba, SharedString,
    Stateful, Subscription, Task, Window, actions, div, prelude::*, svg,
};
use gpui_component::input::{InputEvent, InputState};

use super::banner::{banner_button, error_banner, info_banner, warning_banner};
use super::instructions::io_error_text;
use super::is_home;
use super::mcp_files::Harness;
use super::skills::{
    CollisionOrigin, DiskCollision, InstalledSkill, ListedSkill, SearchHit, SearchQueue,
    SkillSource, add_args, coverage_text, disk_collisions, folder_target_dirs, format_installs,
    hit_target_dirs, json_array_in, keep_version, list_args, parse_list, path_query, propagate,
    remove_args, resolve_path, scan, trash_copies, trash_divergent_copies, visible_to,
};
use super::skills_cli::{HOME_REFUSED, api_base, run_skills, search};
use crate::assets::Icon;
use crate::files::ops::move_to_trash;
use crate::files::panel::{FIELD_HEIGHT, FIELD_PADDING, FIELD_RADIUS, field_input, home_dir};
use crate::hud::log_event;
use crate::mcp_widgets::busy_text;
use crate::settings::controls::{ButtonKind, action_button};
use crate::terminal_panel::BODY_MARGIN_SIDES;
use crate::theme::{
    AGENT_PURPLE, AGENT_PURPLE_TEXT, BADGE_STDIO, BUTTON_HOVER, ERROR_TEXT, FIELD_BACKGROUND,
    FIELD_RING, HOVER, HOVER_STRONG, RUNNING, SEPARATOR, TEXT_MIN, TEXT_PRIMARY, TEXT_SECONDARY,
    Theme, black, color, white,
};

/// Le délai de frappe : la recherche part quand la frappe s'arrête depuis tant de temps (spec §3.3).
pub const SEARCH_DELAY: Duration = Duration::from_millis(300);
/// Le nombre minimal de caractères d'une recherche (`query.length < 2` dans le CLI 1.7.4).
pub const MIN_SEARCH_CHARS: usize = 2;

/// Un dossier proposé par le champ : le chemin résolu, son écriture abrégée en `~` et son existence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderTarget {
    pub path: PathBuf,
    pub display: String,
    pub exists: bool,
}

/// Ce que le champ fait montrer au volet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldView {
    /// Les skills du projet (champ vide, ou un seul caractère).
    Project,
    /// Les résultats de skills.sh pour ce texte.
    Search(String),
    /// Une seule ligne « Ajouter depuis ce dossier ».
    Folder(FolderTarget),
}

/// Un chemin sous `home` s'écrit `~/…`.
pub fn tilde_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Ce que montre le champ pour `text` : un chemin (`/`, `~/`, `./`, `../`) donne la ligne du dossier, deux caractères
/// ou plus une recherche, le reste (vide, un caractère) la liste du projet.
pub fn field_view(text: &str, project: &Path, home: Option<&Path>) -> FieldView {
    let trimmed = text.trim();
    if path_query(trimmed) {
        // Sans dossier personnel connu, `~/x` reste un chemin qui n'existe pas : « Dossier introuvable ».
        let path = resolve_path(trimmed, project, home.unwrap_or(Path::new("~")));
        return FieldView::Folder(FolderTarget {
            display: tilde_path(&path, home),
            exists: path.is_dir(),
            path,
        });
    }
    if trimmed.chars().count() >= MIN_SEARCH_CHARS {
        FieldView::Search(trimmed.to_string())
    } else {
        FieldView::Project
    }
}

/// Une ligne de résultat : le résultat et ce que le projet en a déjà.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRow<'a> {
    pub hit: &'a SearchHit,
    pub installed: bool,
}

impl ResultRow<'_> {
    /// Le texte de l'action de la ligne : « Installée » ou « Ajouter ».
    pub fn label(&self) -> &'static str {
        if self.installed {
            "Installée"
        } else {
            "Ajouter"
        }
    }
}

/// Où en est la liste des skills du projet, pour décider de ce qu'un résultat propose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListStatus {
    Loading,
    Ready,
    Failed,
}

/// Ce que la ligne d'un résultat propose : « Installée », « Ajouter », l'attente de la première lecture, ou rien quand
/// la liste n'a pas pu être lue (sans elle, on ne sait pas si la skill est déjà là).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitAction {
    Installed,
    Add,
    Checking,
    Nothing,
}

/// L'action d'un résultat : l'installation connue l'emporte, puis l'état de la liste du projet.
pub fn hit_action(installed: bool, listing: ListStatus) -> HitAction {
    match (installed, listing) {
        (true, _) => HitAction::Installed,
        (false, ListStatus::Loading) => HitAction::Checking,
        (false, ListStatus::Failed) => HitAction::Nothing,
        (false, ListStatus::Ready) => HitAction::Add,
    }
}

/// Une skill du projet est ce résultat quand sa source est la sienne et que son nom (ou son dossier) est le sien : le
/// même nom chez un autre auteur est une autre skill.
fn is_installed(hit: &SearchHit, skill: &InstalledSkill) -> bool {
    match &skill.source {
        SkillSource::Locked { source, .. } => {
            *source == hit.source && (skill.name == hit.name || skill.dir == hit.skill_id)
        }
        SkillSource::Handwritten => false,
    }
}

/// Les lignes de résultats : une skill du projet de même nom et de même source marque le résultat « Installée ».
pub fn result_rows<'a>(hits: &'a [SearchHit], installed: &[InstalledSkill]) -> Vec<ResultRow<'a>> {
    hits.iter()
        .map(|hit| ResultRow {
            hit,
            installed: installed.iter().any(|skill| is_installed(hit, skill)),
        })
        .collect()
}

/// La question de confirmation d'un retrait (spec §3.3). Une skill écrite à la main ne se retrouve nulle part : la phrase
/// dit qu'elle part à la Corbeille.
pub fn removal_copy(skill: &InstalledSkill) -> String {
    let question = format!(
        "Retirer {} du projet ? Elle disparaît pour les cinq harnais.",
        skill.name
    );
    match skill.source {
        // Le CLI supprime pour de bon : une copie de chaque version part d'abord à la Corbeille, toujours
        // (`trash_divergent_copies`), même quand toutes les copies sont identiques ; la phrase le dit.
        SkillSource::Locked { .. } => {
            format!("{question} Une copie de la skill part à la Corbeille avant le retrait.")
        }
        SkillSource::Handwritten => format!("{question} Elle partira à la Corbeille."),
    }
}

/// Ce que dit la confirmation d'un ajout qui remplace des entrées du projet : le titre et le détail de la fenêtre. Le
/// CLI vide le dossier de chaque cible avant d'y copier, sans rien demander : l'ancienne version part à la Corbeille.
/// `from_folder` : l'ajout vient d'un dossier (« Ce dossier remplace… ») plutôt que d'un résultat de skills.sh.
/// Textes à ratifier à la porte : posés par la revue finale.
pub fn replace_copy(
    collisions: &[DiskCollision],
    from_folder: bool,
    home: Option<&Path>,
) -> (String, String) {
    let [only] = collisions else {
        let dirs: Vec<&str> = collisions
            .iter()
            .map(|collision| collision.dir.as_str())
            .collect();
        let subject = if from_folder {
            "Ce dossier"
        } else {
            "Cet ajout"
        };
        return (
            format!("Remplacer {} skills ?", collisions.len()),
            format!(
                "{subject} remplace {} dans le projet ; les anciennes versions partent à la Corbeille.",
                dirs.join(", ")
            ),
        );
    };
    let origin = match &only.origin {
        CollisionOrigin::Skill(SkillSource::Handwritten) => "écrite à la main".to_string(),
        CollisionOrigin::Skill(source) => {
            format!("installée depuis {}", source_text(source, home))
        }
        CollisionOrigin::Folder => "dossier sans SKILL.md".to_string(),
    };
    (
        format!("Remplacer « {} » ?", only.dir),
        format!(
            "Le projet a déjà une skill « {} » ({origin}). L'ajouter la remplace dans les trois dossiers ; l'ancienne version part à la Corbeille.",
            only.dir
        ),
    )
}

/// La confirmation d'un remplacement vaut encore quand tout ce que le disque dirait écraser maintenant était déjà dans
/// ce qu'elle a nommé, de la même origine : la phrase (« écrite à la main », « installée depuis… », « dossier sans
/// SKILL.md ») ne décrit que cela. Un ensemble qui a grandi (une entrée apparue depuis) ou une origine qui a changé :
/// refus. Un ensemble qui a rétréci écraserait moins que ce qui a été accepté : permis.
pub fn replacement_still_valid(frozen: &[DiskCollision], current: &[DiskCollision]) -> bool {
    current.iter().all(|now| {
        frozen.iter().any(|before| {
            before.dir.to_lowercase() == now.dir.to_lowercase() && before.origin == now.origin
        })
    })
}

/// Une relecture de la liste attend la fin de l'appel en cours sur le même projet : `skills list` lirait le lock et
/// les dossiers pendant que `skills add` ou `skills remove` les réécrit. La fin de l'appel relance la lecture.
pub fn defers_listing(call_project: Option<&Path>, project: &Path) -> bool {
    call_project == Some(project)
}

/// Ce que l'onglet offre pour ce projet : le dossier personnel n'est pas un projet, ses skills sont celles de toute la
/// machine (`~/.claude/skills`, `~/.grok/skills`, `~/.agents/skills`) et l'onglet n'y lit, n'y cherche, n'y ajoute rien.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabAvailability {
    Open,
    HomeIsNotAProject,
}

pub fn tab_availability(project: &Path, home: Option<&Path>) -> TabAvailability {
    if is_home(project, home) {
        TabAvailability::HomeIsNotAProject
    } else {
        TabAvailability::Open
    }
}

/// Comment un retrait se fait : par le CLI (skill inscrite au lock) ou par la Corbeille (écrite à la main).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalKind {
    Locked,
    Handwritten,
}

pub fn removal_kind(skill: &InstalledSkill) -> RemovalKind {
    match skill.source {
        SkillSource::Locked { .. } => RemovalKind::Locked,
        SkillSource::Handwritten => RemovalKind::Handwritten,
    }
}

/// Le retrait confirmé vaut encore quand la skill existe toujours et se retire de la même façon : la phrase de
/// confirmation (« Elle partira à la Corbeille ») ne décrit que celle-là.
pub fn removal_still_valid(frozen: RemovalKind, current: Option<RemovalKind>) -> bool {
    current == Some(frozen)
}

/// La source sous le nom d'une ligne : `owner/repo`, le chemin abrégé d'une source locale, ou « écrite à la main ».
pub fn source_label(skill: &InstalledSkill, home: Option<&Path>) -> String {
    source_text(&skill.source, home)
}

/// Vrai quand `source_label` rend un chemin (source locale) : il se coupe par le début, pour garder le nom du dossier.
pub fn source_is_path(skill: &InstalledSkill) -> bool {
    matches!(&skill.source, SkillSource::Locked { kind, .. } if kind == "local")
}

/// Comme `source_label`, pour une source seule.
fn source_text(source: &SkillSource, home: Option<&Path>) -> String {
    match source {
        SkillSource::Locked { source, kind } if kind == "local" => {
            tilde_path(Path::new(source), home)
        }
        SkillSource::Locked { source, .. } => source.clone(),
        SkillSource::Handwritten => "écrite à la main".to_string(),
    }
}

/// La phrase d'une mise à la Corbeille ou d'une copie qui échoue : la raison en français (`io_error_text`), jamais le
/// texte du système, en anglais.
fn failure_text(prefix: &str, error: &io::Error) -> String {
    format!("{prefix} : {}", io_error_text(error))
}

/// `failure_text`, et le texte brut de l'erreur dans le journal.
fn report_failure(label: &str, prefix: &str, error: &io::Error) -> String {
    log_event(&format!("skills {label} failed error={error}"));
    failure_text(prefix, error)
}

/// Les harnais qui ne voient la skill dans aucun des dossiers qui la portent.
pub fn missing_harnesses(skill: &InstalledSkill) -> Vec<Harness> {
    visible_to(&skill.present)
        .into_iter()
        .filter(|(_, sees)| !sees)
        .map(|(harness, _)| harness)
        .collect()
}

/// Décode la sortie de `skills list --json`.
pub fn parse_list_output(stdout: &str) -> Result<Vec<ListedSkill>, String> {
    let json = json_array_in(stdout)
        .ok_or_else(|| "La liste des compétences est illisible.".to_string())?;
    parse_list(json)
}

// ---------------------------------------------------------------------------------------------------------------------
// La colle GPUI. Le rendu se vérifie dans la VM ; les décisions sont ci-dessus.
// ---------------------------------------------------------------------------------------------------------------------

actions!(project_skills, [ClearField]);

/// Contexte de touches de la boîte du champ : Échap y vide le champ. Le champ du kit lie aussi Échap, au contexte
/// `Input` (plus profond) : son action passe d'abord, puis, faute de rien à fermer, la touche revient à la nôtre.
pub const FIELD_KEY_CONTEXT: &str = "ProjectSkillsField";

/// Les liaisons de l'onglet : Échap, depuis le champ seulement.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new(
        "escape",
        ClearField,
        Some(FIELD_KEY_CONTEXT),
    )]
}

/// La hauteur d'une ligne de skill (maquette `.row`) et le côté de la pastille de son glyphe (`.glyph`).
const ROW_HEIGHT: f32 = 44.;
const GLYPH_SIZE: f32 = 26.;
const SEARCH_FAILED: &str = "skills.sh ne répond pas";
const NO_RESULT: &str = "Aucun résultat";
/// L'onglet sur le dossier personnel (à ratifier à la porte : texte posé par la revue finale).
const HOME_NOT_A_PROJECT: &str = HOME_REFUSED;
/// L'ajout d'un dossier refusé tant que la liste du projet n'est pas lue : sans elle, on ne sait pas ce qu'il écrase.
const LIST_UNREAD: &str = "La liste du projet n'est pas lue : l'ajout est refusé.";
/// La liste a bougé entre la confirmation et le remplacement : rien n'est parti.
const LIST_CHANGED: &str = "La liste du projet a changé : rien n'a été remplacé. Relance l'ajout.";
/// Le texte d'un appel sans libellé propre (propager, garder une version, attente de la liste).
const BUSY: &str = "En cours…";
/// La police des puces du pied, celle des bandeaux (`banner.rs`).
const CHIP_FONT: &str = "Menlo";

/// Ce qui identifie une ligne pour son erreur et son indicateur d'appel.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RowKey {
    /// Une skill du projet, par son dossier.
    Skill(String),
    /// Un résultat de skills.sh, par `source/skillId`.
    Hit(String),
    /// La ligne « Ajouter depuis ce dossier ».
    Folder,
}

/// Ce que fait l'appel en cours : son indicateur en dépend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallKind {
    Add,
    Remove,
    /// Propager ou garder une version : copie de fichiers.
    Fix,
}

impl CallKind {
    fn busy_label(self) -> &'static str {
        match self {
            CallKind::Add => "Ajout…",
            CallKind::Remove => "Retrait…",
            CallKind::Fix => BUSY,
        }
    }
}

/// L'appel en cours, avec le projet pour lequel il tourne : un autre projet n'en montre rien.
struct SkillsCall {
    project: PathBuf,
    key: RowKey,
    kind: CallKind,
}

/// La liste du projet : en lecture, lue, ou illisible (le message du CLI).
enum Listing {
    Loading,
    Ready(Vec<InstalledSkill>),
    Failed(String),
}

impl Listing {
    fn status(&self) -> ListStatus {
        match self {
            Listing::Loading => ListStatus::Loading,
            Listing::Ready(_) => ListStatus::Ready,
            Listing::Failed(_) => ListStatus::Failed,
        }
    }
}

/// Un ajout prêt à partir : la ligne qui le porte, les dossiers qu'il écrit, les arguments du CLI.
struct AddPlan {
    /// Le projet au moment du clic : l'ajout ne part que si c'est encore le projet affiché.
    project: PathBuf,
    key: RowKey,
    clear_field: bool,
    /// Les dossiers du projet que le CLI vide avant d'y copier (`target_dir_for`).
    targets: Vec<String>,
    args: Vec<String>,
    from_folder: bool,
}

/// La recherche affichée : les résultats de la dernière réponse retenue, ou l'échec de skills.sh.
#[derive(Default)]
struct SearchState {
    hits: Option<Vec<SearchHit>>,
    failed: bool,
}

/// `skills list --json` à la racine du projet, puis le disque : tout ce qui est lent (le CLI, la lecture des copies)
/// reste hors du fil d'interface.
fn list_project_skills(project: &Path) -> Result<Vec<InstalledSkill>, String> {
    let stdout = run_skills(project, &list_args()).map_err(|error| error.message)?;
    let listed = parse_list_output(&stdout)?;
    Ok(scan(project, &listed))
}

fn run_cli(project: &Path, args: &[String]) -> Result<(), String> {
    run_skills(project, args)
        .map(|_| ())
        .map_err(|error| error.message)
}

/// La mise à la Corbeille a échoué : la phrase française, le texte brut au journal.
fn trash_failure(error: &io::Error) -> String {
    report_failure("trash", "La mise à la Corbeille a échoué", error)
}

/// Le corps de l'appel d'un ajout : le disque est relu, et si une entrée à écraser n'était pas dans ce que
/// l'utilisateur a confirmé (`frozen`), rien ne part (`LIST_CHANGED`) ; sinon chaque entrée qui existe maintenant part à
/// la Corbeille, puis le CLI. Une Corbeille qui refuse arrête tout avant le CLI.
fn run_add_checked(
    project: &Path,
    targets: &[String],
    known: &[InstalledSkill],
    frozen: &[DiskCollision],
    trash: &mut dyn FnMut(&Path) -> io::Result<PathBuf>,
    cli: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let current = disk_collisions(project, targets, known);
    if !replacement_still_valid(frozen, &current) {
        log_event("skills add skipped reason=collisions-changed");
        return Err(LIST_CHANGED.to_string());
    }
    for collision in &current {
        for name in &collision.names {
            trash_copies(project, name, trash).map_err(|error| trash_failure(&error))?;
        }
    }
    cli()
}

/// L'onglet Compétences : le champ, ce qu'il montre, et l'appel CLI en cours (un seul à la fois).
pub struct SkillsTab {
    project: Option<PathBuf>,
    field: Entity<InputState>,
    view: FieldView,
    listing: Listing,
    availability: TabAvailability,
    /// Une lecture est en route ; son numéro écarte la réponse d'une lecture plus ancienne.
    loading: bool,
    listing_generation: u64,
    listing_task: Option<Task<()>>,
    search: SearchState,
    queue: SearchQueue,
    debounce: Option<Task<()>>,
    search_task: Option<Task<()>>,
    call: Option<SkillsCall>,
    call_task: Option<Task<()>>,
    /// L'erreur de la dernière tentative, sous sa ligne.
    errors: HashMap<RowKey, String>,
    /// La skill dont les versions divergentes sont dépliées.
    open_versions: Option<String>,
    _subscription: Subscription,
}

impl SkillsTab {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = cx.new(|cx| InputState::new(window, cx).placeholder("Chercher sur skills.sh"));
        // `Change` vient d'une frappe, d'un collage ou d'un annuler ; `set_value` n'en émet pas, ceux qui l'appellent
        // relancent donc la suite eux-mêmes. Le focus du champ descend de la racine du volet, qui le suit déjà.
        let subscription = cx.subscribe_in(
            &field,
            window,
            |this, _, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => this.on_field_changed(cx),
                InputEvent::PressEnter { .. } => this.search_now(cx),
                InputEvent::Focus | InputEvent::Blur => {}
            },
        );
        Self {
            project: None,
            field,
            view: FieldView::Project,
            listing: Listing::Loading,
            availability: TabAvailability::Open,
            loading: false,
            listing_generation: 0,
            listing_task: None,
            search: SearchState::default(),
            queue: SearchQueue::default(),
            debounce: None,
            search_task: None,
            call: None,
            call_task: None,
            errors: HashMap::new(),
            open_versions: None,
            _subscription: subscription,
        }
    }

    /// Le focus du champ : là où `ProjectPanel` rend la saisie quand l'onglet est choisi.
    pub fn field_focus(&self, cx: &App) -> FocusHandle {
        self.field.focus_handle(cx)
    }

    /// Relit les skills de `project` (ouverture de l'onglet, changement de projet). Une lecture déjà en route pour ce
    /// projet n'est pas doublée.
    pub fn refresh(&mut self, project: &Path, cx: &mut Context<Self>) {
        self.load(project, false, cx);
    }

    fn load(&mut self, project: &Path, force: bool, cx: &mut Context<Self>) {
        if self.project.as_deref() != Some(project) {
            self.project = Some(project.to_path_buf());
            self.listing = Listing::Loading;
            self.errors.clear();
            self.open_versions = None;
            // Un chemin relatif dans le champ se lit depuis le nouveau projet.
            self.update_view(cx);
        } else if self.loading && !force {
            return;
        }
        // Le dossier personnel n'est pas un projet : ni lecture du CLI, ni liste, ni geste. Une lecture en route est
        // écartée.
        self.availability = tab_availability(project, home_dir().as_deref());
        if self.availability == TabAvailability::HomeIsNotAProject {
            self.listing_generation += 1;
            self.loading = false;
            self.listing = Listing::Ready(Vec::new());
            cx.notify();
            return;
        }
        // Un appel en cours sur ce projet réécrit le lock et les dossiers : la lecture attend sa fin, qui la relance.
        if defers_listing(
            self.call.as_ref().map(|call| call.project.as_path()),
            project,
        ) {
            cx.notify();
            return;
        }
        self.loading = true;
        self.listing_generation += 1;
        let generation = self.listing_generation;
        let project = project.to_path_buf();
        let work = {
            let project = project.clone();
            cx.background_spawn(async move { list_project_skills(&project) })
        };
        self.listing_task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.listing_generation {
                    return;
                }
                this.loading = false;
                if this.project.as_deref() != Some(project.as_path()) {
                    return;
                }
                this.listing = match result {
                    Ok(skills) => Listing::Ready(skills),
                    Err(message) => Listing::Failed(message),
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Les skills du projet, quand la liste est lue.
    fn skills(&self) -> &[InstalledSkill] {
        match &self.listing {
            Listing::Ready(skills) => skills,
            Listing::Loading | Listing::Failed(_) => &[],
        }
    }

    fn skill(&self, dir: &str) -> Option<&InstalledSkill> {
        self.skills().iter().find(|skill| skill.dir == dir)
    }

    /// Recalcule ce que le champ fait montrer, sans lancer de recherche.
    fn update_view(&mut self, cx: &mut Context<Self>) {
        let text = self.field.read(cx).value().to_string();
        let project = self.project.clone().unwrap_or_default();
        self.view = field_view(&text, &project, home_dir().as_deref());
    }

    /// Une frappe : la vue change tout de suite, la recherche part 300 ms après la dernière.
    fn on_field_changed(&mut self, cx: &mut Context<Self>) {
        self.update_view(cx);
        self.debounce = None;
        match self.view.clone() {
            FieldView::Search(query) => {
                self.debounce = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(SEARCH_DELAY).await;
                    let _ = this.update(cx, |this, cx| this.run_search(query, cx));
                }));
            }
            FieldView::Project | FieldView::Folder(_) => {
                // Une réponse en route pour un texte qu'on a quitté ne s'affichera pas.
                self.queue.next();
                self.search_task = None;
                self.search = SearchState::default();
            }
        }
        cx.notify();
    }

    /// Entrée n'attend pas le délai.
    fn search_now(&mut self, cx: &mut Context<Self>) {
        if let FieldView::Search(query) = self.view.clone() {
            self.debounce = None;
            self.run_search(query, cx);
        }
    }

    /// La recherche, hors du fil d'interface. Seule la réponse de la dernière demande s'affiche.
    fn run_search(&mut self, query: String, cx: &mut Context<Self>) {
        let generation = self.queue.next();
        self.search.failed = false;
        let work = cx.background_spawn(async move { search(&api_base(), &query) });
        self.search_task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if !this.queue.accept(generation) {
                    return;
                }
                this.search = match result {
                    Ok(hits) => SearchState {
                        hits: Some(hits),
                        failed: false,
                    },
                    Err(_) => SearchState {
                        hits: None,
                        failed: true,
                    },
                };
                cx.notify();
            });
        }));
    }

    /// Vide le champ (Échap, la croix) : la liste du projet revient.
    fn clear_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.field
            .update(cx, |field, cx| field.set_value("", window, cx));
        self.on_field_changed(cx);
        window.focus(&self.field.focus_handle(cx), cx);
    }

    /// Le bouton dossier : le sélecteur macOS, dossiers seuls. Le chemin choisi prend la place du texte du champ.
    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Sélectionner".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let path = match chosen.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                _ => None,
            };
            if let Some(path) = path {
                let _ = this.update_in(cx, |this, window, cx| {
                    let text = path.to_string_lossy().into_owned();
                    this.field
                        .update(cx, |field, cx| field.set_value(text, window, cx));
                    this.on_field_changed(cx);
                    window.focus(&this.field.focus_handle(cx), cx);
                });
            }
        })
        .detach();
    }

    /// L'appel CLI ou la copie de fichiers en cours pour la ligne `key`, quand c'est celui du projet affiché.
    fn call_on(&self, key: &RowKey) -> Option<CallKind> {
        self.call
            .as_ref()
            .filter(|call| self.project.as_ref() == Some(&call.project) && call.key == *key)
            .map(|call| call.kind)
    }

    /// Lance `work` hors du fil d'interface, dans le projet affiché. Un seul appel à la fois : un second est refusé.
    /// À la fin, l'erreur se pose sous la ligne et la liste est relue.
    fn start_call(
        &mut self,
        key: RowKey,
        kind: CallKind,
        clear_field: bool,
        work: impl FnOnce(&Path) -> Result<(), String> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project.clone() else {
            return;
        };
        if self.call.is_some() {
            return;
        }
        self.errors.remove(&key);
        // Une lecture en route lirait le lock et les dossiers pendant l'appel : son résultat est écarté, celle de la
        // fin de l'appel la remplace.
        self.listing_generation += 1;
        self.loading = false;
        self.call = Some(SkillsCall {
            project: project.clone(),
            key: key.clone(),
            kind,
        });
        let done = {
            let project = project.clone();
            cx.background_spawn(async move { work(&project) })
        };
        self.call_task = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = done.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.call = None;
                if this.project.as_deref() == Some(project.as_path()) {
                    match outcome {
                        Ok(()) if clear_field => {
                            this.field
                                .update(cx, |field, cx| field.set_value("", window, cx));
                            this.on_field_changed(cx);
                        }
                        Ok(()) => {}
                        Err(message) => {
                            this.errors.insert(key, message);
                        }
                    }
                    this.load(&project, true, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn fail(&mut self, key: RowKey, reason: &'static str, cx: &mut Context<Self>) {
        self.errors.insert(key, reason.to_string());
        cx.notify();
    }

    /// « Ajouter » sur un résultat de skills.sh.
    fn add_hit(&mut self, hit: &SearchHit, window: &mut Window, cx: &mut Context<Self>) {
        let key = RowKey::Hit(hit_key(hit));
        let Some(project) = self.project.clone() else {
            return;
        };
        match add_args(&hit.source, Some(&hit.skill_id)) {
            Ok(args) => {
                let plan = AddPlan {
                    project,
                    key,
                    clear_field: false,
                    targets: hit_target_dirs(hit),
                    args,
                    from_folder: false,
                };
                self.add_checked(plan, window, cx);
            }
            Err(reason) => self.fail(key, reason, cx),
        }
    }

    /// « Ajouter depuis ce dossier » : toutes les skills du dossier. Le champ se vide quand c'est fait. La recherche des
    /// `SKILL.md` du dossier (un dossier de `~` peut être vaste) se fait hors du fil d'interface.
    fn add_folder(&mut self, target: &FolderTarget, window: &mut Window, cx: &mut Context<Self>) {
        if !target.exists || self.call.is_some() {
            return;
        }
        let Some(project) = self.project.clone() else {
            return;
        };
        let args = match add_args(&target.path.to_string_lossy(), None) {
            Ok(args) => args,
            Err(reason) => return self.fail(RowKey::Folder, reason, cx),
        };
        let folder = target.path.clone();
        let scan = cx.background_spawn(async move { folder_target_dirs(&folder) });
        cx.spawn_in(window, async move |this, cx| {
            let targets = scan.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let plan = AddPlan {
                    project,
                    key: RowKey::Folder,
                    clear_field: true,
                    targets,
                    args,
                    from_folder: true,
                };
                this.add_checked(plan, window, cx);
            });
        })
        .detach();
    }

    /// L'ajout par le CLI. Il vide (`rm -rf`) le dossier de chaque skill cible avant d'y copier, sans rien demander
    /// (`-y`) : une entrée de même nom dans les trois dossiers (skill écrite à la main, d'une autre source, ou dossier
    /// sans `SKILL.md`) serait perdue sans Corbeille. Ce qui existe se lit sur le disque, au clic : la liste du projet
    /// est en cache, sans surveillance. Quand l'ajout en remplace, la confirmation native le dit, puis chaque copie part
    /// à la Corbeille avant l'appel ; annulée, rien ne bouge. Le projet est celui du clic : un autre projet affiché
    /// entre-temps, ou le dossier personnel, et rien ne part. Sans la liste du projet, refus.
    fn add_checked(&mut self, plan: AddPlan, window: &mut Window, cx: &mut Context<Self>) {
        if self.project.as_deref() != Some(plan.project.as_path()) {
            log_event("skills add skipped reason=project-changed");
            return;
        }
        if is_home(&plan.project, home_dir().as_deref()) {
            log_event("skills add skipped reason=home");
            return self.fail(plan.key, HOME_NOT_A_PROJECT, cx);
        }
        if self.call.is_some() {
            return;
        }
        if self.listing.status() != ListStatus::Ready {
            return self.fail(plan.key, LIST_UNREAD, cx);
        }
        let known = self.skills().to_vec();
        let scan = {
            let project = plan.project.clone();
            let targets = plan.targets.clone();
            let known = known.clone();
            cx.background_spawn(async move { disk_collisions(&project, &targets, &known) })
        };
        cx.spawn_in(window, async move |this, cx| {
            let collisions = scan.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.confirm_add(plan, known, collisions, window, cx);
            });
        })
        .detach();
    }

    /// Ce que le disque dit écraser est connu : rien, l'ajout part ; sinon la confirmation, puis l'ajout.
    fn confirm_add(
        &mut self,
        plan: AddPlan,
        known: Vec<InstalledSkill>,
        collisions: Vec<DiskCollision>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.project.as_deref() != Some(plan.project.as_path()) {
            log_event("skills add skipped reason=project-changed");
            return;
        }
        if self.call.is_some() {
            return;
        }
        if collisions.is_empty() {
            return self.run_add(plan, known, collisions, window, cx);
        }
        let (title, detail) = replace_copy(&collisions, plan.from_folder, home_dir().as_deref());
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(&detail),
            &[
                PromptButton::new("Remplacer"),
                PromptButton::cancel("Annuler"),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.run_add(plan, known, collisions, window, cx);
                });
            }
        })
        .detach();
    }

    /// L'ajout confirmé (ou sans rien à remplacer) : dans l'appel, le disque est relu juste avant la Corbeille, puis
    /// chaque entrée remplacée part à la Corbeille (par la fonction injectée), puis le CLI ajoute. Une Corbeille qui
    /// refuse arrête tout avant le CLI. Un autre projet affiché, ou une entrée apparue depuis la confirmation, et rien
    /// ne part.
    fn run_add(
        &mut self,
        plan: AddPlan,
        known: Vec<InstalledSkill>,
        frozen: Vec<DiskCollision>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.project.as_deref() != Some(plan.project.as_path()) {
            log_event("skills add skipped reason=project-changed");
            return;
        }
        let AddPlan {
            key,
            clear_field,
            targets,
            args,
            ..
        } = plan;
        self.start_call(
            key,
            CallKind::Add,
            clear_field,
            move |project| {
                run_add_checked(
                    project,
                    &targets,
                    &known,
                    &frozen,
                    &mut |path| move_to_trash(path),
                    &mut || run_cli(project, &args),
                )
            },
            window,
            cx,
        );
    }

    /// « Retirer » : la confirmation native, puis le retrait. Le projet visé est celui de la ligne cliquée : la
    /// confirmation laisse le temps de changer de session.
    fn confirm_removal(&mut self, dir: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        if self.call.is_some() {
            return;
        }
        let Some(skill) = self.skill(&dir) else {
            return;
        };
        // La façon de retirer est figée ici : la phrase que l'utilisateur lit en dépend.
        let kind = removal_kind(skill);
        let question = removal_copy(skill);
        let answer = window.prompt(
            PromptLevel::Warning,
            &question,
            None,
            &[
                PromptButton::new("Retirer"),
                PromptButton::cancel("Annuler"),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.run_removal(&project, &dir, kind, window, cx);
                });
            }
        })
        .detach();
    }

    /// Le retrait confirmé. Une skill inscrite au lock se retire par le CLI (il sait où sont ses copies et son entrée du
    /// lock) ; une skill écrite à la main part à la Corbeille, copie par copie. Si le volet montre un autre projet que
    /// celui qu'on a confirmé, ou si la skill ne se retire plus de la façon confirmée (le lock a changé pendant la
    /// confirmation), rien ne part.
    fn run_removal(
        &mut self,
        project: &Path,
        dir: &str,
        frozen: RemovalKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.project.as_deref() != Some(project) {
            log_event("skills removal skipped reason=project-changed");
            return;
        }
        if !removal_still_valid(frozen, self.skill(dir).map(removal_kind)) {
            log_event("skills removal skipped reason=kind-changed");
            return;
        }
        let locked = frozen == RemovalKind::Locked;
        let key = RowKey::Skill(dir.to_string());
        if locked {
            // Le CLI supprime pour de bon : quand les copies diffèrent, une copie de chaque version part d'abord à la
            // Corbeille.
            let skill = self.skill(dir).cloned();
            match remove_args(dir) {
                Ok(args) => self.start_call(
                    key,
                    CallKind::Remove,
                    false,
                    move |project| {
                        if let Some(skill) = &skill {
                            trash_divergent_copies(project, skill, &mut |path| move_to_trash(path))
                                .map_err(|error| trash_failure(&error))?;
                        }
                        run_cli(project, &args)
                    },
                    window,
                    cx,
                ),
                Err(reason) => self.fail(key, reason, cx),
            }
        } else {
            let dir = dir.to_string();
            self.start_call(
                key,
                CallKind::Remove,
                false,
                move |project| {
                    trash_copies(project, &dir, &mut |path| move_to_trash(path))
                        .map_err(|error| trash_failure(&error))
                },
                window,
                cx,
            );
        }
    }

    /// « Propager » : copie le dossier de la skill dans ceux des trois qui manquent.
    fn propagate_skill(&mut self, dir: String, window: &mut Window, cx: &mut Context<Self>) {
        let key = RowKey::Skill(dir.clone());
        self.start_call(
            key,
            CallKind::Fix,
            false,
            move |project| {
                propagate(project, &dir)
                    .map_err(|error| report_failure("propagate", "La propagation a échoué", &error))
            },
            window,
            cx,
        );
    }

    /// « Garder celle-ci » : la version du dossier `from` remplace les autres copies, qui partent à la Corbeille.
    fn keep_skill_version(
        &mut self,
        dir: String,
        from: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = RowKey::Skill(dir.clone());
        self.start_call(
            key,
            CallKind::Fix,
            false,
            move |project| {
                keep_version(project, &dir, from, &mut |path| move_to_trash(path))
                    .map_err(|error| report_failure("keep", "La copie a échoué", &error))
            },
            window,
            cx,
        );
    }

    fn toggle_versions(&mut self, dir: &str, cx: &mut Context<Self>) {
        if self.open_versions.as_deref() == Some(dir) {
            self.open_versions = None;
        } else {
            self.open_versions = Some(dir.to_string());
        }
        cx.notify();
    }
}

/// L'identité d'un résultat dans les erreurs et les indicateurs : `source/skillId`.
fn hit_key(hit: &SearchHit) -> String {
    format!("{}/{}", hit.source, hit.skill_id)
}

/// Les noms de dossiers d'une version, séparés par des virgules.
fn folders_text(roots: &[&str]) -> String {
    roots.join(", ")
}

/// Le titre d'un groupe (maquette `.group`) : 11 px gras à `TEXT_MIN`, et son compteur en maigre.
fn group_title(title: &str, count: Option<String>, theme: &Theme) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .h(theme.px(26.))
        .mt(theme.px(12.))
        .px(theme.px(2.))
        .text_size(theme.text(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(white(TEXT_MIN))
        .child(SharedString::from(title.to_string()))
        .children(count.map(|count| {
            div()
                .ml(theme.px(6.))
                .font_weight(FontWeight::NORMAL)
                .child(SharedString::from(count))
        }))
}

/// La pastille du glyphe d'une ligne (maquette `.glyph`) : le violet des agents à 14 %.
fn glyph_tile(icon: Icon, theme: &Theme) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(theme.px(GLYPH_SIZE))
        .rounded(theme.px(6.))
        .bg(Rgba {
            a: BADGE_STDIO,
            ..color(AGENT_PURPLE)
        })
        .child(
            svg()
                .path(icon.path())
                .size(theme.px(16.))
                .text_color(color(AGENT_PURPLE_TEXT)),
        )
}

/// Le texte d'une ligne : le nom (13, 500) et la source en mono (11, `TEXT_MIN`). Un chemin (`meta_is_path`) perd son
/// début quand la place manque : sa fin, le nom du dossier, est ce qui compte.
fn row_text(
    name: &str,
    meta: String,
    meta_is_path: bool,
    dimmed: bool,
    mono: SharedString,
    theme: &Theme,
) -> Div {
    let meta_line = div().truncate();
    let meta_line = if meta_is_path {
        meta_line.text_ellipsis_start()
    } else {
        meta_line
    };
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(theme.px(1.))
        .child(
            div()
                .truncate()
                .text_size(theme.text(13.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(white(if dimmed { TEXT_MIN } else { TEXT_PRIMARY }))
                .child(SharedString::from(name.to_string())),
        )
        .child(
            meta_line
                .font_family(mono)
                .text_size(theme.text(11.))
                .text_color(white(TEXT_MIN))
                .child(SharedString::from(meta)),
        )
}

/// Le fond d'une ligne (maquette `.row`) : 44 de haut, retrait 8 et 6, écart 10, rayon 7, blanc à 5 % au survol.
fn row_shell(theme: &Theme) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.px(10.))
        .h(theme.px(ROW_HEIGHT))
        .pl(theme.px(8.))
        .pr(theme.px(6.))
        .rounded(theme.px(7.))
        .hover(|style| style.bg(white(HOVER)))
}

/// Le message d'une erreur sous sa ligne, aligné sur le texte de la ligne (retrait 8 + glyphe + écart).
fn row_error(text: &str, theme: &Theme) -> Div {
    div()
        .flex_none()
        .pl(theme.px(8. + GLYPH_SIZE + 10.))
        .pb(theme.px(6.))
        .text_size(theme.text(11.5))
        .text_color(color(ERROR_TEXT))
        .child(SharedString::from(text.to_string()))
}

/// Les cinq puces neutres du pied (maquette `.chip`) : blanc à 7 %, texte à `TEXT_SECONDARY`.
fn harness_chips(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_wrap()
        .gap(theme.px(4.))
        .children(Harness::ALL.into_iter().map(|harness| {
            div()
                .px(theme.px(5.))
                .py(theme.px(2.))
                .rounded(theme.px(4.))
                .bg(white(HOVER_STRONG))
                .font_family(CHIP_FONT)
                .text_size(theme.text(10.))
                .font_weight(FontWeight::BOLD)
                .text_color(white(TEXT_SECONDARY))
                .child(harness.chip())
        }))
}

/// Un bouton de la boîte du champ : 18 de côté, rayon 4, l'icône à `TEXT_MIN`.
fn field_button(id: &'static str, icon: Icon, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(theme.px(18.))
        .rounded(theme.px(4.))
        .cursor_pointer()
        .hover(|style| style.bg(white(BUTTON_HOVER)))
        .child(
            svg()
                .path(icon.path())
                .size(theme.px(14.))
                .text_color(white(TEXT_MIN)),
        )
}

impl SkillsTab {
    /// La boîte du champ : loupe, champ, croix (texte présent) et bouton dossier. Un appui gauche y donne le focus au
    /// champ ; Échap y est pris (`ClearField`).
    fn render_field(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let field_focus = self.field.focus_handle(cx);
        let has_text = !self.field.read(cx).value().is_empty();
        let clear = has_text.then(|| {
            field_button("skills-field-clear", Icon::Close, theme).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.prevent_default();
                    this.clear_field(window, cx);
                }),
            )
        });
        let folder = field_button("skills-field-folder", Icon::Folder, theme).on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                window.prevent_default();
                this.choose_folder(window, cx);
            }),
        );
        div()
            .id("skills-field")
            .key_context(FIELD_KEY_CONTEXT)
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.px(7.))
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
            .on_action(cx.listener(|this, _: &ClearField, window, cx| {
                this.clear_field(window, cx);
            }))
            .child(
                svg()
                    .path(Icon::Search.path())
                    .flex_none()
                    .size(theme.px(15.))
                    .text_color(white(TEXT_MIN)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(field_input(&self.field, theme)),
            )
            .children(clear)
            .child(folder)
    }

    /// Un bouton d'action d'une ligne, caché tant que la ligne n'est pas survolée. Éteint pendant un appel.
    fn row_action(
        &self,
        id: (&'static str, usize),
        label: &'static str,
        icon: Icon,
        kind: ButtonKind,
        group: &str,
        theme: &Theme,
    ) -> Stateful<Div> {
        let idle = self.call.is_none();
        action_button(id, label, Some(icon), kind, idle, theme)
            .opacity(0.)
            .group_hover(group.to_string(), |style| style.opacity(1.))
    }

    /// La ligne d'une skill du projet, avec son erreur et ses bandeaux.
    fn render_skill(
        &self,
        index: usize,
        skill: &InstalledSkill,
        mono: SharedString,
        home: Option<&Path>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let key = RowKey::Skill(skill.dir.clone());
        let group = format!("skill-row-{index}");
        let idle = self.call.is_none();
        let trailing: AnyElement = match self.call_on(&key) {
            Some(kind) => busy_text(kind.busy_label(), theme).into_any_element(),
            None => {
                let dir = skill.dir.clone();
                self.row_action(
                    ("skill-remove", index),
                    "Retirer",
                    Icon::Delete,
                    ButtonKind::Danger,
                    &group,
                    theme,
                )
                .when(idle, |button| {
                    button.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.confirm_removal(dir.clone(), window, cx);
                        }),
                    )
                })
                .into_any_element()
            }
        };
        let row = row_shell(theme)
            .id(("skill-row", index))
            .group(group)
            .child(glyph_tile(Icon::Bolt, theme))
            .child(row_text(
                &skill.name,
                source_label(skill, home),
                source_is_path(skill),
                false,
                mono,
                theme,
            ))
            .child(trailing);
        div()
            .flex_none()
            .flex()
            .flex_col()
            .child(row)
            .children(
                self.errors
                    .get(&key)
                    .map(|message| row_error(message, theme)),
            )
            .children(self.render_skill_banner(index, skill, theme, cx))
    }

    /// Le bandeau de la skill : « Différente selon les harnais » quand ses copies divergent, sinon la couverture quand
    /// un harnais ne la voit pas. Une skill aux copies divergentes ne propose pas « Propager » : il copierait une
    /// version qu'on n'a pas choisie.
    fn render_skill_banner(
        &self,
        index: usize,
        skill: &InstalledSkill,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let key = RowKey::Skill(skill.dir.clone());
        let busy = self.call_on(&key);
        let idle = self.call.is_none();
        if skill.versions.len() > 1 {
            let open = self.open_versions.as_deref() == Some(skill.dir.as_str());
            let dir = skill.dir.clone();
            let chevron = svg()
                .path(
                    if open {
                        Icon::ExpandMore
                    } else {
                        Icon::ChevronRight
                    }
                    .path(),
                )
                .size(theme.px(16.))
                .text_color(white(TEXT_SECONDARY))
                .into_any_element();
            let header = div()
                .id(("skill-versions", index))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.toggle_versions(&dir, cx);
                    }),
                )
                .child(warning_banner(
                    SharedString::from("Différente selon les harnais"),
                    None,
                    vec![chevron],
                    theme,
                ));
            let mut banner = div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(theme.px(4.))
                .pb(theme.px(6.))
                .child(header);
            if open {
                for (position, roots) in skill.versions.iter().enumerate() {
                    let from: &'static str = roots[0];
                    let dir = skill.dir.clone();
                    let action: AnyElement = match busy {
                        Some(kind) => busy_text(kind.busy_label(), theme).into_any_element(),
                        None => div()
                            .id(("skill-keep", index * 8 + position))
                            .when(idle, |wrapper| {
                                wrapper.on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                        cx.stop_propagation();
                                        this.keep_skill_version(dir.clone(), from, window, cx);
                                    }),
                                )
                            })
                            .child(banner_button("skills-keep", "Garder celle-ci", idle, theme))
                            .into_any_element(),
                    };
                    banner = banner.child(info_banner(
                        SharedString::from(folders_text(roots)),
                        Some(visible_to(roots)),
                        vec![action],
                        theme,
                    ));
                }
            }
            return Some(banner);
        }
        let missing = missing_harnesses(skill);
        if missing.is_empty() {
            return None;
        }
        let dir = skill.dir.clone();
        let action: AnyElement = match busy {
            Some(kind) => busy_text(kind.busy_label(), theme).into_any_element(),
            None => div()
                .id(("skill-propagate", index))
                .when(idle, |wrapper| {
                    wrapper.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.propagate_skill(dir.clone(), window, cx);
                        }),
                    )
                })
                .child(banner_button("skills-propagate", "Propager", idle, theme))
                .into_any_element(),
        };
        Some(div().flex_none().pb(theme.px(6.)).child(warning_banner(
            SharedString::from(coverage_text(&missing)),
            Some(visible_to(&skill.present)),
            vec![action],
            theme,
        )))
    }

    /// Le champ vide : « Dans ce projet » et ses skills.
    fn render_project(
        &self,
        mono: SharedString,
        home: Option<&Path>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let count =
            matches!(self.listing, Listing::Ready(_)).then(|| self.skills().len().to_string());
        let mut list =
            div()
                .flex_none()
                .flex()
                .flex_col()
                .child(group_title("Dans ce projet", count, theme));
        for (index, skill) in self.skills().iter().enumerate() {
            list = list.child(self.render_skill(index, skill, mono.clone(), home, theme, cx));
        }
        list
    }

    /// Un texte seul sous le champ (« Aucun résultat »).
    fn render_note(&self, text: &'static str, theme: &Theme) -> Div {
        div()
            .flex_none()
            .px(theme.px(2.))
            .pt(theme.px(6.))
            .text_size(theme.text(12.))
            .text_color(white(TEXT_MIN))
            .child(text)
    }

    /// Un texte, le champ rempli : « Groupe skills.sh » et ses résultats, ou « Aucun résultat ».
    fn render_results(&self, mono: SharedString, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let Some(hits) = &self.search.hits else {
            return div().child(group_title("skills.sh", None, theme));
        };
        let count = match hits.len() {
            0 => None,
            1 => Some("1 résultat".to_string()),
            count => Some(format!("{count} résultats")),
        };
        let mut list =
            div()
                .flex_none()
                .flex()
                .flex_col()
                .child(group_title("skills.sh", count, theme));
        if hits.is_empty() {
            return list.child(self.render_note(NO_RESULT, theme));
        }
        for (index, row) in result_rows(hits, self.skills()).into_iter().enumerate() {
            list = list.child(self.render_hit(index, &row, mono.clone(), theme, cx));
        }
        list
    }

    fn render_hit(
        &self,
        index: usize,
        row: &ResultRow<'_>,
        mono: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let hit = row.hit;
        let key = RowKey::Hit(hit_key(hit));
        let group = format!("hit-row-{index}");
        let idle = self.call.is_none();
        let action = hit_action(row.installed, self.listing.status());
        let trailing: AnyElement = if let Some(kind) = self.call_on(&key) {
            busy_text(kind.busy_label(), theme).into_any_element()
        } else if action == HitAction::Checking {
            busy_text(BUSY, theme).into_any_element()
        } else if action == HitAction::Nothing {
            div().flex_none().into_any_element()
        } else if action == HitAction::Installed {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(theme.px(4.))
                .pr(theme.px(6.))
                .text_size(theme.text(12.))
                .text_color(white(TEXT_MIN))
                .child(
                    svg()
                        .path(Icon::Check.path())
                        .size(theme.px(15.))
                        .text_color(color(RUNNING)),
                )
                .child(row.label())
                .into_any_element()
        } else {
            let target = hit.clone();
            self.row_action(
                ("hit-add", index),
                row.label(),
                Icon::Add,
                ButtonKind::Ghost,
                &group,
                theme,
            )
            .bg(white(HOVER_STRONG))
            .when(idle, |button| {
                button.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.add_hit(&target, window, cx);
                    }),
                )
            })
            .into_any_element()
        };
        let meta = format!("{} · {}", hit.source, format_installs(hit.installs));
        div()
            .flex_none()
            .flex()
            .flex_col()
            .child(
                row_shell(theme)
                    .id(("hit-row", index))
                    .group(group)
                    .child(glyph_tile(Icon::Bolt, theme))
                    .child(row_text(&hit.name, meta, false, false, mono, theme))
                    .child(trailing),
            )
            .children(
                self.errors
                    .get(&key)
                    .map(|message| row_error(message, theme)),
            )
    }

    /// Un chemin dans le champ : une seule ligne « Ajouter depuis ce dossier », inactive si le dossier manque.
    fn render_folder(
        &self,
        target: &FolderTarget,
        mono: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let idle = self.call.is_none();
        let trailing: AnyElement = if let Some(kind) = self.call_on(&RowKey::Folder) {
            busy_text(kind.busy_label(), theme).into_any_element()
        } else if !target.exists {
            div()
                .flex_none()
                .pr(theme.px(6.))
                .text_size(theme.text(12.))
                .text_color(white(TEXT_MIN))
                .child("Dossier introuvable")
                .into_any_element()
        } else {
            let folder = target.clone();
            self.row_action(
                ("folder-add", 0),
                "Ajouter",
                Icon::Add,
                ButtonKind::Ghost,
                "folder-row",
                theme,
            )
            .bg(white(HOVER_STRONG))
            .when(idle, |button| {
                button.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.add_folder(&folder, window, cx);
                    }),
                )
            })
            .into_any_element()
        };
        div()
            .flex_none()
            .flex()
            .flex_col()
            .child(
                row_shell(theme)
                    .id("folder-row")
                    .group("folder-row")
                    .mt(theme.px(12.))
                    .child(glyph_tile(Icon::Folder, theme))
                    .child(row_text(
                        "Ajouter depuis ce dossier",
                        target.display.clone(),
                        true,
                        !target.exists,
                        mono,
                        theme,
                    ))
                    .child(trailing),
            )
            .children(
                self.errors
                    .get(&RowKey::Folder)
                    .map(|message| row_error(message, theme)),
            )
    }
}

impl Render for SkillsTab {
    /// L'onglet : le champ, l'échec de skills.sh sous lui, ce que le champ fait montrer (qui défile), et le pied
    /// « Copiées pour » sous la liste du projet.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        let mono = cx
            .global::<gpui_component::Theme>()
            .mono_font_family
            .clone();
        let home = home_dir();
        let failed = matches!(self.view, FieldView::Search(_)) && self.search.failed;
        let listing_error = match (&self.view, &self.listing) {
            (FieldView::Project, Listing::Failed(message)) => Some(error_banner(
                SharedString::from(message.clone()),
                None,
                Vec::new(),
                &theme,
            )),
            _ => None,
        };
        let content = match self.view.clone() {
            FieldView::Project => self.render_project(mono, home.as_deref(), &theme, cx),
            FieldView::Search(_) if self.search.failed => div(),
            FieldView::Search(_) => self.render_results(mono, &theme, cx),
            FieldView::Folder(target) => self.render_folder(&target, mono, &theme, cx),
        };
        let foot =
            (matches!(self.view, FieldView::Project) && !self.skills().is_empty()).then(|| {
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(theme.px(6.))
                    .mt(theme.px(10.))
                    .pt(theme.px(10.))
                    .border_t_1()
                    .border_color(white(SEPARATOR))
                    .text_size(theme.text(11.5))
                    .text_color(white(TEXT_MIN))
                    .child(
                        svg()
                            .path(Icon::ContentCopy.path())
                            .flex_none()
                            .size(theme.px(14.))
                            .text_color(white(TEXT_MIN)),
                    )
                    .child("Copiées pour")
                    .child(harness_chips(&theme))
            });
        let root = div()
            .id("skills-tab")
            .size_full()
            .flex()
            .flex_col()
            .px(theme.px(BODY_MARGIN_SIDES))
            .pt(theme.px(8.))
            .pb(theme.px(12.));
        // Le dossier personnel n'est pas un projet : un bandeau, ni champ ni liste ni geste.
        if self.availability == TabAvailability::HomeIsNotAProject {
            return root.child(info_banner(
                SharedString::from(HOME_NOT_A_PROJECT),
                None,
                Vec::new(),
                &theme,
            ));
        }
        root.child(self.render_field(&theme, cx))
            .children(failed.then(|| {
                div()
                    .flex_none()
                    .px(theme.px(2.))
                    .pt(theme.px(6.))
                    .text_size(theme.text(11.5))
                    .text_color(color(ERROR_TEXT))
                    .child(SEARCH_FAILED)
            }))
            .children(listing_error.map(|banner| div().flex_none().pt(theme.px(8.)).child(banner)))
            .child(
                div()
                    .id("skills-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(content),
            )
            .children(foot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const LIST_SAMPLE: &str = include_str!("testdata/skills-list.sample.json");
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "idema-skills-view-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn installed(dir: &str, name: &str, source: SkillSource) -> InstalledSkill {
        InstalledSkill {
            dir: dir.to_string(),
            name: name.to_string(),
            source,
            description: String::new(),
            present: vec![".claude/skills", ".agents/skills"],
            versions: vec![vec![".claude/skills", ".agents/skills"]],
        }
    }

    fn locked(source: &str) -> SkillSource {
        SkillSource::Locked {
            source: source.to_string(),
            kind: "github".to_string(),
        }
    }

    fn hit(name: &str, source: &str, installs: u64) -> SearchHit {
        SearchHit {
            name: name.to_string(),
            source: source.to_string(),
            skill_id: name.to_string(),
            installs,
        }
    }

    #[test]
    fn result_rows_mark_installed_skills() {
        let project = [installed(
            "motion-graphics",
            "motion-graphics",
            locked("heygen-com/hyperframes"),
        )];
        let hits = [
            hit("motion-graphics", "heygen-com/hyperframes", 514_070),
            // Même nom, autre source : une autre skill, elle reste à ajouter.
            hit("motion-graphics", "someone/else", 12),
            hit("gpt-taste", "leonxlnx/taste-skill", 399_500),
        ];
        let rows = result_rows(&hits, &project);
        let labels: Vec<_> = rows.iter().map(ResultRow::label).collect();
        assert_eq!(labels, ["Installée", "Ajouter", "Ajouter"]);
        assert!(rows[0].installed && !rows[1].installed);
    }

    #[test]
    fn a_handwritten_skill_never_marks_a_result_installed() {
        let project = [installed(
            "motion-graphics",
            "motion-graphics",
            SkillSource::Handwritten,
        )];
        let hits = [hit("motion-graphics", "heygen-com/hyperframes", 1)];
        assert_eq!(result_rows(&hits, &project)[0].label(), "Ajouter");
    }

    fn write_skill(project: &Path, relative: &str) {
        let path = project.join(relative).join("SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "---\nname: x\ndescription: d\n---\n").unwrap();
    }

    fn collision(dir: &str, origin: CollisionOrigin) -> DiskCollision {
        DiskCollision {
            dir: dir.to_string(),
            names: vec![dir.to_string()],
            origin,
        }
    }

    fn handwritten_origin() -> CollisionOrigin {
        CollisionOrigin::Skill(SkillSource::Handwritten)
    }

    /// Une fausse Corbeille : déplace `path` dans `bin`.
    fn trash_into(bin: &Path, path: &Path) -> io::Result<PathBuf> {
        let target = bin.join(format!("{}", COUNTER.fetch_add(1, Ordering::SeqCst)));
        fs::rename(path, &target)?;
        Ok(target)
    }

    #[test]
    fn a_collision_is_found_for_a_result_that_still_reads_add_and_never_for_an_installed_one() {
        let project = [
            installed(
                "review-animations",
                "review-animations",
                SkillSource::Handwritten,
            ),
            installed(
                "motion-graphics",
                "motion-graphics",
                locked("heygen-com/hyperframes"),
            ),
        ];
        let hits = [
            hit("review-animations", "emilkowalski/skills", 5),
            hit("motion-graphics", "heygen-com/hyperframes", 4),
        ];
        let rows = result_rows(&hits, &project);
        // La skill écrite à la main de même dossier : « Ajouter » reste proposé, et l'ajout la remplacerait.
        assert_eq!(rows[0].label(), "Ajouter");
        let disk = temp_dir("collision-row");
        write_skill(&disk, ".claude/skills/review-animations");
        assert_eq!(
            disk_collisions(&disk, &hit_target_dirs(rows[0].hit), &project).len(),
            1
        );
        // La même source : « Installée », aucun ajout n'est proposé, donc rien à confirmer.
        assert_eq!(rows[1].label(), "Installée");
    }

    #[test]
    fn the_replacement_texts_name_the_skills_and_say_where_they_go() {
        let handwritten = collision("review-animations", handwritten_origin());
        let (title, detail) = replace_copy(std::slice::from_ref(&handwritten), false, None);
        assert_eq!(title, "Remplacer « review-animations » ?");
        assert_eq!(
            detail,
            "Le projet a déjà une skill « review-animations » (écrite à la main). L'ajouter la remplace dans les trois dossiers ; l'ancienne version part à la Corbeille."
        );
        let locked_skill = collision("motion", CollisionOrigin::Skill(locked("someone/else")));
        let (_, detail) = replace_copy(std::slice::from_ref(&locked_skill), false, None);
        assert!(
            detail.contains("(installée depuis someone/else)"),
            "{detail}"
        );
        // Un dossier sans `SKILL.md` : le CLI le vide aussi, la phrase le dit.
        let bare = collision("notes", CollisionOrigin::Folder);
        let (title, detail) = replace_copy(std::slice::from_ref(&bare), false, None);
        assert_eq!(title, "Remplacer « notes » ?");
        assert_eq!(
            detail,
            "Le projet a déjà une skill « notes » (dossier sans SKILL.md). L'ajouter la remplace dans les trois dossiers ; l'ancienne version part à la Corbeille."
        );
        // Un dossier qui en remplace une seule reprend le texte d'une skill.
        assert_eq!(
            replace_copy(std::slice::from_ref(&handwritten), true, None).0,
            "Remplacer « review-animations » ?"
        );
        let both = [handwritten, locked_skill];
        let (title, detail) = replace_copy(&both, true, None);
        assert_eq!(title, "Remplacer 2 skills ?");
        assert_eq!(
            detail,
            "Ce dossier remplace review-animations, motion dans le projet ; les anciennes versions partent à la Corbeille."
        );
        let (_, detail) = replace_copy(&both, false, None);
        assert!(detail.starts_with("Cet ajout remplace"), "{detail}");
    }

    #[test]
    fn a_confirmed_replacement_is_abandoned_when_the_set_grew_or_changed_origin() {
        let handwritten = collision("a", handwritten_origin());
        let locked_a = collision("a", CollisionOrigin::Skill(locked("x/y")));
        let other = collision("b", handwritten_origin());
        let frozen = [handwritten.clone()];
        assert!(replacement_still_valid(
            &frozen,
            std::slice::from_ref(&handwritten)
        ));
        // Le lock a pris la skill, ou le dossier a reçu un `SKILL.md` : la phrase n'est plus vraie.
        assert!(!replacement_still_valid(&frozen, &[locked_a]));
        assert!(!replacement_still_valid(
            &frozen,
            &[collision("a", CollisionOrigin::Folder)]
        ));
        // Une autre entrée s'y est ajoutée : refus.
        assert!(!replacement_still_valid(&frozen, &[handwritten, other]));
        // Elle a disparu : on écraserait moins que ce qui a été accepté.
        assert!(replacement_still_valid(&frozen, &[]));
        // Sans rien de confirmé, la moindre entrée apparue refuse l'ajout.
        assert!(!replacement_still_valid(
            &[],
            &[collision("a", handwritten_origin())]
        ));
    }

    #[test]
    fn an_add_with_an_entry_that_appeared_after_the_confirmation_changes_nothing() {
        let project = temp_dir("add-grew");
        let bin = temp_dir("add-grew-bin");
        write_skill(&project, ".claude/skills/old");
        write_skill(&project, ".claude/skills/fresh");
        let targets = vec!["old".to_string(), "fresh".to_string()];
        // Ce que l'utilisateur a confirmé : `old` seulement ; `fresh` est apparue depuis.
        let frozen = [collision("old", handwritten_origin())];
        let mut cli_calls = 0;
        let outcome = run_add_checked(
            &project,
            &targets,
            &[],
            &frozen,
            &mut |path| trash_into(&bin, path),
            &mut || {
                cli_calls += 1;
                Ok(())
            },
        );
        assert_eq!(outcome, Err(LIST_CHANGED.to_string()));
        assert_eq!(cli_calls, 0);
        assert_eq!(fs::read_dir(&bin).unwrap().count(), 0);
        assert!(project.join(".claude/skills/old/SKILL.md").is_file());
        assert!(project.join(".claude/skills/fresh/SKILL.md").is_file());
    }

    #[test]
    fn a_confirmed_add_trashes_every_copy_then_runs_the_cli_and_a_refusal_stops_before_it() {
        let project = temp_dir("add-confirmed");
        let bin = temp_dir("add-confirmed-bin");
        write_skill(&project, ".claude/skills/old");
        write_skill(&project, ".agents/skills/old");
        let targets = vec!["old".to_string()];
        let frozen = [collision("old", handwritten_origin())];
        let mut cli_calls = 0;
        let outcome = run_add_checked(
            &project,
            &targets,
            &[],
            &frozen,
            &mut |path| trash_into(&bin, path),
            &mut || {
                cli_calls += 1;
                Ok(())
            },
        );
        assert_eq!(outcome, Ok(()));
        assert_eq!(cli_calls, 1);
        assert_eq!(fs::read_dir(&bin).unwrap().count(), 2);
        assert!(!project.join(".claude/skills/old").exists());

        // Une Corbeille qui refuse : aucun appel du CLI, et la raison est en français.
        write_skill(&project, ".grok/skills/old");
        let mut cli_calls = 0;
        let outcome = run_add_checked(
            &project,
            &targets,
            &[],
            &frozen,
            &mut |_| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Operation not permitted (os error 1)",
                ))
            },
            &mut || {
                cli_calls += 1;
                Ok(())
            },
        );
        assert_eq!(
            outcome,
            Err("La mise à la Corbeille a échoué : accès refusé".to_string())
        );
        assert_eq!(cli_calls, 0);
        assert!(project.join(".grok/skills/old/SKILL.md").is_file());
    }

    #[test]
    fn io_failures_reach_the_banners_in_french_only() {
        let denied = io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Operation not permitted (os error 1)",
        );
        assert_eq!(
            failure_text("La mise à la Corbeille a échoué", &denied),
            "La mise à la Corbeille a échoué : accès refusé"
        );
        let other = io::Error::other("No such file or directory (os error 2)");
        let text = failure_text("La copie a échoué", &other);
        assert_eq!(text, "La copie a échoué : erreur du système");
        assert!(!text.contains("os error"));
        assert_eq!(
            failure_text(
                "La propagation a échoué",
                &io::Error::from(io::ErrorKind::StorageFull)
            ),
            "La propagation a échoué : disque plein"
        );
    }

    #[test]
    fn removal_always_says_a_copy_goes_to_the_trash_for_a_locked_skill() {
        let mut skill = installed("review-animations", "review-animations", locked("a/b"));
        let expected = "Retirer review-animations du projet ? Elle disparaît pour les cinq harnais. Une copie de la skill part à la Corbeille avant le retrait.";
        // Une seule version (copies identiques, ou une seule copie) comme plusieurs : la même phrase.
        assert_eq!(removal_copy(&skill), expected);
        skill.versions = vec![vec![".claude/skills"], vec![".agents/skills"]];
        assert_eq!(removal_copy(&skill), expected);
    }

    #[test]
    fn a_listing_waits_for_the_call_running_on_the_same_project_only() {
        let project = Path::new("/work/a");
        assert!(defers_listing(Some(project), project));
        assert!(!defers_listing(Some(Path::new("/work/b")), project));
        assert!(!defers_listing(None, project));
    }

    #[test]
    fn the_home_folder_offers_no_skills_tab() {
        let home = temp_dir("tab-home");
        fs::create_dir_all(home.join("app")).unwrap();
        assert_eq!(
            tab_availability(&home, Some(&home)),
            TabAvailability::HomeIsNotAProject
        );
        assert_eq!(
            tab_availability(&home.join("app"), Some(&home)),
            TabAvailability::Open
        );
        assert_eq!(tab_availability(&home, None), TabAvailability::Open);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn the_field_decides_the_view() {
        let project = temp_dir("project");
        let home = temp_dir("home");
        fs::create_dir_all(home.join("x")).unwrap();
        let view = |text: &str| field_view(text, &project, Some(&home));

        assert_eq!(view(""), FieldView::Project);
        assert_eq!(view("   "), FieldView::Project);
        // Un caractère : pas encore une recherche (le CLI exige deux).
        assert_eq!(view("m"), FieldView::Project);
        assert_eq!(view(" m "), FieldView::Project);
        assert_eq!(view("mo"), FieldView::Search("mo".to_string()));
        assert_eq!(
            view("  motion design "),
            FieldView::Search("motion design".to_string())
        );
        // Les deux caractères se comptent en caractères, pas en octets.
        assert_eq!(view("é"), FieldView::Project);

        match view("~/x") {
            FieldView::Folder(target) => {
                assert_eq!(target.path, home.join("x"));
                assert_eq!(target.display, "~/x");
                assert!(target.exists);
            }
            other => panic!("expected a folder, got {other:?}"),
        }
        match view("~/missing") {
            FieldView::Folder(target) => {
                assert!(!target.exists);
                assert_eq!(target.display, "~/missing");
            }
            other => panic!("expected a folder, got {other:?}"),
        }
        // Un chemin relatif se lit depuis la racine du projet.
        fs::create_dir_all(project.join("local-skills")).unwrap();
        match view("./local-skills") {
            FieldView::Folder(target) => {
                assert_eq!(target.path, project.join("local-skills"));
                assert!(target.exists);
            }
            other => panic!("expected a folder, got {other:?}"),
        }
        // Un fichier n'est pas un dossier.
        fs::write(home.join("file.txt"), "x").unwrap();
        match view("~/file.txt") {
            FieldView::Folder(target) => assert!(!target.exists),
            other => panic!("expected a folder, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&project);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn removal_copy_depends_on_the_lock() {
        let locked_skill = installed("review-animations", "review-animations", locked("a/b"));
        assert_eq!(
            removal_copy(&locked_skill),
            "Retirer review-animations du projet ? Elle disparaît pour les cinq harnais. Une copie de la skill part à la Corbeille avant le retrait."
        );
        let handwritten = installed("manual-one", "manual-one", SkillSource::Handwritten);
        assert_eq!(
            removal_copy(&handwritten),
            "Retirer manual-one du projet ? Elle disparaît pour les cinq harnais. Elle partira à la Corbeille."
        );
    }

    #[test]
    fn a_confirmed_removal_is_abandoned_when_its_kind_changed() {
        let locked_skill = installed("review-animations", "review-animations", locked("a/b"));
        let handwritten = installed("manual-one", "manual-one", SkillSource::Handwritten);
        let frozen = removal_kind(&handwritten);
        assert_eq!(frozen, RemovalKind::Handwritten);
        assert_eq!(removal_kind(&locked_skill), RemovalKind::Locked);
        // Rien n'a bougé : le retrait part.
        assert!(removal_still_valid(
            frozen,
            Some(removal_kind(&handwritten))
        ));
        // Le lock a pris la skill pendant la confirmation : « Elle partira à la Corbeille » n'est plus vrai.
        assert!(!removal_still_valid(
            frozen,
            Some(removal_kind(&locked_skill))
        ));
        // La skill a disparu.
        assert!(!removal_still_valid(frozen, None));
    }

    #[test]
    fn a_result_is_never_offered_unless_the_project_list_is_known() {
        assert_eq!(hit_action(false, ListStatus::Loading), HitAction::Checking);
        assert_eq!(hit_action(false, ListStatus::Failed), HitAction::Nothing);
        assert_eq!(hit_action(false, ListStatus::Ready), HitAction::Add);
        assert_eq!(hit_action(true, ListStatus::Ready), HitAction::Installed);
        // Une skill déjà reconnue installée l'est, liste en relecture ou en échec.
        assert_eq!(hit_action(true, ListStatus::Loading), HitAction::Installed);
        assert_eq!(hit_action(true, ListStatus::Failed), HitAction::Installed);
    }

    #[test]
    fn source_labels_cover_the_three_origins() {
        let home = Path::new("/Users/me");
        assert_eq!(
            source_label(
                &installed("a", "a", locked("emilkowalski/skills")),
                Some(home)
            ),
            "emilkowalski/skills"
        );
        let local = SkillSource::Locked {
            source: "/Users/me/dev/local-skills".to_string(),
            kind: "local".to_string(),
        };
        assert_eq!(
            source_label(&installed("a", "a", local), Some(home)),
            "~/dev/local-skills"
        );
        assert_eq!(
            source_label(&installed("a", "a", SkillSource::Handwritten), Some(home)),
            "écrite à la main"
        );
    }

    #[test]
    fn only_a_local_source_is_a_path_that_keeps_its_end() {
        let local = SkillSource::Locked {
            source: "/Users/me/dev/local-skills".to_string(),
            kind: "local".to_string(),
        };
        assert!(source_is_path(&installed("a", "a", local)));
        assert!(!source_is_path(&installed(
            "a",
            "a",
            locked("emilkowalski/skills")
        )));
        assert!(!source_is_path(&installed(
            "a",
            "a",
            SkillSource::Handwritten
        )));
    }

    #[test]
    fn tilde_path_only_abbreviates_inside_home() {
        let home = Path::new("/Users/me");
        assert_eq!(tilde_path(Path::new("/Users/me"), Some(home)), "~");
        assert_eq!(
            tilde_path(Path::new("/Users/me/a/b"), Some(home)),
            "~/a/b"
        );
        // Un dossier voisin qui commence par le même texte n'est pas sous le dossier personnel.
        assert_eq!(
            tilde_path(Path::new("/Users/me2/a"), Some(home)),
            "/Users/me2/a"
        );
        assert_eq!(tilde_path(Path::new("/opt/x"), None), "/opt/x");
    }

    #[test]
    fn missing_harnesses_follow_the_folders_that_carry_the_skill() {
        let mut skill = installed("a", "a", SkillSource::Handwritten);
        skill.present = vec![".claude/skills"];
        // Seul `.claude/skills` : Codex et Antigravity ne lisent que `.agents/skills`.
        assert_eq!(
            missing_harnesses(&skill),
            [Harness::Codex, Harness::Antigravity]
        );
        skill.present = vec![".agents/skills"];
        assert_eq!(missing_harnesses(&skill), [Harness::Claude]);
        skill.present = vec![".claude/skills", ".agents/skills"];
        assert!(missing_harnesses(&skill).is_empty());
    }

    #[test]
    fn the_list_is_read_past_noise_printed_by_the_shell() {
        let noisy = format!("Welcome back\n[oh-my-zsh] update available\n{LIST_SAMPLE}");
        let plain = parse_list_output(LIST_SAMPLE).unwrap();
        assert!(!plain.is_empty());
        assert_eq!(parse_list_output(&noisy).unwrap(), plain);
        // Du bruit après le JSON ne gêne pas non plus.
        let trailing = format!("{LIST_SAMPLE}\nbye [now]\n");
        assert_eq!(parse_list_output(&trailing).unwrap(), plain);
        assert_eq!(parse_list_output("[]").unwrap(), Vec::new());
        assert_eq!(parse_list_output("noise\n[]\n").unwrap(), Vec::new());
        assert!(parse_list_output("no json at all").is_err());
        assert!(parse_list_output("").is_err());
    }

    #[test]
    fn escape_in_the_skills_field_reaches_clear_field_and_nowhere_else() {
        use crate::files::panel::tests::kit_input_bindings;
        use crate::workspace::KEY_CONTEXT as WORKSPACE_CONTEXT;
        use gpui::{Action, KeyContext, Keymap, Keystroke};
        // Le kit lie Échap au contexte `Input`, plus profond que la boîte : son action passe d'abord, puis, faute de rien
        // à fermer, la touche revient à la liaison suivante, la nôtre.
        let mut bindings = kit_input_bindings();
        bindings.extend(key_bindings());
        let keymap = Keymap::new(bindings);
        let escape_actions = |within: &[&str]| -> Vec<&'static str> {
            let stack: Vec<KeyContext> = within
                .iter()
                .map(|name| KeyContext::parse(name).unwrap())
                .collect();
            let (found, _) =
                keymap.bindings_for_input(&[Keystroke::parse("escape").unwrap()], &stack);
            found
                .iter()
                .map(|binding| binding.action().name())
                .collect()
        };
        let clear = ClearField.name();
        let kit = gpui_component::input::Escape.name();
        let in_field = escape_actions(&[WORKSPACE_CONTEXT, FIELD_KEY_CONTEXT, "Input"]);
        let clear_at = in_field.iter().position(|name| *name == clear);
        let kit_at = in_field.iter().position(|name| *name == kit);
        assert!(clear_at.is_some(), "ClearField absente : {in_field:?}");
        assert!(kit_at < clear_at, "le kit doit passer avant : {in_field:?}");
        // Hors du champ, Échap n'est pas pris par l'onglet.
        assert!(!escape_actions(&[WORKSPACE_CONTEXT]).contains(&clear));
        assert!(!escape_actions(&[WORKSPACE_CONTEXT, "Input"]).contains(&clear));
    }
}
