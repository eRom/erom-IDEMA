//! Idema : les sessions d'agents en vue principale.

mod artifacts;
mod assets;
mod bootstrap;
mod browser;
mod changes;
mod column;
mod commit_message;
mod companion;
mod config;
mod dock;
mod errors;
mod file_open;
mod files;
mod git;
mod gutter;
mod help_bubble;
mod hud;
mod ideation;
mod idnma;
mod instance;
mod integrations;
mod lifecycle;
mod machines;
mod mcp_widgets;
mod menus;
mod pane_view;
mod popover;
mod project;
mod project_panel;
mod reclaim;
mod rename_field;
mod restore;
mod scratchpad;
mod search_bar;
mod servers;
mod session_view;
mod settings;
mod sidebar;
mod sidebar_model;
mod signpost;
mod state;
mod terminal_element;
mod terminal_panel;
mod theme;
mod workspace;

use std::cell::Cell;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::rc::Rc;

use futures::StreamExt;

use gpui::{
    Action, App, AppContext, Bounds, KeyBinding, KeyBindingContextPredicate,
    PlatformKeyboardMapper, TitlebarOptions, WindowBounds, WindowId, WindowOptions, point, px,
    size,
};

use crate::assets::Assets;
use crate::files::panel::ToggleTree;
use crate::hud::{DumpGrid, ToggleHud};
use crate::pane_view::KEY_CONTEXT;
use crate::rename_field::{
    RenameBackspace, RenameCancel, RenameConfirm, RenameDeleteToStart, RenameMoveLeft,
    RenameMoveRight,
};
use crate::search_bar::{CloseSearch, OpenSearch, SearchNext, SearchPrevious};
use crate::session_view::{FocusPaneDown, FocusPaneLeft, FocusPaneRight, FocusPaneUp, ToggleZoom};
use crate::workspace::{
    CloseMenu, CloseSession, DecreaseFontSize, IncreaseFontSize, KEY_CONTEXT as WORKSPACE_CONTEXT,
    NewProject, NewSession, NextSession, OpenConfig, OpenFile, Paths, PreviousSession,
    ResetFontSize, SelectSession1, SelectSession2, SelectSession3, SelectSession4, SelectSession5,
    SelectSession6, SelectSession7, SelectSession8, SelectSession9, ToggleBrowser, ToggleChanges,
    ToggleFiles, ToggleScratchpad, ToggleSidebar, ToggleTerminal, Workspace,
};

/// Nom de l'app tel qu'une personne le lit (menu, titre de fenêtre, message de seconde instance). Les identifiants
/// techniques restent `idema`.
pub const APP_NAME: &str = "IdemA";

/// Crée le dossier d'Idema en 0700 au besoin : Idema n'écrit nulle part ailleurs.
fn create_config_dir(dir: &Path) -> std::io::Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// `⌘0` à `⌘9`, avec les équivalents de touche de la disposition courante : sur un clavier AZERTY, la touche du 2
/// donne `é` sans Maj, et GPUI ne rapporterait `cmd-2` qu'avec Maj. Sur QWERTY la table est vide et `cmd-1` reste `cmd-1`.
fn digit_shortcuts(mapper: &dyn PlatformKeyboardMapper) -> Vec<KeyBinding> {
    let context = Rc::new(
        KeyBindingContextPredicate::parse(WORKSPACE_CONTEXT).expect("a valid context predicate"),
    );
    let actions: [(&str, Box<dyn Action>); 10] = [
        ("cmd-0", Box::new(ResetFontSize)),
        ("cmd-1", Box::new(SelectSession1)),
        ("cmd-2", Box::new(SelectSession2)),
        ("cmd-3", Box::new(SelectSession3)),
        ("cmd-4", Box::new(SelectSession4)),
        ("cmd-5", Box::new(SelectSession5)),
        ("cmd-6", Box::new(SelectSession6)),
        ("cmd-7", Box::new(SelectSession7)),
        ("cmd-8", Box::new(SelectSession8)),
        ("cmd-9", Box::new(SelectSession9)),
    ];
    actions
        .into_iter()
        .map(|(keystroke, action)| {
            KeyBinding::load(keystroke, action, Some(context.clone()), true, None, mapper)
                .expect("a valid keystroke")
        })
        .collect()
}

