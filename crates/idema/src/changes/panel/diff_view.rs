//! Le diff du fichier choisi dans le volet Changes (spec P3 §4.5). Le patch se lit hors du fil d'interface, passe par
//! `DiffFile::parse`, puis le `diff` du kit le dessine en mode unifié. L'en-tête du fichier, la ligne « binaire » et les
//! autres textes sont dessinés par IdemA : le kit n'a pas de locale française.

use std::path::{Path, PathBuf};
use std::time::Instant;

use gpui::{
    AnyElement, AnyWindowHandle, App, Context, Entity, Focusable, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, SharedString, Styled, Task, Window, div, prelude::*, svg,
};
use gpui_component::diff::{
    Diff, DiffChangeIndicator, DiffFile, DiffLineRange, DiffMode, DiffSide, DiffState,
};

use super::ChangesPanel;
use crate::assets::Icon;
use crate::changes::model::{self, LoadError};
use crate::changes::{Change, Snapshot};
use crate::files::document::language_for;
use crate::files::panel::{GLYPH_SLOT, row_lead};
use crate::files::reference::agent_reference;
use crate::hud;
use crate::theme::{
    ERROR_TEXT, ICON_SECONDARY, RUNNING, SEPARATOR, TEXT_MIN, TEXT_PRIMARY, Theme, color, white,
};

/// Contexte montré autour de chaque morceau ; le patch porte tout le fichier, les replis le dépliront.
const CONTEXT_LINES: usize = 3;
/// Hauteur de l'en-tête du fichier.
const HEADER_HEIGHT: f32 = 38.;

/// Ce que la zone du diff montre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// Rien n'est choisi, ou la première lecture n'est pas revenue.
    Nothing,
    Text {
        additions: usize,
        deletions: usize,
    },
    Binary,
    /// Un fichier sans une ligne : un fichier vide ajouté ou supprimé, un fichier non suivi et vide (R7).
    Empty,
    /// Un fichier qui change sans qu'une ligne change : un renommage pur, un changement de mode.
    NoLines,
    /// La lecture a échoué : une phrase, et le message brut de Git (ou du kit) en détail.
    Failed {
        text: String,
        detail: Option<String>,
    },
}

/// Le patch n'a pas pu être lu : la phrase française d'abord, le message brut de Git (ou du kit) en détail.
fn unreadable(detail: String) -> Content {
    Content::Failed {
        text: "Le diff n'a pas pu être lu.".to_string(),
        detail: Some(detail),
    }
}

/// Lit le patch d'un fichier : ce qu'on en montre, et les fichiers à donner au kit (aucun hors du cas `Text`).
pub fn classify(patch: &str, path: &Path) -> (Content, Vec<DiffFile>) {
    let files = match DiffFile::parse(patch) {
        Ok(files) => files,
        Err(error) => return (unreadable(error.message().to_string()), Vec::new()),
    };
    let Some(first) = files.first() else {
        return (Content::Empty, Vec::new());
    };
    if first.is_binary() {
        return (Content::Binary, Vec::new());
    }
    if first.additions() == 0 && first.deletions() == 0 {
        let added_or_deleted = first.original_path().is_none() || first.modified_path().is_none();
        let content = if added_or_deleted {
            Content::Empty
        } else {
            Content::NoLines
        };
        return (content, Vec::new());
    }
    let content = Content::Text {
        additions: first.additions(),
        deletions: first.deletions(),
    };
    // La coloration est celle du volet Fichiers pour ce format ; sinon le kit devine d'après le chemin.
    let language = language_for(path);
    let files = files
        .into_iter()
        .map(|file| match language {
            Some(language) => file.with_language(language),
            None => file,
        })
        .collect();
    (content, files)
}

/// Les numéros d'une ligne du patch : `old` pour une ligne ` ` ou `-`, `new` pour une ligne ` ` ou `+`.
#[derive(Debug, Clone, Copy)]
struct PatchLine {
    old: Option<usize>,
    new: Option<usize>,
}

