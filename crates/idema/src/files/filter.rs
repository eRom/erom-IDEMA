//! Le modèle du champ du volet Fichiers et de ses listes (P2 jalon 3) : le mode du champ (préfixe `?`), les fichiers
//! filtrés et classés, les lignes affichées avec leur sélection au clavier, les cases du remplacement, l'extrait d'une
//! ligne et les libellés français. Aucune dépendance à GPUI : tout se teste sans fenêtre.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::files::fuzzy::rank;
use crate::files::project_search::{
    FileReplace, LineMatch, LineTarget, ReplaceError, SearchOutcome,
};

/// Statut affiché pendant une recherche.
pub const SEARCHING: &str = "Recherche…";
/// Statut affiché pendant un remplacement.
pub const REPLACING: &str = "Remplacement…";
/// Statut du filtre quand le projet n'a aucun fichier.
pub const NO_FILE: &str = "Aucun fichier.";
/// Statut quand le filtre ou la recherche ne trouve rien.
pub const NO_RESULT: &str = "Aucun résultat.";
/// Statut quand l'expression régulière ne compile pas.
pub const INVALID_REGEX: &str = "Expression régulière invalide.";
/// Statut quand `git ls-files` échoue.
pub const GIT_FAILED: &str = "Git n'a pas pu lister les fichiers du projet.";
/// Échec d'un fichier dont des lignes ont changé depuis la recherche.
pub const STALE_LINES: &str = "Des lignes ont changé depuis la recherche.";

/// Le mode du champ, déduit de son seul texte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldMode<'a> {
    /// Texte vide ou de blancs : l'arbre.
    Tree,
    /// Le filtre des fichiers par chemin flou.
    Filter(&'a str),
    /// La recherche dans le contenu : le texte après le `?`.
    Search(&'a str),
}

/// `?` en premier caractère (sans blanc devant) bascule en recherche, même sans texte derrière.
pub fn field_mode(text: &str) -> FieldMode<'_> {
    if text.trim().is_empty() {
        FieldMode::Tree
    } else if let Some(rest) = text.strip_prefix('?') {
        FieldMode::Search(rest)
    } else {
        FieldMode::Filter(text)
    }
}

/// Un fichier retenu par le filtre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterHit {
    pub path: PathBuf,
    /// Le chemin relatif à la racine, séparé par `/`.
    pub relative: String,
    /// Les décalages en octets des caractères trouvés dans `relative`.
    pub positions: Vec<usize>,
}

/// Le chemin de `path` relatif à `root`, séparé par `/`. Hors de `root`, le chemin entier.
fn relative_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Les fichiers qui correspondent à `query`, du meilleur au moins bon, classés sur leur chemin relatif à `root`.
pub fn filter_hits(root: &Path, files: &[PathBuf], query: &str) -> Vec<FilterHit> {
    let relatives: Vec<String> = files.iter().map(|file| relative_path(root, file)).collect();
    let candidates: Vec<&str> = relatives.iter().map(String::as_str).collect();
    rank(query, &candidates)
        .into_iter()
        .map(|(index, found)| FilterHit {
            path: files[index].clone(),
            relative: relatives[index].clone(),
            positions: found.positions,
        })
        .collect()
}

/// Un résultat du filtre découpé pour l'affichage : le nom, puis le dossier en retrait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitParts {
    pub name: String,
    /// Positions dans `name`, en octets.
    pub name_positions: Vec<usize>,
    /// Le dossier, vide à la racine.
    pub dir: String,
    /// Positions dans `dir`, en octets.
    pub dir_positions: Vec<usize>,
}

/// Sépare le nom (après le dernier `/`) du dossier (avant), et ramène les positions à chaque partie. Le `/` lui-même
/// n'appartient à aucune des deux.
pub fn split_hit(hit: &FilterHit) -> HitParts {
    let (dir, name_start) = match hit.relative.rfind('/') {
        Some(slash) => (&hit.relative[..slash], slash + 1),
        None => ("", 0),
    };
    let name = &hit.relative[name_start..];
    let name_positions = hit
        .positions
        .iter()
        .filter(|&&position| position >= name_start)
        .map(|&position| position - name_start)
        .collect();
    let dir_positions = hit
        .positions
        .iter()
        .copied()
        .filter(|&position| position < dir.len())
        .collect();
    HitParts {
        name: name.to_string(),
        name_positions,
        dir: dir.to_string(),
        dir_positions,
    }
}

/// Une ligne de la liste affichée sous le champ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultItem {
    /// Le `n`-ième fichier du filtre.
    Hit(usize),
    /// L'en-tête du `n`-ième fichier d'une recherche.
    File(usize),
    /// Une ligne trouvée : l'indice du fichier, puis celui de la ligne dans ce fichier.
    Line { file: usize, line: usize },
}

impl ResultItem {
    /// Une sélection tombe sur un résultat du filtre ou une ligne trouvée, jamais sur un en-tête de fichier.
    pub fn is_selectable(self) -> bool {
        matches!(self, ResultItem::Hit(_) | ResultItem::Line { .. })
    }
}

/// La liste d'une recherche : chaque fichier, suivi de ses lignes.
pub fn result_items(outcome: &SearchOutcome) -> Vec<ResultItem> {
    let mut items = Vec::new();
    for (file, matches) in outcome.files.iter().enumerate() {
        items.push(ResultItem::File(file));
        items.extend((0..matches.lines.len()).map(|line| ResultItem::Line { file, line }));
    }
    items
}

