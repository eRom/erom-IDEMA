//! L'apparence de l'éditeur du volet Fichiers : le thème de gpui-component (éditeur et barre de recherche) habillé des
//! jetons d'IdemA. Toute couleur vient de `theme.rs` (palette ANSI du handoff pour la coloration) ; ce module n'en
//! invente aucune.

use std::sync::{Arc, OnceLock};

#[cfg(test)]
use gpui::Rgba;
use gpui::{App, Hsla, px};
use gpui_component::highlighter::HighlightTheme;
use gpui_component::{Theme as KitTheme, ThemeColor, ThemeMode};

use crate::terminal_element::TerminalFont;
use crate::theme::{
    BUTTON_HOVER, CONTROL, ERROR, FOCUS, HOVER, HOVER_STRONG, ICON_MUTED, ICON_SECONDARY, PANEL,
    POPOVER, RUNNING, SELECTION, SEPARATOR, SYNTAX_COMMENT, SYNTAX_KEYWORD, SYNTAX_NUMBER,
    SYNTAX_PROPERTY, SYNTAX_STRING, SYNTAX_TAG, SYNTAX_TITLE, TEXT_DISABLED, TEXT_FULL, TEXT_HOVER,
    TEXT_LABEL, TEXT_MIN, TEXT_PRIMARY, TEXT_SECONDARY, Theme, UI_FONT, WAITING, color,
    color_alpha, focus, white,
};

/// La mono de l'interface (spec P2, §4.2) ; la taille est celle du terminal en vigueur.
const PREFERRED_FONT_FAMILY: &str = "SF Mono";

/// La famille de l'éditeur : SF Mono quand la machine la porte, sinon la police du terminal (celle de la config), qui est
/// installée puisque le terminal l'affiche. SF Mono n'est pas une famille que macOS donne à une app par son nom (elle ne
/// vit que dans Terminal.app et Xcode) : sans repli, l'éditeur retomberait sur la police proportionnelle du système.
fn editor_font_family(installed: &[String], terminal_family: &str) -> String {
    if installed.iter().any(|name| name == PREFERRED_FONT_FAMILY) {
        PREFERRED_FONT_FAMILY.to_owned()
    } else {
        terminal_family.to_owned()
    }
}

/// Les familles installées, listées une fois par processus (l'énumération coûte une centaine de millisecondes, et la
/// taille du texte rappelle `apply_editor_theme` à chaque pas).
fn installed_fonts(cx: &App) -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| cx.text_system().all_font_names())
}

/// Toutes les couleurs que l'éditeur et sa barre de recherche reçoivent, par nom. Fonction pure : elle alimente
/// `apply_editor_theme` et c'est elle que le test confronte aux jetons.
pub fn editor_palette() -> Vec<(&'static str, Hsla)> {
    let hex = |value: u32| Hsla::from(color(value));
    let level = |alpha: f32| Hsla::from(white(alpha));
    vec![
        // Surfaces et texte de l'éditeur.
        ("background", hex(PANEL)),
        ("foreground", level(TEXT_PRIMARY)),
        ("muted_foreground", level(TEXT_MIN)),
        ("border", level(SEPARATOR)),
        ("caret", hex(FOCUS)),
        ("selection", Hsla::from(focus(SELECTION))),
        ("active_line", level(HOVER)),
        ("gutter", hex(PANEL)),
        ("line_number", level(TEXT_MIN)),
        ("active_line_number", level(TEXT_HOVER)),
        ("invisible", level(TEXT_DISABLED)),
        // La barre de recherche : sa coque est le popover d'IdemA, ses champs et ses boutons ceux de `search_bar.rs`.
        ("popover", Hsla::from(color_alpha(POPOVER))),
        ("popover_foreground", level(TEXT_PRIMARY)),
        ("input", level(CONTROL)),
        ("ring", hex(FOCUS)),
        ("button", level(HOVER_STRONG)),
        ("button_hover", level(CONTROL)),
        ("button_active", level(BUTTON_HOVER)),
        ("button_foreground", level(TEXT_LABEL)),
        ("accent", level(HOVER_STRONG)),
        ("accent_foreground", level(TEXT_HOVER)),
        ("secondary", level(HOVER_STRONG)),
        ("secondary_hover", level(CONTROL)),
        ("secondary_active", level(CONTROL)),
        ("secondary_foreground", level(TEXT_MIN)),
        ("muted", level(HOVER)),
        ("primary", hex(FOCUS)),
        ("primary_foreground", level(TEXT_FULL)),
        ("scrollbar_thumb", level(ICON_MUTED)),
        ("scrollbar_thumb_hover", level(ICON_SECONDARY)),
        // Les diagnostics.
        ("error", hex(ERROR)),
        ("warning", hex(WAITING)),
        ("info", hex(FOCUS)),
        ("success", hex(RUNNING)),
        ("hint", level(TEXT_SECONDARY)),
        // La coloration : les noms sont ceux de `SyntaxColors`.
        ("syntax.keyword", hex(SYNTAX_KEYWORD)),
        ("syntax.string", hex(SYNTAX_STRING)),
        ("syntax.string.escape", hex(SYNTAX_NUMBER)),
        ("syntax.number", hex(SYNTAX_NUMBER)),
        ("syntax.boolean", hex(SYNTAX_NUMBER)),
        ("syntax.constant", hex(SYNTAX_NUMBER)),
        ("syntax.comment", hex(SYNTAX_COMMENT)),
        ("syntax.comment.doc", hex(SYNTAX_COMMENT)),
        ("syntax.title", hex(SYNTAX_TITLE)),
        ("syntax.property", hex(SYNTAX_PROPERTY)),
        ("syntax.tag", hex(SYNTAX_TAG)),
        ("syntax.attribute", hex(SYNTAX_TAG)),
        ("syntax.punctuation.special", hex(SYNTAX_TITLE)),
        ("syntax.punctuation.list_marker", hex(SYNTAX_KEYWORD)),
        ("syntax.text.literal", hex(SYNTAX_STRING)),
        // Le code inline du Markdown : `text.code.span` n'hérite pas de `text.literal`, il lui faut sa propre entrée.
        ("syntax.text.code.span", hex(SYNTAX_STRING)),
        ("syntax.link_text", hex(SYNTAX_KEYWORD)),
        ("syntax.link_uri", hex(SYNTAX_COMMENT)),
    ]
}

