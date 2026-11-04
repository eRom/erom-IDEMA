//! Sonde jetable de l'éditeur de gpui-component (spec P2 §3.1) : une fenêtre GPUI sans `Workspace` ni `Root`.
//! Usage : `cargo run -p idema --example editor_probe -- <fichier.yml>`.
//! Elle vérifie à l'œil et sur stderr trois points : coloration et barre ⌘F sans `Root`, clic droit interceptable
//! (aucun menu natif), frappe de caractères accentués.

// Le vrai repli d'assets de l'app (icônes d'IdemA, puis icônes de gpui-component).
#[allow(dead_code)]
#[path = "../src/assets.rs"]
mod assets;

use gpui::{
    App, AppContext, Bounds, Context, Entity, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Render, Styled, Subscription, Window, WindowBounds, WindowOptions, div, px,
    size,
};
use gpui_component::input::{Editor, EditorState, InputEvent};
use gpui_component::{ActiveTheme, Theme, ThemeMode};

struct Probe {
    state: Entity<EditorState>,
    _subscription: Subscription,
}

impl Probe {
    fn new(text: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("yaml")
                .context_menu(false);
            state.set_value(text, window, cx);
            state
        });
        let subscription =
            cx.subscribe_in(&state, window, |_, state, event: &InputEvent, _, cx| {
                let length = state.read(cx).value().len();
                let name = match event {
                    InputEvent::Change => "Change",
                    InputEvent::PressEnter { .. } => "PressEnter",
                    InputEvent::Focus => "Focus",
                    InputEvent::Blur => "Blur",
                };
                eprintln!("probe input event={name} length={length}");
            });
        // Le focus clavier doit partir dans l'éditeur dès l'ouverture.
        state.update(cx, |state, cx| state.focus(window, cx));
        Self {
            state,
            _subscription: subscription,
        }
    }
}

impl Render for Probe {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_mouse_down(MouseButton::Right, |_, _, _| {
                eprintln!("probe right-click intercepted");
            })
            .child(Editor::new(&self.state).h(px(860.)))
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: editor_probe <file.yml>");
    let text = std::fs::read_to_string(&path).expect("cannot read the probe file");
    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            Theme::change(ThemeMode::Dark, None, cx);
            let bounds = Bounds::centered(None, size(px(1000.), px(900.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Probe::new(text, window, cx)),
            )
            .expect("failed to open the probe window");
            cx.on_window_closed(|cx, _window_id| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}
