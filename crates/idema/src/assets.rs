//! Icônes embarquées dans le binaire : Material Symbols Rounded (Apache-2.0), paquets
//! `@material-symbols/svg-300@0.48.0` et `svg-400@0.48.0`, récupérés par `scripts/fetch-icons.sh`.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_kit_assets::Assets as KitAssets;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// « + » des projets (graisse 300).
    Add,
    /// « + » de la pastille de la ligne « Ouvrir un projet » (graisse 400).
    AddBold,
    /// Coche de l'agent par défaut (graisse 400).
    Check,
    /// « Personnaliser ».
    Work,
    MoreHoriz,
    MoreVert,
    ExpandMore,
    /// Résultat précédent de la recherche ; son suivant est `ExpandMore` (graisse 300).
    ExpandLess,
    ChevronRight,
    LeftPanelClose,
    LeftPanelOpen,
    ForkRight,
    /// Bouton et en-tête du volet Terminal (graisse 300).
    Terminal,
    OpenInFull,
    CloseFullscreen,
    Close,
    /// Bouton de la barre d'outils pendant le zoom d'un pane (graisse 300).
    ZoomInMap,
    /// Bouton et en-tête du volet Fichiers (graisse 300).
    Folder,
    /// Fichier sans glyphe de type dans l'arbre du volet Fichiers (graisse 300).
    Draft,
    /// Ligne « Ouvrir un fichier » de la barre latérale (graisse 300).
    Description,
    /// Loupe du champ du filtre en tête de l'arbre du volet Fichiers (graisse 300).
    Search,
    /// Les trois bascules de la recherche dans le projet : casse, mot entier, expression régulière (graisse 300).
    MatchCase,
    MatchWord,
    RegularExpression,
    /// Bouton et en-tête du volet Changes (graisse 300).
    Difference,
    /// Bouton « Générer le message » du pied du volet Changes (graisse 300).
    EditNote,
    /// Le même bouton pendant une génération : Stop (graisse 400).
    Stop,
    /// Bouton de la barre d'outils du volet Browser (graisse 300).
    Language,
    /// Onglet d'une page web du volet Browser.
    Public,
    /// Onglet de la page d'accueil du volet Browser.
    Home,
    /// Précédent, suivant et recharger de la rangée d'adresse du volet Browser.
    ArrowBack,
    ArrowForward,
    Refresh,
    /// Marque d'une page `http` hors de la machine, dans le champ d'adresse.
    LockOpen,
    /// Démarrer et ouvrir, dans les actions d'une ligne de serveur de la page d'accueil.
    PlayArrow,
    ArrowOutward,
    /// Bouton de la barre d'outils et en-tête du volet Scratchpad (graisse 300).
    StickyNote,
    /// Les lignes de la barre latérale de « Personnaliser » : Général, Agents, Réseau idnma, Plugins, Marketplaces, Skills (graisse 300).
    Tune,
    SmartToy,
    Hub,
    /// Section « Serveurs MCP » : le logo officiel de MCP, trait ramené à la graisse 300 des autres.
    Mcp,
    Extension,
    Storefront,
    Bolt,
    /// Les contrôles des lignes de réglage : supprimer une entrée, montrer ou masquer la clé d'API (graisse 300).
    Delete,
    Visibility,
    VisibilityOff,
    /// Le bandeau d'information de « Personnaliser » (graisse 300).
    Info,
    /// La galerie Artifacts : le filtre par type, les deux dispositions, et les trois genres (Slides, Pages ; Docs est
    /// `Description`) (graisse 300).
    FilterList,
    GridView,
    ViewList,
    Slideshow,
    FilterNone,
    /// Ligne « Artifacts » de la barre latérale : une pile de pages (graisse 300).
    Stacks,
    /// Bouton de la barre d'outils et en-tête du volet Projet : la roue dentée (graisse 300).
    Settings,
    /// Le bandeau orange du volet Projet (graisse 300).
    Warning,
    /// L'icône du pied « Copiées pour » de l'onglet Compétences (graisse 300).
    ContentCopy,
}