/// L'indice du premier élément sélectionnable.
pub fn first_selectable(items: &[ResultItem]) -> Option<usize> {
    items.iter().position(|item| item.is_selectable())
}

/// La sélection après `↓` (`down`) ou `↑` : le sélectionnable suivant ou précédent, qui s'arrête aux bouts sans boucler.
/// Sans sélection, le premier.
pub fn move_selection(items: &[ResultItem], current: Option<usize>, down: bool) -> Option<usize> {
    let Some(current) = current else {
        return first_selectable(items);
    };
    let target = if down {
        (current + 1..items.len()).find(|&index| items[index].is_selectable())
    } else {
        (0..current.min(items.len()))
            .rev()
            .find(|&index| items[index].is_selectable())
    };
    target.or(Some(current))
}

/// L'indice, dans `hits`, du fichier `previous` s'il est encore listé : une liste refaite pour la même question (le
/// disque a changé) garde la sélection sur son fichier au lieu de la renvoyer à la première ligne.
pub fn kept_hit(previous: Option<&Path>, hits: &[FilterHit]) -> Option<usize> {
    let previous = previous?;
    hits.iter().position(|hit| hit.path == previous)
}

/// L'indice, dans `items`, de la ligne `previous` (fichier et numéro de ligne) si la recherche refaite la trouve encore.
pub fn kept_line(
    previous: Option<(&Path, usize)>,
    items: &[ResultItem],
    outcome: &SearchOutcome,
) -> Option<usize> {
    let (path, number) = previous?;
    items.iter().position(|item| match *item {
        ResultItem::Line { file, line } => outcome.files.get(file).is_some_and(|found| {
            found.path == path && found.lines.get(line).is_some_and(|l| l.line == number)
        }),
        _ => false,
    })
}

/// Le statut d'une recherche arrivée : le décompte, l'aveu de la limite, ou « Aucun résultat. ».
pub fn search_status(outcome: &SearchOutcome, limit: usize) -> String {
    if outcome.truncated {
        truncated_label(limit)
    } else if outcome.files.is_empty() {
        NO_RESULT.to_string()
    } else {
        result_count_label(outcome.line_count(), outcome.files.len())
    }
}

/// Où ouvrir une ligne trouvée : sa ligne et la colonne de sa première occurrence, à partir de 1 (1 + le nombre de
/// caractères qui la précèdent, pas d'octets).
pub fn open_position(line: &LineMatch) -> (u32, u32) {
    let before = line
        .ranges
        .first()
        .map_or(0, |range| line.text[..range.start].chars().count());
    (
        u32::try_from(line.line).unwrap_or(u32::MAX),
        u32::try_from(before + 1).unwrap_or(u32::MAX),
    )
}

/// Les lignes décochées du remplacement : toute ligne trouvée commence cochée, d'où le stockage de l'exception.
#[derive(Debug, Default, Clone)]
pub struct Unchecked(HashSet<(PathBuf, usize)>);

impl Unchecked {
    /// Coche une ligne décochée, décoche une ligne cochée.
    pub fn toggle(&mut self, path: &Path, line: usize) {
        let key = (path.to_path_buf(), line);
        if !self.0.remove(&key) {
            self.0.insert(key);
        }
    }

    pub fn is_checked(&self, path: &Path, line: usize) -> bool {
        !self.0.contains(&(path.to_path_buf(), line))
    }

    /// Le nombre de lignes cochées du résultat.
    pub fn checked_count(&self, outcome: &SearchOutcome) -> usize {
        outcome
            .files
            .iter()
            .flat_map(|file| file.lines.iter().map(|line| (&file.path, line.line)))
            .filter(|(path, line)| self.is_checked(path, *line))
            .count()
    }

    /// Les lignes cochées, groupées par fichier dans l'ordre des résultats ; un fichier sans ligne cochée n'y figure pas.
    /// Chaque groupe porte le nombre de lignes que le fichier avait à la recherche.
    pub fn targets(&self, outcome: &SearchOutcome) -> Vec<FileTargets> {
        outcome
            .files
            .iter()
            .filter_map(|file| {
                let lines: Vec<LineTarget> = file
                    .lines
                    .iter()
                    .filter(|line| self.is_checked(&file.path, line.line))
                    .map(|line| LineTarget {
                        line: line.line,
                        original: line.text.clone(),
                    })
                    .collect();
                (!lines.is_empty()).then(|| FileTargets {
                    path: file.path.clone(),
                    line_count: file.line_count,
                    lines,
                })
            })
            .collect()
    }

    /// Reporte les lignes décochées sur le résultat `next` d'une recherche refaite (après un remplacement). Les cases
    /// sont gardées par chemin et numéro de ligne, ce qui ne vaut que tant que les lignes n'ont pas bougé : un fichier
    /// qui a décoché des lignes les garde si son nombre de lignes est celui du résultat d'avant (`previous`), sinon des
    /// lignes ont été insérées ou retirées et toutes ses lignes reviennent décochées (on ne recoche jamais ce que
    /// l'utilisateur a pu décocher). Les cases d'un fichier absent de `next` s'oublient.
    pub fn carry_over(&mut self, previous: &SearchOutcome, next: &SearchOutcome) {
        let old = std::mem::take(&mut self.0);
        for file in &next.files {
            if !old.iter().any(|(path, _)| *path == file.path) {
                continue;
            }
            let unchanged = previous
                .files
                .iter()
                .any(|found| found.path == file.path && found.line_count == file.line_count);
            for line in &file.lines {
                let key = (file.path.clone(), line.line);
                if !unchanged || old.contains(&key) {
                    self.0.insert(key);
                }
            }
        }
    }
}

