//! Jetons du design system (spec §7.2) : ceux du handoff Claude Design
//! (`maquette/design_handoff_ide_terminal/README.md`), à l'échelle `ui_font_size / 13`, avec la règle des 55 %.

use gpui::{Global, Pixels, Rgba, px, rgb, rgba};

/// Surfaces.
pub const WINDOW: u32 = 0x151516;
pub const SIDEBAR: u32 = 0x1C1C1E;
/// Fond des volets droits (Terminal et Fichiers).
pub const PANEL: u32 = 0x1A1A1C;
/// Accents : les couleurs système de macOS en sombre.
pub const FOCUS: u32 = 0x0A84FF;
pub const RUNNING: u32 = 0x30D158;
pub const WAITING: u32 = 0xFFD60A;
pub const ERROR: u32 = 0xFF453A;
/// Rouge d'un texte d'erreur : `ERROR` ne passait que 4,38:1 sur le popover, celui-ci est le minimum qui passe 4,5:1
/// (spec jalon 4, §4.4). Les pastilles et icônes gardent `ERROR`.
pub const ERROR_TEXT: u32 = 0xFF4C41;
/// Fond des popovers, `rgba(40,40,42,0.94)`, sans flou : GPUI ne sait pas flouter un élément (risque R7).
pub const POPOVER: u32 = 0x28282AF0;

/// Texte principal (88 %), noms de projet (62 %), secondaire (60 %).
pub const TEXT_PRIMARY: f32 = 0.88;
pub const TEXT_PROJECT: f32 = 0.62;
pub const TEXT_SECONDARY: f32 = 0.60;
/// Plancher de tout texte à lire : les 32, 38 et 42 % de la maquette passent à 55 % (spec §7.2, correction 2).
pub const TEXT_MIN: f32 = 0.55;
/// Texte survolé (92 %), libellé « Terminal » et entrées de menu (90 %), blanc plein (survol, sélection, champ de nom).
pub const TEXT_HOVER: f32 = 0.92;
pub const TEXT_LABEL: f32 = 0.90;
pub const TEXT_FULL: f32 = 1.;
/// Raccourci d'une entrée de menu survolée : un peu en retrait du blanc plein.
pub const HOVER_SHORTCUT: f32 = 0.80;
/// Texte désactivé, exempté du plancher comme dans WCAG.
pub const TEXT_DISABLED: f32 = 0.25;
/// Icônes secondaires (chevrons) : ce ne sont pas des textes, elles gardent leur niveau de la maquette.
pub const ICON_MUTED: f32 = 0.35;
pub const ICON_SECONDARY: f32 = 0.45;
/// Icône de l'en-tête du volet Terminal, et icône d'un onglet du navigateur quel que soit son état.
pub const ICON_HEADER: f32 = 0.50;
/// Croix de fermeture d'un onglet du navigateur : l'icône garde le niveau de la maquette (40 %).
pub const ICON_CLOSE: f32 = 0.40;
/// Survol, contrôle, sélection et séparateurs.
pub const HOVER: f32 = 0.05;
pub const HOVER_STRONG: f32 = 0.07;
pub const BUTTON_HOVER: f32 = 0.08;
/// Survol d'une ligne d'action de la barre latérale (6 % dans la maquette).
pub const ACTION_HOVER: f32 = 0.06;
pub const SELECTED: f32 = 0.09;
pub const CONTROL: f32 = 0.10;
/// Un bouton de contrôle de « Personnaliser » sous le pointeur.
pub const CONTROL_HOVER: f32 = 0.14;
pub const SEPARATOR: f32 = 0.06;
/// Sélection du terminal : `FOCUS` à cette opacité, peinte sous le texte qui garde sa couleur.
pub const SELECTION: f32 = 0.35;
/// Résultats d'une recherche (⌘F) : `WAITING` à cette opacité sous le texte des occurrences, et `WAITING` plein sous
/// l'occurrence courante, dont le texte passe en `SEARCH_CURRENT_TEXT` (jetons à ratifier à la porte, spec jalon 4, §4.9).
pub const SEARCH_MATCH: f32 = 0.25;
pub const SEARCH_CURRENT: u32 = WAITING;
pub const SEARCH_CURRENT_TEXT: u32 = WINDOW;
/// Ombre de la barre de recherche : noir à cette opacité, celle des popovers (sans cote dans la maquette, à ratifier).
pub const SEARCH_BAR_SHADOW: f32 = 0.5;
/// Liseré d'un pane qui survole un dépôt de fichiers : `FOCUS` à cette opacité, peint par-dessus le texte (valeur à ratifier).
pub const DROP_OUTLINE: f32 = 0.60;
/// Trait entre deux panes d'une session (blanc), et opacité d'un pane qui n'est pas l'actif (§6.1, maquette ligne 522).
pub const PANE_SEPARATOR: f32 = 0.07;
pub const INACTIVE_PANE: f32 = 0.72;
/// Vignettes du menu « Layout » : fond noir, bord et panes en blanc, au repos puis au survol (écart E22).
pub const THUMB_BACKGROUND: f32 = 0.22;
pub const THUMB_BORDER: f32 = 0.06;
pub const THUMB_BORDER_HOVER: f32 = 0.12;
pub const THUMB_PANE: f32 = 0.30;
pub const THUMB_PANE_HOVER: f32 = 0.50;

