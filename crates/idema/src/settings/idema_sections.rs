//! Les quatre sections IdemA de « Personnaliser » (Général, Agents, Message de commit, Navigateur) : des contrôles typés
//! qui écrivent `config.toml` par `write_edit`, au fil de l'eau. Le disque fait foi : chaque geste construit une seule
//! `ConfigEdit`, la valeur refusée par la validation s'affiche sous son contrôle et le fichier reste intact. Un
//! `FileWatch` relu chaque seconde recharge les valeurs quand le fichier change ailleurs (hors champ en cours de saisie).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, Focusable, FontWeight, MouseButton,
    SharedString, Subscription, Task, Window, div, prelude::*,
};
use gpui_component::input::{InputEvent, InputState, TextareaState};

use super::controls::{
    area_box, button, column_title, error_line, field_box, icon_button, radio, setting_row, toggle,
};
use super::nav::{Section, label};
use super::toml_write::{ConfigEdit, write_edit};
use super::view::SettingsView;
use crate::assets::Icon;
use crate::config::{
    AgentEntry, CommitMessageConfig, Config, ConfigError, ConfigErrorKind, Favorite, FileWatch,
    harness_groups, load_config,
};
use crate::errors::{Explained, explain_config};
use crate::theme::{ERROR_TEXT, SEPARATOR, TEXT_MIN, TEXT_SECONDARY, Theme, color, white};

/// Un champ texte écrit une fois la frappe arrêtée depuis ce délai.
const COMMIT_DELAY: Duration = Duration::from_millis(400);
/// Période de relecture de `config.toml`, comme la fenêtre principale.
const POLL: Duration = Duration::from_secs(1);

const NUMBER_EXPECTED: &str = "Un nombre est attendu, par exemple 1.55.";
const SECONDS_EXPECTED: &str = "Un nombre entier de secondes est attendu, par exemple 120.";
/// Le délai d'absence quand la clé est retirée (spec du compagnon, §3.2).
const DEFAULT_AWAY_AFTER: u64 = 120;
const STALE_LIST: &str = "La liste a changé sur le disque, elle est rechargée.";
const TMUX_HELP: &str =
    "Vide : détection automatique. Pris en compte au prochain lancement d'IdemA.";
const SHELL_NOTE: &str = "Shell seul est intégré à IdemA.";
const INSTRUCTIONS_HELP: &str = "Ajoutées à la demande envoyée au modèle.";
const INVALID_CONFIG: &str = "Config invalide";
const EDIT_CONFIG: &str = "Éditer config.toml";
const NO_FAVORITE: &str = "Aucun favori.";
const NO_AGENT: &str = "Aucun agent.";
const NO_HARNESS: &str = "Sans harness";

/// La liste que porte une ligne : les agents ou les favoris.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ListKind {
    Agents,
    Favorites,
}

/// Un contrôle de ces sections : la clé des erreurs, des écritures en attente et des champs à recharger. Une ligne de
/// liste se désigne par sa clé propre (`EntryRow::key`) : son rang change quand on réordonne.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Field {
    UiFontSize,
    TerminalFontFamily,
    TerminalFontSize,
    TerminalLineHeight,
    SoundOnWait,
    TmuxPath,
    CommitEnabled,
    ApiKey,
    Model,
    IncludeRules,
    Instructions,
    Endpoint,
    CompanionEnabled,
    CompanionNotifyWaiting,
    CompanionNotifyError,
    CompanionAwayAfter,
    Row(ListKind, u64),
    /// Le message qui vaut pour toute une liste (une ligne a changé sur le disque pendant qu'on la modifiait).
    List(ListKind),
}

/// Un geste sur une ligne de liste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Delete,
    Up,
    Down,
    MakeDefault,
}

/// Le geste d'une ligne, pour `plan_row_edit` (`MakeDefault` ne touche pas à l'entrée : `with_default` s'en charge ensuite).
fn row_change<T>(change: Change) -> RowChange<T> {
    match change {
        Change::Delete => RowChange::Delete,
        Change::Up => RowChange::Up,
        Change::Down => RowChange::Down,
        Change::MakeDefault => RowChange::Keep,
    }
}

/// Le texte d'un nombre de la config tel que le fichier l'écrit : `16`, `1.55`.
fn number_text(value: f32) -> String {
    if value.is_finite() && value.fract() == 0. && value.abs() < 1e9 {
        format!("{}", value as i64)
    } else {
        value.to_string()
    }
}

/// Un nombre saisi : la virgule française comme le point décimal. Rien pour un texte qui n'en est pas un (ni `NaN` ni
/// l'infini : la validation de la config les refuserait de toute façon, mais le message serait moins clair).
pub fn parse_number(text: &str) -> Option<f32> {
    text.trim()
        .replace(',', ".")
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite())
}

/// `agents` avec `default` sur l'entrée `index` et nulle part ailleurs. Un index hors liste n'en désigne aucune : plus
/// aucun défaut (IdemA retombe alors sur la première entrée).
pub fn with_default(agents: &[AgentEntry], index: usize) -> Vec<AgentEntry> {
    agents
        .iter()
        .enumerate()
        .map(|(position, agent)| AgentEntry {
            default: position == index,
            ..agent.clone()
        })
        .collect()
}

/// Déplace l'entrée `from` en `to`. Les bornes tiennent : un index hors liste, ou `to == from`, rend la liste telle
/// quelle (le premier ne monte pas, le dernier ne descend pas).
pub fn move_entry<T>(mut items: Vec<T>, from: usize, to: usize) -> Vec<T> {
    if from >= items.len() || to >= items.len() || from == to {
        return items;
    }
    let entry = items.remove(from);
    items.insert(to, entry);
    items
}

/// Le geste demandé sur une entrée de liste.
#[derive(Debug, Clone, PartialEq)]
pub enum RowChange<T> {
    /// Remplace l'entrée.
    Replace(T),
    Delete,
    Up,
    Down,
    /// Ne touche à rien : la liste telle qu'elle est (le geste porte sur d'autres champs, tel `default`).
    Keep,
}

/// La ligne n'est plus celle que l'écran montrait : le fichier a changé dessous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stale;

/// La liste du fichier après `change` sur l'entrée `index`, à condition que cette entrée soit encore celle dont la ligne
/// est partie (`baseline`). Sinon (autre entrée à ce rang, ou rang hors liste), `Stale` : un rang seul ne dit pas de quelle
/// entrée il s'agit, et une édition faite ailleurs ne doit ni toucher la mauvaise ni ressusciter une supprimée. Le résultat
/// peut être égal à `disk` (borne atteinte, valeur inchangée) : l'appelant n'écrit alors rien.
pub fn plan_row_edit<T: PartialEq + Clone>(
    disk: &[T],
    index: usize,
    baseline: &T,
    change: RowChange<T>,
) -> Result<Vec<T>, Stale> {
    if disk.get(index) != Some(baseline) {
        return Err(Stale);
    }
    let mut list = disk.to_vec();
    Ok(match change {
        RowChange::Replace(entry) => {
            list[index] = entry;
            list
        }
        RowChange::Delete => {
            list.remove(index);
            list
        }
        RowChange::Up => move_entry(list, index, index.saturating_sub(1)),
        RowChange::Down => move_entry(list, index, index + 1),
        RowChange::Keep => list,
    })
}

/// Le message d'une erreur d'écriture ou de lecture, tel qu'il se lit sous un contrôle. Une erreur de syntaxe passe par le
/// catalogue français ; les autres portent déjà leur phrase (validation d'IdemA, « écriture impossible »).
fn error_text(error: &ConfigError) -> String {
    match error.kind {
        ConfigErrorKind::Syntax => explain_config(error).text,
        _ => error.message.clone(),
    }
}

