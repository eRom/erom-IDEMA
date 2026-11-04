//! Les menus standard de l'app (écart E25) : « Idema » (masquer, personnaliser, installer, quitter), « Édition »
//! (couper, copier, coller, tout sélectionner) et « Aide » (signaler un problème, versions d'IdemA et d'IdnmA). Les
//! liaisons de touches sont globales (contexte `None`) pour que la barre de menus affiche leurs équivalents ; les
//! gestionnaires globaux évitent qu'AppKit grise les entrées du menu « Idema » (`validateMenuItem:`).
//!
//! AppKit grise une entrée dont l'action n'a pas de gestionnaire : « Installer le réseau IDNMA » n'en a un (posé par
//! `Workspace`) que si l'installation est permise. « Signaler un problème » ouvre la création d'une issue d'IdemA, et
//! les lignes de version le dépôt GitHub de leur produit, dans le navigateur par défaut.

use gpui::{App, KeyBinding, Menu, MenuItem, OsAction, actions};

use crate::APP_NAME;
use crate::idnma::IdnmaStatus;
use crate::workspace::{InstallIntegrations, OpenConfig};

const IDEMA_REPO_URL: &str = "https://github.com/eRom/erom-IDEMA";
const IDNMA_REPO_URL: &str = "https://github.com/eRom/erom-IDNMA";
const NEW_ISSUE_URL: &str = "https://github.com/eRom/erom-IDEMA/issues/new";

actions!(idema, [Quit, Hide, HideOthers, ShowAll, InstallIdnma]);
actions!(edit, [Cut, Copy, Paste, SelectAll]);
actions!(help, [ReportIssue, OpenIdemaRepo, OpenIdnmaRepo]);

/// « IdemA 0.2.0 » : la version du `Cargo.toml` de l'espace de travail, seule source. `scripts/bundle-app.sh` la
/// recopie dans l'`Info.plist` du bundle ; `idema --version` (binaire et commande posée) dit la même ligne.
pub fn version_line() -> String {
    format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"))
}

/// Lie les touches, branche les gestionnaires globaux puis pose la barre de menus. À appeler après les autres
/// `bind_keys` : l'équivalent clavier d'une entrée vient de la première liaison de son action au moment de
/// `set_menus`, donc `cmd-,` (contexte `Workspace`) doit déjà exister.
pub fn install(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
        KeyBinding::new("cmd-x", Cut, None),
        KeyBinding::new("cmd-c", Copy, None),
        KeyBinding::new("cmd-v", Paste, None),
        KeyBinding::new("cmd-a", SelectAll, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &ReportIssue, cx| cx.open_url(NEW_ISSUE_URL));
    cx.on_action(|_: &OpenIdemaRepo, cx| cx.open_url(IDEMA_REPO_URL));
    cx.on_action(|_: &OpenIdnmaRepo, cx| cx.open_url(IDNMA_REPO_URL));
    rebuild(cx);
}

/// Pose (ou repose) la barre de menus d'après l'état d'IdnmA.
pub fn rebuild(cx: &mut App) {
    let idnma_label = cx.default_global::<IdnmaStatus>().label();
    // Pas d'Annuler ni de Rétablir : GPUI les grise toujours.
    cx.set_menus([
        Menu::new(APP_NAME).items([
            MenuItem::action(format!("Masquer {APP_NAME}"), Hide),
            MenuItem::action("Masquer les autres", HideOthers),
            MenuItem::action("Tout afficher", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Personnaliser…", OpenConfig),
            MenuItem::action("Installer le réseau IDNMA", InstallIdnma),
            MenuItem::action(
                "Installer la commande idema et l'action du Finder",
                InstallIntegrations,
            ),
            MenuItem::separator(),
            MenuItem::action(format!("Quitter {APP_NAME}"), Quit),
        ]),
        Menu::new("Édition").items([
            MenuItem::os_action("Couper", Cut, OsAction::Cut),
            MenuItem::os_action("Copier", Copy, OsAction::Copy),
            MenuItem::os_action("Coller", Paste, OsAction::Paste),
            MenuItem::os_action("Tout sélectionner", SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("Aide").items([
            MenuItem::action("Signaler un problème", ReportIssue),
            MenuItem::separator(),
            MenuItem::action(version_line(), OpenIdemaRepo),
            MenuItem::action(idnma_label, OpenIdnmaRepo),
        ]),
    ]);
}