/// Arbre du volet Fichiers (maquette, onglet Files et ligne sélectionnée de Changes) : fond noir à cette opacité sous
/// l'arbre, texte d'une ligne à 78 % (le `dim` de la maquette), puis la ligne sélectionnée en `FOCUS` à 16 % avec un
/// contour `FOCUS` à 55 %. Un fichier ignoré par Git est à `TEXT_MIN` (écart E39, 40 % dans la maquette).
pub const TREE_BACKGROUND: f32 = 0.14;
pub const TREE_TEXT: f32 = 0.78;
pub const TREE_SELECTED: f32 = 0.16;
pub const TREE_SELECTED_BORDER: f32 = 0.55;
/// Les lignes du dossier qui reçoit un dépôt pendant un glisser dans l'arbre : `FOCUS` à cette opacité (valeur à ratifier).
pub const TREE_DROP: f32 = 0.14;
/// Le champ du filtre en tête de l'arbre (maquette, `IDEWindow.dc.html:328`) : fond noir à 28 % sur le creux de l'arbre,
/// contour intérieur blanc à 8 %.
pub const FIELD_BACKGROUND: f32 = 0.28;
pub const FIELD_RING: f32 = 0.08;
/// Une bascule active de la recherche dans le projet (maquette, onglet Changes : `rgba(10,132,255,0.18)` et icône
/// `#409CFF`) : `FOCUS` à cette opacité en fond, et l'icône dans le bleu clair `FOCUS_TEXT`.
pub const TOGGLE_ACTIVE: f32 = 0.18;
pub const FOCUS_TEXT: u32 = 0x409CFF;
/// L'interrupteur de la fenêtre « Personnaliser » (maquette `.tog`) : piste blanche à 14 % éteinte, `RUNNING` allumée, pouce
/// blanc plein.
pub const TOGGLE_OFF: f32 = 0.14;
/// Les badges de type de la section « Serveurs MCP » (maquette `.b-stdio` et `.b-http`) : STDIO sur le violet des agents
/// (`#BF5AF2` à 14 %, texte `#D49CF7`), HTTP sur `FOCUS` (à 16 %, texte `FOCUS_TEXT`).
pub const AGENT_PURPLE: u32 = 0xBF5AF2;
pub const AGENT_PURPLE_TEXT: u32 = 0xD49CF7;
pub const BADGE_STDIO: f32 = 0.14;
pub const BADGE_HTTP: f32 = 0.16;
/// Une ligne de liste dépliée de « Personnaliser » (maquette `.row.open`, blanc à 3 %) et son bandeau d'information
/// (`.notice`, blanc à 4 %).
pub const OPEN_ROW: f32 = 0.03;
pub const NOTICE: f32 = 0.04;
/// La lettre `M` d'un fichier modifié dans la liste de Changes (maquette `#FF9F0A`). Les autres lettres reprennent
/// `RUNNING` (`A`, `U`), `ERROR_TEXT` (`D`, `!`) et `FOCUS_TEXT` (`R`).
pub const STATUS_MODIFIED: u32 = 0xFF9F0A;
/// Le volet Projet (maquette `volet-projet-connecteurs.html`, valeurs à ratifier à la porte) : le bandeau orange
/// (`.drift`, `STATUS_MODIFIED` à 8 % avec un liseré intérieur à 22 %), la puce d'un harnais qui porte le serveur
/// (`.chip.ok`, `RUNNING` à 14 %, texte `CHIP_PRESENT_TEXT`), le liseré de celle qui ne le porte pas (`.chip.no`, blanc à
/// 18 %), et la hauteur de la rangée d'onglets sous l'en-tête (28 d'onglet et 6 de marge de chaque côté ; la maquette
/// met les onglets dans l'en-tête de 48, que le plancher de 364 ne laisse pas tenir, spec §3.1).
pub const WARNING_BACKGROUND: f32 = 0.08;
pub const WARNING_RING: f32 = 0.22;
pub const CHIP_PRESENT: f32 = 0.14;
pub const CHIP_PRESENT_TEXT: u32 = 0x7EE29A;
pub const CHIP_ABSENT_RING: f32 = 0.18;
pub const PROJECT_TAB_ROW: f32 = 40.;
/// Le contour d'une case vide de la liste de Changes : blanc à 30 %, sur le creux de l'arbre.
pub const CHECKBOX_OUTLINE: f32 = 0.30;
/// Glyphes de type de fichier de l'arbre. `md` et `json` sont ceux de la maquette ; les cinq suivants complètent pour
/// les formats courants, pris dans la palette ANSI du handoff (valeurs à ratifier à la porte).
pub const GLYPH_MD: u32 = 0x5BD47E;
pub const GLYPH_JSON: u32 = 0xF5A524;
pub const GLYPH_YML: u32 = 0xFF8A80;
pub const GLYPH_TOML: u32 = 0x9ADCF9;
pub const GLYPH_XML: u32 = 0xFFE566;
pub const GLYPH_HTML: u32 = 0xC2E89A;
pub const GLYPH_ENV: u32 = 0xDDB3FF;