/// Une ligne éditable d'une liste : ses champs (nom, commande, icône, harness pour un agent ; nom, adresse pour un favori)
/// et une clé stable pour la retrouver malgré les réordonnancements.
struct EntryRow {
    key: u64,
    fields: Vec<Entity<InputState>>,
    /// L'entrée du fichier dont la ligne est partie ; `None` pour le brouillon, qui n'y est pas encore.
    baseline: Option<Baseline>,
    _subscriptions: Vec<Subscription>,
}

/// L'entrée de liste d'où une ligne est née : l'identité qu'on revérifie sur le disque avant d'écrire.
#[derive(Debug, Clone, PartialEq)]
enum Baseline {
    Agent(AgentEntry),
    Favorite(Favorite),
}

impl EntryRow {
    /// Les textes des champs, sans les blancs de tête et de queue.
    fn values(&self, cx: &App) -> Vec<String> {
        self.fields
            .iter()
            .map(|field| field.read(cx).value().trim().to_string())
            .collect()
    }

    fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.fields
            .iter()
            .any(|field| field.focus_handle(cx).is_focused(window))
    }
}

/// Les champs fixes des sections Général et Message de commit.
struct Inputs {
    ui_font_size: Entity<InputState>,
    terminal_font_family: Entity<InputState>,
    terminal_font_size: Entity<InputState>,
    terminal_line_height: Entity<InputState>,
    tmux_path: Entity<InputState>,
    api_key: Entity<InputState>,
    model: Entity<InputState>,
    endpoint: Entity<InputState>,
    instructions: Entity<TextareaState>,
    away_after: Entity<InputState>,
}

impl Inputs {
    /// Le champ d'une clé, pour les contrôles à une ligne.
    fn input(&self, field: Field) -> Option<&Entity<InputState>> {
        match field {
            Field::UiFontSize => Some(&self.ui_font_size),
            Field::TerminalFontFamily => Some(&self.terminal_font_family),
            Field::TerminalFontSize => Some(&self.terminal_font_size),
            Field::TerminalLineHeight => Some(&self.terminal_line_height),
            Field::TmuxPath => Some(&self.tmux_path),
            Field::ApiKey => Some(&self.api_key),
            Field::Model => Some(&self.model),
            Field::Endpoint => Some(&self.endpoint),
            Field::CompanionAwayAfter => Some(&self.away_after),
            _ => None,
        }
    }
}

/// L'état des quatre sections, tenu par `SettingsView`.
pub(super) struct IdemaState {
    watch: FileWatch,
    /// La dernière lecture de `config.toml` : la config, ou l'erreur que les quatre sections montrent.
    config: Result<Config, ConfigError>,
    /// L'erreur de chaque contrôle, sous lui.
    errors: HashMap<Field, String>,
    /// Les écritures qui attendent la fin de la frappe. Une écriture en attente protège son champ d'un rechargement.
    pending: HashMap<Field, Task<()>>,
    /// Le texte que la config donnait à chaque champ à la dernière réconciliation : tant qu'il n'a pas changé, le champ
    /// garde sa saisie (même refusée) ; dès qu'il change et que le champ est libre, le champ le reprend.
    synced: HashMap<Field, String>,
    synced_rows: HashMap<ListKind, Vec<Vec<String>>>,
    /// La clé d'API est montrée en clair.
    show_key: bool,
    next_key: u64,
    inputs: Inputs,
    agents: Vec<EntryRow>,
    /// L'affichage des agents : chaque harness et les clés de ses lignes, triés. `agents` reste dans l'ordre du fichier.
    /// Figé pendant une saisie : une ligne ne change pas de place sous les doigts.
    agent_groups: Vec<(Option<String>, Vec<u64>)>,
    /// La ligne « Ajouter un agent » pas encore écrite : le fichier refuse un agent sans nom ni commande.
    agent_draft: Option<EntryRow>,
    favorites: Vec<EntryRow>,
    favorite_draft: Option<EntryRow>,
    _subscriptions: Vec<Subscription>,
    _poll: Task<()>,
}

