//! L'ouverture de la fenêtre « Personnaliser » : une seule instance, ramenée au premier plan quand elle existe.

use std::path::PathBuf;

use gpui::{
    App, AppContext, Bounds, Entity, Global, TitlebarOptions, WindowBounds, WindowHandle,
    WindowOptions, point, px, size,
};

use super::nav::Section;
use super::view::SettingsView;
use crate::hud;
use crate::workspace::Workspace;

/// Titre de la fenêtre.
const TITLE: &str = "Personnaliser";

/// La fenêtre « Personnaliser » ouverte, si elle l'est. Une poignée qui pointe une fenêtre fermée échoue à la mise à jour :
/// c'est ainsi qu'on sait qu'il faut en ouvrir une neuve.
#[derive(Default)]
pub struct SettingsWindow(Option<WindowHandle<SettingsView>>);

impl Global for SettingsWindow {}

/// `⌘,`, la ligne « Personnaliser » et le menu : ramène la fenêtre vivante au premier plan, sinon en ouvre une neuve sur
/// « Général ». `section`, quand elle est donnée, est celle qu'on affiche dans les deux cas. `workspace` est la fenêtre principale, que « Éditer config.toml » pilote ; `main` est sa vue, que la
/// fenêtre observe (la fenêtre principale est en pleine mise à jour quand `⌘,` arrive : on ne peut pas l'y relire).
pub fn open_or_focus(
    workspace: WindowHandle<Workspace>,
    main: Entity<Workspace>,
    system_enabled: bool,
    config_path: PathBuf,
    section: Option<Section>,
    cx: &mut App,
) {
    let open = cx
        .try_global::<SettingsWindow>()
        .and_then(|global| global.0);
    if let Some(handle) = open
        && handle
            .update(cx, |view, window, cx| {
                window.activate_window();
                if let Some(section) = section {
                    view.select(section, cx);
                }
            })
            .is_ok()
    {
        return;
    }
    let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // Même habillage que la fenêtre principale (`main.rs`) : titre transparent, feux sur la bande de 52.
            titlebar: Some(TitlebarOptions {
                title: Some(TITLE.into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(20.), px(19.))),
            }),
            window_min_size: Some(size(px(1320.), px(700.))),
            app_owns_titlebar_drag: true,
            ..Default::default()
        },
        |window, cx| {
            cx.new(|cx| {
                let mut view =
                    SettingsView::new(workspace, main, system_enabled, config_path, window, cx);
                if let Some(section) = section {
                    view.select(section, cx);
                }
                view
            })
        },
    );
    match opened {
        Ok(handle) => {
            cx.set_global(SettingsWindow(Some(handle)));
            cx.activate(true);
        }
        Err(error) => hud::log_event(&format!("settings window failed: {error}")),
    }
}