/// Les pastilles des genres d'artifact de la galerie : fond sombre et glyphe clair de la même teinte, à raison d'une
/// paire par genre nommé (Pages garde la pastille neutre).
pub const ARTIFACT_DOCS_BG: u32 = 0x0D3D7E;
pub const ARTIFACT_DOCS_GLYPH: u32 = 0x9DC8F7;
pub const ARTIFACT_SLIDES_BG: u32 = 0x642508;
pub const ARTIFACT_SLIDES_GLYPH: u32 = 0xF6B293;
pub const ARTIFACT_SHEETS_BG: u32 = 0x0B4D10;
pub const ARTIFACT_SHEETS_GLYPH: u32 = 0x91DA8E;
pub const ARTIFACT_DESIGN_BG: u32 = 0x3A2684;
pub const ARTIFACT_DESIGN_GLYPH: u32 = 0xC5BDFA;
pub const ARTIFACT_DATA_BG: u32 = 0x761A1C;
pub const ARTIFACT_DATA_GLYPH: u32 = 0xF7A6A7;
pub const ARTIFACT_WHITEBOARD_BG: u32 = 0x651839;
pub const ARTIFACT_WHITEBOARD_GLYPH: u32 = 0xF7ABC9;

/// Coloration de l'éditeur du volet Fichiers (spec P2, §4.2) : la palette ANSI du handoff (celle du terminal), sauf les
/// titres Markdown (orange système de macOS). Le commentaire de la maquette (`#5A5A5E`, 2,5:1) est relevé au plus petit
/// gris qui passe 4,5:1, aussi sur la ligne courante : `#8C8C90` (valeur à ratifier à la porte).
pub const SYNTAX_KEYWORD: u32 = 0x3AA0FF;
pub const SYNTAX_STRING: u32 = 0xA8D672;
pub const SYNTAX_NUMBER: u32 = 0xFFD60A;
pub const SYNTAX_COMMENT: u32 = 0x8C8C90;
pub const SYNTAX_TITLE: u32 = 0xFF9F0A;
pub const SYNTAX_PROPERTY: u32 = 0x64C3F5;
pub const SYNTAX_TAG: u32 = 0xC58AF9;

/// HUD de mesure : fond noir à 60 %, texte blanc à 55 %. Exempté du contrôle de contraste (fond translucide sur un
/// contenu quelconque).
pub const HUD_BACKGROUND: u32 = 0x0000_0099;
pub const HUD_TEXT: u32 = 0xFFFF_FF8C;

/// Seuil de lisibilité WCAG 2 (AA, texte courant), contractuel (spec P1, §7.2).
#[cfg(test)]
pub const MIN_CONTRAST: f32 = 4.5;

/// Blanc à `alpha` : la maquette exprime ainsi labels, survols et séparateurs.
pub fn white(alpha: f32) -> Rgba {
    Rgba {
        r: 1.,
        g: 1.,
        b: 1.,
        a: alpha,
    }
}

pub fn color(hex: u32) -> Rgba {
    rgb(hex)
}

pub fn color_alpha(hex_with_alpha: u32) -> Rgba {
    rgba(hex_with_alpha)
}

/// Noir à `alpha` : les creux de la maquette (champs, arbres) sont du noir translucide sur le fond du volet.
pub fn black(alpha: f32) -> Rgba {
    Rgba {
        r: 0.,
        g: 0.,
        b: 0.,
        a: alpha,
    }
}

/// `FOCUS` à `alpha` : sélection d'une ligne de l'arbre et son contour.
pub fn focus(alpha: f32) -> Rgba {
    Rgba {
        a: alpha,
        ..color(FOCUS)
    }
}

/// `top` à son opacité posé sur `bottom`, supposé opaque : le résultat est opaque.
#[cfg(test)]
pub fn composite(top: Rgba, bottom: Rgba) -> Rgba {
    let mix = |over: f32, under: f32| over * top.a + under * (1. - top.a);
    Rgba {
        r: mix(top.r, bottom.r),
        g: mix(top.g, bottom.g),
        b: mix(top.b, bottom.b),
        a: 1.,
    }
}