impl IdemaState {
    pub(super) fn new(
        config_path: &Path,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) -> Self {
        let mut watch = FileWatch::new(config_path.to_path_buf());
        // Le premier `changed` répond vrai : la lecture qui suit est la lecture initiale.
        watch.changed();
        let config = load_config(config_path);

        let single = |placeholder: &str, window: &mut Window, cx: &mut Context<SettingsView>| {
            let placeholder = SharedString::from(placeholder.to_string());
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let inputs = Inputs {
            ui_font_size: single("", window, cx),
            terminal_font_family: single("", window, cx),
            terminal_font_size: single("", window, cx),
            terminal_line_height: single("", window, cx),
            tmux_path: single("Détection automatique", window, cx),
            api_key: cx.new(|cx| InputState::new(window, cx).masked(true)),
            // Le modèle par défaut du fichier : un champ vide l'utilise.
            model: single(&CommitMessageConfig::default().model, window, cx),
            endpoint: single("Point d'accès par défaut", window, cx),
            instructions: cx.new(|cx| TextareaState::new(window, cx)),
            away_after: single("120", window, cx),
        };

        let mut subscriptions = Vec::new();
        for field in [
            Field::UiFontSize,
            Field::TerminalFontFamily,
            Field::TerminalFontSize,
            Field::TerminalLineHeight,
            Field::TmuxPath,
            Field::ApiKey,
            Field::Model,
            Field::Endpoint,
            Field::CompanionAwayAfter,
        ] {
            if let Some(input) = inputs.input(field) {
                subscriptions.push(watch_input(field, input, window, cx));
            }
        }
        subscriptions.push(watch_input(
            Field::Instructions,
            &inputs.instructions,
            window,
            cx,
        ));

        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                if this
                    .update_in(cx, |this, window, cx| this.poll_idema(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            watch,
            config,
            errors: HashMap::new(),
            pending: HashMap::new(),
            synced: HashMap::new(),
            synced_rows: HashMap::new(),
            show_key: false,
            next_key: 0,
            inputs,
            agents: Vec::new(),
            agent_groups: Vec::new(),
            agent_draft: None,
            favorites: Vec::new(),
            favorite_draft: None,
            _subscriptions: subscriptions,
            _poll: poll,
        }
    }

    /// La dernière lecture de `config.toml`, que la section « Compagnon » lit aussi.
    pub(super) fn config(&self) -> &Result<Config, ConfigError> {
        &self.config
    }

    fn rows(&self, kind: ListKind) -> &Vec<EntryRow> {
        match kind {
            ListKind::Agents => &self.agents,
            ListKind::Favorites => &self.favorites,
        }
    }

    fn draft(&self, kind: ListKind) -> &Option<EntryRow> {
        match kind {
            ListKind::Agents => &self.agent_draft,
            ListKind::Favorites => &self.favorite_draft,
        }
    }

    fn draft_mut(&mut self, kind: ListKind) -> &mut Option<EntryRow> {
        match kind {
            ListKind::Agents => &mut self.agent_draft,
            ListKind::Favorites => &mut self.favorite_draft,
        }
    }

    fn rows_mut(&mut self, kind: ListKind) -> &mut Vec<EntryRow> {
        match kind {
            ListKind::Agents => &mut self.agents,
            ListKind::Favorites => &mut self.favorites,
        }
    }
}

/// Branche la frappe d'un champ sur le geste d'écriture de `field`.
fn watch_input<T: EventEmitter<InputEvent> + 'static>(
    field: Field,
    input: &Entity<T>,
    window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> Subscription {
    cx.subscribe_in(
        input,
        window,
        move |this, _, event: &InputEvent, window, cx| {
            this.on_input_event(field, event, window, cx);
        },
    )
}

impl SettingsView {
    // ---- Lecture et rechargement ------------------------------------------------------------------------------------

    /// Un pas de la minuterie : relit `config.toml` s'il a changé, puis rattrape les champs qui n'ont pas pu suivre (ils
    /// avaient le focus ou une saisie en attente). La relecture du fichier ne dépend que de `changed()`, la comparaison qui
    /// suit est peu coûteuse et tourne à chaque pas.
    fn poll_idema(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.idema.watch.changed() {
            self.idema.config = load_config(&self.config_path);
            self.update_agent_count(cx);
            self.sync_companion_feed(cx);
            cx.notify();
        }
        self.sync_idema(window, cx);
    }

    /// La première lecture : compteur de la barre latérale, puis champs et listes.
    pub(super) fn init_idema(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update_agent_count(cx);
        self.sync_companion_feed(cx);
        self.sync_idema(window, cx);
    }

    /// Remet à jour les champs et les listes depuis la config lue, sauf ceux qu'on est en train de saisir : ceux-là
    /// attendent que le focus parte et que leur écriture soit faite, et se rattrapent au pas suivant ou à la perte du focus.
    fn sync_idema(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(config) = self.idema.config.clone() else {
            return;
        };
        let mut changed = false;
        let commit = config.commit_message.clone().unwrap_or_default();
        let tmux = config
            .tmux_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        changed |= self.sync_input(
            Field::UiFontSize,
            &number_text(config.ui_font_size),
            window,
            cx,
        );
        changed |= self.sync_input(
            Field::TerminalFontFamily,
            &config.terminal_font_family,
            window,
            cx,
        );
        changed |= self.sync_input(
            Field::TerminalFontSize,
            &number_text(config.terminal_font_size),
            window,
            cx,
        );
        changed |= self.sync_input(
            Field::TerminalLineHeight,
            &number_text(config.terminal_line_height),
            window,
            cx,
        );
        changed |= self.sync_input(Field::TmuxPath, &tmux, window, cx);
        let away = config
            .companion
            .as_ref()
            .map(|section| section.away_after_seconds.to_string())
            .unwrap_or_default();
        changed |= self.sync_input(Field::CompanionAwayAfter, &away, window, cx);
        changed |= self.sync_input(
            Field::ApiKey,
            commit.api_key.as_deref().unwrap_or(""),
            window,
            cx,
        );
        changed |= self.sync_input(Field::Model, &commit.model, window, cx);
        changed |= self.sync_input(
            Field::Endpoint,
            commit.endpoint.as_deref().unwrap_or(""),
            window,
            cx,
        );
        changed |= self.sync_instructions(commit.instructions.as_deref().unwrap_or(""), window, cx);
        changed |= self.sync_rows(ListKind::Agents, false, window, cx);
        changed |= self.sync_rows(ListKind::Favorites, false, window, cx);
        if !self.list_busy(ListKind::Agents, window, cx) {
            changed |= self.regroup_agents();
        }
        if changed {
            cx.notify();
        }
    }

    /// Le compteur « Agents » de la barre latérale.
    fn update_agent_count(&mut self, cx: &mut Context<Self>) {
        let count = self
            .idema
            .config
            .as_ref()
            .ok()
            .map(|config| config.agents.len().to_string());
        self.set_count(Section::Agents, count, cx);
    }

    /// Pose `wanted` dans le champ de `field` quand la config l'a changé depuis la dernière réconciliation et que le champ
    /// est libre (ni focus ni écriture en attente) : le disque ne reprend jamais la main sur ce qu'on tape, et ne reprend
    /// pas non plus un champ dont la config n'a pas bougé. Rend vrai si l'affichage a changé.
    fn sync_input(
        &mut self,
        field: Field,
        wanted: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .idema
            .synced
            .get(&field)
            .is_some_and(|seen| seen == wanted)
            || self.idema.pending.contains_key(&field)
        {
            return false;
        }
        let Some(input) = self.idema.inputs.input(field).cloned() else {
            return false;
        };
        if input.focus_handle(cx).is_focused(window) {
            return false;
        }
        if input.read(cx).value().as_ref() != wanted {
            input.update(cx, |state, cx| state.set_value(wanted, window, cx));
        }
        self.idema.synced.insert(field, wanted.to_string());
        self.idema.errors.remove(&field);
        true
    }

    fn sync_instructions(
        &mut self,
        wanted: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let field = Field::Instructions;
        if self
            .idema
            .synced
            .get(&field)
            .is_some_and(|seen| seen == wanted)
            || self.idema.pending.contains_key(&field)
        {
            return false;
        }
        let area = self.idema.inputs.instructions.clone();
        if area.focus_handle(cx).is_focused(window) {
            return false;
        }
        if area.read(cx).value().as_ref() != wanted {
            area.update(cx, |state, cx| state.set_value(wanted, window, cx));
        }
        self.idema.synced.insert(field, wanted.to_string());
        self.idema.errors.remove(&field);
        true
    }

    /// Les textes que la config met dans les lignes d'une liste, dans l'ordre des champs.
    fn wanted_rows(&self, kind: ListKind) -> Vec<Vec<String>> {
        let Ok(config) = &self.idema.config else {
            return Vec::new();
        };
        match kind {
            ListKind::Agents => config
                .agents
                .iter()
                .map(|agent| {
                    vec![
                        agent.name.clone(),
                        agent.command.clone(),
                        agent.icon.clone().unwrap_or_default(),
                        agent.harness.clone().unwrap_or_default(),
                    ]
                })
                .collect(),
            ListKind::Favorites => config
                .browser
                .iter()
                .flat_map(|browser| &browser.favorites)
                .map(|favorite| vec![favorite.name.clone(), favorite.url.clone()])
                .collect(),
        }
    }

    /// Les entrées du fichier dont chaque ligne de `kind` est issue, dans l'ordre.
    fn config_baselines(&self, kind: ListKind) -> Vec<Baseline> {
        let Ok(config) = &self.idema.config else {
            return Vec::new();
        };
        match kind {
            ListKind::Agents => config.agents.iter().cloned().map(Baseline::Agent).collect(),
            ListKind::Favorites => config
                .browser
                .iter()
                .flat_map(|browser| &browser.favorites)
                .cloned()
                .map(Baseline::Favorite)
                .collect(),
        }
    }

    /// Refait les lignes de `kind` d'après la config. Sans `force` : si les lignes montrent déjà la config, seules leurs
    /// entrées d'origine sont remises à jour ; si la config n'a pas changé depuis la dernière réconciliation, les lignes
    /// gardent leur saisie ; sinon elles sont refaites, mais jamais pendant qu'un champ de la liste a le focus ou une
    /// écriture en attente (la saisie n'est pas encore dans le fichier : un pas suivant rattrape). La ligne brouillon, pas
    /// encore écrite, survit. Rend vrai si l'affichage a changé.
    fn sync_rows(
        &mut self,
        kind: ListKind,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.idema.config.is_err() {
            return false;
        }
        let wanted = self.wanted_rows(kind);
        let baselines = self.config_baselines(kind);
        if !force {
            let current: Vec<Vec<String>> = self
                .idema
                .rows(kind)
                .iter()
                .map(|row| row.values(cx))
                .collect();
            if current == wanted {
                // Les textes sont ceux du fichier : une entrée qui a changé sans cela se voir (`default`) est reprise.
                for (row, baseline) in self.idema.rows_mut(kind).iter_mut().zip(baselines) {
                    row.baseline = Some(baseline);
                }
                self.idema.synced_rows.insert(kind, wanted);
                return false;
            }
            if self.idema.synced_rows.get(&kind) == Some(&wanted) {
                return false;
            }
            if self.list_busy(kind, window, cx) {
                return false;
            }
        }
        let rows: Vec<EntryRow> = wanted
            .iter()
            .zip(baselines)
            .map(|(values, baseline)| self.new_row(kind, values, Some(baseline), window, cx))
            .collect();
        *self.idema.rows_mut(kind) = rows;
        self.idema.synced_rows.insert(kind, wanted);
        if kind == ListKind::Agents {
            self.regroup_agents();
        }
        let alive: Vec<u64> = self
            .idema
            .rows(kind)
            .iter()
            .chain(self.idema.draft(kind).iter())
            .map(|row| row.key)
            .collect();
        self.idema.errors.retain(
            |field, _| !matches!(field, Field::Row(k, key) if *k == kind && !alive.contains(key)),
        );
        true
    }

    /// Un champ de la liste a le focus ou une écriture en attente.
    fn list_busy(&self, kind: ListKind, window: &Window, cx: &App) -> bool {
        self.idema
            .pending
            .keys()
            .any(|field| matches!(field, Field::Row(k, _) if *k == kind))
            || self
                .idema
                .rows(kind)
                .iter()
                .any(|row| row.is_focused(window, cx))
    }

    /// Reclasse les lignes d'agents d'après leurs entrées du fichier. Rend vrai si l'affichage a changé.
    fn regroup_agents(&mut self) -> bool {
        let (keys, agents): (Vec<u64>, Vec<AgentEntry>) = self
            .idema
            .agents
            .iter()
            .filter_map(|row| match &row.baseline {
                Some(Baseline::Agent(agent)) => Some((row.key, agent.clone())),
                _ => None,
            })
            .unzip();
        let groups: Vec<(Option<String>, Vec<u64>)> = harness_groups(&agents)
            .into_iter()
            .map(|group| {
                let rows = group.agents.iter().map(|&index| keys[index]).collect();
                (group.harness, rows)
            })
            .collect();
        let changed = groups != self.idema.agent_groups;
        self.idema.agent_groups = groups;
        changed
    }

    /// Une ligne neuve de `kind`, ses champs remplis de `values`.
    fn new_row(
        &mut self,
        kind: ListKind,
        values: &[String],
        baseline: Option<Baseline>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EntryRow {
        let key = self.idema.next_key;
        self.idema.next_key += 1;
        let placeholders: &[&str] = match kind {
            ListKind::Agents => &["Nom", "Commande", "Icône", "Harness"],
            ListKind::Favorites => &["Nom", "Adresse"],
        };
        let mut fields = Vec::new();
        let mut subscriptions = Vec::new();
        for (position, placeholder) in placeholders.iter().enumerate() {
            let placeholder = SharedString::from((*placeholder).to_string());
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
            if let Some(value) = values.get(position).filter(|value| !value.is_empty()) {
                input.update(cx, |state, cx| state.set_value(value.as_str(), window, cx));
            }
            subscriptions.push(watch_input(Field::Row(kind, key), &input, window, cx));
            fields.push(input);
        }
        EntryRow {
            key,
            fields,
            baseline,
            _subscriptions: subscriptions,
        }
    }

    // ---- Écriture ---------------------------------------------------------------------------------------------------

    fn on_input_event(
        &mut self,
        field: Field,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => self.schedule_commit(field, window, cx),
            // Sortir du champ ou valider écrit tout de suite ce qui attendait.
            InputEvent::Blur | InputEvent::PressEnter { .. }
                if self.idema.pending.contains_key(&field) =>
            {
                self.commit(field, window, cx);
            }
            _ => {}
        }
        // Le champ qui vient de rendre le focus rattrape la config sans attendre le pas de la minuterie.
        if matches!(event, InputEvent::Blur) {
            self.sync_idema(window, cx);
        }
    }

    /// Écrit tout de suite chaque saisie qui attendait la fin de la frappe : avant de fermer la fenêtre, et avant un geste
    /// qui refait les lignes (supprimer, réordonner, choisir le défaut).
    pub(super) fn flush_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fields: Vec<Field> = self.idema.pending.keys().copied().collect();
        for field in fields {
            self.commit(field, window, cx);
        }
    }

    /// Écrira `field` quand la frappe se sera arrêtée 400 ms ; une frappe de plus repousse l'échéance.
    fn schedule_commit(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(COMMIT_DELAY).await;
            let _ = this.update_in(cx, |this, window, cx| this.commit(field, window, cx));
        });
        self.idema.pending.insert(field, task);
    }

    /// Écrit la valeur du champ `field` : une seule `ConfigEdit`, ou une erreur sous le contrôle, ou rien quand la valeur
    /// est déjà celle du fichier.
    fn commit(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(kind) = self.commit_without_window(field, cx) {
            self.resync_stale(kind, window, cx);
        }
    }

    /// Écrit tout de suite les saisies en attente à la sortie de l'app (`⌘Q`, fermeture de la fenêtre principale), où
    /// aucune fenêtre n'est à portée. Une liste devenue périmée n'est pas resynchronisée : l'app part.
    pub(super) fn flush_pending_on_quit(&mut self, cx: &mut Context<Self>) {
        let fields: Vec<Field> = self.idema.pending.keys().copied().collect();
        for field in fields {
            self.commit_without_window(field, cx);
        }
    }

    /// Le corps de `commit`, sans fenêtre. Rend la liste à resynchroniser quand l'écriture a trouvé la sienne périmée.
    fn commit_without_window(&mut self, field: Field, cx: &mut Context<Self>) -> Option<ListKind> {
        self.idema.pending.remove(&field);
        let mut stale = None;
        match self.build_edit(field, cx) {
            Err(message) if message == STALE_LIST => {
                if let Field::Row(kind, _) = field {
                    stale = Some(kind);
                }
            }
            Err(message) => {
                self.idema.errors.insert(field, message);
            }
            Ok(None) => {
                self.idema.errors.remove(&field);
            }
            Ok(Some(edit)) => {
                if self.write_field(field, edit, cx)
                    && let Field::Row(kind, key) = field
                {
                    self.promote_draft(kind, key);
                    self.refresh_baseline(kind, key);
                }
            }
        }
        cx.notify();
        stale
    }

    /// Écrit `edit` et range le résultat : la config rendue, ou l'erreur sous le contrôle de `field`.
    fn write_field(&mut self, field: Field, edit: ConfigEdit, cx: &mut Context<Self>) -> bool {
        let written = match write_edit(&self.config_path, &edit) {
            Ok(config) => {
                self.idema.errors.remove(&field);
                if let Field::Row(kind, _) = field {
                    self.idema.errors.remove(&Field::List(kind));
                }
                self.idema.config = Ok(config);
                self.update_agent_count(cx);
                true
            }
            Err(error) => {
                self.idema.errors.insert(field, error_text(&error));
                false
            }
        };
        cx.notify();
        written
    }

    /// La ligne `key` vient d'être écrite : son entrée d'origine est désormais celle du fichier à son rang.
    fn refresh_baseline(&mut self, kind: ListKind, key: u64) {
        let baselines = self.config_baselines(kind);
        if let Some(index) = self.idema.rows(kind).iter().position(|row| row.key == key)
            && let Some(baseline) = baselines.into_iter().nth(index)
        {
            self.idema.rows_mut(kind)[index].baseline = Some(baseline);
        }
    }

    /// Une ligne ne correspond plus au fichier : on relit le disque, les lignes sont refaites d'après lui, et un message le dit
    /// au-dessus de la liste.
    fn resync_stale(&mut self, kind: ListKind, window: &mut Window, cx: &mut Context<Self>) {
        self.idema.config = load_config(&self.config_path);
        self.update_agent_count(cx);
        self.sync_rows(kind, true, window, cx);
        self.idema
            .errors
            .insert(Field::List(kind), STALE_LIST.to_string());
        cx.notify();
    }

    /// La ligne brouillon écrite devient une ligne du fichier, sans perdre le focus.
    fn promote_draft(&mut self, kind: ListKind, key: u64) {
        if self
            .idema
            .draft(kind)
            .as_ref()
            .is_some_and(|row| row.key == key)
            && let Some(row) = self.idema.draft_mut(kind).take()
        {
            self.idema.rows_mut(kind).push(row);
        }
    }

    /// L'édition que la valeur courante du champ demande, d'après la config du fichier.
    fn build_edit(&self, field: Field, cx: &App) -> Result<Option<ConfigEdit>, String> {
        let Ok(config) = &self.idema.config else {
            return Ok(None);
        };
        let inputs = &self.idema.inputs;
        let read = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let optional = |text: String| {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let number = |input: &Entity<InputState>,
                      current: f32,
                      make: fn(f32) -> ConfigEdit|
         -> Result<Option<ConfigEdit>, String> {
            let value = parse_number(&read(input)).ok_or_else(|| NUMBER_EXPECTED.to_string())?;
            Ok((value != current).then(|| make(value)))
        };
        let commit = config.commit_message.clone().unwrap_or_default();
        match field {
            Field::UiFontSize => number(
                &inputs.ui_font_size,
                config.ui_font_size,
                ConfigEdit::UiFontSize,
            ),
            Field::TerminalFontSize => number(
                &inputs.terminal_font_size,
                config.terminal_font_size,
                ConfigEdit::TerminalFontSize,
            ),
            Field::TerminalLineHeight => number(
                &inputs.terminal_line_height,
                config.terminal_line_height,
                ConfigEdit::TerminalLineHeight,
            ),
            Field::TerminalFontFamily => {
                let wanted = read(&inputs.terminal_font_family).trim().to_string();
                Ok((wanted != config.terminal_font_family)
                    .then_some(ConfigEdit::TerminalFontFamily(wanted)))
            }
            Field::TmuxPath => {
                let wanted = optional(read(&inputs.tmux_path));
                let current = config
                    .tmux_path
                    .as_ref()
                    .map(|path| path.display().to_string());
                Ok((wanted != current).then_some(ConfigEdit::TmuxPath(wanted)))
            }
            Field::ApiKey => {
                let wanted = optional(read(&inputs.api_key));
                Ok((wanted != commit.api_key).then_some(ConfigEdit::CommitApiKey(wanted)))
            }
            Field::Model => {
                let wanted = read(&inputs.model).trim().to_string();
                Ok((wanted != commit.model).then_some(ConfigEdit::CommitModel(wanted)))
            }
            Field::Endpoint => {
                let wanted = optional(read(&inputs.endpoint));
                Ok((wanted != commit.endpoint).then_some(ConfigEdit::CommitEndpoint(wanted)))
            }
            Field::Instructions => {
                let wanted = optional(inputs.instructions.read(cx).value().to_string());
                let current = commit
                    .instructions
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string);
                Ok((wanted != current).then_some(ConfigEdit::CommitInstructions(wanted)))
            }
            Field::CompanionAwayAfter => {
                // Sans section `[companion]`, il n'y a rien à écrire (la clé ne l'allumerait pas : voir `ConfigEdit`).
                let Some(section) = &config.companion else {
                    return Ok(None);
                };
                let wanted = optional(read(&inputs.away_after))
                    .map(|text| {
                        text.parse::<u64>()
                            .map_err(|_| SECONDS_EXPECTED.to_string())
                    })
                    .transpose()?;
                // Champ vidé : la clé est retirée, le délai par défaut reprend la main.
                let current = Some(section.away_after_seconds);
                Ok((wanted.or(Some(DEFAULT_AWAY_AFTER)) != current)
                    .then_some(ConfigEdit::CompanionAwayAfter(wanted)))
            }
            Field::Row(kind, key) => self.build_row_edit(kind, key, cx),
            Field::SoundOnWait
            | Field::CommitEnabled
            | Field::IncludeRules
            | Field::CompanionEnabled
            | Field::CompanionNotifyWaiting
            | Field::CompanionNotifyError
            | Field::List(_) => Ok(None),
        }
    }

    /// L'édition d'une ligne de liste : le tableau du fichier, relu, avec cette seule ligne changée (ou ajoutée, si c'est
    /// le brouillon et qu'il est complet).
    fn build_row_edit(
        &self,
        kind: ListKind,
        key: u64,
        cx: &App,
    ) -> Result<Option<ConfigEdit>, String> {
        let rows = self.idema.rows(kind);
        let (index, row, is_draft) = match rows.iter().position(|row| row.key == key) {
            Some(index) => (index, &rows[index], false),
            None => match self.idema.draft(kind).as_ref().filter(|row| row.key == key) {
                Some(row) => (rows.len(), row, true),
                None => return Ok(None),
            },
        };
        let values = row.values(cx);
        let baseline = row.baseline.clone();
        let disk = load_config(&self.config_path).map_err(|error| error_text(&error))?;
        let stale = || STALE_LIST.to_string();
        match kind {
            ListKind::Agents => {
                let list = disk.agents;
                let entry = AgentEntry {
                    name: values[0].clone(),
                    harness: (!values[3].is_empty()).then(|| values[3].clone()),
                    command: values[1].clone(),
                    default: false,
                    icon: (!values[2].is_empty()).then(|| values[2].clone()),
                };
                let planned = if is_draft {
                    if entry.name.is_empty() || entry.command.is_empty() {
                        return Ok(None);
                    }
                    if list.len() != index {
                        return Err(stale());
                    }
                    let mut list = list.clone();
                    list.push(entry);
                    list
                } else {
                    let Some(Baseline::Agent(base)) = baseline else {
                        return Err(stale());
                    };
                    let slot = list.get(index).ok_or_else(stale)?;
                    let updated = AgentEntry {
                        default: slot.default,
                        ..entry
                    };
                    plan_row_edit(&list, index, &base, RowChange::Replace(updated))
                        .map_err(|Stale| stale())?
                };
                Ok((planned != list).then_some(ConfigEdit::Agents(planned)))
            }
            ListKind::Favorites => {
                let list = disk
                    .browser
                    .map(|browser| browser.favorites)
                    .unwrap_or_default();
                let entry = Favorite {
                    name: values[0].clone(),
                    url: values[1].clone(),
                };
                let planned = if is_draft {
                    if entry.name.is_empty() || entry.url.is_empty() {
                        return Ok(None);
                    }
                    if list.len() != index {
                        return Err(stale());
                    }
                    let mut list = list.clone();
                    list.push(entry);
                    list
                } else {
                    let Some(Baseline::Favorite(base)) = baseline else {
                        return Err(stale());
                    };
                    plan_row_edit(&list, index, &base, RowChange::Replace(entry))
                        .map_err(|Stale| stale())?
                };
                Ok((planned != list).then_some(ConfigEdit::Favorites(planned)))
            }
        }
    }

    /// Bascule un interrupteur : la valeur contraire de celle de la config, écrite aussitôt.
    pub(super) fn flip(&mut self, field: Field, cx: &mut Context<Self>) {
        let Ok(config) = &self.idema.config else {
            return;
        };
        let commit = config.commit_message.clone().unwrap_or_default();
        let edit = match field {
            Field::CompanionEnabled => ConfigEdit::CompanionEnabled(config.companion.is_none()),
            Field::CompanionNotifyWaiting => {
                let Some(section) = &config.companion else {
                    return;
                };
                ConfigEdit::CompanionNotifyWaiting(!section.notify_waiting)
            }
            Field::CompanionNotifyError => {
                let Some(section) = &config.companion else {
                    return;
                };
                ConfigEdit::CompanionNotifyError(!section.notify_error)
            }
            Field::SoundOnWait => ConfigEdit::SoundOnWait(!config.sound_on_wait),
            Field::CommitEnabled => ConfigEdit::CommitEnabled(!commit.enabled),
            Field::IncludeRules => {
                ConfigEdit::CommitIncludeProjectRules(!commit.include_project_rules)
            }
            _ => return,
        };
        if self.write_field(field, edit, cx) && field == Field::CompanionEnabled {
            // Le service démarre ou s'arrête avec la section : la veille de son état suit tout de suite.
            self.sync_companion_feed(cx);
        }
    }

    /// Montre ou masque la clé d'API.
    fn flip_key_visibility(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.idema.show_key = !self.idema.show_key;
        let masked = !self.idema.show_key;
        self.idema
            .inputs
            .api_key
            .update(cx, |state, cx| state.set_masked(masked, window, cx));
        cx.notify();
    }

    /// Supprime, déplace ou fait agent par défaut la ligne `key`, d'après le tableau relu sur le disque.
    fn change_entry(
        &mut self,
        kind: ListKind,
        key: u64,
        change: Change,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = Field::Row(kind, key);
        // Une saisie en attente part avant que les lignes ne soient refaites : sinon elle serait perdue avec elles.
        self.flush_pending(window, cx);
        // Supprimer un brouillon n'écrit rien.
        if change == Change::Delete
            && self
                .idema
                .draft(kind)
                .as_ref()
                .is_some_and(|row| row.key == key)
        {
            *self.idema.draft_mut(kind) = None;
            self.idema.errors.remove(&field);
            cx.notify();
            return;
        }
        let Some(index) = self.idema.rows(kind).iter().position(|row| row.key == key) else {
            return;
        };
        let Some(baseline) = self.idema.rows(kind)[index].baseline.clone() else {
            return;
        };
        let disk = match load_config(&self.config_path) {
            Ok(config) => config,
            Err(error) => {
                self.idema.errors.insert(field, error_text(&error));
                cx.notify();
                return;
            }
        };
        let planned = match (kind, baseline) {
            (ListKind::Agents, Baseline::Agent(base)) => {
                plan_row_edit(&disk.agents, index, &base, row_change(change)).map(|list| {
                    let list = if change == Change::MakeDefault {
                        with_default(&list, index)
                    } else {
                        list
                    };
                    (list != disk.agents).then_some(ConfigEdit::Agents(list))
                })
            }
            (ListKind::Favorites, Baseline::Favorite(base)) => {
                let list = disk
                    .browser
                    .map(|browser| browser.favorites)
                    .unwrap_or_default();
                plan_row_edit(&list, index, &base, row_change(change))
                    .map(|updated| (updated != list).then_some(ConfigEdit::Favorites(updated)))
            }
            _ => return,
        };
        match planned {
            Err(Stale) => self.resync_stale(kind, window, cx),
            Ok(None) => {}
            Ok(Some(edit)) => {
                if self.write_field(field, edit, cx) {
                    self.sync_rows(kind, true, window, cx);
                }
            }
        }
    }

    /// « Ajouter » : une ligne brouillon en bas de la liste, le focus dans son premier champ. Elle s'écrit dès que ses
    /// champs obligatoires sont remplis.
    pub(super) fn add_row(&mut self, kind: ListKind, window: &mut Window, cx: &mut Context<Self>) {
        let first = match self.idema.draft(kind) {
            Some(row) => row.fields[0].clone(),
            None => {
                let row = self.new_row(kind, &[], None, window, cx);
                let first = row.fields[0].clone();
                *self.idema.draft_mut(kind) = Some(row);
                first
            }
        };
        first.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }
}

