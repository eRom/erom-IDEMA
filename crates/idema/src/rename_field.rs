//! Le champ de renommage d'une ligne de session (spec §4.3, écart E10) : un champ à une ligne, sans curseur
//! mobile ni sélection à la souris. Au départ tout le nom est « sélectionné » (la première frappe le remplace,
//! comme dans le Finder) ; la saisie arrive par le gestionnaire de saisie de GPUI, donc l'IME et les touches
//! mortes fonctionnent. Il rend `Confirm` à Entrée, `Cancel` à Échap et à la perte du focus.
//!
//! La barre de recherche (⌘F, spec jalon 4 §4.9) reprend ce chemin de saisie avec d'autres `FieldOptions` : son
//! propre contexte de touches, un `Changed` à chaque frappe, et le focus perdu ne l'annule pas.

use std::ops::Range;

use gpui::{
    App, Bounds, Context, ElementInputHandler, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, HighlightStyle, Pixels, Rgba, StyledText, Subscription, UTF16Selection,
    UnderlineStyle, Window, actions, canvas, div, prelude::*, px,
};

use crate::menus::{Paste, SelectAll};
use crate::theme::{FOCUS, TEXT_FULL, TEXT_MIN, Theme, color, white};

actions!(
    rename_field,
    [
        RenameBackspace,
        RenameConfirm,
        RenameCancel,
        RenameDeleteToStart,
        RenameMoveLeft,
        RenameMoveRight
    ]
);

/// Contexte de touches du champ de renommage : `backspace`, `cmd-backspace`, `enter`, `escape`, `left` et `right` s'y
/// accrochent (`⌘V` et `⌘A` arrivent par les actions globales de `menus`).
pub const KEY_CONTEXT: &str = "RenameField";

/// Le corps du texte du renommage, en cotes de la maquette.
pub const DEFAULT_TEXT_SIZE: f32 = 13.;

/// Opacité du fond de la sélection initiale : `FOCUS` à 30 %.
const SELECTION_ALPHA: f32 = 0.30;

/// Ce que le champ rend à l'espace de travail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameEvent {
    /// Le texte rogné (peut être vide : l'espace de travail l'annule alors).
    Confirm(String),
    Cancel,
    /// Le texte tel quel après une frappe, un collage ou une composition : seulement pour un champ qui le demande
    /// (`FieldOptions::emit_changes`), jamais pour le renommage.
    Changed(String),
}

/// Ce qui distingue un usage du champ d'un autre.
#[derive(Debug, Clone, Copy)]
pub struct FieldOptions {
    /// Le contexte de touches du champ : ses liaisons (`backspace`, `enter`, `escape`) s'y accrochent.
    pub key_context: &'static str,
    /// Perdre le focus annule, comme Échap.
    pub cancel_on_blur: bool,
    /// Le texte de départ est sélectionné : la première frappe le remplace.
    pub select_initial: bool,
    /// Chaque modification du texte émet `Changed`.
    pub emit_changes: bool,
    /// Texte grisé tant que le champ est vide.
    pub placeholder: Option<&'static str>,
    /// Le corps du texte, en cotes de la maquette (par défaut `DEFAULT_TEXT_SIZE`).
    pub text_size: f32,
}

impl FieldOptions {
    /// Le renommage d'une ligne de session.
    pub const RENAME: Self = Self {
        key_context: KEY_CONTEXT,
        cancel_on_blur: true,
        select_initial: true,
        emit_changes: false,
        placeholder: None,
        text_size: DEFAULT_TEXT_SIZE,
    };

    /// Le nom d'un fichier ou d'un dossier de l'arbre : le volet efface son message d'erreur à chaque `Changed`.
    pub const FILE_NAME: Self = Self {
        key_context: KEY_CONTEXT,
        cancel_on_blur: true,
        select_initial: true,
        emit_changes: true,
        placeholder: None,
        text_size: DEFAULT_TEXT_SIZE,
    };
}