impl Icon {
    pub const ALL: [Icon; 57] = [
        Icon::Add,
        Icon::AddBold,
        Icon::Check,
        Icon::Work,
        Icon::MoreHoriz,
        Icon::MoreVert,
        Icon::ExpandMore,
        Icon::ExpandLess,
        Icon::ChevronRight,
        Icon::LeftPanelClose,
        Icon::LeftPanelOpen,
        Icon::ForkRight,
        Icon::Terminal,
        Icon::OpenInFull,
        Icon::CloseFullscreen,
        Icon::Close,
        Icon::ZoomInMap,
        Icon::Folder,
        Icon::Draft,
        Icon::Description,
        Icon::Search,
        Icon::MatchCase,
        Icon::MatchWord,
        Icon::RegularExpression,
        Icon::Difference,
        Icon::EditNote,
        Icon::Stop,
        Icon::Language,
        Icon::Public,
        Icon::Home,
        Icon::ArrowBack,
        Icon::ArrowForward,
        Icon::Refresh,
        Icon::LockOpen,
        Icon::PlayArrow,
        Icon::ArrowOutward,
        Icon::StickyNote,
        Icon::Tune,
        Icon::SmartToy,
        Icon::Hub,
        Icon::Mcp,
        Icon::Extension,
        Icon::Storefront,
        Icon::Bolt,
        Icon::Delete,
        Icon::Visibility,
        Icon::VisibilityOff,
        Icon::Info,
        Icon::FilterList,
        Icon::GridView,
        Icon::ViewList,
        Icon::Slideshow,
        Icon::FilterNone,
        Icon::Stacks,
        Icon::Settings,
        Icon::Warning,
        Icon::ContentCopy,
    ];