// ---- Rendu --------------------------------------------------------------------------------------------------------------

/// La colonne « Par défaut » d'une ligne d'agent, et la place des boutons à droite d'une ligne : trois pour un favori,
/// un pour un agent, que le tri range tout seul.
const DEFAULT_COLUMN: f32 = 64.;
const ACTIONS_COLUMN: f32 = 76.;
const AGENT_ACTIONS_COLUMN: f32 = 28.;
const NAME_COLUMN: f32 = 130.;
const HARNESS_COLUMN: f32 = 120.;
const ICON_COLUMN: f32 = 72.;
const FAVORITE_NAME_COLUMN: f32 = 170.;
/// Largeur des champs d'une ligne de réglage.
const NUMBER_WIDTH: f32 = 80.;
const TEXT_WIDTH: f32 = 260.;
const AREA_WIDTH: f32 = 320.;
const AREA_ROWS: f32 = 4.;

impl SettingsView {
    /// Le corps d'une des quatre sections : ses lignes, ou l'erreur de la config avec « Éditer config.toml » quand le fichier
    /// est invalide (aucun contrôle alors).
    pub(super) fn render_idema_section(
        &mut self,
        section: Section,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match self.idema.config.clone() {
            Err(error) => self
                .render_config_error(&error, theme, cx)
                .into_any_element(),
            Ok(config) => match section {
                Section::General => self.general_rows(&config, theme, cx).into_any_element(),
                Section::Agents => self.agent_rows(&config, theme, cx).into_any_element(),
                Section::CommitMessage => self.commit_rows(&config, theme, cx).into_any_element(),
                _ => self.favorite_rows(theme, cx).into_any_element(),
            },
        };
        div()
            .id(("settings-body", section as usize))
            .size_full()
            .overflow_y_scroll()
            .px(theme.px(20.))
            .pb(theme.px(20.))
            .child(content)
            .into_any_element()
    }