/// Luminance relative WCAG 2 d'une couleur opaque.
#[cfg(test)]
fn relative_luminance(color: Rgba) -> f32 {
    let linear = |channel: f32| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

/// Ratio de contraste WCAG 2 d'un texte sur un fond opaque : le texte est d'abord composé sur le fond.
#[cfg(test)]
pub fn contrast_ratio(text: Rgba, background: Rgba) -> f32 {
    let text = relative_luminance(composite(text, background));
    let background = relative_luminance(background);
    let (light, dark) = if text > background {
        (text, background)
    } else {
        (background, text)
    };
    (light + 0.05) / (dark + 0.05)
}

/// Une paire texte et fond de la table de contraste, avec son ratio.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub struct ContrastPair {
    pub text: &'static str,
    pub background: &'static str,
    pub ratio: f32,
}

/// `layers` posés l'un après l'autre sur `base` : un fond de survol composé sur la surface où il apparaît.
#[cfg(test)]
fn stacked(base: Rgba, layers: &[Rgba]) -> Rgba {
    layers
        .iter()
        .fold(base, |under, layer| composite(*layer, under))
}

/// Toute paire texte et fond que l'interface peut afficher (spec jalon 4, §4.4, AC-12) : le produit croisé des
/// niveaux de texte à lire et des fonds réels. Un niveau de texte ou une surface neuve s'ajoute d'une ligne dans
/// son tableau. Exemptés : le texte désactivé, les icônes, la palette ANSI et le HUD.
#[cfg(test)]
pub fn contrast_table() -> Vec<ContrastPair> {
    let window = color(WINDOW);
    let sidebar = color(SIDEBAR);
    let panel = color(PANEL);
    let popover = stacked(window, &[color_alpha(POPOVER)]);
    let texts = [
        ("TEXT_PRIMARY", white(TEXT_PRIMARY)),
        ("TEXT_PROJECT", white(TEXT_PROJECT)),
        ("TEXT_SECONDARY", white(TEXT_SECONDARY)),
        ("TEXT_MIN", white(TEXT_MIN)),
        ("TEXT_HOVER", white(TEXT_HOVER)),
        ("TEXT_LABEL", white(TEXT_LABEL)),
        ("TEXT_FULL", white(TEXT_FULL)),
        ("TREE_TEXT", white(TREE_TEXT)),
    ];
    // Le texte d'erreur ne s'affiche que sur ces cinq surfaces : « Config invalide » (barre latérale et sections de « Personnaliser »), la bulle
    // d'échec du dump (fond de popover), le refus d'un nom sous le champ de l'arbre (creux de l'arbre) et le `−M` de l'en-tête
    // du diff (fond du volet). Un rouge sur
    // un fond de survol n'existe pas dans l'interface.
    let error_text = color(ERROR_TEXT);
    let error_surfaces = ["SIDEBAR", "POPOVER", "TREE", "PANEL", "WINDOW"];
    // L'arbre du volet Fichiers : creux noir sur `PANEL`, puis son survol (`ACTION_HOVER`) et sa ligne sélectionnée.
    let tree = stacked(panel, &[black(TREE_BACKGROUND)]);
    let tree_surfaces = [
        ("TREE", tree),
        ("TREE+HOVER", stacked(tree, &[white(ACTION_HOVER)])),
        ("TREE+SELECTED", stacked(tree, &[focus(TREE_SELECTED)])),
        ("TREE+DROP", stacked(tree, &[focus(TREE_DROP)])),
    ];
    // Le champ du filtre : son creux noir sur celui de l'arbre. Le texte tapé (`TEXT_FULL`) et le texte indicatif
    // (`TEXT_MIN`) s'y posent ; le contour n'est pas un texte.
    // Le champ d'adresse du volet Browser pose le même creux directement sur le fond du volet, et ceux de « Personnaliser »
    // directement sur le fond de la fenêtre (le message d'erreur d'une saisie s'y pose aussi : `WINDOW` ci-dessous).
    let fields = [
        ("WINDOW+FIELD", stacked(window, &[black(FIELD_BACKGROUND)])),
        ("TREE+FIELD", stacked(tree, &[black(FIELD_BACKGROUND)])),
        ("PANEL+FIELD", stacked(panel, &[black(FIELD_BACKGROUND)])),
    ];
    let field_pairs = fields.into_iter().flat_map(|field| {
        [("TEXT_FULL", TEXT_FULL), ("TEXT_MIN", TEXT_MIN)]
            .into_iter()
            .map(move |(name, alpha)| ContrastPair {
                text: name,
                background: field.0,
                ratio: contrast_ratio(white(alpha), field.1),
            })
    });
    let surfaces = [
        ("WINDOW", window),
        ("SIDEBAR", sidebar),
        ("PANEL", panel),
        ("POPOVER", popover),
        ("WINDOW+HOVER", stacked(window, &[white(HOVER)])),
        (
            "WINDOW+HOVER+HOVER_STRONG",
            stacked(window, &[white(HOVER), white(HOVER_STRONG)]),
        ),
        ("SIDEBAR+HOVER", stacked(sidebar, &[white(HOVER)])),
        (
            "SIDEBAR+HOVER_STRONG",
            stacked(sidebar, &[white(HOVER_STRONG)]),
        ),
        ("SIDEBAR+SELECTED", stacked(sidebar, &[white(SELECTED)])),
        (
            "SIDEBAR+ACTION_HOVER",
            stacked(sidebar, &[white(ACTION_HOVER)]),
        ),
        ("PANEL+HOVER_STRONG", stacked(panel, &[white(HOVER_STRONG)])),
        // La puce du sélecteur de Changes survolée : `CONTROL` sur l'en-tête du volet (`PANEL`), dont le texte est
        // `TEXT_LABEL`.
        ("PANEL+CONTROL", stacked(panel, &[white(CONTROL)])),
        // L'onglet actif du volet Browser : `BUTTON_HOVER` sur l'en-tête du volet.
        ("PANEL+BUTTON_HOVER", stacked(panel, &[white(BUTTON_HOVER)])),
        // La ligne courante de l'éditeur : `HOVER` sur `PANEL`.
        ("PANEL+HOVER", stacked(panel, &[white(HOVER)])),
        tree_surfaces[0],
        tree_surfaces[1],
        tree_surfaces[2],
        tree_surfaces[3],
    ];
    // Les glyphes de type de fichier se posent sur les surfaces de l'arbre, et sur le fond de popover du fantôme d'un
    // glisser (une ligne de l'arbre au style d'un popover).
    let glyph_surfaces = [
        tree_surfaces[0],
        tree_surfaces[1],
        tree_surfaces[2],
        tree_surfaces[3],
        ("POPOVER", popover),
        // L'en-tête du diff du volet Changes porte le glyphe du fichier sur le fond du volet.
        ("PANEL", panel),
    ];
    let glyphs = [
        ("GLYPH_MD", GLYPH_MD),
        ("GLYPH_JSON", GLYPH_JSON),
        ("GLYPH_YML", GLYPH_YML),
        ("GLYPH_TOML", GLYPH_TOML),
        ("GLYPH_XML", GLYPH_XML),
        ("GLYPH_HTML", GLYPH_HTML),
        ("GLYPH_ENV", GLYPH_ENV),
    ];
    let glyph_pairs = glyphs.into_iter().flat_map(|(name, hex)| {
        glyph_surfaces
            .into_iter()
            .map(move |(background, surface)| ContrastPair {
                text: name,
                background,
                ratio: contrast_ratio(color(hex), surface),
            })
    });
    // Les lettres de statut de la liste de Changes (`M`, `A` et `U`, `D` et `!`, `R`) se posent sur l'arbre, son survol et
    // sa ligne sélectionnée.
    let letter_surfaces = [tree_surfaces[0], tree_surfaces[1], tree_surfaces[2]];
    let letters = [
        ("STATUS_MODIFIED", STATUS_MODIFIED),
        ("RUNNING", RUNNING),
        ("ERROR_TEXT", ERROR_TEXT),
        ("FOCUS_TEXT", FOCUS_TEXT),
    ];
    let letter_pairs = letters.into_iter().flat_map(|(name, hex)| {
        letter_surfaces
            .into_iter()
            .map(move |(background, surface)| ContrastPair {
                text: name,
                background,
                ratio: contrast_ratio(color(hex), surface),
            })
    });
    // Les couleurs de coloration de l'éditeur ne se posent que sur le fond du volet et sur sa ligne courante.
    let syntax = [
        ("SYNTAX_KEYWORD", SYNTAX_KEYWORD),
        ("SYNTAX_STRING", SYNTAX_STRING),
        ("SYNTAX_NUMBER", SYNTAX_NUMBER),
        ("SYNTAX_COMMENT", SYNTAX_COMMENT),
        ("SYNTAX_TITLE", SYNTAX_TITLE),
        ("SYNTAX_PROPERTY", SYNTAX_PROPERTY),
        ("SYNTAX_TAG", SYNTAX_TAG),
    ];
    let editor_surfaces = [
        ("PANEL", panel),
        ("PANEL+HOVER", stacked(panel, &[white(HOVER)])),
    ];
    let syntax_pairs = syntax.into_iter().flat_map(|(name, hex)| {
        editor_surfaces
            .into_iter()
            .map(move |(background, surface)| ContrastPair {
                text: name,
                background,
                ratio: contrast_ratio(color(hex), surface),
            })
    });
    let errors = surfaces
        .iter()
        .filter(|(name, _)| error_surfaces.contains(name))
        .map(|(name, background)| ContrastPair {
            text: "ERROR_TEXT",
            background: name,
            ratio: contrast_ratio(error_text, *background),
        });
    // La recherche (⌘F) : le texte par défaut du terminal sur le voile d'une occurrence, composé sur le fond du pane
    // (fenêtre ou volet Terminal), et le texte de l'occurrence courante (`WINDOW`) sur son fond `WAITING` plein. Les
    // textes de la barre de recherche sont des niveaux de `texts` sur la surface `POPOVER` : le produit croisé les couvre.
    let [red, green, blue] = idema_term::palette::DEFAULT_FOREGROUND;
    let terminal_text = Rgba {
        r: f32::from(red) / 255.,
        g: f32::from(green) / 255.,
        b: f32::from(blue) / 255.,
        a: 1.,
    };
    let match_veil = Rgba {
        a: SEARCH_MATCH,
        ..color(WAITING)
    };
    // La recherche dans le projet : le texte d'une ligne de résultat (`TREE_TEXT`, ou `TEXT_FULL` quand elle est
    // sélectionnée) sur le voile d'une occurrence, composé sur l'arbre ou sur sa ligne sélectionnée ; l'icône d'une
    // bascule active (`FOCUS_TEXT`) sur son fond, pour mémoire (une icône n'est pas un texte) ; le texte du bouton
    // « Remplacer ».
    let stdio_veil = Rgba {
        a: BADGE_STDIO,
        ..color(AGENT_PURPLE)
    };
    let http_veil = Rgba {
        a: BADGE_HTTP,
        ..color(FOCUS)
    };
    let warning_veil = Rgba {
        a: WARNING_BACKGROUND,
        ..color(STATUS_MODIFIED)
    };
    let error_veil = Rgba {
        a: WARNING_BACKGROUND,
        ..color(ERROR)
    };
    let chip_veil = Rgba {
        a: CHIP_PRESENT,
        ..color(RUNNING)
    };
    let tree_match = stacked(tree, &[match_veil]);
    let selected_match = stacked(tree, &[focus(TREE_SELECTED), match_veil]);
    // Une ligne de résultat survolée (elle ne l'est que non sélectionnée) : le survol, puis le voile de l'occurrence.
    let hovered_match = stacked(tree, &[white(ACTION_HOVER), match_veil]);
    let search = [
        (
            "TREE_TEXT",
            white(TREE_TEXT),
            "TREE+SEARCH_MATCH",
            tree_match,
        ),
        (
            "TEXT_FULL",
            white(TEXT_FULL),
            "TREE+SEARCH_MATCH",
            tree_match,
        ),
        (
            "TREE_TEXT",
            white(TREE_TEXT),
            "TREE+HOVER+SEARCH_MATCH",
            hovered_match,
        ),
        (
            "TEXT_FULL",
            white(TEXT_FULL),
            "TREE+HOVER+SEARCH_MATCH",
            hovered_match,
        ),
        (
            "TREE_TEXT",
            white(TREE_TEXT),
            "TREE+SELECTED+SEARCH_MATCH",
            selected_match,
        ),
        (
            "TEXT_FULL",
            white(TEXT_FULL),
            "TREE+SELECTED+SEARCH_MATCH",
            selected_match,
        ),
        // Le bouton « Remplacer » et le bouton Push du pied de Changes (même fond `HOVER_STRONG` posé sur l'arbre, `CONTROL`
        // au survol, texte `TEXT_PRIMARY`) : leur texte au repos (`HOVER_STRONG`) et au survol (`CONTROL`). Désactivé, son texte
        // (`TEXT_DISABLED`) est exempté, comme tout contrôle désactivé.
        (
            "TEXT_PRIMARY",
            white(TEXT_PRIMARY),
            "TREE+HOVER_STRONG",
            stacked(tree, &[white(HOVER_STRONG)]),
        ),
        (
            "TEXT_PRIMARY",
            white(TEXT_PRIMARY),
            "TREE+CONTROL",
            stacked(tree, &[white(CONTROL)]),
        ),
        (
            "FOCUS_TEXT",
            color(FOCUS_TEXT),
            "TREE+TOGGLE_ACTIVE",
            stacked(tree, &[focus(TOGGLE_ACTIVE)]),
        ),
        (
            "TERMINAL_TEXT",
            terminal_text,
            "WINDOW+SEARCH_MATCH",
            stacked(window, &[match_veil]),
        ),
        (
            "TERMINAL_TEXT",
            terminal_text,
            "PANEL+SEARCH_MATCH",
            stacked(panel, &[match_veil]),
        ),
        (
            "SEARCH_CURRENT_TEXT",
            color(SEARCH_CURRENT_TEXT),
            "SEARCH_CURRENT",
            color(SEARCH_CURRENT),
        ),
        // Le compte des lignes ajoutées (`+N`) dans l'en-tête du diff du volet Changes ; son pendant retiré (`ERROR_TEXT`)
        // est dans `errors`, sur `PANEL`.
        ("RUNNING", color(RUNNING), "PANEL", panel),
        // Le lien « Ouvrir config.toml » de la page d'accueil du volet Browser.
        ("FOCUS_TEXT", color(FOCUS_TEXT), "PANEL", panel),
        // Le chiffre noir de la pastille du bouton Browser (nombre de serveurs en marche) sur son fond vert.
        ("BADGE_TEXT", black(1.), "RUNNING", color(RUNNING)),
        // La fenêtre « Personnaliser » : le texte du bandeau sur son voile, les badges STDIO et HTTP sur leur voile, au
        // repos (`WINDOW`) comme sur une ligne dépliée (`WINDOW+OPEN_ROW`).
        (
            "TEXT_MIN",
            white(TEXT_MIN),
            "WINDOW+NOTICE",
            stacked(window, &[white(NOTICE)]),
        ),
        (
            "AGENT_PURPLE_TEXT",
            color(AGENT_PURPLE_TEXT),
            "WINDOW+BADGE_STDIO",
            stacked(window, &[stdio_veil]),
        ),
        (
            "AGENT_PURPLE_TEXT",
            color(AGENT_PURPLE_TEXT),
            "WINDOW+OPEN_ROW+BADGE_STDIO",
            stacked(window, &[white(OPEN_ROW), stdio_veil]),
        ),
        (
            "FOCUS_TEXT",
            color(FOCUS_TEXT),
            "WINDOW+BADGE_HTTP",
            stacked(window, &[http_veil]),
        ),
        (
            "FOCUS_TEXT",
            color(FOCUS_TEXT),
            "WINDOW+OPEN_ROW+BADGE_HTTP",
            stacked(window, &[white(OPEN_ROW), http_veil]),
        ),
        // Le volet Projet : le texte du bandeau orange (`TREE_TEXT`, 78 % comme `.dt` de la maquette) et sa raison
        // (`TEXT_MIN`) sur son voile, celui du bandeau d'erreur (même voile en `ERROR`), le bandeau d'information
        // (`NOTICE` sur le fond du volet), le texte du CHIP d'un harnais présent sur son voile vert, le bouton du bandeau et l'onglet actif
        // (`TEXT_FULL` sur `SELECTED`).
        (
            "TREE_TEXT",
            white(TREE_TEXT),
            "PANEL+WARNING_BACKGROUND",
            stacked(panel, &[warning_veil]),
        ),
        (
            "TEXT_MIN",
            white(TEXT_MIN),
            "PANEL+WARNING_BACKGROUND",
            stacked(panel, &[warning_veil]),
        ),
        (
            "TREE_TEXT",
            white(TREE_TEXT),
            "PANEL+ERROR_BACKGROUND",
            stacked(panel, &[error_veil]),
        ),
        (
            "TEXT_MIN",
            white(TEXT_MIN),
            "PANEL+NOTICE",
            stacked(panel, &[white(NOTICE)]),
        ),
        (
            "CHIP_PRESENT_TEXT",
            color(CHIP_PRESENT_TEXT),
            "PANEL+CHIP_PRESENT",
            stacked(panel, &[chip_veil]),
        ),
        (
            "CHIP_PRESENT_TEXT",
            color(CHIP_PRESENT_TEXT),
            "PANEL+WARNING_BACKGROUND+CHIP_PRESENT",
            stacked(panel, &[warning_veil, chip_veil]),
        ),
        (
            "TEXT_FULL",
            white(TEXT_FULL),
            "PANEL+SELECTED",
            stacked(panel, &[white(SELECTED)]),
        ),
        // Le bouton du bandeau : `HOVER_STRONG` au repos, `CONTROL` au survol, texte `TEXT_PRIMARY`.
        (
            "TEXT_PRIMARY",
            white(TEXT_PRIMARY),
            "PANEL+WARNING_BACKGROUND+HOVER_STRONG",
            stacked(panel, &[warning_veil, white(HOVER_STRONG)]),
        ),
        (
            "TEXT_PRIMARY",
            white(TEXT_PRIMARY),
            "PANEL+WARNING_BACKGROUND+CONTROL",
            stacked(panel, &[warning_veil, white(CONTROL)]),
        ),
    ]
    .into_iter()
    .map(
        |(text, text_color, background, background_color)| ContrastPair {
            text,
            background,
            ratio: contrast_ratio(text_color, background_color),
        },
    );
    texts
        .iter()
        .flat_map(|(text_name, text)| {
            surfaces
                .iter()
                .map(move |(background_name, background)| ContrastPair {
                    text: text_name,
                    background: background_name,
                    ratio: contrast_ratio(*text, *background),
                })
        })
        .chain(errors)
        .chain(field_pairs)
        .chain(search)
        .chain(glyph_pairs)
        .chain(letter_pairs)
        .chain(syntax_pairs)
        .collect()
}