/// Les premiers numéros (ancien, nouveau) d'une en-tête de morceau `@@ -a[,b] +c[,d] @@`.
fn hunk_start(header: &str) -> Option<(usize, usize)> {
    let mut parts = header.strip_prefix("@@ ")?.split(' ');
    let first = |part: Option<&str>, sign: char| -> Option<usize> {
        part?.strip_prefix(sign)?.split(',').next()?.parse().ok()
    };
    Some((first(parts.next(), '-')?, first(parts.next(), '+')?))
}

/// Les lignes de contenu du patch, dans l'ordre, avec leurs numéros de chaque côté. Les lignes avant le premier morceau
/// (en-tête du fichier) et les lignes `\` (« No newline at end of file ») n'en font pas partie.
fn patch_lines(patch: &str) -> Vec<PatchLine> {
    let mut lines = Vec::new();
    // Les prochains numéros du morceau en cours ; `None` hors d'un morceau.
    let mut next: Option<(usize, usize)> = None;
    for line in patch.lines() {
        if line.starts_with("@@ ") {
            next = hunk_start(line);
            continue;
        }
        if line.starts_with("diff ") {
            next = None;
            continue;
        }
        let Some((old, new)) = next.as_mut() else {
            continue;
        };
        match line.chars().next() {
            Some(' ') => {
                lines.push(PatchLine {
                    old: Some(*old),
                    new: Some(*new),
                });
                *old += 1;
                *new += 1;
            }
            Some('-') => {
                lines.push(PatchLine {
                    old: Some(*old),
                    new: None,
                });
                *old += 1;
            }
            Some('+') => {
                lines.push(PatchLine {
                    old: None,
                    new: Some(*new),
                });
                *new += 1;
            }
            _ => {}
        }
    }
    lines
}

/// Le plus petit et le plus grand numéro du côté nouveau des lignes du patch que `range` couvre, ou `None` quand elles n'en
/// ont pas (des lignes retirées seules) ou que la sélection sort du patch. Même côté aux deux bouts : les lignes de ce
/// côté entre les deux numéros. Côtés différents : les lignes du patch entre les deux positions, dans l'ordre du patch.
pub fn new_side_span(patch: &str, range: &DiffLineRange) -> Option<(usize, usize)> {
    let lines = patch_lines(patch);
    let number = |line: &PatchLine, side: DiffSide| match side {
        DiffSide::Original => line.old,
        DiffSide::Modified => line.new,
    };
    let kept: Vec<&PatchLine> = if range.side() == range.end_side() {
        let side = range.side();
        let (low, high) = (
            range.start().min(range.end()),
            range.start().max(range.end()),
        );
        lines
            .iter()
            .filter(|line| number(line, side).is_some_and(|n| (low..=high).contains(&n)))
            .collect()
    } else {
        let position = |side: DiffSide, wanted: usize| {
            lines
                .iter()
                .position(|line| number(line, side) == Some(wanted))
        };
        let start = position(range.side(), range.start())?;
        let end = position(range.end_side(), range.end())?;
        lines[start.min(end)..=start.max(end)].iter().collect()
    };
    let mut news = kept.iter().filter_map(|line| line.new);
    let first = news.next()?;
    Some(news.fold((first, first), |(low, high), n| (low.min(n), high.max(n))))
}

/// La référence envoyée à l'agent pour des lignes nouvelles du fichier `rel` de la cible `root`, écrite comme celle de
/// l'éditeur de Fichiers. `None` sans lignes à référencer.
pub fn diff_reference(
    root: &Path,
    rel: &Path,
    span: Option<(usize, usize)>,
    home: Option<&Path>,
) -> Option<String> {
    let span = span?;
    Some(agent_reference(&root.join(rel), home, Some(span)))
}