    /// Le bouton d'ajout de l'en-tête (« Ajouter un agent », « Ajouter un favori »), s'il y en a un pour `section`.
    pub(super) fn idema_actions(
        &mut self,
        section: Section,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.idema.config.is_err() {
            return None;
        }
        let theme = *cx.global::<Theme>();
        let (kind, text) = match section {
            Section::Agents => (ListKind::Agents, label::ADD_AGENT),
            Section::Browser => (ListKind::Favorites, label::ADD_FAVORITE),
            _ => return None,
        };
        Some(
            button(
                SharedString::from(format!("settings-add-{kind:?}")),
                text,
                Some(Icon::Add),
                true,
                &theme,
            )
            // L'en-tête déplace la fenêtre sur un appui : le bouton garde le sien.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, cx| this.add_row(kind, window, cx)))
            .into_any_element(),
        )
    }

    pub(super) fn render_config_error(
        &self,
        error: &ConfigError,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let Explained { text, detail } = explain_config(error);
        div()
            .flex()
            .flex_col()
            .items_start()
            .gap(theme.px(8.))
            .pt(theme.px(8.))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color(ERROR_TEXT))
                    .child(INVALID_CONFIG),
            )
            .child(div().text_color(white(TEXT_SECONDARY)).child(text))
            .children(detail.map(|raw| div().text_color(white(TEXT_MIN)).child(raw)))
            .child(
                button(
                    "settings-config-error-edit",
                    EDIT_CONFIG,
                    Some(Icon::Description),
                    false,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| this.edit_config_file(cx))),
            )
    }

    pub(super) fn error_of(&self, field: Field) -> Option<String> {
        self.idema.errors.get(&field).cloned()
    }

    /// Une ligne de réglage dont le contrôle est un champ d'une ligne.
    pub(super) fn field_row(
        &self,
        text: &str,
        help: Option<&str>,
        field: Field,
        width: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let Some(input) = self.idema.inputs.input(field) else {
            return div();
        };
        let boxed = field_box(
            SharedString::from(format!("settings-field-{field:?}")),
            input,
            Some(width),
            self.idema.errors.contains_key(&field),
            theme,
            cx,
        );
        setting_row(text, help, boxed, self.error_of(field), theme)
    }

    /// Une ligne de réglage dont le contrôle est un interrupteur.
    pub(super) fn toggle_row(
        &self,
        text: &str,
        field: Field,
        on: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let control = div()
            .id(SharedString::from(format!("settings-toggle-{field:?}")))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.flip(field, cx)))
            .child(toggle(on, theme));
        setting_row(text, None, control, self.error_of(field), theme)
    }

    fn general_rows(&self, config: &Config, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .child(self.field_row(
                label::UI_FONT_SIZE,
                None,
                Field::UiFontSize,
                NUMBER_WIDTH,
                theme,
                cx,
            ))
            .child(self.field_row(
                label::TERMINAL_FONT_FAMILY,
                None,
                Field::TerminalFontFamily,
                TEXT_WIDTH,
                theme,
                cx,
            ))
            .child(self.field_row(
                label::TERMINAL_FONT_SIZE,
                None,
                Field::TerminalFontSize,
                NUMBER_WIDTH,
                theme,
                cx,
            ))
            .child(self.field_row(
                label::TERMINAL_LINE_HEIGHT,
                None,
                Field::TerminalLineHeight,
                NUMBER_WIDTH,
                theme,
                cx,
            ))
            .child(self.toggle_row(
                label::SOUND_ON_WAIT,
                Field::SoundOnWait,
                config.sound_on_wait,
                theme,
                cx,
            ))
            .child(self.field_row(
                label::TMUX_PATH,
                Some(TMUX_HELP),
                Field::TmuxPath,
                TEXT_WIDTH,
                theme,
                cx,
            ))
    }

    fn commit_rows(&self, config: &Config, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let commit = config.commit_message.clone().unwrap_or_default();
        let key_field = Field::ApiKey;
        let key_box = field_box(
            "settings-field-api-key",
            &self.idema.inputs.api_key,
            Some(TEXT_WIDTH - 30.),
            self.idema.errors.contains_key(&key_field),
            theme,
            cx,
        );
        let eye = icon_button(
            "settings-api-key-eye",
            if self.idema.show_key {
                Icon::VisibilityOff
            } else {
                Icon::Visibility
            },
            true,
            theme,
        )
        .on_click(cx.listener(|this, _, window, cx| this.flip_key_visibility(window, cx)));
        let key_control = div()
            .flex()
            .items_center()
            .gap(theme.px(6.))
            .child(key_box)
            .child(eye);
        let area = area_box(
            "settings-field-instructions",
            &self.idema.inputs.instructions,
            AREA_WIDTH,
            AREA_ROWS,
            self.idema.errors.contains_key(&Field::Instructions),
            theme,
            cx,
        );
        div()
            .flex()
            .flex_col()
            .child(self.toggle_row(
                label::COMMIT_ENABLED,
                Field::CommitEnabled,
                commit.enabled,
                theme,
                cx,
            ))
            .child(setting_row(
                label::API_KEY,
                None,
                key_control,
                self.error_of(key_field),
                theme,
            ))
            .child(self.field_row(label::MODEL, None, Field::Model, TEXT_WIDTH, theme, cx))
            .child(self.toggle_row(
                label::INCLUDE_PROJECT_RULES,
                Field::IncludeRules,
                commit.include_project_rules,
                theme,
                cx,
            ))
            .child(setting_row(
                label::INSTRUCTIONS,
                Some(INSTRUCTIONS_HELP),
                area,
                self.error_of(Field::Instructions),
                theme,
            ))
            .child(self.field_row(
                label::ENDPOINT,
                None,
                Field::Endpoint,
                TEXT_WIDTH,
                theme,
                cx,
            ))
    }

    fn agent_rows(&self, config: &Config, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let defaults: Vec<bool> = config.agents.iter().map(|agent| agent.default).collect();
        let mut list = self.list_header(ListKind::Agents, theme);
        list = list.children(
            self.error_of(Field::List(ListKind::Agents))
                .map(|text| error_line(text, theme)),
        );
        list = self.push_agent_groups(list, &defaults, theme, cx);
        if self.idema.agents.is_empty() && self.idema.agent_draft.is_none() {
            list = list.child(empty_line(NO_AGENT, theme));
        }
        list.child(
            div()
                .pt(theme.px(12.))
                .text_size(theme.text(12.))
                .text_color(white(TEXT_MIN))
                .child(SHELL_NOTE),
        )
    }

    fn favorite_rows(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let mut list = self.list_header(ListKind::Favorites, theme);
        list = list.children(
            self.error_of(Field::List(ListKind::Favorites))
                .map(|text| error_line(text, theme)),
        );
        list = self.push_entries(list, ListKind::Favorites, &[], theme, cx);
        if self.idema.favorites.is_empty() && self.idema.favorite_draft.is_none() {
            list = list.child(empty_line(NO_FAVORITE, theme));
        }
        list
    }

    /// Les titres de colonnes d'une liste, alignés sur ses lignes.
    fn list_header(&self, kind: ListKind, theme: &Theme) -> gpui::Div {
        let row = div()
            .flex()
            .items_center()
            .gap(theme.px(8.))
            .h(theme.px(28.))
            .border_b_1()
            .border_color(white(SEPARATOR));
        match kind {
            ListKind::Agents => div().flex().flex_col().child(
                row.child(
                    column_title("Nom", theme)
                        .w(theme.px(NAME_COLUMN))
                        .flex_none(),
                )
                .child(
                    column_title(label::HARNESS, theme)
                        .w(theme.px(HARNESS_COLUMN))
                        .flex_none(),
                )
                .child(column_title("Commande", theme).flex_1())
                .child(
                    column_title("Icône", theme)
                        .w(theme.px(ICON_COLUMN))
                        .flex_none(),
                )
                .child(
                    column_title(label::DEFAULT_AGENT, theme)
                        .w(theme.px(DEFAULT_COLUMN))
                        .flex_none(),
                )
                .child(div().w(theme.px(AGENT_ACTIONS_COLUMN)).flex_none()),
            ),
            ListKind::Favorites => div().flex().flex_col().child(
                row.child(
                    column_title("Nom", theme)
                        .w(theme.px(FAVORITE_NAME_COLUMN))
                        .flex_none(),
                )
                .child(column_title("Adresse", theme).flex_1())
                .child(div().w(theme.px(ACTIONS_COLUMN)).flex_none()),
            ),
        }
    }

    /// Ajoute à `list` les lignes d'agents sous le titre de leur harness, puis celles écrites depuis le dernier classement
    /// et le brouillon éventuel.
    fn push_agent_groups(
        &self,
        mut list: gpui::Div,
        defaults: &[bool],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let kind = ListKind::Agents;
        let rows = &self.idema.agents;
        let total = rows.len();
        let line = |index: usize, cx: &mut Context<Self>| {
            let default = defaults.get(index).copied().unwrap_or(false);
            self.entry_row(kind, &rows[index], Some((index, total)), default, theme, cx)
        };
        let mut placed = HashSet::new();
        for (harness, keys) in &self.idema.agent_groups {
            let title = harness.as_deref().unwrap_or(NO_HARNESS);
            list = list.child(
                column_title(title, theme)
                    .pt(theme.px(12.))
                    .pb(theme.px(2.)),
            );
            for key in keys {
                if let Some(index) = rows.iter().position(|row| row.key == *key) {
                    placed.insert(*key);
                    list = list.child(line(index, cx));
                }
            }
        }
        for (index, row) in rows.iter().enumerate() {
            if !placed.contains(&row.key) {
                list = list.child(line(index, cx));
            }
        }
        if let Some(draft) = &self.idema.agent_draft {
            list = list.child(self.entry_row(kind, draft, None, false, theme, cx));
        }
        list
    }

    /// Ajoute à `list` les lignes de fichier puis le brouillon éventuel.
    fn push_entries(
        &self,
        mut list: gpui::Div,
        kind: ListKind,
        defaults: &[bool],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let rows = self.idema.rows(kind);
        let total = rows.len();
        for (index, row) in rows.iter().enumerate() {
            let default = defaults.get(index).copied().unwrap_or(false);
            list = list.child(self.entry_row(kind, row, Some((index, total)), default, theme, cx));
        }
        if let Some(draft) = self.idema.draft(kind) {
            list = list.child(self.entry_row(kind, draft, None, false, theme, cx));
        }
        list
    }

    /// Une ligne de liste : ses champs, la pastille « Par défaut » (agents), monter et descendre (favoris), supprimer, puis
    /// son erreur. `position` : le rang et le nombre de lignes du fichier, `None` pour le brouillon (ni radio ni réordonner).
    fn entry_row(
        &self,
        kind: ListKind,
        row: &EntryRow,
        position: Option<(usize, usize)>,
        is_default: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let key = row.key;
        let field = Field::Row(kind, key);
        let invalid = self.idema.errors.contains_key(&field);
        let id = |part: &str| SharedString::from(format!("settings-{kind:?}-{key}-{part}"));
        let box_of = |index: usize, width: Option<f32>, part: &str, cx: &mut Context<Self>| {
            field_box(id(part), &row.fields[index], width, invalid, theme, cx)
        };

        let mut line = div().flex().items_center().gap(theme.px(8.));
        match kind {
            ListKind::Agents => {
                line = line
                    .child(box_of(0, Some(NAME_COLUMN), "name", cx))
                    .child(box_of(3, Some(HARNESS_COLUMN), "harness", cx))
                    .child(box_of(1, None, "command", cx))
                    .child(box_of(2, Some(ICON_COLUMN), "icon", cx));
                let radio_cell = div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(theme.px(DEFAULT_COLUMN))
                    .flex_none();
                line = line.child(match position {
                    Some(_) => radio_cell.child(
                        div()
                            .id(id("default"))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.change_entry(kind, key, Change::MakeDefault, window, cx);
                            }))
                            .child(radio(is_default, theme)),
                    ),
                    None => radio_cell,
                });
            }
            ListKind::Favorites => {
                line = line
                    .child(box_of(0, Some(FAVORITE_NAME_COLUMN), "name", cx))
                    .child(box_of(1, None, "address", cx));
            }
        }

        let (can_up, can_down) = match position {
            Some((index, total)) => (index > 0, index + 1 < total),
            None => (false, false),
        };
        let actions_width = match kind {
            ListKind::Agents => AGENT_ACTIONS_COLUMN,
            ListKind::Favorites => ACTIONS_COLUMN,
        };
        let mut actions = div()
            .flex()
            .items_center()
            .gap(theme.px(2.))
            .w(theme.px(actions_width))
            .flex_none();
        for (part, icon, enabled, change) in [
            ("up", Icon::ExpandLess, can_up, Change::Up),
            ("down", Icon::ExpandMore, can_down, Change::Down),
            ("delete", Icon::Delete, true, Change::Delete),
        ] {
            if kind == ListKind::Agents && change != Change::Delete {
                continue;
            }
            let mut button = icon_button(id(part), icon, enabled, theme);
            if enabled {
                button = button.on_click(cx.listener(move |this, _, window, cx| {
                    this.change_entry(kind, key, change, window, cx);
                }));
            }
            actions = actions.child(button);
        }

        div()
            .flex()
            .flex_col()
            .py(theme.px(6.))
            .border_b_1()
            .border_color(white(SEPARATOR))
            .child(line.child(actions))
            .children(self.error_of(field).map(|text| error_line(text, theme)))
    }
}

