//! La liste du volet Changes (spec P3 §4.3) : les changements en arbre, groupes puis dossiers puis fichiers, en liste
//! virtualisée. Chaque ligne fait 26 de haut, avec un retrait de 14 par niveau ; un fichier porte son glyphe, son nom, sa
//! lettre de statut et sa case, dessinée à la main (elle n'agit pas encore). La colonne a sa poignée (P3-10, `column.rs`).

use std::ops::Range;

use super::*;

use gpui::{AnyElement, FontWeight, uniform_list};

use crate::changes::Letter;
use crate::changes::rows::CheckAction;
use crate::column::HandleActions;
use crate::files::panel::{GLYPH_SLOT, row_lead};
use crate::theme::{
    ACTION_HOVER, CHECKBOX_OUTLINE, ERROR_TEXT, FOCUS, ICON_SECONDARY, RUNNING, SEPARATOR,
    STATUS_MODIFIED, TEXT_FULL, TEXT_SECONDARY, TREE_BACKGROUND, TREE_SELECTED,
    TREE_SELECTED_BORDER, TREE_TEXT, black,
};

/// Hauteur d'une ligne de la liste (maquette : 26) et retrait par niveau de profondeur (14).
const ROW_HEIGHT: f32 = 26.;
const ROW_INDENT: f32 = 14.;
/// Le côté de la case (maquette : 14), son rayon, et la barre de la case mixte (8 sur 2).
const CHECKBOX_SIZE: f32 = 14.;
const CHECKBOX_RADIUS: f32 = 3.;
const MIXED_BAR_WIDTH: f32 = 8.;
const MIXED_BAR_HEIGHT: f32 = 2.;
/// La largeur de la case de la lettre de statut, qui aligne les lettres d'une ligne à l'autre.
const LETTER_SLOT: f32 = 12.;
/// Police de la lettre de statut : une mono du système, comme les glyphes de type (la maquette a SF Mono).
const LETTER_FONT: &str = "Menlo";
/// Le côté de l'icône de dossier (maquette : 15).
const FOLDER_ICON_SIZE: f32 = 15.;

/// La couleur de la lettre de statut : `M` orange, `A` et `U` vert, `D` et `!` rouge, `R` bleu.
fn letter_color(letter: Letter) -> u32 {
    match letter {
        Letter::Modified => STATUS_MODIFIED,
        Letter::Added | Letter::Untracked => RUNNING,
        Letter::Deleted | Letter::Conflict => ERROR_TEXT,
        Letter::Renamed => FOCUS_TEXT,
    }
}

/// La case, dessinée à la main : un carré de 14. Vide, un contour ; cochée, le fond `FOCUS` et une coche blanche ; mixte,
/// le fond `FOCUS` et une barre blanche. Une ligne sans case garde la place, pour que les lettres restent alignées.
fn checkbox(theme: &Theme, check: Check) -> AnyElement {
    let slot = div().flex_none().size(theme.px(CHECKBOX_SIZE));
    let filled = |slot: gpui::Div| {
        slot.rounded(theme.px(CHECKBOX_RADIUS))
            .bg(color(FOCUS))
            .flex()
            .items_center()
            .justify_center()
    };
    match check {
        Check::NoBox => slot.into_any_element(),
        Check::Unchecked => slot
            .rounded(theme.px(CHECKBOX_RADIUS))
            .border_1()
            .border_color(white(CHECKBOX_OUTLINE))
            .into_any_element(),
        Check::Checked => filled(slot)
            .child(
                svg()
                    .path(Icon::Check.path())
                    .size(theme.px(CHECKBOX_SIZE - 2.))
                    .text_color(white(TEXT_FULL)),
            )
            .into_any_element(),
        Check::Mixed => filled(slot)
            .child(
                div()
                    .w(theme.px(MIXED_BAR_WIDTH))
                    .h(theme.px(MIXED_BAR_HEIGHT))
                    .rounded(theme.px(1.))
                    .bg(white(TEXT_FULL)),
            )
            .into_any_element(),
    }
}