/// Une couleur de la palette par son nom. Un nom absent est une faute de programmation : `kit_colors` et
/// `highlight_theme` sont exercés par les tests, aucun appel à l'exécution ne la découvre le premier.
pub(super) fn pick(palette: &[(&'static str, Hsla)], name: &str) -> Hsla {
    palette
        .iter()
        .find(|(entry, _)| *entry == name)
        .map(|(_, colour)| *colour)
        .unwrap_or_else(|| panic!("editor palette has no entry named {name}"))
}

/// Les couleurs du thème de gpui-component : celles de `base` (le sombre du kit), recouvertes par la palette.
fn kit_colors(mut base: ThemeColor, palette: &[(&'static str, Hsla)]) -> ThemeColor {
    let colour = |name: &str| pick(palette, name);
    base.background = colour("background");
    base.foreground = colour("foreground");
    base.muted = colour("muted");
    base.muted_foreground = colour("muted_foreground");
    base.border = colour("border");
    base.caret = colour("caret");
    base.selection = colour("selection");
    base.popover = colour("popover");
    base.popover_foreground = colour("popover_foreground");
    base.input = colour("input");
    base.ring = colour("ring");
    base.button = colour("button");
    base.button_hover = colour("button_hover");
    base.button_active = colour("button_active");
    base.button_foreground = colour("button_foreground");
    base.accent = colour("accent");
    base.accent_foreground = colour("accent_foreground");
    base.secondary = colour("secondary");
    base.secondary_hover = colour("secondary_hover");
    base.secondary_active = colour("secondary_active");
    base.secondary_foreground = colour("secondary_foreground");
    base.primary = colour("primary");
    base.primary_hover = colour("primary");
    base.primary_active = colour("primary");
    base.primary_foreground = colour("primary_foreground");
    // Le diff du volet Changes teinte les lignes ajoutées et retirées de `success` et `danger` (fond à 12 %).
    base.success = colour("success");
    base.danger = colour("error");
    base.scrollbar = gpui::transparent_black();
    base.scrollbar_thumb = colour("scrollbar_thumb");
    base.scrollbar_thumb_hover = colour("scrollbar_thumb_hover");
    base
}

/// Le thème de coloration. Les couleurs de l'éditeur (fond, ligne courante, gouttière) sont des champs publics ; les
/// couleurs de syntaxe et de diagnostic ont des champs privés, on les passe par le format JSON du thème.
pub(super) fn highlight_theme(palette: &[(&'static str, Hsla)]) -> HighlightTheme {
    let json_colour = |name: &str| {
        serde_json::to_value(pick(palette, name)).expect("a colour serialises to a hex string")
    };
    let syntax: serde_json::Map<String, serde_json::Value> = palette
        .iter()
        .filter_map(|(name, _)| name.strip_prefix("syntax."))
        .map(|key| {
            (
                key.to_owned(),
                serde_json::json!({ "color": json_colour(&format!("syntax.{key}")) }),
            )
        })
        .chain([
            (
                "emphasis".to_owned(),
                serde_json::json!({ "font_style": "italic" }),
            ),
            (
                "emphasis.strong".to_owned(),
                serde_json::json!({ "font_weight": 700 }),
            ),
        ])
        .collect();
    let mut theme: HighlightTheme = serde_json::from_value(serde_json::json!({
        "name": "IdemA",
        "appearance": "dark",
        "style": {
            "error": json_colour("error"),
            "warning": json_colour("warning"),
            "info": json_colour("info"),
            "success": json_colour("success"),
            "hint": json_colour("hint"),
            "syntax": syntax,
        },
    }))
    .expect("the IdemA highlight theme is valid");
    theme.style.editor_background = Some(pick(palette, "background"));
    theme.style.editor_foreground = Some(pick(palette, "foreground"));
    theme.style.editor_active_line = Some(pick(palette, "active_line"));
    theme.style.editor_line_number = Some(pick(palette, "line_number"));
    theme.style.editor_active_line_number = Some(pick(palette, "active_line_number"));
    theme.style.editor_invisible = Some(pick(palette, "invisible"));
    theme.style.editor_gutter_background = Some(pick(palette, "gutter"));
    theme
}

/// Habille l'éditeur et sa barre de recherche de l'apparence d'IdemA : thème sombre du kit recouvert par les jetons,
/// police mono du volet à la taille du terminal en vigueur. À rappeler quand la taille du texte ou la config change.
pub fn apply_editor_theme(theme: &Theme, font: &TerminalFont, cx: &mut App) {
    // Charge le sombre du kit (c'est lui qui pose les couleurs que la palette ne couvre pas) ; la palette vient après.
    KitTheme::change(ThemeMode::Dark, None, cx);
    let palette = editor_palette();
    let highlight = Arc::new(highlight_theme(&palette));
    let family = editor_font_family(installed_fonts(cx), &font.family);
    KitTheme::update(cx, |kit| {
        kit.colors = kit_colors(kit.colors, &palette);
        kit.highlight_theme = highlight;
        kit.font_family = UI_FONT.into();
        kit.font_size = theme.text(13.);
        kit.mono_font_family = family.into();
        kit.mono_font_size = px(font.size);
        kit.radius = theme.px(6.);
        kit.radius_lg = theme.px(9.);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{
        ACTION_HOVER, ERROR_TEXT, GLYPH_MD, HOVER_SHORTCUT, MIN_CONTRAST, SELECTED, SIDEBAR,
        TEXT_PROJECT, TREE_SELECTED, TREE_TEXT, WINDOW, black, composite, contrast_ratio,
        contrast_table,
    };

    /// Toutes les valeurs de jetons de `theme.rs` que l'éditeur a le droit d'employer.
    fn token_colours() -> Vec<Hsla> {
        let hexes = [
            WINDOW,
            SIDEBAR,
            PANEL,
            FOCUS,
            RUNNING,
            WAITING,
            ERROR,
            ERROR_TEXT,
            GLYPH_MD,
            SYNTAX_KEYWORD,
            SYNTAX_STRING,
            SYNTAX_NUMBER,
            SYNTAX_COMMENT,
            SYNTAX_TITLE,
            SYNTAX_PROPERTY,
            SYNTAX_TAG,
        ];
        let levels = [
            TEXT_PRIMARY,
            TEXT_PROJECT,
            TEXT_SECONDARY,
            TEXT_MIN,
            TEXT_HOVER,
            TEXT_LABEL,
            TEXT_FULL,
            TREE_TEXT,
            HOVER_SHORTCUT,
            TEXT_DISABLED,
            ICON_MUTED,
            ICON_SECONDARY,
            HOVER,
            HOVER_STRONG,
            BUTTON_HOVER,
            ACTION_HOVER,
            SELECTED,
            CONTROL,
            SEPARATOR,
            TREE_SELECTED,
        ];
        hexes
            .into_iter()
            .map(|value| Hsla::from(color(value)))
            .chain(levels.into_iter().map(|alpha| Hsla::from(white(alpha))))
            .chain([
                Hsla::from(color_alpha(POPOVER)),
                Hsla::from(focus(SELECTION)),
                Hsla::from(black(0.)),
            ])
            .collect()
    }

    #[test]
    fn every_editor_colour_comes_from_the_tokens() {
        let palette = editor_palette();
        assert!(!palette.is_empty());
        let tokens = token_colours();
        let strangers: Vec<&str> = palette
            .iter()
            .filter(|(_, colour)| !tokens.contains(colour))
            .map(|(name, _)| *name)
            .collect();
        assert!(
            strangers.is_empty(),
            "colours outside the tokens: {strangers:?}"
        );
    }

    #[test]
    fn the_editor_font_falls_back_to_the_terminal_font_when_sf_mono_is_missing() {
        let installed =
            |names: &[&str]| -> Vec<String> { names.iter().map(|n| n.to_string()).collect() };
        assert_eq!(
            editor_font_family(&installed(&["Menlo", "SF Mono"]), "Hack Nerd Font Mono"),
            "SF Mono"
        );
        assert_eq!(
            editor_font_family(&installed(&["Menlo", "SF Pro"]), "Hack Nerd Font Mono"),
            "Hack Nerd Font Mono"
        );
    }

    #[test]
    fn the_palette_feeds_the_kit_theme_and_the_highlight_theme() {
        let palette = editor_palette();
        let colors = kit_colors(ThemeColor::default(), &palette);
        assert_eq!(colors.caret, pick(&palette, "caret"));
        assert_eq!(colors.background, pick(&palette, "background"));
        let highlight = highlight_theme(&palette);
        assert_eq!(
            highlight.style.editor_background,
            Some(pick(&palette, "background"))
        );
        // Chaque couleur de syntaxe de la palette arrive dans le thème de coloration.
        for (name, colour) in &palette {
            let Some(key) = name.strip_prefix("syntax.") else {
                continue;
            };
            let style = highlight
                .style(key)
                .unwrap_or_else(|| panic!("{key} is missing from the highlight theme"));
            let applied = style.color.expect("a syntax style carries a colour");
            assert_eq!(
                Rgba::from(applied),
                Rgba::from(*colour),
                "{key} does not have the palette colour"
            );
        }
    }

    #[test]
    fn syntax_colours_are_readable_on_the_panel_and_on_its_current_line() {
        let panel = color(PANEL);
        let current_line = composite(white(HOVER), panel);
        let table = contrast_table();
        let syntax: Vec<_> = editor_palette()
            .into_iter()
            .filter(|(name, _)| name.starts_with("syntax."))
            .collect();
        assert!(!syntax.is_empty());
        for (name, colour) in &syntax {
            let text = Rgba::from(*colour);
            for (surface_name, surface) in [("PANEL", panel), ("PANEL+HOVER", current_line)] {
                let ratio = contrast_ratio(text, surface);
                assert!(
                    ratio >= MIN_CONTRAST,
                    "{name} on {surface_name}: {ratio:.2}"
                );
                // Et la paire est dans la table de contraste d'IdemA, sous le même ratio.
                assert!(
                    table.iter().any(|pair| pair.background == surface_name
                        && (pair.ratio - ratio).abs() < 0.001),
                    "{name} on {surface_name} is missing from contrast_table"
                );
            }
        }
    }

    #[test]
    fn inline_code_in_markdown_is_coloured_like_a_code_block() {
        use gpui_component::Rope;
        use gpui_component::highlighter::SyntaxHighlighter;

        // La grammaire inline capture `code` en `text.code.span`, qui ne retombe sur aucun parent (`text` n'existe pas).
        let markdown = "Run `cargo test` now.\n";
        let mut highlighter = SyntaxHighlighter::new("markdown");
        highlighter.update(None, &Rope::from_str(markdown), None);
        let palette = editor_palette();
        let theme = highlight_theme(&palette);
        let styles = highlighter.styles(&(0..markdown.len()), &theme);
        let colour_of = |text: &str| {
            let start = markdown.find(text).unwrap();
            styles
                .iter()
                .find(|(range, _)| range.start <= start && range.end >= start + text.len())
                .and_then(|(_, style)| style.color)
                .map(Rgba::from)
        };
        assert_eq!(
            colour_of("cargo test"),
            Some(Rgba::from(pick(&palette, "syntax.text.literal")))
        );
        assert_eq!(colour_of("now"), None);
    }
}
