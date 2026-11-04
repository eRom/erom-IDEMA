//! Les grammaires que gpui-component 0.7.3 livre sans de quoi colorer (spec P2 §4.5) : Swift a une requête de
//! coloration vide, TSX n'a que celle de `tree-sitter-typescript` (l'ajout de TypeScript à JavaScript, sans le reste),
//! et la requête JavaScript du kit n'a aucun motif JSX. Toujours vrai en 0.8.1 (26/10).

use std::sync::Once;

use gpui_component::highlighter::{GrammarConfig, LanguageRegistry};

/// Réenregistre `swift`, `tsx` et `javascript` avec des requêtes complètes. À appeler avant d'ouvrir un éditeur ; les
/// appels suivants ne font rien (le JSX ne s'ajoute qu'une fois).
pub fn register_code_grammars() {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| {
        let registry = LanguageRegistry::singleton();
        let typescript = registry
            .language("typescript")
            .expect("the kit is built with tree-sitter-typescript");
        let javascript = registry
            .language("javascript")
            .expect("the kit is built with tree-sitter-javascript");
        registry.register("swift", &swift());
        registry.register(
            "tsx",
            &with_jsx(
                "tsx",
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                typescript,
            ),
        );
        registry.register(
            "javascript",
            &with_jsx(
                "javascript",
                tree_sitter_javascript::LANGUAGE.into(),
                javascript,
            ),
        );
    });
}

/// Swift avec la requête de sa grammaire. Ses injections (regex, commentaires) visent des langages que le kit n'a pas.
fn swift() -> GrammarConfig {
    GrammarConfig::new(
        "swift",
        tree_sitter_swift::LANGUAGE.into(),
        vec![],
        tree_sitter_swift::HIGHLIGHTS_QUERY,
        "",
        tree_sitter_swift::LOCALS_QUERY,
    )
}

/// La configuration `base` du kit (sa requête, ses injections, ses locals) sur la grammaire `language`, plus les
/// balises JSX. Le `.ts` et le `.tsx`, le `.js` et le `.jsx` se colorent ainsi pareil, balises en plus.
fn with_jsx(
    name: &'static str,
    language: tree_sitter::Language,
    base: GrammarConfig,
) -> GrammarConfig {
    GrammarConfig {
        name: name.into(),
        language: Some(language),
        highlights: format!(
            "{}\n{}",
            base.highlights,
            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
        )
        .into(),
        ..base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::editor_theme::{editor_palette, highlight_theme, pick};
    use gpui::Rgba;
    use gpui_component::Rope;
    use gpui_component::highlighter::SyntaxHighlighter;

    /// La couleur que l'éditeur donne à la première occurrence de `token` dans `source`, avec le highlighter du kit
    /// (requêtes compilées ensemble, priorité des captures) et le thème d'IdemA.
    fn colour_of(language: &str, source: &str, token: &str) -> Option<Rgba> {
        register_code_grammars();
        let mut highlighter = SyntaxHighlighter::new(language);
        // Une requête qui ne compile pas fait retomber le kit sur `text`, sans erreur.
        assert_eq!(
            highlighter.language().as_ref(),
            language,
            "fell back to plain text"
        );
        highlighter.update(None, &Rope::from_str(source), None);
        let theme = highlight_theme(&editor_palette());
        let start = source.find(token).unwrap();
        highlighter
            .styles(&(0..source.len()), &theme)
            .iter()
            .find(|(range, _)| range.start <= start && range.end >= start + token.len())
            .and_then(|(_, style)| style.color)
            .map(Rgba::from)
    }

    fn token(name: &str) -> Option<Rgba> {
        Some(Rgba::from(pick(&editor_palette(), name)))
    }

    #[test]
    fn swift_colours_keywords_strings_numbers_and_comments() {
        let source = "// A view.\nstruct A { var count = 42\n let title = \"hello\" }\n";
        assert_eq!(
            colour_of("swift", source, "struct"),
            token("syntax.keyword")
        );
        assert_eq!(colour_of("swift", source, "hello"), token("syntax.string"));
        assert_eq!(colour_of("swift", source, "42"), token("syntax.number"));
        assert_eq!(
            colour_of("swift", source, "A view"),
            token("syntax.comment")
        );
    }

    #[test]
    fn tsx_colours_typescript_and_jsx_tags() {
        let source = "// A widget.\nconst n: number = 42;\nexport const B = () => <button onClick={f}>{n}</button>;\n";
        assert_eq!(colour_of("tsx", source, "const"), token("syntax.keyword"));
        assert_eq!(colour_of("tsx", source, "42"), token("syntax.number"));
        assert_eq!(
            colour_of("tsx", source, "A widget"),
            token("syntax.comment")
        );
        assert_eq!(colour_of("tsx", source, "button"), token("syntax.tag"));
        assert_eq!(
            colour_of("tsx", source, "onClick"),
            token("syntax.attribute")
        );
    }

    #[test]
    fn javascript_colours_jsx_tags() {
        let source = "const n = 42;\nexport const B = () => <button onClick={f}>{n}</button>;\n";
        assert_eq!(
            colour_of("javascript", source, "const"),
            token("syntax.keyword")
        );
        assert_eq!(
            colour_of("javascript", source, "button"),
            token("syntax.tag")
        );
        assert_eq!(
            colour_of("javascript", source, "onClick"),
            token("syntax.attribute")
        );
    }

    #[test]
    fn registering_twice_adds_the_jsx_query_once() {
        register_code_grammars();
        register_code_grammars();
        let highlights = LanguageRegistry::singleton()
            .language("javascript")
            .unwrap()
            .highlights;
        assert_eq!(
            highlights
                .matches(tree_sitter_javascript::JSX_HIGHLIGHT_QUERY)
                .count(),
            1
        );
    }
}