/// Une écriture de case à jouer : cocher ou décocher ces chemins, relatifs à la racine du dépôt.
pub(super) struct CheckWrite {
    root: PathBuf,
    action: CheckAction,
    paths: Vec<PathBuf>,
}

/// Les changements tels que la liste les montre : ceux de la lecture, avec la case que le clic a posée quand son écriture
/// n'est pas finie. Sans clic en attente, la lecture elle-même.
pub(super) fn with_pending<'a>(
    changes: &'a [Change],
    pending: &HashMap<PathBuf, Check>,
) -> Cow<'a, [Change]> {
    if pending.is_empty() {
        return Cow::Borrowed(changes);
    }
    Cow::Owned(
        changes
            .iter()
            .map(|change| Change {
                check: pending.get(&change.path).copied().unwrap_or(change.check),
                ..change.clone()
            })
            .collect(),
    )
}

impl ChangesPanel {
    pub(super) fn snapshot(&self) -> Option<&Snapshot> {
        match &self.load {
            Load::Loaded(snapshot) => Some(snapshot),
            _ => None,
        }
    }

    /// La case d'une ligne, ou rien pour une ligne sans case. Un appui sur la case agit sans choisir la ligne.
    fn render_checkbox(&self, index: usize, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let row = &self.rows[index];
        let slot = checkbox(theme, row.check);
        if row.check == Check::NoBox {
            return slot;
        }
        let key = row.key.clone();
        div()
            .id(("changes-check", index))
            .flex_none()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.list_focus, cx);
                    this.toggle_check(&key, window, cx);
                }),
            )
            .child(slot)
            .into_any_element()
    }

    /// Un clic sur la case de `key` (fichier, dossier ou groupe), ou `espace` sur la ligne choisie : une case mixte ou vide
    /// stage tout ce qui est dessous, une case cochée le retire de l'index. La case montre le nouvel état aussitôt, puis
    /// l'écriture se joue hors du fil d'interface.
    fn toggle_check(&mut self, key: &RowKey, window: &mut Window, cx: &mut Context<Self>) {
        // Un commit en cours tient `index.lock` (un hook `pre-commit` long, par exemple) : un `git add` lancé maintenant
        // échouerait en « Git est occupé » sans que l'utilisateur ait rien fait de mal. Le clic est ignoré, pas retenu : la case
        // ne change pas, et la liste se relit à la fin du commit.
        if self.committing {
            return;
        }
        let Some(check) = self
            .rows
            .iter()
            .find(|row| row.key == *key)
            .map(|row| row.check)
        else {
            return;
        };
        let (Some(action), Some(snapshot)) = (rows::next_action(check), self.snapshot()) else {
            return;
        };
        let targets = rows::row_targets(key, &snapshot.changes);
        if targets.is_empty() {
            return;
        }
        let shown = match action {
            CheckAction::Stage => Check::Checked,
            CheckAction::Unstage => Check::Unchecked,
        };
        let shown_paths: Vec<PathBuf> = targets
            .iter()
            .map(|&index| snapshot.changes[index].path.clone())
            .collect();
        let write = CheckWrite {
            root: snapshot.root.clone(),
            action,
            paths: rows::write_paths(action, &snapshot.changes, &targets),
        };
        for path in shown_paths {
            self.pending_checks.insert(path, shown);
        }
        self.writes.push_back(write);
        // Une écriture neuve : une lecture déjà lancée ne dit pas encore l'index d'après.
        self.settling = false;
        self.settled_load = false;
        self.rebuild_rows();
        cx.notify();
        self.pump_writes(window, cx);
    }

    /// `espace` : coche ou décoche le fichier choisi.
    fn toggle_selected_check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.selected.as_ref() else {
            return;
        };
        let key = self
            .rows
            .iter()
            .find(|row| {
                row.check != Check::NoBox
                    && matches!(&row.key, RowKey::File(_, path) if path == selected)
            })
            .map(|row| row.key.clone());
        if let Some(key) = key {
            self.toggle_check(&key, window, cx);
        }
    }

    /// Joue l'écriture suivante de la file, hors du fil d'interface. File vide, les cases cliquées s'effacent à la fin de
    /// la première lecture lancée à partir d'ici.
    fn pump_writes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writing {
            return;
        }
        let Some(write) = self.writes.pop_front() else {
            self.settling = true;
            self.refresh(cx);
            return;
        };
        self.writing = true;
        let gesture = match write.action {
            CheckAction::Stage => "stage",
            CheckAction::Unstage => "unstage",
        };
        // Le geste, au journal avant l'écriture : AC-72 exige cette ligne devant toute commande qui écrit.
        hud::log_event(&format!(
            "changes gesture {gesture} paths={}",
            write.paths.len()
        ));
        hud::log_event(&format!("changes {gesture} paths={}", write.paths.len()));
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    match write.action {
                        CheckAction::Stage => ops::stage(&write.root, &write.paths),
                        CheckAction::Unstage => ops::unstage(&write.root, &write.paths),
                    }
                })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| panel.write_done(result, window, cx));
        })
        .detach();
    }

    /// Une écriture de case est revenue. Réussie, la suivante part. Refusée (verrou pris par un agent, par exemple), les
    /// cases reviennent à l'état de la lecture, le reste de la file est lâché, et la bulle donne le message de Git.
    fn write_done(
        &mut self,
        result: Result<(), ops::GitFailureRaw>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.writing = false;
        match result {
            Ok(()) => self.pump_writes(window, cx),
            Err(error) => {
                self.writes.clear();
                self.pending_checks.clear();
                self.settling = false;
                self.settled_load = false;
                self.rebuild_rows();
                cx.notify();
                self.refresh(cx);
                let Explained { text, detail } = explain_git(error.failure, &error.raw);
                alert(window, cx, &text, detail.as_deref());
            }
        }
    }

    /// Une ligne de la liste dessinée : un groupe (chevron, nom en 11 / 600), un dossier (chevron, nom) ou un fichier
    /// (glyphe, nom, ancien chemin d'un renommage, lettre). La case est à droite de toutes.
    fn render_row(&self, index: usize, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let row = &self.rows[index];
        let change = row
            .change
            .and_then(|change| self.snapshot().map(|snapshot| &snapshot.changes[change]));
        let selected = change.is_some_and(|change| self.selected.as_ref() == Some(&change.path));
        let key = row.key.clone();
        let is_file = change.is_some();
        let (lead, name) = match &row.key {
            RowKey::Group(_) => {
                let expanded = !self.collapsed.contains(&row.key);
                (
                    row_lead(theme, "", true, expanded),
                    div()
                        .text_size(theme.text(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(white(TEXT_SECONDARY)),
                )
            }
            RowKey::Folder(..) => {
                let expanded = !self.collapsed.contains(&row.key);
                (
                    row_lead(theme, "", true, expanded),
                    div()
                        .text_size(theme.text(12.5))
                        .text_color(white(TREE_TEXT)),
                )
            }
            RowKey::File(..) => (
                row_lead(theme, &row.label, false, false),
                div()
                    .text_size(theme.text(12.5))
                    .text_color(white(if selected { TEXT_FULL } else { TREE_TEXT })),
            ),
        };
        let name = name
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .child(row.label.clone());
        // Un renommage montre l'ancien chemin derrière le nouveau nom, en `TEXT_MIN`.
        let old_path = change
            .and_then(|change| change.orig_path.as_ref())
            .map(|path| {
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(theme.text(11.))
                    .text_color(white(TEXT_MIN))
                    .child(path.to_string_lossy().into_owned())
            });
        let letter = change.map(|change| {
            div()
                .flex_none()
                .w(theme.px(LETTER_SLOT))
                .flex()
                .justify_center()
                .font_family(LETTER_FONT)
                .font_weight(FontWeight::BOLD)
                .text_size(theme.text(11.))
                .text_color(color(letter_color(change.letter)))
                .child(change.letter.glyph())
        });
        let radius = theme.px(5.);
        let menu_key = key.clone();
        div()
            .id(("changes-row", index))
            .flex()
            .items_center()
            .gap(theme.px(7.))
            .h(theme.px(ROW_HEIGHT))
            .w_full()
            .pl(theme.px(8. + ROW_INDENT * row.depth as f32))
            .pr(theme.px(8.))
            .rounded(radius)
            // Le contour est toujours là, transparent hors sélection : la ligne ne change pas de taille.
            .border_1()
            .border_color(black(0.))
            .when(!is_file, |row| row.cursor_pointer())
            .when(selected, |row| {
                row.bg(focus(TREE_SELECTED))
                    .border_color(focus(TREE_SELECTED_BORDER))
            })
            .when(!selected, |row| {
                row.hover(|style| style.bg(white(ACTION_HOVER)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.list_focus, cx);
                    this.on_row_pressed(&key, event.click_count, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.on_row_menu_requested(&menu_key, event.position, window, cx);
                    // Le volet n'a pas d'autre menu : l'appui s'arrête ici, qu'un groupe en ait un ou non.
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex_none()
                    .w(theme.px(GLYPH_SLOT))
                    .flex()
                    .justify_center()
                    .child(lead),
            )
            // Un dossier porte son icône après le chevron, comme l'écran 04 de la maquette (blanc 45 %).
            .when(matches!(row.key, RowKey::Folder(..)), |row| {
                row.child(
                    div()
                        .flex_none()
                        .w(theme.px(GLYPH_SLOT))
                        .flex()
                        .justify_center()
                        .child(
                            svg()
                                .path(Icon::Folder.path())
                                .size(theme.px(FOLDER_ICON_SIZE))
                                .text_color(white(ICON_SECONDARY)),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(theme.px(6.))
                    .overflow_hidden()
                    .child(name)
                    .children(old_path),
            )
            .children(letter)
            .child(self.render_checkbox(index, theme, cx))
            .into_any_element()
    }

    /// Un appui sur une ligne. Un groupe ou un dossier se replie ou se déplie au premier appui ; un fichier se choisit
    /// (son diff s'affiche), et un double-clic l'ouvre dans Fichiers.
    fn on_row_pressed(&mut self, key: &RowKey, click_count: usize, cx: &mut Context<Self>) {
        match key {
            RowKey::Group(_) | RowKey::Folder(..) => {
                if click_count == 1 {
                    self.toggle_row(key.clone(), cx);
                }
            }
            RowKey::File(_, path) => {
                self.select(path.clone(), cx);
                if click_count >= 2 {
                    self.open_in_files(path, cx);
                }
            }
        }
    }

    /// Le double-clic : Fichiers ouvre le fichier à la place de Changes. Un fichier qui n'existe plus (supprimé) n'a rien à
    /// ouvrir.
    fn open_in_files(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot() else {
            return;
        };
        let absolute = snapshot.root.join(path);
        if absolute.is_file() {
            cx.emit(ChangesEvent::OpenInFiles(absolute));
        }
    }

    /// `↑` et `↓` : le fichier visible qui précède ou suit celui qui est choisi (les groupes et les dossiers se passent).
    /// Aucun fichier choisi parmi les visibles : le premier pour `↓`, le dernier pour `↑`.
    fn move_selection(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot() else {
            return;
        };
        let files: Vec<(usize, &Path)> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                row.change
                    .map(|change| (index, snapshot.changes[change].path.as_path()))
            })
            .collect();
        let current = files
            .iter()
            .position(|(_, path)| Some(*path) == self.selected.as_deref());
        let target = match current {
            Some(current) => current
                .checked_add_signed(step)
                .filter(|target| *target < files.len()),
            None if step > 0 => Some(0),
            None => files.len().checked_sub(1),
        };
        let Some((row, path)) = target.map(|target| files[target]) else {
            return;
        };
        let path = path.to_path_buf();
        self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        self.select(path, cx);
    }

    /// La colonne de la liste : les lignes en liste virtualisée sur le creux de l'arbre, un filet à droite. Elle porte le
    /// focus de la liste et son contexte de touches.
    pub(super) fn render_list(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = uniform_list(
            "changes-rows",
            self.rows.len(),
            cx.processor(
                |this, range: Range<usize>, _window, cx: &mut Context<Self>| {
                    let theme = *cx.global::<Theme>();
                    range
                        .map(|index| this.render_row(index, &theme, cx))
                        .collect::<Vec<_>>()
                },
            ),
        )
        .track_scroll(&self.scroll)
        .size_full();
        div()
            .id("changes-list")
            .flex_none()
            .w(theme.px(self.shown_list_width()))
            .h_full()
            .bg(black(TREE_BACKGROUND))
            .border_r_1()
            .border_color(white(SEPARATOR))
            .flex()
            .flex_col()
            // Les lignes, puis le pied sous elles (maquette) : la liste défile dans ce qui reste. Le contexte de touches et
            // le focus de la liste n'enveloppent que les lignes : le pied n'est pas dessous, sinon `espace`, `↑` et `↓` du
            // champ du message seraient pris par la liste.
            .child(
                div()
                    .id("changes-rows-area")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .p(theme.px(6.))
                    .track_focus(&self.list_focus)
                    .key_context(LIST_KEY_CONTEXT)
                    .on_action(cx.listener(|this, _: &ListUp, _, cx| this.move_selection(-1, cx)))
                    .on_action(cx.listener(|this, _: &ListDown, _, cx| this.move_selection(1, cx)))
                    .on_action(cx.listener(|this, _: &ToggleCheck, window, cx| {
                        this.toggle_selected_check(window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &GenerateMessage, window, cx| {
                        this.generate(window, cx);
                    }))
                    .child(rows),
            )
            .child(self.render_footer(theme, false, cx))
    }

    /// La poignée du bord droit de la liste (le geste est celui de `column::render_handle`).
    pub(super) fn render_list_handle(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        column::render_handle(
            cx.entity(),
            self.list_drag.is_some(),
            theme.px(self.shown_list_width()),
            HandleActions {
                begin: Self::begin_list_drag,
                drag: Self::drag_list_to,
                end: Self::end_list_drag,
                reset: Self::reset_list_width,
            },
        )
    }

    /// La largeur que la colonne montre : la largeur voulue, rognée au corps mesuré (le diff garde son plancher).
    fn shown_list_width(&self) -> f32 {
        column::width_shown(
            self.list_width,
            self.body_width.unwrap_or(f32::MAX),
            LIST_WIDTH,
            LIST_MIN_WIDTH,
            DIFF_MIN_WIDTH,
        )
    }

    /// La peinture a mesuré le corps du volet. Rend `true` si la valeur a changé : à l'appelant de demander une image,
    /// car GPUI ignore les `notify` faits pendant le dessin.
    pub(super) fn note_body_width(&mut self, width: f32) -> bool {
        let changed = self
            .body_width
            .is_none_or(|known| (known - width).abs() > 0.01);
        if changed {
            self.body_width = Some(width);
        }
        changed
    }

    /// La préférence de largeur, changée depuis le volet d'un autre projet : on la suit sans la renvoyer.
    pub fn apply_list_width(&mut self, width: Option<f32>, cx: &mut Context<Self>) {
        if self.list_width != width {
            self.list_width = width;
            cx.notify();
        }
    }

    /// L'appui sur la poignée : le glisser part de la largeur montrée (pas de saut).
    fn begin_list_drag(&mut self, x: Pixels, cx: &mut Context<Self>) {
        self.list_drag = Some(ColumnDrag {
            start_x: x,
            start_width: self.shown_list_width(),
            start_wanted: self.list_width,
            moved: false,
        });
        cx.notify();
    }

    /// Le mouvement : la largeur voulue devient la largeur montrée, bornée comme au rendu. Rien n'est émis avant le
    /// relâché.
    fn drag_list_to(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some(drag) = self.list_drag.as_mut() else {
            return;
        };
        drag.moved = true;
        let scale = cx.global::<Theme>().scale;
        let delta = f32::from(x - drag.start_x) / scale;
        self.list_width = Some(column::dragged_width(
            drag.start_width,
            delta,
            self.body_width.unwrap_or(f32::MAX),
            LIST_WIDTH,
            LIST_MIN_WIDTH,
            DIFF_MIN_WIDTH,
        ));
        cx.notify();
    }

    /// Le relâché (ou le relâché perdu) : s'il y a eu mouvement, la largeur est écrite par `Workspace`.
    fn end_list_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(drag) = self.list_drag.take() {
            if drag.moved {
                self.announce_list_width(cx);
            }
            cx.notify();
        }
    }

    /// Le glisser s'arrête sans relâché : le volet est masqué ou la liste se cache en plein glisser, et la poignée n'est
    /// plus peinte pour recevoir le relâché. La largeur voulue d'avant l'appui revient et rien n'est écrit.
    pub(super) fn cancel_list_drag(&mut self, cx: &mut Context<Self>) {
        if column::cancel_drag(&mut self.list_drag, &mut self.list_width) {
            cx.notify();
        }
    }

    /// Le double-clic sur la poignée : la largeur par défaut revient, et s'écrit.
    fn reset_list_width(&mut self, cx: &mut Context<Self>) {
        self.list_drag = None;
        self.list_width = None;
        self.announce_list_width(cx);
        cx.notify();
    }

    /// `Workspace` écrit la largeur dans `state.json` ; le journal la donne aux scripts d'acceptation.
    fn announce_list_width(&self, cx: &mut Context<Self>) {
        let wanted = self
            .list_width
            .map_or_else(|| "none".to_string(), |width| format!("{}", width.round()));
        hud::log_event(&format!(
            "changes list width shown={} wanted={wanted} body={}",
            self.shown_list_width().round(),
            self.body_width.unwrap_or_default().round()
        ));
        cx.emit(ChangesEvent::ListWidthChanged(self.list_width));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{Group, Letter};

    fn change(path: &str, check: Check) -> Change {
        Change {
            path: PathBuf::from(path),
            orig_path: None,
            group: Group::Modified,
            letter: Letter::Modified,
            check,
        }
    }

    #[test]
    fn with_pending_borrows_the_read_when_no_click_is_waiting() {
        let changes = [change("a.md", Check::Unchecked)];

        let shown = with_pending(&changes, &HashMap::new());

        assert!(matches!(shown, Cow::Borrowed(_)));
        assert_eq!(shown[0].check, Check::Unchecked);
    }

    #[test]
    fn with_pending_overrides_only_the_clicked_paths() {
        let changes = [
            change("a.md", Check::Unchecked),
            change("b.md", Check::Checked),
            change("c.md", Check::Mixed),
        ];
        let pending = HashMap::from([
            (PathBuf::from("a.md"), Check::Checked),
            (PathBuf::from("b.md"), Check::Unchecked),
            // Un chemin qui n'est plus dans la lecture ne crée rien.
            (PathBuf::from("gone.md"), Check::Checked),
        ]);

        let shown = with_pending(&changes, &pending);

        let checks: Vec<(&str, Check)> = shown
            .iter()
            .map(|change| (change.path.to_str().unwrap(), change.check))
            .collect();
        assert_eq!(
            checks,
            [
                ("a.md", Check::Checked),
                ("b.md", Check::Unchecked),
                ("c.md", Check::Mixed)
            ]
        );
        // Le reste de chaque ligne est celui de la lecture.
        assert_eq!(shown[0].group, Group::Modified);
        // La lecture elle-même n'a pas bougé.
        assert_eq!(changes[0].check, Check::Unchecked);
    }
}