/// Un travail qui ne tourne qu'une fois à la fois (le listage `git ls-files`) : une demande pendant qu'il tourne n'en
/// lance pas un second, elle note qu'il faut le refaire, une seule fois, à sa fin. Comme `refresh_ignored`
/// (`ignored_running` / `ignored_redo`) : un agent qui écrit sans cesse n'empile pas les processus.
#[derive(Debug, Default, Clone, Copy)]
pub struct SingleFlight {
    running: bool,
    redo: bool,
}

impl SingleFlight {
    /// Une demande de travail. Vrai : rien ne tourne, l'appelant le lance maintenant. Faux : il tourne déjà, il sera refait.
    pub fn request(&mut self) -> bool {
        if self.running {
            self.redo = true;
            return false;
        }
        self.running = true;
        true
    }

    /// Le travail est fini. Vrai : une demande est arrivée pendant qu'il tournait, l'appelant le relance.
    pub fn finish(&mut self) -> bool {
        self.running = false;
        std::mem::take(&mut self.redo)
    }

    /// Abandonne le travail en cours et sa demande de refaire : son résultat sera jeté (l'appelant change d'époque).
    pub fn cancel(&mut self) {
        self.running = false;
        self.redo = false;
    }

    pub fn is_running(&self) -> bool {
        self.running
    }
}

/// Le remplacement que le bouton vient de faire : tant qu'il est « consommé », « Remplacer » reste inactif. La recherche
/// refaite après un remplacement retrouve des lignes cochées quand le remplacement contient encore le motif
/// (`Config` en `ConfigV2`) : sans cela un double clic l'appliquerait deux fois. Il se réarme quand la requête, une
/// option ou le texte de « Remplacer par » change.
#[derive(Debug, Default, Clone)]
pub struct SpentReplace(Option<String>);

impl SpentReplace {
    /// Un remplacement part avec ce texte de « Remplacer par ».
    pub fn spend(&mut self, replacement: &str) {
        self.0 = Some(replacement.to_string());
    }

    /// La requête ou une option a changé : le prochain résultat est une autre question.
    pub fn rearm(&mut self) {
        self.0 = None;
    }

    /// Le texte de « Remplacer par » est maintenant `typed` (un événement qui ne le change pas ne réarme rien).
    pub fn replacement_edited(&mut self, typed: &str) {
        if self.0.as_deref() != Some(typed) {
            self.0 = None;
        }
    }

    pub fn is_spent(&self) -> bool {
        self.0.is_some()
    }
}

/// Le bouton « Remplacer » est actif quand ni recherche ni remplacement ne sont en cours, que le dernier remplacement
/// n'est pas encore « consommé », et qu'au moins une ligne du résultat affiché est cochée.
pub fn replace_enabled(
    searching: bool,
    replacing: bool,
    spent: &SpentReplace,
    checked: usize,
) -> bool {
    !searching && !replacing && !spent.is_spent() && checked > 0
}

/// Un appui sur « Remplacer » n'agit qu'au premier clic d'une série : un double clic ne rejoue pas le remplacement.
pub fn is_replace_press(click_count: usize) -> bool {
    click_count <= 1
}

/// Les lignes cochées d'un fichier, avec le nombre de lignes qu'il avait à la recherche : ce que le remplacement reçoit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTargets {
    pub path: PathBuf,
    pub line_count: usize,
    pub lines: Vec<LineTarget>,
}

/// Le nombre de caractères qui précèdent une ligne trouvée dans son extrait.
pub const SNIPPET_LEAD: usize = 8;

/// L'extrait affiché d'une ligne trouvée : blancs de tête retirés ; si la première occurrence commence plus de `lead`
/// caractères après, le texte garde les `lead` caractères qui la précèdent, derrière « … ». Les plages (en octets, sur
/// des limites de caractère) sont décalées d'autant ; une plage qui tombait dans ce qui est retiré est écartée.
pub fn snippet(line: &str, ranges: &[Range<usize>], lead: usize) -> (String, Vec<Range<usize>>) {
    const ELLIPSIS: &str = "…";
    let trimmed = line.trim_start();
    let mut cut = line.len() - trimmed.len();
    let mut prefix = "";
    if let Some(first) = ranges.iter().map(|range| range.start).min() {
        let first = first.max(cut);
        let skipped = line[cut..first].chars().count();
        if skipped > lead {
            // Le début du texte gardé : `lead` caractères avant la première occurrence.
            cut = match lead {
                0 => first,
                _ => line[..first]
                    .char_indices()
                    .rev()
                    .nth(lead - 1)
                    .map_or(first, |(offset, _)| offset),
            };
            prefix = ELLIPSIS;
        }
    }
    let shift = |offset: usize| offset - cut + prefix.len();
    let shifted = ranges
        .iter()
        .filter(|range| range.end > cut)
        .map(|range| shift(range.start.max(cut))..shift(range.end))
        .collect();
    (format!("{prefix}{}", &line[cut..]), shifted)
}

/// « 1 ligne dans 1 fichier », « 7 lignes dans 3 fichiers ».
pub fn result_count_label(lines: usize, files: usize) -> String {
    format!(
        "{lines}\u{a0}{} dans {files}\u{a0}{}",
        plural(lines, "ligne", "lignes"),
        plural(files, "fichier", "fichiers")
    )
}

/// Le statut d'une recherche arrêtée à la limite de lignes.
pub fn truncated_label(limit: usize) -> String {
    format!("Plus de {limit}\u{a0}lignes, seules les {limit}\u{a0}premières sont listées.")
}