/// Un patch lu et déjà classé par la tâche de fond : le texte, ce qu'on en montre, et les fichiers du kit.
struct ReadPatch {
    patch: String,
    content: Content,
    files: Vec<DiffFile>,
}

/// Le diff du volet : l'état du kit, le fichier montré, et la lecture en cours.
pub struct DiffView {
    state: Entity<DiffState>,
    /// La fenêtre du volet : la mesure de la première image s'y accroche depuis une tâche.
    window: AnyWindowHandle,
    content: Content,
    /// Le fichier montré (chemin relatif à la racine du dépôt).
    path: Option<PathBuf>,
    /// Le chemin que le kit donne à ce fichier (celui du patch, sans `a/` ni `b/`) : la clé de son repli.
    kit_path: Option<SharedString>,
    /// Le dernier patch lu : une relecture qui rend le même ne touche à rien (les replis dépliés et le défilement restent).
    patch: Option<String>,
    /// Le numéro de la dernière lecture demandée : une lecture plus ancienne qui revient après ne montre rien.
    generation: u64,
    _task: Option<Task<()>>,
}

impl DiffView {
    pub fn new(window: &Window, cx: &mut Context<ChangesPanel>) -> Self {
        let state = cx.new(|cx| {
            DiffState::new(Vec::new(), cx)
                .with_mode(DiffMode::Unified)
                .with_context_lines(Some(CONTEXT_LINES))
        });
        Self {
            state,
            window: window.window_handle(),
            content: Content::Nothing,
            path: None,
            kit_path: None,
            patch: None,
            generation: 0,
            _task: None,
        }
    }

    /// Le fichier montré, par son chemin relatif à la racine du dépôt.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Les lignes de la sélection du kit (celle des numéros de la gouttière, `⇧↑` `⇧↓`, `⌘A`). Le niveau extérieur dit s'il
    /// y a une sélection de lignes, le niveau intérieur si elle a un côté nouveau (des lignes retirées seules n'en ont pas).
    pub fn selected_span(&self, cx: &App) -> Option<Option<(usize, usize)>> {
        let range = self.state.read(cx).selected_lines()?;
        let patch = self.patch.as_deref()?;
        Some(new_side_span(patch, &range))
    }

    /// Le diff du kit est-il peint (un fichier texte) ? Sinon son focus n'existe pas à l'écran.
    pub fn is_painted(&self) -> bool {
        matches!(self.content, Content::Text { .. })
    }

    /// Donne le focus au diff : `⌘⌥C` n'y vaut que là.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        let handle = self.state.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Plus de fichier choisi : la zone est vide, et une lecture en route ne montrera rien.
    pub fn clear(&mut self, cx: &mut Context<ChangesPanel>) {
        self.generation += 1;
        self._task = None;
        self.path = None;
        self.kit_path = None;
        self.patch = None;
        self.content = Content::Nothing;
        self.state.update(cx, |state, cx| state.set_files([], cx));
    }

    /// Le fichier montré est-il replié sur son en-tête ?
    fn is_folded(&self, cx: &App) -> bool {
        self.kit_path
            .as_ref()
            .is_some_and(|path| self.state.read(cx).is_file_collapsed(path))
    }

    /// Un clic sur l'en-tête : le corps du fichier se cache ou revient. Le kit ne garde un repli que pour une relecture du
    /// même fichier (`set_files` oublie les chemins qui n'y sont plus) : changer de fichier ne le reporte pas. Replier
    /// efface la sélection de lignes, que le kit garde sinon : `⌘⌥C` et le menu enverraient des lignes que personne ne voit.
    fn toggle_fold(&mut self, cx: &mut Context<ChangesPanel>) {
        let Some(path) = self.kit_path.clone() else {
            return;
        };
        let folded = !self.is_folded(cx);
        self.state.update(cx, |state, cx| {
            if folded {
                state.set_selected_lines(None, cx);
            }
            state.set_file_collapsed(&path, folded, cx);
        });
        cx.notify();
    }