    /// Chemin à donner à `svg().path(...)`. Les icônes d'IdemA vivent sous `icons/idema/` : les chemins
    /// `icons/<nom>.svg` appartiennent à gpui-component (Lucide), que le repli sert sans masque.
    pub fn path(self) -> &'static str {
        match self {
            Self::Add => "icons/idema/add.svg",
            Self::AddBold => "icons/idema/add-400.svg",
            Self::Check => "icons/idema/check.svg",
            Self::Work => "icons/idema/work.svg",
            Self::MoreHoriz => "icons/idema/more_horiz.svg",
            Self::MoreVert => "icons/idema/more_vert.svg",
            Self::ExpandMore => "icons/idema/expand_more.svg",
            Self::ExpandLess => "icons/idema/expand_less.svg",
            Self::ChevronRight => "icons/idema/chevron_right.svg",
            Self::LeftPanelClose => "icons/idema/left_panel_close.svg",
            Self::LeftPanelOpen => "icons/idema/left_panel_open.svg",
            Self::ForkRight => "icons/idema/fork_right.svg",
            Self::Terminal => "icons/idema/terminal.svg",
            Self::OpenInFull => "icons/idema/open_in_full.svg",
            Self::CloseFullscreen => "icons/idema/close_fullscreen.svg",
            Self::Close => "icons/idema/close.svg",
            Self::ZoomInMap => "icons/idema/zoom_in_map.svg",
            Self::Folder => "icons/idema/folder.svg",
            Self::Draft => "icons/idema/draft.svg",
            Self::Description => "icons/idema/description.svg",
            Self::Search => "icons/idema/search.svg",
            Self::MatchCase => "icons/idema/match_case.svg",
            Self::MatchWord => "icons/idema/match_word.svg",
            Self::RegularExpression => "icons/idema/regular_expression.svg",
            Self::Difference => "icons/idema/difference.svg",
            Self::EditNote => "icons/idema/edit_note.svg",
            Self::Stop => "icons/idema/stop.svg",
            Self::Language => "icons/idema/language.svg",
            Self::Public => "icons/idema/public.svg",
            Self::Home => "icons/idema/home.svg",
            Self::ArrowBack => "icons/idema/arrow_back.svg",
            Self::ArrowForward => "icons/idema/arrow_forward.svg",
            Self::Refresh => "icons/idema/refresh.svg",
            Self::LockOpen => "icons/idema/lock_open.svg",
            Self::PlayArrow => "icons/idema/play_arrow.svg",
            Self::ArrowOutward => "icons/idema/arrow_outward.svg",
            Self::StickyNote => "icons/idema/sticky_note_2.svg",
            Self::Tune => "icons/idema/tune.svg",
            Self::SmartToy => "icons/idema/smart_toy.svg",
            Self::Hub => "icons/idema/hub.svg",
            Self::Mcp => "icons/idema/mcp.svg",
            Self::Extension => "icons/idema/extension.svg",
            Self::Storefront => "icons/idema/storefront.svg",
            Self::Bolt => "icons/idema/bolt.svg",
            Self::Delete => "icons/idema/delete.svg",
            Self::Visibility => "icons/idema/visibility.svg",
            Self::VisibilityOff => "icons/idema/visibility_off.svg",
            Self::Info => "icons/idema/info.svg",
            Self::FilterList => "icons/idema/filter_list.svg",
            Self::GridView => "icons/idema/grid_view.svg",
            Self::ViewList => "icons/idema/view_list.svg",
            Self::Slideshow => "icons/idema/slideshow.svg",
            Self::FilterNone => "icons/idema/filter_none.svg",
            Self::Stacks => "icons/idema/stacks.svg",
            Self::Settings => "icons/idema/settings.svg",
            Self::Warning => "icons/idema/warning.svg",
            Self::ContentCopy => "icons/idema/content_copy.svg",
        }
    }

    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Add => include_bytes!("../assets/icons/add.svg"),
            Self::AddBold => include_bytes!("../assets/icons/add-400.svg"),
            Self::Check => include_bytes!("../assets/icons/check.svg"),
            Self::Work => include_bytes!("../assets/icons/work.svg"),
            Self::MoreHoriz => include_bytes!("../assets/icons/more_horiz.svg"),
            Self::MoreVert => include_bytes!("../assets/icons/more_vert.svg"),
            Self::ExpandMore => include_bytes!("../assets/icons/expand_more.svg"),
            Self::ExpandLess => include_bytes!("../assets/icons/expand_less.svg"),
            Self::ChevronRight => include_bytes!("../assets/icons/chevron_right.svg"),
            Self::LeftPanelClose => include_bytes!("../assets/icons/left_panel_close.svg"),
            Self::LeftPanelOpen => include_bytes!("../assets/icons/left_panel_open.svg"),
            Self::ForkRight => include_bytes!("../assets/icons/fork_right.svg"),
            Self::Terminal => include_bytes!("../assets/icons/terminal.svg"),
            Self::OpenInFull => include_bytes!("../assets/icons/open_in_full.svg"),
            Self::CloseFullscreen => include_bytes!("../assets/icons/close_fullscreen.svg"),
            Self::Close => include_bytes!("../assets/icons/close.svg"),
            Self::ZoomInMap => include_bytes!("../assets/icons/zoom_in_map.svg"),
            Self::Folder => include_bytes!("../assets/icons/folder.svg"),
            Self::Draft => include_bytes!("../assets/icons/draft.svg"),
            Self::Description => include_bytes!("../assets/icons/description.svg"),
            Self::Search => include_bytes!("../assets/icons/search.svg"),
            Self::MatchCase => include_bytes!("../assets/icons/match_case.svg"),
            Self::MatchWord => include_bytes!("../assets/icons/match_word.svg"),
            Self::RegularExpression => include_bytes!("../assets/icons/regular_expression.svg"),
            Self::Difference => include_bytes!("../assets/icons/difference.svg"),
            Self::EditNote => include_bytes!("../assets/icons/edit_note.svg"),
            Self::Stop => include_bytes!("../assets/icons/stop.svg"),
            Self::Language => include_bytes!("../assets/icons/language.svg"),
            Self::Public => include_bytes!("../assets/icons/public.svg"),
            Self::Home => include_bytes!("../assets/icons/home.svg"),
            Self::ArrowBack => include_bytes!("../assets/icons/arrow_back.svg"),
            Self::ArrowForward => include_bytes!("../assets/icons/arrow_forward.svg"),
            Self::Refresh => include_bytes!("../assets/icons/refresh.svg"),
            Self::LockOpen => include_bytes!("../assets/icons/lock_open.svg"),
            Self::PlayArrow => include_bytes!("../assets/icons/play_arrow.svg"),
            Self::ArrowOutward => include_bytes!("../assets/icons/arrow_outward.svg"),
            Self::StickyNote => include_bytes!("../assets/icons/sticky_note_2.svg"),
            Self::Tune => include_bytes!("../assets/icons/tune.svg"),
            Self::SmartToy => include_bytes!("../assets/icons/smart_toy.svg"),
            Self::Hub => include_bytes!("../assets/icons/hub.svg"),
            Self::Mcp => include_bytes!("../assets/icons/mcp.svg"),
            Self::Extension => include_bytes!("../assets/icons/extension.svg"),
            Self::Storefront => include_bytes!("../assets/icons/storefront.svg"),
            Self::Bolt => include_bytes!("../assets/icons/bolt.svg"),
            Self::Delete => include_bytes!("../assets/icons/delete.svg"),
            Self::Visibility => include_bytes!("../assets/icons/visibility.svg"),
            Self::VisibilityOff => include_bytes!("../assets/icons/visibility_off.svg"),
            Self::Info => include_bytes!("../assets/icons/info.svg"),
            Self::FilterList => include_bytes!("../assets/icons/filter_list.svg"),
            Self::GridView => include_bytes!("../assets/icons/grid_view.svg"),
            Self::ViewList => include_bytes!("../assets/icons/view_list.svg"),
            Self::Slideshow => include_bytes!("../assets/icons/slideshow.svg"),
            Self::FilterNone => include_bytes!("../assets/icons/filter_none.svg"),
            Self::Stacks => include_bytes!("../assets/icons/stacks.svg"),
            Self::Settings => include_bytes!("../assets/icons/settings.svg"),
            Self::Warning => include_bytes!("../assets/icons/warning.svg"),
            Self::ContentCopy => include_bytes!("../assets/icons/content_copy.svg"),
        }
    }
}

/// Source d'assets de l'app : `gpui_platform::application().with_assets(Assets)`.
pub struct Assets;

