//! Les menus standard de l'app (écart E25) : « Idema » (masquer, personnaliser, installer, quitter), « Édition »
//! (couper, copier, coller, tout sélectionner) et « Aide » (signaler un problème, versions d'IdemA et d'IdnmA). Les
//! liaisons de touches sont globales (contexte `None`) pour que la barre de menus affiche leurs équivalents ; les
//! gestionnaires globaux évitent qu'AppKit grise les entrées du menu « Idema » (`validateMenuItem:`).
//!
//! AppKit grise une entrée dont l'action n'a pas de gestionnaire : les lignes de version et « Signaler un problème »
//! n'en ont pas, et « Installer le réseau IDNMA » n'en a un (posé par `Workspace`) que si l'installation est permise.

use gpui::{App, KeyBinding, Menu, MenuItem, OsAction, actions};

use crate::APP_NAME;
use crate::idnma::IdnmaStatus;
use crate::workspace::{InstallIntegrations, OpenConfig};

actions!(idema, [Quit, Hide, HideOthers, ShowAll, InstallIdnma]);
actions!(edit, [Cut, Copy, Paste, SelectAll]);
// Sans gestionnaire, à dessein : ces lignes restent grisées.
actions!(help, [ReportIssue, VersionInfo]);

/// L'`Info.plist` du bundle, seule source de la version affichée : `Cargo.toml` n'en porte pas (resté à 0.1.0).
const BUNDLE_INFO: &str = include_str!("../../../packaging/Info.plist");

/// La valeur de `CFBundleShortVersionString` dans un `Info.plist` XML.
fn short_version(plist: &str) -> Option<&str> {
    let (_, after_key) = plist.split_once("<key>CFBundleShortVersionString</key>")?;
    let (_, after_open) = after_key.split_once("<string>")?;
    let (value, _) = after_open.split_once("</string>")?;
    Some(value.trim()).filter(|value| !value.is_empty())
}

/// La version d'IdemA, celle que le Finder lit dans le bundle.
fn app_version() -> &'static str {
    short_version(BUNDLE_INFO).unwrap_or(env!("CARGO_PKG_VERSION"))
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
            MenuItem::action(format!("{APP_NAME} {}", app_version()), VersionInfo),
            MenuItem::action(idnma_label, VersionInfo),
        ]),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_short_version_is_read_from_an_info_plist() {
        let plist =
            "<dict>\n\t<key>CFBundleShortVersionString</key>\n\t<string>1.2.3</string>\n</dict>";
        assert_eq!(short_version(plist), Some("1.2.3"));
        assert_eq!(short_version("<dict></dict>"), None);
    }

    #[test]
    fn the_bundle_plist_declares_a_version() {
        assert!(short_version(BUNDLE_INFO).is_some());
    }
}