    /// Lit le patch de `change` hors du fil d'interface, puis le montre. Le fichier précédent reste à l'écran tant que
    /// le patch n'est pas là.
    pub fn read(&mut self, snapshot: &Snapshot, change: &Change, cx: &mut Context<ChangesPanel>) {
        self.generation += 1;
        let generation = self.generation;
        let (snapshot, change) = (snapshot.clone(), change.clone());
        self._task = Some(cx.spawn(async move |panel, cx| {
            let path = change.path.clone();
            // Le patch d'un gros fichier se lit puis se parse ici, hors du fil d'interface : `finish` ne fait que le poser.
            let patch = cx
                .background_spawn(async move {
                    let patch = model::file_patch(&snapshot, &change)?;
                    let (content, files) = classify(&patch, &change.path);
                    Ok(ReadPatch {
                        patch,
                        content,
                        files,
                    })
                })
                .await;
            let _ = panel.update(cx, |panel, cx| {
                panel.diff.finish(generation, path, patch, cx);
            });
        }));
    }

    fn finish(
        &mut self,
        generation: u64,
        path: PathBuf,
        read: Result<ReadPatch, LoadError>,
        cx: &mut Context<ChangesPanel>,
    ) {
        if generation != self.generation {
            return;
        }
        let ReadPatch {
            patch,
            content,
            files,
        } = match read {
            Ok(read) => read,
            Err(error) => {
                let content = match error {
                    LoadError::NotARepository => Content::Failed {
                        text: "Ce projet n'est pas un dépôt Git.".to_string(),
                        detail: None,
                    },
                    LoadError::Missing => Content::Failed {
                        text: "Git est introuvable.".to_string(),
                        detail: None,
                    },
                    LoadError::Git(message) => unreadable(message),
                };
                self.show_failure(path, content, cx);
                return;
            }
        };
        if self.path.as_deref() == Some(path.as_path()) && self.patch.as_deref() == Some(&patch) {
            return;
        }
        if matches!(content, Content::Failed { .. }) {
            self.show_failure(path, content, cx);
            return;
        }
        let started = Instant::now();
        self.kit_path = files.first().map(|file| file.path().clone());
        self.state
            .update(cx, |state, cx| state.set_files(files, cx));
        self.path = Some(path);
        self.patch = Some(patch);
        self.content = content;
        cx.notify();
        // `took_ms` est la pose du patch sur le fil d'interface ; `frame_ms` court jusqu'à l'image qui le peint. Un rappel
        // `on_next_frame` part au début de la demande d'image, avant le dessin : le second, posé depuis le premier, part
        // à l'image d'après, quand la première est dessinée.
        let took = started.elapsed();
        let shown = self.path.as_deref().map(|path| path.display().to_string());
        let _ = cx.update_window(self.window, |_, window, _| {
            window.on_next_frame(move |window, _| {
                window.on_next_frame(move |_, _| {
                    hud::log_event(&format!(
                        "changes diff shown took_ms={} frame_ms={} path={}",
                        took.as_millis(),
                        started.elapsed().as_millis(),
                        shown.unwrap_or_default()
                    ));
                });
            });
        });
    }

    /// Le patch n'a pas pu être lu : la zone le dit, et le journal le note une fois par fichier (une relecture qui échoue
    /// de nouveau sur le même fichier ne le répète pas).
    fn show_failure(&mut self, path: PathBuf, content: Content, cx: &mut Context<ChangesPanel>) {
        let already = self.path.as_deref() == Some(path.as_path())
            && matches!(self.content, Content::Failed { .. });
        if !already {
            hud::log_event(&format!("changes patch failed path={}", path.display()));
        }
        self.path = Some(path);
        self.patch = None;
        self.content = content;
        cx.notify();
    }