/// Les logos des agents de la barre latérale (réduits à 48 px : 16 points en
/// @3x, de quoi suivre le zoom du texte). Chemin pour `img()`, nom du logo, octets.
const LOGOS: &[(&str, &str, &[u8])] = &[
    (
        "logos/claude.png",
        "claude",
        include_bytes!("../assets/logos/claude.png"),
    ),
    (
        "logos/codex.png",
        "codex",
        include_bytes!("../assets/logos/codex.png"),
    ),
    (
        "logos/deepseek.png",
        "deepseek",
        include_bytes!("../assets/logos/deepseek.png"),
    ),
    (
        "logos/gemini.png",
        "gemini",
        include_bytes!("../assets/logos/gemini.png"),
    ),
    (
        "logos/glm.png",
        "glm",
        include_bytes!("../assets/logos/glm.png"),
    ),
    (
        "logos/grok.png",
        "grok",
        include_bytes!("../assets/logos/grok.png"),
    ),
    (
        "logos/kimi.png",
        "kimi",
        include_bytes!("../assets/logos/kimi.png"),
    ),
    (
        "logos/muse.png",
        "muse",
        include_bytes!("../assets/logos/muse.png"),
    ),
    (
        "logos/nvidia.png",
        "nvidia",
        include_bytes!("../assets/logos/nvidia.png"),
    ),
    (
        "logos/qwen.png",
        "qwen",
        include_bytes!("../assets/logos/qwen.png"),
    ),
    (
        "logos/mistral.png",
        "mistral",
        include_bytes!("../assets/logos/mistral.png"),
    ),
];

/// Le chemin à donner à `img()` pour le logo `name` (« claude »), s'il existe.
pub fn logo_path(name: &str) -> Option<&'static str> {
    LOGOS
        .iter()
        .find(|(_, logo, _)| *logo == name)
        .map(|(path, _, _)| *path)
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(icon) = Icon::ALL.iter().find(|icon| icon.path() == path) {
            return Ok(Some(Cow::Borrowed(icon.bytes())));
        }
        if let Some((_, _, bytes)) = LOGOS.iter().find(|(logo, _, _)| *logo == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        // Repli sur les icônes de gpui-component ; son `load` rend une erreur pour un chemin
        // inconnu, là où `AssetSource` attend `None`.
        Ok(KitAssets.load(path).ok().flatten())
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut listed: Vec<SharedString> = Icon::ALL
            .iter()
            .map(|icon| icon.path())
            .filter(|candidate| candidate.starts_with(path))
            .map(SharedString::from)
            .collect();
        for kit_path in KitAssets.list(path)? {
            if !listed.contains(&kit_path) {
                listed.push(kit_path);
            }
        }
        Ok(listed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_logo_loads_as_a_png() {
        for (path, name, _) in LOGOS {
            assert_eq!(logo_path(name), Some(*path));
            let bytes = Assets.load(path).unwrap().unwrap();
            assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "{path}");
        }
        assert_eq!(logo_path("antigravity"), None);
    }

    #[test]
    fn every_icon_loads_as_svg() {
        for icon in Icon::ALL {
            let bytes = Assets.load(icon.path()).unwrap().expect("embedded icon");
            assert!(bytes.starts_with(b"<svg"), "{icon:?}");
        }
        assert!(Assets.load("icons/absent.svg").unwrap().is_none());
    }

    #[test]
    fn idema_icons_never_shadow_a_kit_icon() {
        for icon in Icon::ALL {
            // Le kit ne sert pas ce chemin (`None` ou erreur), donc aucune icône de gpui-component n'est masquée.
            assert!(
                !matches!(KitAssets.load(icon.path()), Ok(Some(_))),
                "{icon:?} shadows a kit icon"
            );
            let bytes = Assets.load(icon.path()).unwrap().expect("embedded icon");
            assert_eq!(bytes.as_ref(), icon.bytes(), "{icon:?}");
        }
    }

    #[test]
    fn list_has_no_duplicates() {
        let listed = Assets.list("icons/").unwrap();
        let mut unique = listed.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(listed.len(), unique.len());
    }

    #[test]
    fn component_icons_fall_back_to_the_kit_bundle() {
        // Icône de la barre de recherche de gpui-component (`IconName::CaseSensitive`).
        let bytes = Assets
            .load("icons/case-sensitive.svg")
            .unwrap()
            .expect("kit icon");
        assert!(bytes.starts_with(b"<svg"));
        let listed = Assets.list("icons/").unwrap();
        for icon in Icon::ALL {
            assert!(listed.iter().any(|path| path.as_ref() == icon.path()));
        }
        assert!(
            listed
                .iter()
                .any(|path| path.as_ref() == "icons/case-sensitive.svg")
        );
    }
}