/// Le bilan d'un remplacement ; le singulier vaut pour 0 et 1, et les échecs ne sont dits que s'il y en a.
pub fn replace_summary(files: usize, occurrences: usize, failed: usize) -> String {
    let mut summary = format!(
        "{files}\u{a0}{} {}, {occurrences}\u{a0}{} {}.",
        plural(files, "fichier", "fichiers"),
        plural(files, "modifié", "modifiés"),
        plural(occurrences, "occurrence", "occurrences"),
        plural(occurrences, "remplacée", "remplacées"),
    );
    if failed > 0 {
        summary.push_str(&format!(
            " {failed}\u{a0}{} en échec.",
            plural(failed, "fichier", "fichiers")
        ));
    }
    summary
}

/// L'échec d'un fichier au remplacement : le message affiché sous son nom, l'étiquette du journal (`reason=…`) et, pour
/// une panne d'écriture, le brut de l'erreur (au journal seulement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub reason: &'static str,
    pub detail: Option<String>,
}

/// Ce que le remplacement a fait d'un fichier : ses occurrences remplacées (il peut en avoir eu et échouer quand même)
/// et son échec s'il y en a un.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutcome {
    pub path: PathBuf,
    pub occurrences: usize,
    pub failure: Option<Failure>,
}

/// Le bilan d'un fichier. Des lignes changées depuis la recherche (`stale_lines`) font un échec, même quand d'autres
/// lignes du fichier ont été remplacées ; une `ReplaceError` en est un aussi, sans occurrence.
pub fn file_outcome(path: PathBuf, result: Result<FileReplace, ReplaceError>) -> FileOutcome {
    match result {
        Ok(done) => FileOutcome {
            path,
            occurrences: done.occurrences,
            failure: (done.stale_lines > 0).then(|| Failure {
                message: STALE_LINES.to_string(),
                reason: "stale",
                detail: None,
            }),
        },
        Err(error) => FileOutcome {
            path,
            occurrences: 0,
            failure: Some(Failure {
                message: error.message(),
                reason: error.reason(),
                detail: match &error {
                    ReplaceError::Io(io) => Some(io.to_string()),
                    ReplaceError::Changed | ReplaceError::NotText => None,
                },
            }),
        },
    }
}

/// Ce que la sauvegarde d'un onglet remplacé a réellement fait (`FilesPanel::save_tab_with`).
#[derive(Debug)]
pub enum TabSave {
    /// Le disque porte maintenant le texte de l'onglet.
    Written,
    /// L'écriture a été refusée : le texte reste dans l'onglet, marqué « non sauvegardé ».
    Failed(std::io::Error),
    /// Rien à écrire : le texte de l'onglet est celui que le disque porte déjà (un remplacement qui rend le même texte).
    Unchanged,
    /// Le disque a changé sous la sauvegarde (l'onglet se recharge, le disque gagne), ou rien n'a pu être écrit.
    DiskChanged,
}

/// Ce que rend la sauvegarde d'un onglet qui n'a rien en attente (`needs_save` faux). Le disque porte déjà son texte :
/// `Unchanged`. Si le texte de l'éditeur n'est pas celui que le disque a (`buffer_is_on_disk` faux), la sauvegarde n'a
/// rien écrit alors que le texte attend de l'être : le remplacement ne peut pas dire « écrit », c'est `DiskChanged`.
pub fn idle_save(buffer_is_on_disk: bool) -> TabSave {
    if buffer_is_on_disk {
        TabSave::Unchanged
    } else {
        TabSave::DiskChanged
    }
}

/// Le bilan d'un fichier ouvert dans un onglet, décidé après sa sauvegarde : seul ce que le disque a reçu compte. Écrit :
/// les occurrences remplacées (et l'échec des lignes périmées s'il y en a) ; refus d'écriture : un échec au message
/// d'`explain_write` ; disque changé : l'échec « a changé pendant le remplacement ». Rien à écrire (le disque porte déjà
/// le texte) compte comme écrit. Aucune occurrence n'est comptée pour un fichier dont l'écriture a échoué.
pub fn tab_outcome(path: PathBuf, replaced: FileReplace, saved: TabSave) -> FileOutcome {
    match saved {
        TabSave::Written | TabSave::Unchanged => file_outcome(path, Ok(replaced)),
        TabSave::Failed(error) => file_outcome(path, Err(ReplaceError::Io(error))),
        TabSave::DiskChanged => file_outcome(path, Err(ReplaceError::Changed)),
    }
}

/// Les trois nombres du bilan : fichiers modifiés (au moins une occurrence remplacée), occurrences remplacées, fichiers
/// en échec. Un fichier à moitié remplacé compte dans le premier et dans le dernier.
pub fn totals(outcomes: &[FileOutcome]) -> (usize, usize, usize) {
    let files = outcomes.iter().filter(|file| file.occurrences > 0).count();
    let occurrences = outcomes.iter().map(|file| file.occurrences).sum();
    let failed = outcomes
        .iter()
        .filter(|file| file.failure.is_some())
        .count();
    (files, occurrences, failed)
}

