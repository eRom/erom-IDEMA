//! Le volet Scratchpad : une entité qui montre la note dans un éditeur Markdown. Elle ne porte que la colle GPUI
//! (minuteries, éditeur, focus, événements) ; ce qui touche le disque est dans `store.rs` et se teste sans fenêtre.
//! Les minuteries et le rendu se vérifient dans la VM (jalon 4 du plan).

use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    MouseDownEvent, Pixels, Point, Subscription, Task, Window, actions, div, prelude::*,
};
use gpui_component::input::{Editor, EditorState, InputEvent};

use super::rules::clamp_offset;
pub use super::store::{CopyMode, NoteOptions};
use super::store::{ScratchpadStore, Verdict, WriteOutcome, copy_payload};
use crate::assets::Icon;
use crate::files::gestures::EditorGesture;
use crate::files::line_ops::{Direction, LineEdit, duplicate_lines, move_lines};
use crate::files::panel::{
    AUTOSAVE_DELAY, DuplicateLineDown, DuplicateLineUp, MoveLineDown, MoveLineUp, home_dir,
};
use crate::hud;
use crate::terminal_panel::{panel_header, panel_shell};
use crate::theme::{ERROR_TEXT, PANEL, SEPARATOR, Theme, color, white};
use crate::workspace::Workspace;

actions!(scratchpad, [CopyToAgent, SaveNow]);

/// Contexte de touches du volet : l'éditeur en descend. Ses liaisons doivent se poser après celles de Fichiers (voir
/// `key_bindings`).
pub const KEY_CONTEXT: &str = "Scratchpad";

/// L'éditeur du volet, à l'exclusion des autres champs du kit : l'éditeur du kit porte `Input mode=editor`
/// (`gpui-base-0.7.3/src/input/editor/mod.rs`), un champ du kit non.
const EDITOR_CONTEXT: &str = "Scratchpad > Input && mode == editor";

/// Le rythme de la relecture du disque : 500 ms tiennent l'AC-3 (le texte écrit ailleurs apparaît en moins de 2 s) sans
/// toucher à la surveillance de Fichiers. Une relecture ne coûte qu'un `stat` tant que le fichier n'a pas bougé.
const RELOAD_INTERVAL: Duration = Duration::from_millis(500);

/// Les liaisons du volet, à poser APRÈS `files::panel::key_bindings`. `⌘⌥C` envoie la sélection à l'agent, dans l'éditeur
/// du Scratchpad seulement : Fichiers lie la sienne au contexte `Input` tout entier, au même niveau de profondeur que
/// celle-ci, et c'est la dernière liée qui gagne. Le champ de la recherche `⌘F` de l'éditeur (`SearchPanel > Input`,
/// `gpui-component-0.7.3/src/input/search.rs`) est un `Input` sans `mode=editor` : il n'y prend pas le geste. `⌘S`
/// écrit tout de suite. Les gestes de lignes (`⌥↑` `⌥↓` `⇧⌥↑` `⇧⌥↓`) sont liés par Fichiers au contexte de tout éditeur : ils
/// arrivent donc aussi ici, et le volet en tient les actions.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-alt-c", CopyToAgent, Some(EDITOR_CONTEXT)),
        KeyBinding::new("cmd-s", SaveNow, Some(KEY_CONTEXT)),
    ]
}

/// Ce que le volet apprend à `Workspace`.
#[derive(Debug, Clone, PartialEq)]
pub enum ScratchpadEvent {
    /// Le volet (ou l'éditeur) a gagné ou perdu le focus : le clavier change peut-être de côté.
    FocusChanged,
    /// « Copier vers l'agent » : la sélection, telle quelle (sans espace ni retour à la ligne de plus).
    CopyToAgent(String),
    /// Un clic droit dans l'éditeur : `Workspace` ouvre le menu. Le curseur et la sélection n'ont pas bougé ;
    /// `has_selection` dit si Couper, Copier et « Copier vers l'agent » ont de quoi travailler.
    EditorMenu {
        position: Point<Pixels>,
        has_selection: bool,
    },
}

pub struct ScratchpadPanel {
    store: ScratchpadStore,
    options: NoteOptions,
    editor: Entity<EditorState>,
    /// Le corps du volet : `contains_focused` en est la mesure, l'éditeur en descend.
    focus_handle: FocusHandle,
    /// La minuterie de la sauvegarde d'une seconde ; la remplacer à chaque frappe annule la précédente.
    _save_timer: Option<Task<()>>,
    _poll_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ScratchpadEvent> for ScratchpadPanel {}

impl ScratchpadPanel {
    /// Le volet de la note `path`. Le fichier est lu ici, jamais créé : il naît à la première frappe sauvegardée.
    pub fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_options(path, NoteOptions::scratchpad(), window, cx)
    }