fn main() {
    // `idema --version` répond sans ouvrir de fenêtre : c'est ainsi que le pipeline de release lit la version du
    // binaire livré.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--version")
    {
        println!("{}", menus::version_line());
        return;
    }
    hud::mark_process_start();
    let Some(dir) = instance::config_dir(std::env::var_os("HOME"), std::env::var_os("IDEMA_HOME"))
    else {
        eprintln!("HOME is not set");
        std::process::exit(1);
    };
    if let Err(error) = create_config_dir(&dir) {
        eprintln!("cannot create {}: {error}", dir.display());
        std::process::exit(1);
    }
    // Le verrou vit jusqu'à la sortie de `main` : une seconde instance repart aussitôt (spec §3.1).
    let _instance_lock = match instance::acquire(&dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("{APP_NAME} is already running");
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("cannot take the instance lock: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = hud::init_journal(&dir) {
        // Pas de journal ne doit pas empêcher d'ouvrir le terminal.
        eprintln!("cannot open the journal: {error}");
    }
    // Après une mise à jour de l'app, la commande `idema` déjà posée reprend la version de ce binaire. Une instance
    // isolée (`IDEMA_HOME`, tests d'acceptation) ne touche pas à la commande de l'utilisateur.
    if std::env::var_os("IDEMA_HOME").is_none()
        && let Some(home) = files::panel::home_dir()
    {
        match integrations::refresh_cli(&home) {
            Ok(true) => hud::log_event("integrations cli refreshed"),
            Ok(false) => {}
            Err(error) => hud::log_event(&format!("integrations cli refresh failed: {error}")),
        }
    }
    let socket = instance::tmux_socket(std::env::var_os("IDEMA_TMUX_SOCKET"));
    // tmux est interrogé pendant que GPUI crée la fenêtre (P-1) : la barre n'attend plus que lui.
    let bootstrap = bootstrap::spawn(dir.clone(), socket);

    // Les dossiers que macOS demande d'ouvrir (`open -b`, CLI `idema`, action rapide du Finder) arrivent hors de
    // l'app, parfois avant la fenêtre : ils attendent dans ce canal que la fenêtre les prenne.
    let (open_sender, mut open_requests) = futures::channel::mpsc::unbounded::<Vec<String>>();
    let application = gpui_platform::application().with_assets(Assets);
    application.on_open_urls(move |urls| {
        let _ = open_sender.unbounded_send(urls);
    });
    application.run(move |cx: &mut App| {
        cx.bind_keys(digit_shortcuts(cx.keyboard_mapper().as_ref()));
        // `⌘0` du zoom de la page et les crochets de l'historique et des onglets, liés avec les équivalents de la
        // disposition (`⌘0` est `⌘à` en AZERTY). `⌘0` vient après celui de la taille du texte : le contexte `Browser`
        // est plus profond.
        cx.bind_keys(browser::panel::layout_key_bindings(
            cx.keyboard_mapper().as_ref(),
        ));
        // Thème, liaisons et calques de l'éditeur du volet Fichiers (P2). Le thème d'IdemA pour l'éditeur est posé
        // par `Workspace::new` (`apply_editor_theme`), une fois la config lue : il dépend de la taille du texte.
        gpui_component::init(cx);
        // Swift et TSX, que le kit livre sans coloration complète.
        files::grammars::register_code_grammars();
        // Celles du volet Fichiers : `⌘W` y ferme l'onglet, pas la session, parce que le contexte `Files` est plus
        // profond que `Workspace` ; `⌘⌥F` (remplacer), le bouclier de `⌘⇧F` et `⌘⇧F` lui-même (la recherche dans le projet,
        // de partout) y passent après celles du kit, où la dernière liée gagne à profondeur égale.
        cx.bind_keys(files::panel::key_bindings());
        // Celles du Scratchpad, APRÈS celles de Fichiers : toutes deux lient `⌘⌥C` à la même profondeur et la dernière liée
        // gagne (le Scratchpad la prend dans son éditeur, Fichiers garde le reste).
        cx.bind_keys(scratchpad::panel::key_bindings());
        cx.bind_keys(project_panel::skills_view::key_bindings());
        // Celles de la liste du volet Changes : `↑` et `↓` y déplacent la sélection.
        cx.bind_keys(changes::panel::key_bindings());
        // Celles du volet Browser (P4) : `⌘L` y met le focus dans la barre d'adresse.
        cx.bind_keys(browser::panel::key_bindings());
        cx.bind_keys(artifacts::view::key_bindings());
        cx.bind_keys(app_key_bindings());
        cx.bind_keys(settings::view::key_bindings());
        menus::install(cx);
        idnma::refresh(cx);
        // Les lignes encore dans le tampon du journal (mode debug) partent avant la sortie.
        cx.on_app_quit(|_| async { hud::flush() }).detach();
        // Fermer la fenêtre principale quitte l'app, et la fenêtre « Personnaliser » avec elle : elle ne vit jamais
        // seule. Fermer « Personnaliser » seule ne touche pas la principale. Sans fenêtre, ⌘Q n'aurait plus de focus
        // pour répondre : la dernière fermée quitte aussi.
        let main_window = Rc::new(Cell::new(None::<WindowId>));
        let closing_main = main_window.clone();
        cx.on_window_closed(move |cx, window_id| {
            if closing_main.get() == Some(window_id) || cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        let workspace = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(APP_NAME.into()),
                        appears_transparent: true,
                        // 19 : le bouton de 14 px centré sur la bande de 52 px.
                        traffic_light_position: Some(point(px(20.), px(19.))),
                    }),
                    window_min_size: Some(size(px(900.), px(560.))),
                    // Le déplacement passe par `start_window_move` (bande de titre dessinée par l'app) : AppKit
                    // ne doit pas aussi traiter le glissement et le double-clic de la barre de titre.
                    app_owns_titlebar_drag: true,
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Workspace::new(Paths { dir, bootstrap }, window, cx)),
            )
            .expect("failed to open the main window");
        main_window.set(Some(workspace.window_id()));
        // Sonde de la phase Artifacts : une lecture de la liste claude.ai, résultat au journal (lignes `artifacts probe`).
        if std::env::var_os("IDEMA_ARTIFACTS_PROBE").is_some_and(|value| value == "1") {
            artifacts::source::run_probe(workspace.into(), cx);
        }
        // Sonde de la création : crée UN Doc vide sur le compte claude.ai (lignes `artifacts create probe`).
        if std::env::var_os("IDEMA_ARTIFACTS_CREATE_PROBE").is_some_and(|value| value == "1") {
            artifacts::source::run_create_probe(workspace.into(), cx);
        }
        cx.activate(true);
        cx.spawn(async move |cx| {
            while let Some(urls) = open_requests.next().await {
                let paths = project::opened_paths(&urls);
                let _ = workspace.update(cx, |workspace, window, cx| {
                    workspace.open_paths(paths, window, cx);
                });
            }
        })
        .detach();
    });
}