/// Police de l'interface : SF Pro, par l'alias système de GPUI (`gpui-pre/src/text_system.rs:1420`).
pub const UI_FONT: &str = ".SystemUIFont";

/// Échelle de l'interface, tenue en global GPUI : toute cote de la maquette passe par `px`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub scale: f32,
}

impl Global for Theme {}

impl Theme {
    /// La maquette est dessinée pour un texte de 13 (spec §7.2, correction 1).
    pub fn new(ui_font_size: f32) -> Self {
        Self {
            scale: ui_font_size / 13.,
        }
    }

    /// Une cote de la maquette, à l'échelle, arrondie au pixel pour des bords nets.
    pub fn px(&self, mockup: f32) -> Pixels {
        px((mockup * self.scale).round())
    }

    /// Une taille de texte de la maquette, à l'échelle, sans arrondi.
    pub fn text(&self, mockup: f32) -> Pixels {
        px(mockup * self.scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mockup_dimensions_scale_with_the_ui_font_size() {
        let theme = Theme::new(20.);
        assert_eq!(theme.px(30.), px(46.));
        assert_eq!(theme.px(248.), px(382.));
        assert_eq!(theme.text(13.), px(20.));
        assert_eq!(Theme::new(13.).px(30.), px(30.));
    }

    #[test]
    fn contrast_ratio_matches_the_wcag_reference_values() {
        let black = color(0x000000);
        assert!((contrast_ratio(white(1.), black) - 21.).abs() < 0.01);
        for background in [black, color(WINDOW), color(SIDEBAR), color(0xFFFFFF)] {
            assert!((contrast_ratio(white(0.), background) - 1.).abs() < 0.001);
        }
    }

    #[test]
    fn composite_blends_the_top_colour_by_its_alpha() {
        let grey = composite(white(0.5), color(0x000000));
        for channel in [grey.r, grey.g, grey.b] {
            let value = (channel * 255.).round() as u32;
            assert!((127..=128).contains(&value), "canal à {value}");
        }
        assert_eq!(grey.a, 1.);
        assert_eq!(composite(white(1.), color(WINDOW)), white(1.));
    }

    #[test]
    fn a_hovered_search_line_over_an_occurrence_is_in_the_contrast_table() {
        let table = contrast_table();
        for text in ["TREE_TEXT", "TEXT_FULL"] {
            assert!(
                table
                    .iter()
                    .any(|pair| pair.text == text && pair.background == "TREE+HOVER+SEARCH_MATCH"),
                "{text} on TREE+HOVER+SEARCH_MATCH is missing from contrast_table"
            );
        }
    }

    #[test]
    fn the_push_button_label_is_in_the_contrast_table_at_rest_and_hovered() {
        // Le pied de Changes peint Push en `HOVER_STRONG` sur le creux de l'arbre, puis en `CONTROL` au survol.
        let table = contrast_table();
        for background in ["TREE+HOVER_STRONG", "TREE+CONTROL"] {
            assert!(
                table
                    .iter()
                    .any(|pair| pair.text == "TEXT_PRIMARY" && pair.background == background),
                "TEXT_PRIMARY on {background} is missing from contrast_table"
            );
        }
    }

    #[test]
    fn the_target_chip_label_is_in_the_contrast_table_at_rest_and_hovered() {
        // La puce du sélecteur de Changes : `HOVER_STRONG` sur l'en-tête du volet, `CONTROL` au survol, texte `TEXT_LABEL`.
        let table = contrast_table();
        for background in ["PANEL+HOVER_STRONG", "PANEL+CONTROL"] {
            assert!(
                table
                    .iter()
                    .any(|pair| pair.text == "TEXT_LABEL" && pair.background == background),
                "TEXT_LABEL on {background} is missing from contrast_table"
            );
        }
    }

    #[test]
    fn every_readable_text_reaches_four_and_a_half_to_one() {
        let table = contrast_table();
        assert!(!table.is_empty());
        for pair in &table {
            eprintln!("{} on {}: {:.2}", pair.text, pair.background, pair.ratio);
        }
        // Écart connu, rapporté et non exigé : l'entrée de menu survolée est blanche sur le bleu `FOCUS` de macOS,
        // et le blanc plein ne peut pas monter plus haut (décision de la porte, spec jalon 4, §4.4). Le texte du bouton
        // Commit du volet Changes (blanc plein sur `FOCUS`, bleu primaire fixé par la spec P3 §4.6) est le même écart.
        for (text, alpha) in [("TEXT_FULL", TEXT_FULL), ("HOVER_SHORTCUT", HOVER_SHORTCUT)] {
            let ratio = contrast_ratio(white(alpha), color(FOCUS));
            eprintln!("{text} on FOCUS (rapporté, hors seuil): {ratio:.2}");
        }
        let failing: Vec<String> = table
            .iter()
            .filter(|pair| pair.ratio < MIN_CONTRAST)
            .map(|pair| format!("{} on {}: {:.2}", pair.text, pair.background, pair.ratio))
            .collect();
        assert!(failing.is_empty(), "sous {MIN_CONTRAST}:1 : {failing:?}");
    }
}