    /// `new`, pour une note qui a ses propres messages, son journal et son « Copier vers l'agent » (`AGENTS.md` du volet
    /// Projet).
    pub fn with_options(
        path: PathBuf,
        options: NoteOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (store, text) = ScratchpadStore::open_with(path, &options);
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("markdown")
                .soft_wrap(true)
                .line_number(false)
                .context_menu(false);
            state.set_value(text, window, cx);
            state
        });
        let focus_handle = cx.focus_handle();
        let subscriptions = vec![
            cx.subscribe_in(
                &editor,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => this.on_edit(window, cx),
                    InputEvent::Blur => this.write_now(window, cx),
                    InputEvent::Focus | InputEvent::PressEnter { .. } => {}
                },
            ),
            cx.on_focus_in(&focus_handle, window, |_, _, cx| {
                cx.emit(ScratchpadEvent::FocusChanged);
            }),
            cx.on_focus_out(&focus_handle, window, |_, _, _, cx| {
                cx.emit(ScratchpadEvent::FocusChanged);
            }),
        ];
        let background = cx.background_executor().clone();
        let poll_task = cx.spawn_in(window, async move |this, cx| {
            loop {
                background.timer(RELOAD_INTERVAL).await;
                if this
                    .update_in(cx, |panel, window, cx| panel.poll_disk(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            store,
            options,
            editor,
            focus_handle,
            _save_timer: None,
            _poll_task: poll_task,
            _subscriptions: subscriptions,
        }
    }

    /// Donne le focus à l'éditeur.
    pub fn focus_editor(&self, window: &mut Window, cx: &mut App) {
        self.editor.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Le focus est-il dans le volet (lui-même ou l'éditeur) ?
    pub fn contains_focused(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    /// Exécute un geste du menu de l'éditeur. Le menu a pris le focus : l'éditeur le reprend d'abord, puis l'action du
    /// kit part vers lui, comme celles du menu « Édition ».
    pub fn run_editor_gesture(
        &mut self,
        gesture: EditorGesture,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_editor(window, cx);
        match gesture {
            EditorGesture::CopyToAgent => self.copy_to_agent(cx),
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

    /// Écrit tout de suite ce qui attend, sans fenêtre (`⌘Q`, fermeture du volet) : un disque qui a changé est journalisé
    /// et abandonné, le disque gagne. `reason` est celle du journal (« quit », « hide »). Rend faux quand le texte n'a pas
    /// pu partir (la fusion d'`AGENTS.md` ne continue pas dans ce cas).
    pub fn flush(&mut self, reason: &str, cx: &mut App) -> bool {
        let text = self.editor.read(cx).value();
        self.store.flush(&text, reason)
    }

    /// Relit le disque tout de suite, sans attendre la minuterie : après la fusion, l'éditeur reprend `AGENTS.md`.
    pub fn reload_from_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.poll_disk(window, cx);
    }

    /// « Copier vers l'agent » : ce que `options.copy` dit d'envoyer (la sélection pour le Scratchpad, la référence du
    /// fichier pour `AGENTS.md`).
    fn copy_to_agent(&mut self, cx: &mut Context<Self>) {
        let (text, selection) = self
            .editor
            .read_with(cx, |state, _| (state.value(), state.selected_range()));
        match copy_payload(&self.options, &text, selection, home_dir().as_deref()) {
            Some(payload) => cx.emit(ScratchpadEvent::CopyToAgent(payload)),
            None => hud::log_event(&format!(
                "{} copy skipped reason=no-selection",
                self.options.log_tag
            )),
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
        let selection = self.editor.read(cx).selected_range();
        window.prevent_default();
        cx.emit(ScratchpadEvent::EditorMenu {
            position,
            has_selection: selection.start != selection.end,
        });
    }

    /// Une frappe : la sauvegarde d'une seconde repart de zéro.
    fn on_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let generation = self.store.on_edit();
        let background = cx.background_executor().clone();
        self._save_timer = Some(cx.spawn_in(window, async move |this, cx| {
            background.timer(AUTOSAVE_DELAY).await;
            let _ = this.update_in(cx, |panel, window, cx| {
                if panel.store.due(generation) {
                    panel.write_now(window, cx);
                }
            });
        }));
    }

    /// Écrit le texte de l'éditeur s'il a quelque chose à écrire (`⌘S`, perte du focus, minuterie). Si le disque a changé
    /// sous l'écriture, il gagne : l'éditeur le reprend.
    fn write_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value();
        match self.store.write(&text) {
            WriteOutcome::Idle => {}
            WriteOutcome::Written | WriteOutcome::Failed => cx.notify(),
            WriteOutcome::Conflict(verdict) => self.apply_verdict(verdict, window, cx),
        }
    }

    /// Le rythme de `RELOAD_INTERVAL` : le disque a-t-il changé sous l'éditeur ?
    fn poll_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let buffer = self.editor.read(cx).value();
        let verdict = self.store.poll(&buffer);
        self.apply_verdict(verdict, window, cx);
    }

    /// Applique ce que le disque a dit. Un texte rechargé remplace celui de l'éditeur, dont le curseur et la sélection
    /// sont ramenés dans la nouvelle longueur (`clamp_offset`) et dont le défilement est gardé.
    fn apply_verdict(&mut self, verdict: Verdict, window: &mut Window, cx: &mut Context<Self>) {
        match verdict {
            Verdict::Nothing => return,
            Verdict::Reload { text, .. } => {
                self.editor.update(cx, |state, cx| {
                    let selection = state.selected_range();
                    let scroll = state.scroll_offset();
                    let start = clamp_offset(selection.start, &text);
                    let end = clamp_offset(selection.end, &text);
                    state.set_value(text, window, cx);
                    state.set_selected_range(start..end, cx);
                    state.set_scroll_offset(scroll, cx);
                });
            }
            // Le texte reste dans l'éditeur ; le bandeau (illisible) ou la prochaine frappe (supprimé) font le reste.
            Verdict::Deleted | Verdict::Unreadable => {}
        }
        cx.notify();
    }

    /// Applique à l'éditeur l'édition de lignes que rend `edit` : un remplacement, donc un seul pas d'annulation. La
    /// frappe qui en résulte déclenche la sauvegarde comme n'importe quelle autre.
    fn edit_lines(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&str, std::ops::Range<usize>) -> Option<LineEdit>,
    ) {
        self.editor.update(cx, |state, cx| {
            let Some(edit) = edit(&state.value(), state.selected_range()) else {
                return;
            };
            state.set_selected_range(edit.range, cx);
            state.replace(edit.text, window, cx);
            state.set_selected_range(edit.selection, cx);
        });
    }
}

impl Focusable for ScratchpadPanel {
    /// Là où la saisie doit aller quand `Workspace` rend le focus au volet : l'éditeur.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl Render for ScratchpadPanel {
    /// Le corps sous l'en-tête : le bandeau d'erreur s'il y en a un, puis l'éditeur. Le bandeau reprend la ligne d'erreur
    /// du navigateur (`download_line`, chrome.rs : retrait de 12, texte de 12 en `ERROR_TEXT`, 6 de marge verticale) ; la
    /// maquette n'a pas d'écran d'erreur. Sa hauteur est celle du texte plus ces marges, sans cote propre. Le corps porte
    /// le focus du volet, le contexte de touches et les actions.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        let banner = self.store.banner().map(|message| {
            div()
                .flex_none()
                .px(theme.px(12.))
                .py(theme.px(6.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .bg(color(PANEL))
                .border_b_1()
                .border_color(white(SEPARATOR))
                .text_size(theme.text(12.))
                .text_color(color(ERROR_TEXT))
                .child(message.to_owned())
        });
        div()
            .id("scratchpad-body")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(|this, _: &SaveNow, window, cx| this.write_now(window, cx)))
            .on_action(cx.listener(|this, _: &CopyToAgent, _, cx| this.copy_to_agent(cx)))
            // Le menu « Édition » envoie les actions d'IdemA (`menus::Copy`…), que l'éditeur ne connaît pas : elles
            // remontent jusqu'ici et repartent vers l'éditeur en actions du kit.
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
            // Les gestes de lignes de Fichiers, liés au contexte de tout éditeur : ils arrivent aussi ici.
            .on_action(cx.listener(|this, _: &MoveLineUp, window, cx| {
                this.edit_lines(window, cx, |text, selection| {
                    move_lines(text, selection, Direction::Up)
                });
            }))
            .on_action(cx.listener(|this, _: &MoveLineDown, window, cx| {
                this.edit_lines(window, cx, |text, selection| {
                    move_lines(text, selection, Direction::Down)
                });
            }))
            .on_action(cx.listener(|this, _: &DuplicateLineUp, window, cx| {
                this.edit_lines(window, cx, |text, selection| {
                    Some(duplicate_lines(text, selection, Direction::Up))
                });
            }))
            .on_action(cx.listener(|this, _: &DuplicateLineDown, window, cx| {
                this.edit_lines(window, cx, |text, selection| {
                    Some(duplicate_lines(text, selection, Direction::Down))
                });
            }))
            .children(banner)
            .child(
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
                    .child(Editor::new(&self.editor).bordered(false).size_full()),
            )
    }
}

/// Le volet : en-tête commun aux volets droits (pastille « Scratchpad », sans projet, agrandir, fermer) puis le corps, qui
/// est l'entité `ScratchpadPanel`. `header_inset` est le retrait gauche de l'en-tête. `on_focus`, `on_maximize` et
/// `on_close` sont les gestes de `Workspace` que l'en-tête déclenche.
#[allow(clippy::too_many_arguments)]
pub fn render_panel(
    panel: Entity<ScratchpadPanel>,
    maximized: bool,
    header_inset: Pixels,
    width: Pixels,
    theme: &Theme,
    on_focus: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    on_maximize: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    on_close: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> gpui::Stateful<gpui::Div> {
    let header = panel_header(
        theme,
        Icon::StickyNote,
        "Scratchpad",
        String::new(),
        ("scratchpad-maximize", "scratchpad-close"),
        maximized,
        header_inset,
        on_focus,
        on_maximize,
        on_close,
        cx,
    );
    panel_shell("scratchpad-panel", maximized, width)
        .child(header)
        .child(div().flex_1().min_h_0().child(panel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::KEY_CONTEXT as WORKSPACE_CONTEXT;
    use gpui::{KeyContext, Keymap, Keystroke};

    /// Le keymap de l'app dans l'ordre de `main.rs` : le kit, Fichiers, puis le Scratchpad.
    fn app_keymap() -> Keymap {
        let mut bindings = crate::files::panel::tests::kit_input_bindings();
        bindings.extend(crate::files::panel::key_bindings());
        bindings.extend(key_bindings());
        Keymap::new(bindings)
    }

    fn resolve(keymap: &Keymap, names: &[&str], key: &str) -> Vec<KeyBinding> {
        let stack: Vec<KeyContext> = names
            .iter()
            .map(|name| KeyContext::parse(name).unwrap())
            .collect();
        let keystrokes = [Keystroke::parse(key).unwrap()];
        keymap
            .bindings_for_input(&keystrokes, &stack)
            .0
            .into_iter()
            .collect()
    }

    #[test]
    fn copy_to_agent_is_bound_only_in_the_scratchpad_editor() {
        let keymap = app_keymap();
        // L'éditeur du Scratchpad au focus : le geste est celui du Scratchpad, devant celui de Fichiers (même profondeur).
        let in_editor = resolve(
            &keymap,
            &[WORKSPACE_CONTEXT, KEY_CONTEXT, "Input mode=editor"],
            "cmd-alt-c",
        );
        assert!(in_editor[0].action().partial_eq(&CopyToAgent));
        // Un pane au focus (le contexte `Workspace` seul) : aucune liaison, l'agent garde `⌘⌥C`.
        assert!(resolve(&keymap, &[WORKSPACE_CONTEXT], "cmd-alt-c").is_empty());
        // Le champ de la recherche `⌘F` de l'éditeur est un `Input` frère de l'éditeur, sous `SearchPanel`
        // (`gpui-component-0.7.3/src/input/search.rs:23`) : il ne déclenche pas le geste du Scratchpad.
        let in_search = resolve(
            &keymap,
            &[WORKSPACE_CONTEXT, KEY_CONTEXT, "SearchPanel", "Input"],
            "cmd-alt-c",
        );
        assert!(
            in_search
                .iter()
                .all(|binding| !binding.action().partial_eq(&CopyToAgent)),
            "{in_search:?}"
        );
        // `⌘S` écrit tout de suite, dans le volet seulement.
        let save = resolve(
            &keymap,
            &[WORKSPACE_CONTEXT, KEY_CONTEXT, "Input mode=editor"],
            "cmd-s",
        );
        assert!(save[0].action().partial_eq(&SaveNow));
        assert!(resolve(&keymap, &[WORKSPACE_CONTEXT], "cmd-s").is_empty());
    }

    #[test]
    fn line_moves_work_in_the_scratchpad() {
        let keymap = app_keymap();
        let editor = [WORKSPACE_CONTEXT, KEY_CONTEXT, "Input mode=editor"];
        // Les touches de Fichiers arrivent jusqu'à l'éditeur du Scratchpad, dont le volet tient les actions.
        let cases: [(&str, Box<dyn gpui::Action>); 4] = [
            ("alt-up", Box::new(MoveLineUp)),
            ("alt-down", Box::new(MoveLineDown)),
            ("shift-alt-up", Box::new(DuplicateLineUp)),
            ("shift-alt-down", Box::new(DuplicateLineDown)),
        ];
        for (key, action) in cases {
            let found = resolve(&keymap, &editor, key);
            assert!(found[0].action().partial_eq(action.as_ref()), "{key}");
        }
        // Le geste lui-même : `l1\nl2`, curseur sur la ligne 2, `MoveLineUp` donne `l2\nl1`.
        let mut text = "l1\nl2".to_owned();
        let edit = move_lines(&text, 4..4, Direction::Up).expect("the second line can move up");
        text.replace_range(edit.range, &edit.text);
        assert_eq!(text, "l2\nl1");
        assert_eq!(edit.selection, 1..1);
    }
}