/// Les liaisons de l'app hors volets (fenêtre, renommage, recherche des panes, gestes de session), dans l'ordre où elles
/// se posent : à profondeur égale, la dernière liée gagne.
fn app_key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-b", ToggleSidebar, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-o", NewProject, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-shift-o", OpenFile, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-n", NewProject, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-t", NewSession, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-w", CloseSession, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-,", OpenConfig, Some(WORKSPACE_CONTEXT)),
        // Taille du texte. Le clavier français tape `+` en `⇧=` : on lie les trois écritures de la touche.
        KeyBinding::new("cmd-=", IncreaseFontSize, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-+", IncreaseFontSize, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-shift-=", IncreaseFontSize, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd--", DecreaseFontSize, Some(WORKSPACE_CONTEXT)),
        // Sans les équivalents de touche de la disposition : avec eux, la table française de GPUI ferait de
        // `` ` `` la touche `<`. Sans eux, la touche morte du clavier français se résout en `` ` ``.
        KeyBinding::new("ctrl-`", ToggleTerminal, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-p", ToggleFiles, Some(WORKSPACE_CONTEXT)),
        // `⇧⌘G` : Changes, de partout. Dans un pane dont la barre `⌘F` est fermée, `search_previous` laisse passer la
        // touche (`cx.propagate()`) ; barre ouverte, elle reste « précédent » (les liaisons de `KEY_CONTEXT` plus bas).
        KeyBinding::new("cmd-shift-g", ToggleChanges, Some(WORKSPACE_CONTEXT)),
        // `⇧⌘B` : le navigateur, de partout.
        KeyBinding::new("cmd-shift-b", ToggleBrowser, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-shift-e", ToggleTree, Some(WORKSPACE_CONTEXT)),
        // `⌘J` : le Scratchpad, de partout.
        KeyBinding::new("cmd-j", ToggleScratchpad, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(
            "backspace",
            RenameBackspace,
            Some(rename_field::KEY_CONTEXT),
        ),
        KeyBinding::new(
            "cmd-backspace",
            RenameDeleteToStart,
            Some(rename_field::KEY_CONTEXT),
        ),
        KeyBinding::new("enter", RenameConfirm, Some(rename_field::KEY_CONTEXT)),
        KeyBinding::new("escape", RenameCancel, Some(rename_field::KEY_CONTEXT)),
        KeyBinding::new("left", RenameMoveLeft, Some(rename_field::KEY_CONTEXT)),
        KeyBinding::new("right", RenameMoveRight, Some(rename_field::KEY_CONTEXT)),
        KeyBinding::new("escape", CloseMenu, Some(popover::KEY_CONTEXT)),
        // Recherche : `⌘F` ouvre la barre depuis le pane ou depuis son champ (second appui : le champ reprend le
        // focus et son texte est sélectionné) ; `⌘G` va au suivant, `⇧⌘G` au précédent, des deux endroits.
        KeyBinding::new("cmd-f", OpenSearch, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-f", OpenSearch, Some(search_bar::KEY_CONTEXT)),
        KeyBinding::new("cmd-g", SearchNext, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-g", SearchNext, Some(search_bar::KEY_CONTEXT)),
        KeyBinding::new("cmd-shift-g", SearchPrevious, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-shift-g", SearchPrevious, Some(search_bar::KEY_CONTEXT)),
        KeyBinding::new("enter", SearchNext, Some(search_bar::KEY_CONTEXT)),
        KeyBinding::new("shift-enter", SearchPrevious, Some(search_bar::KEY_CONTEXT)),
        KeyBinding::new("escape", CloseSearch, Some(search_bar::KEY_CONTEXT)),
        KeyBinding::new("backspace", RenameBackspace, Some(search_bar::KEY_CONTEXT)),
        // Ces liaisons passent avant `on_key_down` de la vue : le terminal ne reçoit jamais ces touches.
        KeyBinding::new("ctrl-tab", NextSession, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PreviousSession, Some(WORKSPACE_CONTEXT)),
        // `⌘⌥` + flèche change de pane, même dans une session à un seul pane : le programme ne les reçoit pas.
        KeyBinding::new("cmd-alt-left", FocusPaneLeft, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-alt-right", FocusPaneRight, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-alt-up", FocusPaneUp, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-alt-down", FocusPaneDown, Some(KEY_CONTEXT)),
        // `⌘⇧↩` zoome le pane actif par le zoom de tmux, ou rétablit la fenêtre.
        KeyBinding::new("cmd-shift-enter", ToggleZoom, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-alt-cmd-h", ToggleHud, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-alt-cmd-d", DumpGrid, Some(KEY_CONTEXT)),
        // ⌃⌥⌘D n'arrive pas à l'app sur ce Mac (raccourci probablement réservé par macOS) : seconde touche.
        KeyBinding::new("ctrl-alt-cmd-g", DumpGrid, Some(KEY_CONTEXT)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{DummyKeyboardMapper, KeyContext, Keymap, Keystroke};

    #[test]
    fn digit_shortcuts_keep_their_qwerty_spelling_without_key_equivalents() {
        let bindings = digit_shortcuts(&DummyKeyboardMapper);
        let keys: Vec<&str> = bindings
            .iter()
            .map(|binding| binding.keystrokes()[0].inner().key.as_str())
            .collect();
        assert_eq!(keys, ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]);
        assert!(
            bindings
                .iter()
                .all(|binding| binding.keystrokes()[0].inner().modifiers.platform)
        );
    }

    #[test]
    fn cmd_j_toggles_the_scratchpad_from_the_workspace() {
        let keymap = Keymap::new(app_key_bindings());
        let stack = [KeyContext::parse(WORKSPACE_CONTEXT).unwrap()];
        let keystrokes = [Keystroke::parse("cmd-j").unwrap()];
        let (found, _) = keymap.bindings_for_input(&keystrokes, &stack);
        assert!(
            found
                .first()
                .is_some_and(|binding| binding.action().partial_eq(&ToggleScratchpad))
        );
    }
}