/// Le singulier pour 0 et 1, le pluriel au-delà (accord du français).
fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count > 1 { plural } else { singular }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::project_search::{FileMatches, LineMatch};

    /// Des plages d'octets écrites en paires (début, fin).
    fn spans(pairs: &[(usize, usize)]) -> Vec<Range<usize>> {
        pairs.iter().map(|&(start, end)| start..end).collect()
    }

    fn line(number: usize, text: &str) -> LineMatch {
        LineMatch {
            line: number,
            text: text.to_string(),
            ranges: spans(&[(0, 1)]),
        }
    }

    /// Un résultat de deux fichiers : `a.md` (lignes 2 et 5) et `b.md` (ligne 1).
    fn outcome() -> SearchOutcome {
        SearchOutcome {
            files: vec![
                FileMatches {
                    path: PathBuf::from("/p/a.md"),
                    lines: vec![line(2, "alpha"), line(5, "beta")],
                    line_count: 6,
                },
                FileMatches {
                    path: PathBuf::from("/p/b.md"),
                    lines: vec![line(1, "gamma")],
                    line_count: 3,
                },
            ],
            truncated: false,
        }
    }

    #[test]
    fn the_question_mark_prefix_switches_to_search() {
        assert_eq!(field_mode(""), FieldMode::Tree);
        assert_eq!(field_mode("   "), FieldMode::Tree);
        assert_eq!(field_mode("pan"), FieldMode::Filter("pan"));
        assert_eq!(field_mode("?foo"), FieldMode::Search("foo"));
        assert_eq!(field_mode("?"), FieldMode::Search(""));
        assert_eq!(field_mode(" ?foo"), FieldMode::Filter(" ?foo"));
    }

    #[test]
    fn hits_rank_relative_paths_and_split_the_name_from_its_folder() {
        let root = Path::new("/p");
        let files = vec![
            PathBuf::from("/p/src/special.rs"),
            PathBuf::from("/p/docs/spec.md"),
        ];
        let hits = filter_hits(root, &files, "spec");
        assert_eq!(hits[0].relative, "docs/spec.md");
        assert_eq!(hits[0].path, PathBuf::from("/p/docs/spec.md"));
        let parts = split_hit(&hits[0]);
        assert_eq!(parts.name, "spec.md");
        assert_eq!(parts.name_positions, vec![0, 1, 2, 3]);
        assert_eq!(parts.dir, "docs");
        assert!(parts.dir_positions.is_empty());
    }

    #[test]
    fn a_hit_at_the_root_has_no_folder_and_a_folder_match_keeps_its_positions() {
        let root = Path::new("/p");
        let top = filter_hits(root, &[PathBuf::from("/p/README.md")], "read");
        let parts = split_hit(&top[0]);
        assert_eq!(parts.dir, "");
        assert_eq!(parts.name, "README.md");
        assert!(parts.dir_positions.is_empty());

        let nested = filter_hits(root, &[PathBuf::from("/p/docs/x.md")], "dx");
        let parts = split_hit(&nested[0]);
        assert_eq!(parts.dir_positions, vec![0]);
        assert_eq!(parts.name_positions, vec![0]);
    }

    #[test]
    fn selection_moves_over_selectable_items_and_stops_at_the_ends() {
        let items = [
            ResultItem::File(0),
            ResultItem::Line { file: 0, line: 0 },
            ResultItem::Line { file: 0, line: 1 },
            ResultItem::File(1),
            ResultItem::Line { file: 1, line: 0 },
        ];
        assert_eq!(first_selectable(&items), Some(1));
        let mut current = move_selection(&items, None, true);
        assert_eq!(current, Some(1));
        current = move_selection(&items, current, true);
        assert_eq!(current, Some(2));
        current = move_selection(&items, current, true);
        assert_eq!(current, Some(4));
        current = move_selection(&items, current, true);
        assert_eq!(current, Some(4));
        current = move_selection(&items, current, false);
        assert_eq!(current, Some(2));
        current = move_selection(&items, current, false);
        assert_eq!(current, Some(1));
        current = move_selection(&items, current, false);
        assert_eq!(current, Some(1));
        assert_eq!(move_selection(&[], None, true), None);
        assert_eq!(move_selection(&items[..1], None, false), None);
    }

    #[test]
    fn result_items_list_each_file_then_its_lines() {
        assert_eq!(
            result_items(&outcome()),
            vec![
                ResultItem::File(0),
                ResultItem::Line { file: 0, line: 0 },
                ResultItem::Line { file: 0, line: 1 },
                ResultItem::File(1),
                ResultItem::Line { file: 1, line: 0 },
            ]
        );
        assert!(ResultItem::Hit(3).is_selectable());
        assert!(!ResultItem::File(0).is_selectable());
    }

    #[test]
    fn every_line_starts_checked_and_a_toggle_unchecks_it() {
        let outcome = outcome();
        let a = Path::new("/p/a.md");
        let b = Path::new("/p/b.md");
        let mut unchecked = Unchecked::default();
        assert_eq!(unchecked.checked_count(&outcome), 3);
        let all = unchecked.targets(&outcome);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].path, a);
        assert_eq!(all[0].line_count, 6);
        assert_eq!(all[1].line_count, 3);
        assert_eq!(
            all[0].lines,
            vec![
                LineTarget {
                    line: 2,
                    original: "alpha".to_string()
                },
                LineTarget {
                    line: 5,
                    original: "beta".to_string()
                },
            ]
        );

        unchecked.toggle(a, 2);
        assert!(!unchecked.is_checked(a, 2));
        assert!(unchecked.is_checked(a, 5));
        assert_eq!(unchecked.checked_count(&outcome), 2);
        assert_eq!(unchecked.targets(&outcome)[0].lines.len(), 1);

        unchecked.toggle(a, 2);
        assert!(unchecked.is_checked(a, 2));
        assert_eq!(unchecked.checked_count(&outcome), 3);

        unchecked.toggle(b, 1);
        let targets = unchecked.targets(&outcome);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].path, a);
        assert_eq!(unchecked.checked_count(&outcome), 2);
    }

    /// Un résultat à un seul fichier de lignes jumelles : `- foo` aux lignes 2 et 3, sur `lines` lignes.
    fn twins(lines: usize) -> SearchOutcome {
        SearchOutcome {
            files: vec![FileMatches {
                path: PathBuf::from("/p/a.md"),
                lines: vec![line(2, "- foo"), line(3, "- foo")],
                line_count: lines,
            }],
            truncated: false,
        }
    }

    #[test]
    fn unchecked_lines_survive_a_rerun_when_the_file_kept_its_line_count() {
        let a = Path::new("/p/a.md");
        let before = twins(4);
        let mut unchecked = Unchecked::default();
        unchecked.toggle(a, 3);

        unchecked.carry_over(&before, &twins(4));

        assert!(unchecked.is_checked(a, 2));
        assert!(!unchecked.is_checked(a, 3));
    }

    #[test]
    fn a_file_whose_line_count_changed_comes_back_fully_unchecked() {
        // `- foo` aux lignes 2 et 3, la 3 décochée ; un agent retire la ligne 1 : la recherche refaite trouve les
        // jumelles aux lignes 1 et 2, et la case de la ligne 3 ne désigne plus la même ligne.
        let a = Path::new("/p/a.md");
        let before = twins(4);
        let mut unchecked = Unchecked::default();
        unchecked.toggle(a, 3);
        let after = SearchOutcome {
            files: vec![FileMatches {
                path: a.to_path_buf(),
                lines: vec![line(1, "- foo"), line(2, "- foo")],
                line_count: 3,
            }],
            truncated: false,
        };

        unchecked.carry_over(&before, &after);

        assert_eq!(unchecked.checked_count(&after), 0);
        assert!(unchecked.targets(&after).is_empty());
    }

    #[test]
    fn a_rerun_forgets_the_files_it_no_longer_finds_and_leaves_untouched_files_checked() {
        let a = Path::new("/p/a.md");
        let b = Path::new("/p/b.md");
        let before = outcome();
        let mut unchecked = Unchecked::default();
        unchecked.toggle(a, 2);
        // Le second fichier change de longueur, mais n'avait aucune case décochée : il reste coché.
        let after = SearchOutcome {
            files: vec![FileMatches {
                path: b.to_path_buf(),
                lines: vec![line(1, "gamma")],
                line_count: 9,
            }],
            truncated: false,
        };

        unchecked.carry_over(&before, &after);

        assert_eq!(unchecked.checked_count(&after), 1);
        assert!(unchecked.is_checked(a, 2));
    }

    #[test]
    fn a_request_during_a_run_marks_one_redo_instead_of_a_second_run() {
        let mut listing = SingleFlight::default();
        assert!(!listing.is_running());

        assert!(listing.request());
        assert!(listing.is_running());

        // Dix lots du disque pendant le listage : aucun listage de plus, un seul à refaire.
        for _ in 0..10 {
            assert!(!listing.request());
        }
        assert!(listing.finish());
        assert!(!listing.is_running());

        // La reprise est un travail comme un autre : sans nouvelle demande, rien à refaire à sa fin.
        assert!(listing.request());
        assert!(!listing.finish());
    }

    #[test]
    fn a_cancel_drops_the_running_work_and_its_redo() {
        let mut listing = SingleFlight::default();
        assert!(listing.request());
        assert!(!listing.request());
        listing.cancel();
        assert!(!listing.is_running());
        assert!(listing.request());
        assert!(!listing.finish());
    }

    #[test]
    fn a_spent_replace_stays_disabled_until_the_replacement_text_changes() {
        let mut spent = SpentReplace::default();
        assert!(replace_enabled(false, false, &spent, 2));

        spent.spend("bar");
        assert!(!replace_enabled(false, false, &spent, 2));

        // Un événement qui ne change pas le texte (une chasse arrière sans effet) ne réarme pas.
        spent.replacement_edited("bar");
        assert!(!replace_enabled(false, false, &spent, 2));

        spent.replacement_edited("barx");
        assert!(replace_enabled(false, false, &spent, 2));
    }

    #[test]
    fn a_spent_replace_is_rearmed_by_a_new_question() {
        let mut spent = SpentReplace::default();
        spent.spend("bar");

        spent.rearm();

        assert!(replace_enabled(false, false, &spent, 1));
    }

    #[test]
    fn the_replace_button_needs_an_idle_panel_and_a_checked_line() {
        let spent = SpentReplace::default();
        assert!(!replace_enabled(true, false, &spent, 1));
        assert!(!replace_enabled(false, true, &spent, 1));
        assert!(!replace_enabled(false, false, &spent, 0));
    }

    #[test]
    fn only_the_first_press_of_a_click_series_replaces() {
        assert!(is_replace_press(1));
        assert!(!is_replace_press(2));
        assert!(!is_replace_press(3));
    }

    #[test]
    fn a_snippet_starts_a_little_before_the_match() {
        assert_eq!(
            snippet("    foo", &spans(&[(4, 7)]), 8),
            ("foo".to_string(), spans(&[(0, 3)]))
        );
        assert_eq!(
            snippet("abcdefghijklmnop foo", &spans(&[(17, 20)]), 4),
            ("…nop foo".to_string(), spans(&[(7, 10)]))
        );
        assert_eq!(
            snippet("ééééé foo", &spans(&[(11, 14)]), 2),
            ("…é foo".to_string(), spans(&[(6, 9)]))
        );
    }

    #[test]
    fn a_snippet_keeps_every_range_on_a_character_boundary() {
        let (text, ranges) = snippet("  ééééééééééé foo foo", &spans(&[(25, 28), (29, 32)]), 3);
        assert_eq!(text, "…éé foo foo");
        assert_eq!(&text[ranges[0].clone()], "foo");
        assert_eq!(&text[ranges[1].clone()], "foo");
        let (text, ranges) = snippet("no match here", &[], 3);
        assert_eq!(text, "no match here");
        assert!(ranges.is_empty());
    }

    #[test]
    fn a_rebuilt_list_keeps_the_selected_file_while_it_is_still_listed() {
        let hit = |relative: &str| FilterHit {
            path: PathBuf::from("/p").join(relative),
            relative: relative.to_string(),
            positions: Vec::new(),
        };
        let hits = [hit("a.md"), hit("docs/b.md"), hit("c.md")];
        assert_eq!(kept_hit(Some(Path::new("/p/docs/b.md")), &hits), Some(1));
        // Le fichier a disparu de la liste, ou rien n'était sélectionné : la sélection repart du premier.
        assert_eq!(kept_hit(Some(Path::new("/p/gone.md")), &hits), None);
        assert_eq!(kept_hit(None, &hits), None);
    }

    #[test]
    fn a_rebuilt_search_keeps_the_selected_line_by_file_and_line_number() {
        let outcome = outcome();
        let items = result_items(&outcome);
        // `b.md` ligne 1 est le cinquième élément ; `a.md` ligne 5 le troisième.
        assert_eq!(
            kept_line(Some((Path::new("/p/b.md"), 1)), &items, &outcome),
            Some(4)
        );
        assert_eq!(
            kept_line(Some((Path::new("/p/a.md"), 5)), &items, &outcome),
            Some(2)
        );
        // La même ligne d'un autre fichier, ou une ligne qui n'est plus trouvée : rien à garder.
        assert_eq!(
            kept_line(Some((Path::new("/p/b.md"), 2)), &items, &outcome),
            None
        );
        assert_eq!(
            kept_line(Some((Path::new("/p/c.md"), 1)), &items, &outcome),
            None
        );
        assert_eq!(kept_line(None, &items, &outcome), None);
    }

    #[test]
    fn the_status_of_a_search_counts_lists_or_admits_the_limit() {
        let mut outcome = outcome();
        assert_eq!(
            search_status(&outcome, 1000),
            "3\u{a0}lignes dans 2\u{a0}fichiers"
        );
        outcome.truncated = true;
        assert_eq!(
            search_status(&outcome, 1000),
            "Plus de 1000\u{a0}lignes, seules les 1000\u{a0}premières sont listées."
        );
        let nothing = SearchOutcome {
            files: Vec::new(),
            truncated: false,
        };
        assert_eq!(search_status(&nothing, 1000), "Aucun résultat.");
    }

    #[test]
    fn a_line_opens_at_its_first_occurrence_counted_in_characters() {
        // `deux foo foo` : « deux␣ » fait 5 caractères, la colonne de la première occurrence est la 6e.
        let first = LineMatch {
            line: 2,
            text: "deux foo foo".to_string(),
            ranges: spans(&[(5, 8), (9, 12)]),
        };
        assert_eq!(open_position(&first), (2, 6));
        // Les caractères accentués comptent pour un : `é` fait deux octets.
        let accented = LineMatch {
            line: 7,
            text: "été foo".to_string(),
            ranges: spans(&[(6, 9)]),
        };
        assert_eq!(open_position(&accented), (7, 5));
        // Sans occurrence (jamais le cas d'une ligne trouvée), le début de la ligne.
        let bare = LineMatch {
            line: 3,
            text: "x".to_string(),
            ranges: Vec::new(),
        };
        assert_eq!(open_position(&bare), (3, 1));
    }

    #[test]
    fn counts_and_summaries_speak_french_plurals() {
        assert_eq!(result_count_label(1, 1), "1\u{a0}ligne dans 1\u{a0}fichier");
        assert_eq!(
            result_count_label(7, 3),
            "7\u{a0}lignes dans 3\u{a0}fichiers"
        );
        assert_eq!(
            truncated_label(1000),
            "Plus de 1000\u{a0}lignes, seules les 1000\u{a0}premières sont listées."
        );
        assert_eq!(
            replace_summary(2, 4, 0),
            "2\u{a0}fichiers modifiés, 4\u{a0}occurrences remplacées."
        );
        assert_eq!(
            replace_summary(1, 1, 0),
            "1\u{a0}fichier modifié, 1\u{a0}occurrence remplacée."
        );
        assert_eq!(
            replace_summary(0, 0, 1),
            "0\u{a0}fichier modifié, 0\u{a0}occurrence remplacée. 1\u{a0}fichier en échec."
        );
        assert_eq!(
            replace_summary(1, 2, 2),
            "1\u{a0}fichier modifié, 2\u{a0}occurrences remplacées. 2\u{a0}fichiers en échec."
        );
    }

    fn replaced(occurrences: usize, stale_lines: usize) -> Result<FileReplace, ReplaceError> {
        Ok(FileReplace {
            occurrences,
            stale_lines,
        })
    }

    #[test]
    fn a_file_with_stale_lines_fails_even_when_other_lines_were_replaced() {
        let clean = file_outcome(PathBuf::from("/p/a.md"), replaced(2, 0));
        assert_eq!(clean.occurrences, 2);
        assert!(clean.failure.is_none());

        let partial = file_outcome(PathBuf::from("/p/b.md"), replaced(1, 2));
        assert_eq!(partial.occurrences, 1);
        let failure = partial.failure.expect("stale lines are a failure");
        assert_eq!(failure.message, STALE_LINES);
        assert_eq!(failure.reason, "stale");

        // Rien de remplacé et des lignes périmées : un échec sans occurrence.
        let none = file_outcome(PathBuf::from("/p/c.md"), replaced(0, 1));
        assert_eq!(none.occurrences, 0);
        assert!(none.failure.is_some());
    }

    #[test]
    fn a_replace_error_fails_the_file_with_its_message_and_journal_reason() {
        let changed = file_outcome(PathBuf::from("/p/a.md"), Err(ReplaceError::Changed));
        let failure = changed.failure.expect("changed is a failure");
        assert_eq!(
            failure.message,
            "Le fichier a changé pendant le remplacement."
        );
        assert_eq!(failure.reason, "changed");
        assert_eq!(changed.occurrences, 0);

        let not_text = file_outcome(PathBuf::from("/p/a.bin"), Err(ReplaceError::NotText));
        let failure = not_text.failure.expect("not text is a failure");
        assert_eq!(
            failure.message,
            "Le fichier n'est plus du texte modifiable."
        );
        assert_eq!(failure.reason, "not-text");

        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let denied = file_outcome(PathBuf::from("/p/ro.md"), Err(ReplaceError::Io(io)));
        let failure = denied.failure.expect("an io error is a failure");
        assert_eq!(failure.reason, "io");
        assert!(
            failure
                .detail
                .is_some_and(|detail| detail.contains("denied"))
        );
    }

    #[test]
    fn the_summary_counts_modified_files_occurrences_and_failed_files() {
        let outcomes = vec![
            file_outcome(PathBuf::from("/p/a.md"), replaced(3, 0)),
            // Un fichier à moitié remplacé compte comme modifié et comme en échec.
            file_outcome(PathBuf::from("/p/b.md"), replaced(1, 1)),
            file_outcome(PathBuf::from("/p/c.md"), Err(ReplaceError::Changed)),
            // Rien à remplacer (l'expression ne trouve plus rien) : ni modifié ni en échec.
            file_outcome(PathBuf::from("/p/d.md"), replaced(0, 0)),
        ];
        assert_eq!(totals(&outcomes), (2, 4, 2));
        let (files, occurrences, failed) = totals(&outcomes);
        assert_eq!(
            replace_summary(files, occurrences, failed),
            "2\u{a0}fichiers modifiés, 4\u{a0}occurrences remplacées. 2\u{a0}fichiers en échec."
        );
    }

    #[test]
    fn a_tab_file_counts_only_what_its_save_wrote() {
        let replaced = FileReplace {
            occurrences: 3,
            stale_lines: 0,
        };
        let written = tab_outcome(PathBuf::from("/p/a.md"), replaced, TabSave::Written);
        assert_eq!(written.occurrences, 3);
        assert!(written.failure.is_none());

        // Un refus d'écriture : aucune occurrence comptée, l'échec porte le texte d'`explain_write` et `reason=io`.
        let denied = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let refused = tab_outcome(PathBuf::from("/p/a.md"), replaced, TabSave::Failed(denied));
        assert_eq!(refused.occurrences, 0);
        let failure = refused.failure.expect("a refused write is a failure");
        assert_eq!(failure.reason, "io");
        assert_eq!(
            failure.message,
            crate::errors::explain_write(&std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            ))
            .text
        );
        assert!(
            failure
                .detail
                .is_some_and(|detail| detail.contains("denied"))
        );

        // Le disque a changé sous la sauvegarde : aucune occurrence, `reason=changed`.
        let changed = tab_outcome(PathBuf::from("/p/a.md"), replaced, TabSave::DiskChanged);
        assert_eq!(changed.occurrences, 0);
        let failure = changed.failure.expect("a changed disk is a failure");
        assert_eq!(failure.reason, "changed");
        assert_eq!(
            failure.message,
            "Le fichier a changé pendant le remplacement."
        );

        // Rien à écrire (le remplacement a rendu le texte que le disque porte déjà) : ce n'est pas un disque changé, le
        // bilan garde les occurrences comptées par le remplacement.
        let identity = tab_outcome(PathBuf::from("/p/a.md"), replaced, TabSave::Unchanged);
        assert_eq!(identity.occurrences, 3);
        assert!(identity.failure.is_none());

        // Un onglet sans rien à écrire : il n'est « inchangé » que si son texte est celui du disque. Sinon rien n'a été
        // écrit (le remplacement n'a pas atteint la sauvegarde) et le fichier est en échec, sans occurrence comptée.
        let on_disk = tab_outcome(PathBuf::from("/p/a.md"), replaced, idle_save(true));
        assert_eq!(on_disk.occurrences, 3);
        assert!(on_disk.failure.is_none());
        let not_on_disk = tab_outcome(PathBuf::from("/p/a.md"), replaced, idle_save(false));
        assert_eq!(not_on_disk.occurrences, 0);
        assert_eq!(
            not_on_disk
                .failure
                .expect("an unwritten tab is a failure")
                .reason,
            "changed"
        );

        // Les lignes périmées d'un fichier écrit restent un échec, avec leurs occurrences comptées.
        let partial = tab_outcome(
            PathBuf::from("/p/b.md"),
            FileReplace {
                occurrences: 1,
                stale_lines: 1,
            },
            TabSave::Written,
        );
        assert_eq!(partial.occurrences, 1);
        assert_eq!(partial.failure.map(|failure| failure.reason), Some("stale"));
    }
}