    /// L'en-tête du fichier (38) puis le diff, ou la ligne qui dit pourquoi il n'y en a pas.
    pub fn render(&self, theme: &Theme, cx: &mut Context<ChangesPanel>) -> AnyElement {
        let Some(path) = &self.path else {
            return div().flex_1().min_w_0().into_any_element();
        };
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let counts = match &self.content {
            Content::Text {
                additions,
                deletions,
            } => Some((*additions, *deletions)),
            _ => None,
        };
        // Le chevron et le clic de repli n'existent que pour un fichier que le kit dessine ; les autres en-têtes gardent la
        // place du chevron, pour que le glyphe ne bouge pas d'un fichier à l'autre.
        let foldable = counts.is_some();
        let folded = self.is_folded(cx);
        let chevron = div()
            .flex_none()
            .w(theme.px(GLYPH_SLOT))
            .flex()
            .justify_center()
            .when(foldable, |slot| {
                let icon = if folded {
                    Icon::ChevronRight
                } else {
                    Icon::ExpandMore
                };
                slot.child(
                    svg()
                        .path(icon.path())
                        .size(theme.px(GLYPH_SLOT))
                        .text_color(white(ICON_SECONDARY)),
                )
            });
        let header = div()
            .id("changes-diff-header")
            .h(theme.px(HEADER_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.px(7.))
            .px(theme.px(12.))
            .border_b_1()
            .border_color(white(SEPARATOR))
            .text_size(theme.text(12.5))
            .when(foldable, |header| {
                header.cursor_pointer().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|panel, _: &MouseDownEvent, _, cx| panel.diff.toggle_fold(cx)),
                )
            })
            .child(chevron)
            .child(
                div()
                    .flex_none()
                    .w(theme.px(GLYPH_SLOT))
                    .flex()
                    .justify_center()
                    .child(row_lead(theme, &name, false, false)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(white(TEXT_PRIMARY))
                    .child(path.to_string_lossy().into_owned()),
            )
            .children(counts.map(|(additions, deletions)| {
                div()
                    .flex_none()
                    .flex()
                    .gap(theme.px(8.))
                    .child(
                        div()
                            .text_color(color(RUNNING))
                            .child(format!("+{additions}")),
                    )
                    .child(
                        div()
                            .text_color(color(ERROR_TEXT))
                            .child(format!("\u{2212}{deletions}")),
                    )
            }));
        let body = match &self.content {
            // Le kit ne gère que le bouton gauche : l'appui droit arrive ici, la sélection de lignes intacte.
            Content::Text { .. } => div()
                .flex_1()
                .min_h_0()
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|panel, event: &MouseDownEvent, window, cx| {
                        panel.on_diff_menu_requested(event.position, window, cx);
                    }),
                )
                .child(
                    Diff::new(&self.state)
                        .header_visible(false)
                        .soft_wrap(true)
                        .line_number(true)
                        // La barre du kit remplace les signes `+` et `−` (écart E66).
                        .change_indicator(DiffChangeIndicator::Bars)
                        .size_full(),
                )
                .into_any_element(),
            Content::Nothing => div().flex_1().into_any_element(),
            Content::Binary => message(theme, "Fichier binaire modifié.", None),
            Content::Empty => message(theme, "Fichier vide.", None),
            Content::NoLines => message(theme, "Aucune ligne modifiée.", None),
            Content::Failed { text, detail } => message(theme, text, detail.as_deref()),
        };
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }
}