/// Retire les caractères de contrôle : un saut de ligne dans `@idema_name` casserait `parse_sessions`.
fn sanitize(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// Le texte du champ, sans GPUI : validé, en composition, une sélection éventuelle et le point d'insertion.
/// `selection` et `cursor` sont des décalages en octets dans `text` ; le point d'insertion est toujours à la fin de la
/// sélection quand il y en a une. La composition s'affiche au point d'insertion.
#[derive(Debug)]
struct Buffer {
    text: String,
    marked: Option<String>,
    selection: Option<Range<usize>>,
    cursor: usize,
}

/// Le plus grand décalage de caractère inférieur ou égal à `offset`.
fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Le plus petit décalage de caractère supérieur ou égal à `offset` (la fin d'un caractère coupé en deux).
fn ceil_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

impl Buffer {
    /// Au départ tout le nom est sélectionné et le point d'insertion est à la fin.
    fn new(initial: &str) -> Self {
        let text = sanitize(initial);
        let len = text.len();
        Self {
            selection: (len > 0).then_some(0..len),
            cursor: len,
            text,
            marked: None,
        }
    }

    /// La sélection est remplacée par rien : le point d'insertion prend sa place.
    fn remove_selection(&mut self) {
        if let Some(range) = self.selection.take() {
            self.text.replace_range(range.clone(), "");
            self.cursor = range.start;
        }
    }

    /// Sélectionne `range` (en octets), bornes ramenées aux limites de caractère (la fin vers le caractère suivant) ;
    /// le point d'insertion passe à la fin. Une plage vide ne sélectionne rien et pose seulement le point.
    fn select_range(&mut self, range: Range<usize>) {
        self.unmark();
        let start = floor_boundary(&self.text, range.start);
        let end = ceil_boundary(&self.text, range.end).max(start);
        self.selection = (start < end).then_some(start..end);
        self.cursor = end;
    }

    /// Tout le nom est sélectionné : la frappe suivante le remplace.
    fn select_everything(&mut self) {
        self.unmark();
        let len = self.text.len();
        self.selection = (len > 0).then_some(0..len);
        self.cursor = len;
    }

    /// Plus rien n'est sélectionné ; le point d'insertion reste où il est.
    fn deselect(&mut self) {
        self.selection = None;
    }

    /// `←` : avec une sélection, le point va à son début et elle tombe ; sinon il recule d'un caractère. Pendant une
    /// composition, les flèches sont à l'IME.
    fn move_left(&mut self) {
        if self.marked.is_some() {
            return;
        }
        if let Some(range) = self.selection.take() {
            self.cursor = range.start;
        } else if let Some(previous) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= previous.len_utf8();
        }
    }

    /// `→` : avec une sélection, le point va à sa fin et elle tombe ; sinon il avance d'un caractère.
    fn move_right(&mut self) {
        if self.marked.is_some() {
            return;
        }
        if let Some(range) = self.selection.take() {
            self.cursor = range.end;
        } else if let Some(next) = self.text[self.cursor..].chars().next() {
            self.cursor += next.len_utf8();
        }
    }

    /// Texte validé : la composition en cours est remplacée, la sélection aussi.
    fn insert(&mut self, text: &str) {
        self.marked = None;
        self.remove_selection();
        let text = sanitize(text);
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    /// Texte en composition : affiché souligné, jamais validé avant `insert` ou `unmark`.
    fn mark(&mut self, text: &str) {
        self.remove_selection();
        let text = sanitize(text);
        self.marked = (!text.is_empty()).then_some(text);
    }

    /// L'IME accepte la composition telle quelle.
    fn unmark(&mut self) {
        if let Some(marked) = self.marked.take() {
            self.text.insert_str(self.cursor, &marked);
            self.cursor += marked.len();
        }
    }

    fn backspace(&mut self) {
        // Pendant une composition, l'IME gère lui-même l'effacement.
        if self.marked.is_some() {
            return;
        }
        if self.selection.is_some() {
            self.remove_selection();
        } else if let Some(previous) = self.text[..self.cursor].chars().next_back() {
            let start = self.cursor - previous.len_utf8();
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    /// `⌘⌫` : du début du texte au point d'insertion, ou la sélection ; ce qui suit reste.
    fn delete_to_start(&mut self) {
        if self.marked.is_some() {
            return;
        }
        if self.selection.is_some() {
            self.remove_selection();
        } else {
            self.text.replace_range(..self.cursor, "");
            self.cursor = 0;
        }
    }

    /// Le texte tel qu'on le voit : avant le point, composition, après le point.
    fn full(&self) -> String {
        format!(
            "{}{}{}",
            &self.text[..self.cursor],
            self.marked.as_deref().unwrap_or_default(),
            &self.text[self.cursor..]
        )
    }

    /// Où le point d'insertion se voit dans `full()` : après la composition.
    fn visual_cursor(&self) -> usize {
        self.cursor + self.marked.as_deref().map_or(0, str::len)
    }

    /// La sélection pour l'IME, en UTF-16 ; sans sélection, le point d'insertion.
    fn selected_utf16(&self) -> Range<usize> {
        match &self.selection {
            Some(range) => utf16_len(&self.text[..range.start])..utf16_len(&self.text[..range.end]),
            None => {
                let at = utf16_len(&self.text[..self.cursor]);
                at..at
            }
        }
    }

    /// La composition pour l'IME, en UTF-16, à partir du point d'insertion.
    fn marked_utf16(&self) -> Option<Range<usize>> {
        let marked = self.marked.as_deref()?;
        let start = utf16_len(&self.text[..self.cursor]);
        Some(start..start + utf16_len(marked))
    }

    fn result(&self) -> String {
        self.full().trim().to_owned()
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Décalage en octets de la position `utf16` (en unités UTF-16) dans `text`, borné à la fin.
fn byte_offset(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (offset, c) in text.char_indices() {
        if units >= utf16 {
            return offset;
        }
        units += c.len_utf16();
    }
    text.len()
}

pub struct RenameField {
    buffer: Buffer,
    options: FieldOptions,
    focus_handle: FocusHandle,
    _blur: Option<Subscription>,
}

impl EventEmitter<RenameEvent> for RenameField {}

impl RenameField {
    pub fn new(
        initial: &str,
        options: FieldOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // Perdre le focus (clic ailleurs, autre session choisie) annule, comme Échap.
        let blur = options.cancel_on_blur.then(|| {
            cx.on_blur(&focus_handle, window, |_, _, cx| {
                cx.emit(RenameEvent::Cancel)
            })
        });
        let mut buffer = Buffer::new(initial);
        if !options.select_initial {
            buffer.deselect();
        }
        Self {
            buffer,
            options,
            focus_handle,
            _blur: blur,
        }
    }

    /// Sélectionne `range` (en octets, ramenée aux limites de caractère) : l'arbre ouvre le nom d'un fichier sans son
    /// extension. Le point d'insertion passe à la fin de la sélection.
    pub fn select_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        self.buffer.select_range(range);
        cx.notify();
    }

    /// Tout le texte redevient sélectionné : la frappe suivante le remplace (second ⌘F de la recherche).
    pub fn select_everything(&mut self, cx: &mut Context<Self>) {
        self.buffer.select_everything();
        cx.notify();
    }

    /// Le texte a changé : le champ se repeint et, s'il le demande, rend le nouveau texte.
    fn edited(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        if self.options.emit_changes {
            cx.emit(RenameEvent::Changed(self.buffer.full()));
        }
    }

    fn backspace(&mut self, _: &RenameBackspace, _window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.backspace();
        self.edited(cx);
    }

    /// `⌘⌫` : efface du début au point d'insertion, ou la sélection.
    fn delete_to_start(
        &mut self,
        _: &RenameDeleteToStart,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.delete_to_start();
        self.edited(cx);
    }

    fn move_left(&mut self, _: &RenameMoveLeft, _window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_left();
        cx.notify();
    }

    fn move_right(&mut self, _: &RenameMoveRight, _window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_right();
        cx.notify();
    }

    fn confirm(&mut self, _: &RenameConfirm, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(RenameEvent::Confirm(self.buffer.result()));
    }

    fn cancel(&mut self, _: &RenameCancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(RenameEvent::Cancel);
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.buffer.insert(&text);
            self.edited(cx);
        }
    }

    /// `⌘A` : tout le nom redevient sélectionné, comme à l'ouverture.
    fn select_all(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_everything(cx);
    }
}

impl Focusable for RenameField {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RenameField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        let full = self.buffer.full();
        let is_empty = full.is_empty();
        let split = self.buffer.visual_cursor();
        let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        if let Some(selection) = &self.buffer.selection {
            highlights.push((
                selection.clone(),
                HighlightStyle {
                    background_color: Some(
                        Rgba {
                            a: SELECTION_ALPHA,
                            ..color(FOCUS)
                        }
                        .into(),
                    ),
                    ..Default::default()
                },
            ));
        }
        if self.buffer.marked.is_some() {
            highlights.push((
                self.buffer.cursor..split,
                HighlightStyle {
                    underline: Some(UnderlineStyle {
                        thickness: px(1.),
                        color: None,
                        wavy: false,
                    }),
                    ..Default::default()
                },
            ));
        }
        // Le trait du curseur sépare le texte en deux : la sélection et la composition finissent toutes deux au point.
        let after = full[split..].to_owned();
        let before = full[..split].to_owned();
        let focused = self.focus_handle.is_focused(window);
        let entity = cx.entity();
        let focus_handle = self.focus_handle.clone();

        div()
            .key_context(self.options.key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete_to_start))
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::select_all))
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .overflow_hidden()
            .text_size(theme.text(self.options.text_size))
            .text_color(white(TEXT_FULL))
            .child(StyledText::new(before).with_highlights(highlights))
            // Le curseur : un trait au point d'insertion, absent tant qu'une sélection est affichée.
            .when(focused && self.buffer.selection.is_none(), |field| {
                field.child(
                    div()
                        .flex_none()
                        .w(px(1.5))
                        .h(theme.px(15.))
                        .bg(color(FOCUS)),
                )
            })
            .child(StyledText::new(after))
            .when_some(
                self.options.placeholder.filter(|_| is_empty),
                |field, placeholder| {
                    field.child(
                        div()
                            .flex_none()
                            .text_color(white(TEXT_MIN))
                            .child(placeholder),
                    )
                },
            )
            // Le gestionnaire de saisie se pose à la peinture, comme dans `examples/input.rs` de GPUI.
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        window.handle_input(
                            &focus_handle,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                    },
                )
                .absolute()
                .size_full(),
            )
    }
}

