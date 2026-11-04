//! Le menu `more_vert` de l'en-tête (spec P4 §4.2) : ce qu'il propose selon l'onglet actif. La partie pure ici ;
//! l'ouverture du menu et ses gestes sont dans `panel.rs`.

/// Un geste du menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    OpenInDefaultBrowser,
    CopyAddress,
    Inspect,
    ReloadFromOrigin,
}

impl MenuAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenInDefaultBrowser => "Ouvrir dans le navigateur par défaut",
            Self::CopyAddress => "Copier l'adresse",
            Self::Inspect => "Inspecter",
            Self::ReloadFromOrigin => "Recharger sans le cache",
        }
    }
}

/// Une ligne du menu : son geste, et s'il est possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuLine {
    pub action: MenuAction,
    pub enabled: bool,
}

/// Les lignes du menu. `web_tab` : l'onglet actif montre une adresse ; `has_page` : sa page existe (elle naît dans le
/// tour qui suit son affichage). `inspector_available` : l'inspecteur n'a pas déjà refusé de s'ouvrir (E-P4-c, l'API
/// privée peut manquer) ; s'il l'a fait, l'entrée n'apparaît plus.
pub fn menu_lines(web_tab: bool, has_page: bool, inspector_available: bool) -> Vec<MenuLine> {
    let line = |action, enabled| MenuLine { action, enabled };
    let mut lines = vec![
        line(MenuAction::OpenInDefaultBrowser, web_tab),
        line(MenuAction::CopyAddress, web_tab),
    ];
    if inspector_available {
        lines.push(line(MenuAction::Inspect, has_page));
    }
    lines.push(line(MenuAction::ReloadFromOrigin, has_page));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(lines: &[MenuLine]) -> Vec<&'static str> {
        lines.iter().map(|line| line.action.label()).collect()
    }

    #[test]
    fn the_menu_offers_its_four_gestures_in_order() {
        let lines = menu_lines(true, true, true);
        assert_eq!(
            labels(&lines),
            [
                "Ouvrir dans le navigateur par défaut",
                "Copier l'adresse",
                "Inspecter",
                "Recharger sans le cache",
            ]
        );
        assert!(lines.iter().all(|line| line.enabled));
    }

    #[test]
    fn the_start_page_switches_every_web_entry_off() {
        let lines = menu_lines(false, false, true);
        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|line| !line.enabled));
    }

    #[test]
    fn a_tab_whose_page_is_not_born_yet_can_still_be_copied_and_opened() {
        let lines = menu_lines(true, false, true);
        let enabled: Vec<MenuAction> = lines
            .iter()
            .filter(|line| line.enabled)
            .map(|line| line.action)
            .collect();
        assert_eq!(
            enabled,
            [MenuAction::OpenInDefaultBrowser, MenuAction::CopyAddress]
        );
    }

    #[test]
    fn a_refused_inspector_leaves_the_menu() {
        let lines = menu_lines(true, true, false);
        assert_eq!(
            labels(&lines),
            [
                "Ouvrir dans le navigateur par défaut",
                "Copier l'adresse",
                "Recharger sans le cache",
            ]
        );
    }
}