/// Une ligne de texte posée sous l'en-tête, à la place du corps du kit, et son détail (le message brut de Git) dessous.
fn message(theme: &Theme, text: &str, detail: Option<&str>) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(theme.px(6.))
        .p(theme.px(16.))
        .text_size(theme.text(12.5))
        .text_color(white(TEXT_MIN))
        .child(text.to_string())
        .children(detail.map(|detail| div().text_size(theme.text(11.5)).child(detail.to_string())))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDITED: &str = "diff --git a/README.md b/README.md\nindex 1111111..2222222 100644\n--- a/README.md\n+++ b/README.md\n@@ -1,3 +1,3 @@\n un\n-deux\n+DEUX\n trois\n";

    #[test]
    fn a_text_patch_shows_its_counts_and_feeds_the_kit() {
        let (content, files) = classify(EDITED, Path::new("README.md"));
        assert_eq!(
            content,
            Content::Text {
                additions: 1,
                deletions: 1
            }
        );
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn an_empty_patch_is_an_empty_file() {
        // Un fichier non suivi et vide : `git diff --no-index` ne rend rien, et le kit ne dessinerait rien (R7).
        let (content, files) = classify("", Path::new("vide.txt"));
        assert_eq!(content, Content::Empty);
        assert!(files.is_empty());
    }

    #[test]
    fn a_binary_patch_is_not_given_to_the_kit() {
        let patch = "diff --git a/logo.bin b/logo.bin\nindex 1111111..2222222 100644\nBinary files a/logo.bin and b/logo.bin differ\n";
        let (content, files) = classify(patch, Path::new("logo.bin"));
        assert_eq!(content, Content::Binary);
        assert!(files.is_empty());
    }

    #[test]
    fn an_added_empty_file_is_an_empty_file_and_a_pure_rename_is_not() {
        let added =
            "diff --git a/vide.txt b/vide.txt\nnew file mode 100644\nindex 0000000..e69de29\n";
        assert_eq!(classify(added, Path::new("vide.txt")).0, Content::Empty);
        let renamed =
            "diff --git a/a.md b/b.md\nsimilarity index 100%\nrename from a.md\nrename to b.md\n";
        assert_eq!(classify(renamed, Path::new("b.md")).0, Content::NoLines);
    }

    #[test]
    fn the_fold_key_of_the_kit_is_the_relative_path_of_the_change() {
        // Le repli se retient par `DiffFile::path()` : il doit valoir le chemin relatif d'IdemA, y compris pour un fichier
        // ajouté (pas de côté `a/`), supprimé (pas de côté `b/`) ou renommé (le nouveau chemin).
        let added = "diff --git a/dir/new.md b/dir/new.md\nnew file mode 100644\nindex 0000000..2222222\n--- /dev/null\n+++ b/dir/new.md\n@@ -0,0 +1 @@\n+un\n";
        let deleted = "diff --git a/dir/old.md b/dir/old.md\ndeleted file mode 100644\nindex 1111111..0000000\n--- a/dir/old.md\n+++ /dev/null\n@@ -1 +0,0 @@\n-un\n";
        let renamed = "diff --git a/a.md b/dir/b.md\nsimilarity index 80%\nrename from a.md\nrename to dir/b.md\n--- a/a.md\n+++ b/dir/b.md\n@@ -1 +1 @@\n-un\n+deux\n";
        for (patch, rel) in [
            (EDITED, "README.md"),
            (added, "dir/new.md"),
            (deleted, "dir/old.md"),
            (renamed, "dir/b.md"),
        ] {
            let (_, files) = classify(patch, Path::new(rel));
            assert_eq!(files.first().map(|file| file.path().as_ref()), Some(rel));
        }
    }

    /// Un morceau : deux lignes de contexte, `c` remplacée par `x` `y` `z`, puis `d`. Côté nouveau : a1 b2 x3 y4 z5 d6.
    const SPAN_PATCH: &str = "diff --git a/f.md b/f.md\n--- a/f.md\n+++ b/f.md\n@@ -1,4 +1,6 @@\n a\n b\n-c\n+x\n+y\n+z\n d\n";

    fn range(side: DiffSide, start: usize, end: usize) -> DiffLineRange {
        DiffLineRange::new("f.md", side, start, end)
    }

    fn span(range: &DiffLineRange) -> Option<(usize, usize)> {
        new_side_span(SPAN_PATCH, range)
    }

    #[test]
    fn new_side_span_of_added_lines() {
        assert_eq!(span(&range(DiffSide::Modified, 3, 5)), Some((3, 5)));
        assert_eq!(span(&range(DiffSide::Modified, 4, 4)), Some((4, 4)));
        // Une sélection tirée vers le haut vaut la même, bornes dans l'autre ordre.
        assert_eq!(span(&range(DiffSide::Modified, 5, 3)), Some((3, 5)));
    }

    #[test]
    fn new_side_span_of_removed_lines_is_none() {
        assert_eq!(span(&range(DiffSide::Original, 3, 3)), None);
    }

    #[test]
    fn new_side_span_keeps_the_context_of_an_old_side_range() {
        // `b` (ancien 2, nouveau 2) est du contexte, `c` (ancien 3) est retirée : seule `b` a un côté nouveau.
        assert_eq!(span(&range(DiffSide::Original, 2, 3)), Some((2, 2)));
    }

    #[test]
    fn new_side_span_across_sides_follows_the_patch() {
        // De la ligne retirée `c` à l'ajout `z` : `c` `x` `y` `z`, donc les lignes nouvelles 3 à 5.
        let down = range(DiffSide::Original, 3, 5).with_end_side(DiffSide::Modified);
        assert_eq!(span(&down), Some((3, 5)));
        // Dans l'autre sens, de `y` (nouveau 4) à `c` (ancien 3) : `c` `x` `y`.
        let up = range(DiffSide::Modified, 4, 3).with_end_side(DiffSide::Original);
        assert_eq!(span(&up), Some((3, 4)));
    }

    #[test]
    fn new_side_span_outside_the_patch_is_none() {
        assert_eq!(span(&range(DiffSide::Modified, 9, 9)), None);
        // Un bout hors du patch, côtés différents : pas de lignes à suivre.
        let across = range(DiffSide::Original, 3, 9).with_end_side(DiffSide::Modified);
        assert_eq!(span(&across), None);
    }

    #[test]
    fn new_side_span_reads_each_hunk_header() {
        let patch = "diff --git a/f.md b/f.md\n--- a/f.md\n+++ b/f.md\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -10,2 +10,3 @@\n j\n k\n+l\n";
        let in_second = range(DiffSide::Modified, 11, 12);
        assert_eq!(new_side_span(patch, &in_second), Some((11, 12)));
        // Le premier morceau garde ses propres numéros.
        let in_first = range(DiffSide::Modified, 1, 2);
        assert_eq!(new_side_span(patch, &in_first), Some((1, 2)));
    }

    #[test]
    fn new_side_span_ignores_the_no_newline_marker() {
        let patch = "diff --git a/f.md b/f.md\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+B\n\\ No newline at end of file\n";
        let all = range(DiffSide::Modified, 1, 2);
        assert_eq!(new_side_span(patch, &all), Some((1, 2)));
    }

    #[test]
    fn diff_reference_formats_like_the_editor() {
        let root = Path::new("/Users/x/r");
        let rel = Path::new("README.md");
        let home = Some(Path::new("/Users/x"));
        assert_eq!(
            diff_reference(root, rel, Some((3, 5)), home).as_deref(),
            Some("'~/r/README.md  #L3-L5'")
        );
        assert_eq!(
            diff_reference(root, rel, Some((4, 4)), home).as_deref(),
            Some("~/r/README.md:L4")
        );
        assert_eq!(diff_reference(root, rel, None, home), None);
    }

    #[test]
    fn a_patch_the_kit_cannot_read_is_reported_not_swallowed() {
        let (content, files) = classify("@@ -x +y @@\n+\n", Path::new("a.md"));
        // La phrase française d'abord, le message brut du kit en détail.
        let Content::Failed { text, detail } = content else {
            panic!("{content:?}");
        };
        assert_eq!(text, "Le diff n'a pas pu être lu.");
        assert!(detail.is_some_and(|detail| !detail.is_empty()));
        assert!(files.is_empty());
    }
}