impl EntityInputHandler for RenameField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let full = self.buffer.full();
        let (start, end) = (
            byte_offset(&full, range.start),
            byte_offset(&full, range.end),
        );
        adjusted_range.replace(utf16_len(&full[..start])..utf16_len(&full[..end]));
        Some(full[start..end].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.buffer.selected_utf16(),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.buffer.marked_utf16()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.unmark();
        self.edited(cx);
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.insert(text);
        self.edited(cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.mark(new_text);
        self.edited(cx);
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // La fenêtre de candidats de l'IME s'ancre sous le champ.
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// Crée le champ de renommage et lui donne le focus.
pub fn open(initial: &str, window: &mut Window, cx: &mut App) -> Entity<RenameField> {
    open_with(initial, FieldOptions::RENAME, window, cx)
}

/// Crée un champ aux `options` données et lui donne le focus.
pub fn open_with(
    initial: &str,
    options: FieldOptions,
    window: &mut Window,
    cx: &mut App,
) -> Entity<RenameField> {
    let field = cx.new(|cx| RenameField::new(initial, options, window, cx));
    window.focus(&field.focus_handle(cx), cx);
    field
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_keystroke_replaces_the_whole_name() {
        let mut buffer = Buffer::new("Claude 2");
        assert!(buffer.selection.is_some());
        buffer.insert("R");
        buffer.insert("evue");
        assert_eq!(buffer.result(), "Revue");
        assert!(buffer.selection.is_none());
    }

    #[test]
    fn selecting_everything_again_makes_the_next_keystroke_replace_the_edited_name() {
        let mut buffer = Buffer::new("Claude");
        buffer.insert("Revue");
        assert!(buffer.selection.is_none());
        buffer.select_everything();
        buffer.insert("Tests");
        assert_eq!(buffer.result(), "Tests");
    }

    #[test]
    fn backspace_on_the_initial_selection_clears_everything_then_erases_one_character() {
        let mut buffer = Buffer::new("Shell");
        buffer.backspace();
        assert_eq!(buffer.full(), "");
        buffer.insert("abé");
        buffer.backspace();
        assert_eq!(buffer.full(), "ab");
    }

    #[test]
    fn control_characters_are_stripped_from_typed_and_pasted_text() {
        let mut buffer = Buffer::new("x");
        buffer.insert("a\nb\tc\u{1b}d\r");
        assert_eq!(buffer.full(), "abcd");
        assert_eq!(Buffer::new("a\nb").full(), "ab");
    }

    #[test]
    fn a_blank_name_trims_to_nothing() {
        let mut buffer = Buffer::new("x");
        buffer.insert("  \t \n ");
        assert_eq!(buffer.result(), "");
    }

    #[test]
    fn a_composition_is_shown_but_only_committed_by_the_ime() {
        let mut buffer = Buffer::new("Nom");
        buffer.mark("に");
        assert_eq!(buffer.full(), "に");
        assert_eq!(buffer.text, "");
        buffer.mark("にほ");
        buffer.insert("日本");
        assert_eq!(buffer.full(), "日本");
        assert!(buffer.marked.is_none());
    }

    #[test]
    fn accepting_a_composition_keeps_it_and_an_empty_one_clears_it() {
        let mut buffer = Buffer::new("");
        buffer.insert("a");
        buffer.mark("é");
        buffer.unmark();
        assert_eq!(buffer.text, "aé");
        buffer.mark("x");
        buffer.mark("");
        assert!(buffer.marked.is_none());
    }

    #[test]
    fn backspace_leaves_a_composition_to_the_ime() {
        let mut buffer = Buffer::new("");
        buffer.insert("ab");
        buffer.mark("c");
        buffer.backspace();
        assert_eq!(buffer.full(), "abc");
    }

    #[test]
    fn utf16_offsets_map_back_to_byte_offsets() {
        let text = "a😀b";
        assert_eq!(utf16_len(text), 4);
        assert_eq!(byte_offset(text, 0), 0);
        assert_eq!(byte_offset(text, 1), 1);
        assert_eq!(byte_offset(text, 3), 5);
        assert_eq!(byte_offset(text, 99), text.len());
    }

    #[test]
    fn typing_replaces_the_stem_and_keeps_the_extension() {
        let mut buffer = Buffer::new("brouillon.md");
        buffer.select_range(0..9);
        buffer.insert("draft");
        assert_eq!(buffer.full(), "draft.md");
        buffer.insert("s");
        assert_eq!(buffer.full(), "drafts.md");
        assert!(buffer.selection.is_none());
    }

    #[test]
    fn backspace_edits_before_the_extension() {
        let mut buffer = Buffer::new("draft.md");
        buffer.select_range(5..5);
        buffer.backspace();
        assert_eq!(buffer.full(), "draf.md");
        assert_eq!(buffer.cursor, 4);
    }

    #[test]
    fn backspace_on_a_partial_selection_removes_only_it() {
        let mut buffer = Buffer::new("brouillon.md");
        buffer.select_range(0..9);
        buffer.backspace();
        assert_eq!(buffer.full(), ".md");
        assert_eq!(buffer.cursor, 0);
    }

    #[test]
    fn composition_happens_at_the_insertion_point() {
        let mut buffer = Buffer::new("brouillon.md");
        buffer.select_range(0..9);
        buffer.insert("draft");
        buffer.mark("e");
        assert_eq!(buffer.full(), "drafte.md");
        buffer.insert("é");
        assert_eq!(buffer.full(), "drafté.md");
    }

    #[test]
    fn select_all_after_a_partial_selection_takes_the_whole_name() {
        let mut buffer = Buffer::new("brouillon.md");
        buffer.select_range(0..9);
        buffer.select_everything();
        buffer.insert("x");
        assert_eq!(buffer.full(), "x");
    }

    #[test]
    fn delete_to_start_keeps_what_follows() {
        let mut buffer = Buffer::new("draft.md");
        buffer.select_range(5..5);
        buffer.delete_to_start();
        assert_eq!(buffer.full(), ".md");
        assert_eq!(buffer.cursor, 0);
    }

    #[test]
    fn delete_to_start_on_a_selection_deletes_the_selection() {
        let mut buffer = Buffer::new("brouillon.md");
        buffer.select_range(0..9);
        buffer.delete_to_start();
        assert_eq!(buffer.full(), ".md");
    }

    #[test]
    fn a_range_inside_a_character_is_widened_to_it() {
        let mut buffer = Buffer::new("é.md");
        buffer.select_range(0..1);
        assert_eq!(buffer.selection, Some(0..2));
        assert_eq!(buffer.cursor, 2);
    }

    #[test]
    fn the_ime_sees_the_selection_in_utf16() {
        let mut buffer = Buffer::new("é.md");
        buffer.select_range(0..2);
        assert_eq!(buffer.selected_utf16(), 0..1);
        assert_eq!(buffer.marked_utf16(), None);
    }

    #[test]
    fn the_ime_sees_the_composition_at_the_insertion_point() {
        let mut buffer = Buffer::new("é.md");
        buffer.select_range(2..2);
        buffer.mark("ab");
        assert_eq!(buffer.selected_utf16(), 1..1);
        assert_eq!(buffer.marked_utf16(), Some(1..3));
    }

    #[test]
    fn arrows_move_by_one_character_and_collapse_a_selection() {
        let mut buffer = Buffer::new("é.md");
        buffer.select_range(2..2);
        assert_eq!(buffer.cursor, 2);
        buffer.move_left();
        assert_eq!(buffer.cursor, 0);
        buffer.move_left();
        assert_eq!(buffer.cursor, 0);
        buffer.move_right();
        assert_eq!(buffer.cursor, 2);
        // Une sélection tombe vers son bord : la fin pour la droite, le début pour la gauche.
        buffer.select_range(0..2);
        buffer.move_right();
        assert_eq!((buffer.cursor, buffer.selection.clone()), (2, None));
        buffer.select_range(0..2);
        buffer.move_left();
        assert_eq!((buffer.cursor, buffer.selection.clone()), (0, None));
        // Une frappe se place au point d'insertion.
        buffer.insert("x");
        assert_eq!(buffer.full(), "xé.md");
        // Le point s'arrête à la fin du texte.
        buffer.select_range(100..100);
        buffer.move_right();
        assert_eq!(buffer.cursor, buffer.text.len());
    }
}