fn empty_line(text: &str, theme: &Theme) -> gpui::Div {
    div()
        .py(theme.px(12.))
        .text_color(white(TEXT_MIN))
        .child(SharedString::from(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentEntry;

    fn agent(name: &str, default: bool) -> AgentEntry {
        AgentEntry {
            name: name.into(),
            harness: None,
            command: format!("run {name}"),
            default,
            icon: None,
        }
    }

    #[test]
    fn setting_default_clears_the_others() {
        let agents = vec![agent("A", true), agent("B", false), agent("C", false)];
        let updated = with_default(&agents, 2);
        let flags: Vec<bool> = updated.iter().map(|entry| entry.default).collect();
        assert_eq!(flags, vec![false, false, true]);
        assert_eq!(updated[2].name, "C");
        // Un index hors liste ne désigne personne : plus aucun agent par défaut n'est inventé.
        assert!(with_default(&agents, 9).iter().all(|entry| !entry.default));
    }

    #[test]
    fn moving_an_entry_swaps_neighbours() {
        assert_eq!(move_entry(vec!['a', 'b', 'c'], 0, 1), vec!['b', 'a', 'c']);
        assert_eq!(move_entry(vec!['a', 'b', 'c'], 2, 1), vec!['a', 'c', 'b']);
        // Les bornes tiennent : le premier ne monte pas, le dernier ne descend pas.
        assert_eq!(move_entry(vec!['a', 'b', 'c'], 0, 0), vec!['a', 'b', 'c']);
        assert_eq!(move_entry(vec!['a', 'b', 'c'], 2, 3), vec!['a', 'b', 'c']);
        assert_eq!(move_entry(vec!['a', 'b', 'c'], 7, 1), vec!['a', 'b', 'c']);
    }

    #[test]
    fn numeric_field_parses_french_and_dot_decimals() {
        assert_eq!(parse_number("1,55"), Some(1.55));
        assert_eq!(parse_number("1.55"), Some(1.55));
        assert_eq!(parse_number(" 16 "), Some(16.));
        assert_eq!(parse_number("abc"), None);
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("NaN"), None);
    }

    #[test]
    fn a_row_edit_applies_only_to_the_entry_it_was_built_from() {
        let disk = vec!['a', 'b', 'c'];
        // L'entrée au rang 1 est bien celle de la ligne : chaque geste rend la liste attendue.
        assert_eq!(
            plan_row_edit(&disk, 1, &'b', RowChange::Replace('x')),
            Ok(vec!['a', 'x', 'c'])
        );
        assert_eq!(
            plan_row_edit(&disk, 1, &'b', RowChange::Delete),
            Ok(vec!['a', 'c'])
        );
        assert_eq!(
            plan_row_edit(&disk, 1, &'b', RowChange::Up),
            Ok(vec!['b', 'a', 'c'])
        );
        assert_eq!(
            plan_row_edit(&disk, 1, &'b', RowChange::Down),
            Ok(vec!['a', 'c', 'b'])
        );
        assert_eq!(
            plan_row_edit(&disk, 1, &'b', RowChange::Keep),
            Ok(disk.clone())
        );
        // Les bornes tiennent : la liste revient telle quelle, l'appelant n'écrit rien.
        assert_eq!(
            plan_row_edit(&disk, 0, &'a', RowChange::Up),
            Ok(disk.clone())
        );
        assert_eq!(
            plan_row_edit(&disk, 2, &'c', RowChange::Down),
            Ok(disk.clone())
        );
    }

    #[test]
    fn a_row_edit_refuses_a_list_that_changed_underneath() {
        let disk = vec!['a', 'b', 'c'];
        // Une autre entrée à ce rang : rien n'est touché, quel que soit le geste.
        for change in [
            RowChange::Replace('x'),
            RowChange::Delete,
            RowChange::Up,
            RowChange::Down,
            RowChange::Keep,
        ] {
            assert_eq!(plan_row_edit(&disk, 1, &'z', change), Err(Stale));
        }
        // Une entrée supprimée ailleurs ne ressuscite pas : le rang sort de la liste.
        assert_eq!(
            plan_row_edit(&disk, 3, &'d', RowChange::Replace('x')),
            Err(Stale)
        );
        assert_eq!(
            plan_row_edit(&[] as &[char], 0, &'a', RowChange::Delete),
            Err(Stale)
        );
    }

    #[test]
    fn numbers_read_back_as_the_file_writes_them() {
        assert_eq!(number_text(16.), "16");
        assert_eq!(number_text(1.55), "1.55");
        assert_eq!(parse_number(&number_text(1.55)), Some(1.55));
    }
}
