//! Le dessin de l'arbre du volet Fichiers : glyphes, lignes, champ ouvert, fantôme de glisser et liste.

use super::*;

use crate::files::filter::FieldMode;
use crate::files::gestures::{TreeGesture, drop_dir, in_drop_region, region_edges};
use crate::files::ops::NameError;
use crate::files::tree_edit::EditKind;
use crate::theme::{
    ACTION_HOVER, CONTROL, ERROR_TEXT, GLYPH_ENV, GLYPH_HTML, GLYPH_JSON, GLYPH_MD, GLYPH_TOML,
    GLYPH_XML, GLYPH_YML, POPOVER, TREE_BACKGROUND, TREE_SELECTED, TREE_SELECTED_BORDER, TREE_TEXT,
    black, color, color_alpha,
};
use gpui::{AnyElement, ClickEvent, DragMoveEvent, uniform_list};
use std::ops::Range;

/// Ligne de l'arbre (maquette : 25), retrait par niveau de profondeur (14, comme l'arbre de Changes), et la case du
/// glyphe ou du chevron en tête de ligne (maquette : 16).
pub(super) const ROW_HEIGHT: f32 = 25.;
const ROW_INDENT: f32 = 14.;
pub(crate) const GLYPH_SLOT: f32 = 16.;
/// Le retrait sous la liste de l'arbre, hors des lignes : la zone des lignes s'arrête là, pour le glisser comme pour le
/// retrait du conteneur.
const LIST_BOTTOM_INSET: f32 = 6.;
/// Le décalage du fantôme de glisser sous et à droite du pointeur.
const GHOST_OFFSET: f32 = 14.;

/// Le côté du bouton de repli de l'en-tête (maquette : 18) : celui qui tient seul dans la bande repliée de 22.
const TREE_BUTTON: f32 = 18.;
/// Police des glyphes de type : une mono du système (la maquette a SF Mono).
pub(super) const GLYPH_FONT: &str = "Menlo";

/// Le glyphe de type d'un fichier : au plus deux caractères en mono gras, dans la couleur de son format.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    pub text: &'static str,
    /// Corps du texte en cotes de la maquette (9 à 10,5, graisse 700 comme les pastilles de la maquette).
    pub size: f32,
    pub color: u32,
}

/// Le glyphe du fichier `name`, d'après son extension ou son nom (`.env`, `.env.local`, `prod.env`). Un format sans
/// glyphe propre rend `None` et l'arbre dessine le fichier générique.
pub fn glyph_for(name: &str) -> Option<Glyph> {
    let lower = name.to_lowercase();
    if lower == ".env" || lower.starts_with(".env.") || lower.ends_with(".env") {
        return Some(Glyph {
            text: "EV",
            size: 9.,
            color: GLYPH_ENV,
        });
    }
    let extension = lower.rsplit_once('.')?.1;
    let (text, size, color) = match extension {
        "md" | "markdown" => ("M↓", 9., GLYPH_MD),
        "json" => ("{}", 10.5, GLYPH_JSON),
        "yml" | "yaml" => ("YM", 9., GLYPH_YML),
        "toml" => ("TL", 9., GLYPH_TOML),
        "xml" => ("XM", 9., GLYPH_XML),
        "html" | "htm" => ("<>", 10.5, GLYPH_HTML),
        _ => return None,
    };
    Some(Glyph { text, size, color })
}

/// Ce qu'un glisser de l'arbre porte : l'entrée saisie.
#[derive(Clone)]
pub struct DraggedEntry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub name: String,
}

/// Le fantôme qui suit la souris (une ligne de l'arbre, glyphe et nom, sur le fond d'un popover). Il est décalé en bas à
/// droite du pointeur : la ligne sous la souris, la cible du dépôt, reste lisible.
struct DragGhost {
    entry: DraggedEntry,
    /// Où la souris a saisi la ligne, depuis son coin haut gauche : GPUI place le fantôme à la souris moins ce décalage.
    offset: Point<Pixels>,
    /// La largeur de la colonne au départ du glisser, moins ses retraits : le fantôme ne la dépasse pas.
    max_width: f32,
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        // GPUI ignore la marge de la racine d'un fantôme : le décalage passe par le remplissage d'une enveloppe sans fond.
        div()
            .pl(self.offset.x + theme.px(GHOST_OFFSET))
            .pt(self.offset.y + theme.px(GHOST_OFFSET))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.px(7.))
                    .h(theme.px(ROW_HEIGHT))
                    .max_w(theme.px(self.max_width))
                    .px(theme.px(8.))
                    .rounded(theme.px(5.))
                    .border_1()
                    .border_color(white(CONTROL))
                    .bg(color_alpha(POPOVER))
                    .text_color(white(TREE_TEXT))
                    .text_size(theme.text(12.5))
                    .child(
                        div()
                            .flex_none()
                            .w(theme.px(GLYPH_SLOT))
                            .flex()
                            .justify_center()
                            .child(row_lead(&theme, &self.entry.name, self.entry.is_dir, false)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(self.entry.name.clone()),
                    ),
            )
    }
}

/// Le glyphe d'une ligne de l'arbre : le chevron d'un dossier, le glyphe de type d'un fichier reconnu, sinon la page.
pub(crate) fn row_lead(theme: &Theme, name: &str, is_dir: bool, expanded: bool) -> AnyElement {
    if is_dir {
        let chevron = if expanded {
            Icon::ExpandMore
        } else {
            Icon::ChevronRight
        };
        svg()
            .path(chevron.path())
            .size(theme.px(GLYPH_SLOT))
            .text_color(white(ICON_SECONDARY))
            .into_any_element()
    } else if let Some(glyph) = glyph_for(name) {
        div()
            .font_family(GLYPH_FONT)
            .font_weight(gpui::FontWeight::BOLD)
            .text_size(theme.text(glyph.size))
            .line_height(theme.px(GLYPH_SLOT))
            .text_color(color(glyph.color))
            .child(glyph.text)
            .into_any_element()
    } else {
        svg()
            .path(Icon::Draft.path())
            .size(theme.px(14.))
            .text_color(white(ICON_SECONDARY))
            .into_any_element()
    }
}

impl FilesPanel {
    /// Une ligne de la liste dessinée : une ligne de l'arbre, le champ ouvert, ou le message de son refus.
    fn render_item(&self, index: usize, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        match self.items[index] {
            ListItem::Row(row) => self.render_row(row, theme, cx).into_any_element(),
            ListItem::Field { depth, is_dir } => self.render_field(depth, is_dir, theme),
            ListItem::Message { depth } => self.render_message(depth, theme),
        }
    }

    /// Le champ de création ou de renommage : une ligne de l'arbre (retrait de sa profondeur, chevron ou glyphe en
    /// tête) dont le nom est le champ. Il est sélectionné : fond et contour de la ligne sélectionnée.
    fn render_field(&self, depth: usize, is_dir: bool, theme: &Theme) -> AnyElement {
        let Some(edit) = self.edit.as_ref() else {
            return div().into_any_element();
        };
        let lead = if is_dir {
            let expanded = edit.slot.kind == EditKind::Rename
                && self
                    .rows
                    .iter()
                    .any(|row| row.path == edit.slot.anchor && row.expanded);
            let chevron = if expanded {
                Icon::ExpandMore
            } else {
                Icon::ChevronRight
            };
            svg()
                .path(chevron.path())
                .size(theme.px(GLYPH_SLOT))
                .text_color(white(ICON_SECONDARY))
                .into_any_element()
        } else {
            svg()
                .path(Icon::Draft.path())
                .size(theme.px(14.))
                .text_color(white(ICON_SECONDARY))
                .into_any_element()
        };
        div()
            .id("files-edit-field")
            .flex()
            .items_center()
            .gap(theme.px(7.))
            .h(theme.px(ROW_HEIGHT))
            .w_full()
            .pl(theme.px(8. + ROW_INDENT * depth as f32))
            .pr(theme.px(8.))
            .rounded(theme.px(5.))
            .border_1()
            .border_color(focus(TREE_SELECTED_BORDER))
            .bg(focus(TREE_SELECTED))
            // Un appui droit dans le champ n'ouvre pas le menu de la racine (la colonne l'écoute) et n'annule rien.
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex_none()
                    .w(theme.px(GLYPH_SLOT))
                    .flex()
                    .justify_center()
                    .child(lead),
            )
            .child(edit.field.clone())
            .into_any_element()
    }

    /// Le message d'un refus de nom, sous le champ : une ligne de la même hauteur, à l'aplomb du nom.
    fn render_message(&self, depth: usize, theme: &Theme) -> AnyElement {
        let message = self
            .edit
            .as_ref()
            .and_then(|edit| edit.slot.error)
            .map_or("", NameError::message);
        div()
            .id("files-edit-message")
            .flex()
            .items_center()
            .h(theme.px(ROW_HEIGHT))
            .w_full()
            .pl(theme.px(1. + 8. + ROW_INDENT * depth as f32 + GLYPH_SLOT + 7.))
            .pr(theme.px(8.))
            .text_size(theme.text(11.))
            .text_color(color(ERROR_TEXT))
            .child(message)
            .into_any_element()
    }

    fn render_row(
        &self,
        index: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let row = &self.rows[index];
        let selected = self.selected.as_ref() == Some(&row.path);
        let text = if selected {
            TEXT_FULL
        } else if row.ignored {
            TEXT_MIN
        } else {
            TREE_TEXT
        };
        let lead = row_lead(theme, &row.name, row.is_dir, row.expanded);
        let path = row.path.clone();
        let is_dir = row.is_dir;
        // Le dossier qui recevrait un dépôt ici (E47) : celui de la ligne, ou le dossier d'un fichier.
        let drop_into = drop_dir(Some(row), &self.root);
        // La zone qui reçoit s'éclaire pendant un glisser seulement : un glisser abandonné ailleurs ne laisse rien.
        let lit = cx.has_active_drag()
            && self
                .drop_target
                .as_deref()
                .is_some_and(|target| in_drop_region(&row.path, target));
        // Dans la zone éclairée, seules les lignes du haut et du bas arrondissent leurs coins : la zone se lit d'un seul
        // tenant, sans pastilles ni bord déchiqueté.
        let (region_top, region_bottom) = match self.drop_target.as_deref() {
            Some(target) if lit => region_edges(
                index
                    .checked_sub(1)
                    .and_then(|i| self.rows.get(i))
                    .map(|row| row.path.as_path()),
                self.rows.get(index + 1).map(|row| row.path.as_path()),
                target,
            ),
            _ => (true, true),
        };
        let dragged = DraggedEntry {
            path: row.path.clone(),
            is_dir: row.is_dir,
            name: row.name.clone(),
        };
        let radius = theme.px(5.);
        div()
            .id(("files-row", index))
            .flex()
            .items_center()
            .gap(theme.px(7.))
            .h(theme.px(ROW_HEIGHT))
            .w_full()
            .pl(theme.px(8. + ROW_INDENT * row.depth as f32))
            .pr(theme.px(8.))
            .rounded(radius)
            // Le contour est toujours là, transparent hors sélection : la ligne ne change pas de taille.
            .border_1()
            .border_color(black(0.))
            .text_color(white(text))
            .text_size(theme.text(12.5))
            .when(is_dir, |row| row.cursor_pointer())
            .when(selected, |row| {
                row.bg(focus(TREE_SELECTED))
                    .border_color(focus(TREE_SELECTED_BORDER))
            })
            .when(!selected, |row| {
                row.hover(|style| style.bg(white(ACTION_HOVER)))
            })
            .when(lit, |row| {
                row.bg(focus(TREE_DROP))
                    .rounded_tl(if region_top { radius } else { px(0.) })
                    .rounded_tr(if region_top { radius } else { px(0.) })
                    .rounded_bl(if region_bottom { radius } else { px(0.) })
                    .rounded_br(if region_bottom { radius } else { px(0.) })
            })
            .on_mouse_down(MouseButton::Left, {
                let path = path.clone();
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.on_row_pressed(&path, window, cx);
                })
            })
            .on_mouse_down(MouseButton::Right, {
                let target = TreeTarget::Entry {
                    path: path.clone(),
                    is_dir,
                };
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.on_tree_menu_requested(target.clone(), event.position, window, cx);
                    // La colonne ouvrirait le menu de la racine par-dessus celui de la ligne.
                    cx.stop_propagation();
                })
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.on_row_clicked(&path, is_dir, event.click_count(), window, cx);
            }))
            .on_drag(dragged, {
                let panel = cx.entity().downgrade();
                let ghost_width = self.shown_tree_width() - 12.;
                move |entry, offset, _window, cx| {
                    // Un nouveau glisser part sans rien d'éclairé, quoi qu'un précédent ait laissé.
                    panel
                        .update(cx, |this, cx| {
                            this.drop_target = None;
                            this.drop_candidate = None;
                            this.pointer_over_list = false;
                            cx.notify();
                        })
                        .ok();
                    cx.new(|_| DragGhost {
                        entry: entry.clone(),
                        offset,
                        max_width: ghost_width,
                    })
                }
            })
            // La capture donne la souris à la ligne qu'elle contient, après la colonne qui a posé la racine.
            .on_drag_move::<DraggedEntry>(cx.listener({
                let target = drop_into.clone();
                move |this, event: &DragMoveEvent<DraggedEntry>, _window, _cx| {
                    if this.pointer_over_list && event.bounds.contains(&event.event.position) {
                        this.drop_candidate = Some(target.clone());
                    }
                }
            }))
            .on_drop::<DraggedEntry>(cx.listener(move |this, entry: &DraggedEntry, window, cx| {
                this.move_dragged(entry, &drop_into, window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .w(theme.px(GLYPH_SLOT))
                    .flex()
                    .justify_center()
                    .child(lead),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(row.name.clone()),
            )
    }

    /// Le bouton de l'en-tête de l'arbre : replie l'arbre en bande, ou le déplie.
    fn tree_toggle_button(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let icon = if self.tree_collapsed {
            Icon::LeftPanelOpen
        } else {
            Icon::LeftPanelClose
        };
        div()
            .id("files-tree-toggle")
            .group("files-tree-toggle")
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(theme.px(TREE_BUTTON))
            .rounded(theme.px(5.))
            .cursor_pointer()
            .hover(|style| style.bg(white(HOVER_STRONG)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    this.toggle_tree(cx);
                }),
            )
            .child(
                svg()
                    .path(icon.path())
                    .size(theme.px(14.))
                    .text_color(white(TEXT_MIN))
                    .group_hover("files-tree-toggle", |style| {
                        style.text_color(white(TEXT_HOVER))
                    }),
            )
    }

    /// La poignée du bord droit de la colonne (le geste est celui de `column::render_handle`) : le relâché écrit la largeur
    /// s'il y a eu mouvement, un double-clic rend la largeur de la maquette.
    pub(super) fn render_tree_handle(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        column::render_handle(
            cx.entity(),
            self.tree_drag.is_some(),
            theme.px(self.shown_tree_width()),
            HandleActions {
                begin: Self::begin_tree_drag,
                drag: Self::drag_tree_to,
                end: Self::end_tree_drag,
                reset: Self::reset_tree_width,
            },
        )
    }

    /// La colonne de l'arbre : en-tête avec son bouton, puis les lignes ; repliée, la bande ne garde que le bouton.
    pub(super) fn render_tree(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let width = self.shown_tree_width();
        // Déplié : un retrait de 8, la boîte du champ du filtre et le bouton. Replié, la bande ne garde que le bouton.
        let collapsed = self.tree_collapsed;
        let header = div()
            .h(theme.px(TREE_HEADER_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.px(6.))
            .when(collapsed, |header| header.px(theme.px(2.)))
            .when(!collapsed, |header| {
                header
                    .px(theme.px(8.))
                    .child(self.render_filter_box(theme, cx))
            })
            .child(self.tree_toggle_button(theme, cx));
        let column = div()
            .id("files-tree")
            .flex_none()
            .w(theme.px(width))
            .h_full()
            .flex()
            .flex_col()
            .bg(black(TREE_BACKGROUND))
            .border_r_1()
            .border_color(white(SEPARATOR))
            .child(header);
        if self.tree_collapsed {
            return column;
        }
        let tree_list = uniform_list(
            "files-rows",
            self.items.len(),
            cx.processor(
                |this, range: Range<usize>, _window, cx: &mut Context<Self>| {
                    let theme = *cx.global::<Theme>();
                    range
                        .map(|index| this.render_item(index, &theme, cx))
                        .collect::<Vec<_>>()
                },
            ),
        )
        .track_scroll(&self.scroll)
        .size_full();
        // Le texte du champ décide de la zone : l'arbre, la liste du filtre, ou les résultats de la recherche.
        let in_tree = matches!(self.filter_mode(), FieldMode::Tree);
        let list = if in_tree {
            tree_list.into_any_element()
        } else {
            self.render_filter_results(theme, cx)
        };
        // La colonne porte le focus de l'arbre et son contexte de touches : `↩` et `⌘⌫` n'y valent que là. Un appui
        // droit qui n'a pas été pris par une ligne (le vide, l'en-tête) vise la racine.
        column
            .track_focus(&self.tree_focus)
            .key_context(TREE_KEY_CONTEXT)
            // Sous la liste du filtre ou de la recherche, l'arbre est caché : un appui du bouton gauche dans la colonne,
            // hors des lignes de résultats (le vide, les marges de l'en-tête), donne le focus au champ et non à l'arbre
            // invisible, dont les touches n'iraient nulle part. Les lignes et les bascules ont déjà pris leur appui.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    if !this.tree_collapsed && !matches!(this.filter_mode(), FieldMode::Tree) {
                        this.focus_filter_field(window, cx);
                        window.prevent_default();
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &RenameSelected, window, cx| {
                // Un champ ouvert tient le clavier : la touche est la sienne, jamais celle de l'arbre. Et l'arbre n'agit que
                // s'il est à l'écran : la liste du filtre prend sa place tant que le champ du filtre n'est pas vide.
                if this.edit.is_none() && matches!(this.filter_mode(), FieldMode::Tree) {
                    this.run_on_selected(TreeGesture::Rename, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &TrashSelected, window, cx| {
                // Même garde que `RenameSelected` : ni champ ouvert, ni liste du filtre à la place de l'arbre.
                if this.edit.is_none() && matches!(this.filter_mode(), FieldMode::Tree) {
                    this.run_on_selected(TreeGesture::Trash, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    // Le menu de la racine agit sur l'arbre : il n'a pas de sens sous la liste du filtre, qui le cache. Le
                    // focus reste alors là où il est (le champ), la colonne ne le reprend pas.
                    if matches!(this.filter_mode(), FieldMode::Tree) {
                        this.on_tree_menu_requested(TreeTarget::Root, event.position, window, cx);
                    } else {
                        window.prevent_default();
                    }
                }),
            )
            // Un glisser passe d'abord ici (capture, de la racine aux feuilles) : la racine est le candidat tant que
            // la souris est dans la colonne, les lignes l'écrasent ensuite. Le choix se rend après leur passage.
            .on_drag_move::<DraggedEntry>(cx.listener(
                |this, event: &DragMoveEvent<DraggedEntry>, window, cx| {
                    let theme = *cx.global::<Theme>();
                    let (bounds, position) = (event.bounds, event.event.position);
                    let inside = bounds.contains(&position);
                    // La zone des lignes : sous l'en-tête, au-dessus du retrait sous la liste. Hors d'elle, une ligne
                    // rognée par le défilement ne parle pas, même si sa boîte entière couvre la souris.
                    this.pointer_over_list = inside
                        && position.y >= bounds.origin.y + theme.px(TREE_HEADER_HEIGHT)
                        && position.y
                            < bounds.origin.y + bounds.size.height - theme.px(LIST_BOTTOM_INSET);
                    this.drop_candidate = inside.then(|| this.root.clone());
                    let source = event.drag(cx).path.clone();
                    cx.defer_in(window, move |this, _window, cx| {
                        this.settle_drop_target(&source, cx);
                    });
                },
            ))
            // Le vide sous les lignes et l'en-tête valent la racine (E47) ; une ligne prend le dépôt avant.
            .on_drop::<DraggedEntry>(cx.listener(|this, entry: &DraggedEntry, window, cx| {
                let root = this.root.clone();
                this.move_dragged(entry, &root, window, cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(theme.px(6.))
                    .pb(theme.px(LIST_BOTTOM_INSET))
                    .child(list),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_follow_the_extension_or_the_env_name() {
        for name in ["README.md", "notes.MD", "a.markdown"] {
            assert_eq!(glyph_for(name).unwrap().color, GLYPH_MD, "{name}");
        }
        assert_eq!(glyph_for("package.json").unwrap().text, "{}");
        for name in ["docker-compose.yml", "ci.yaml"] {
            assert_eq!(glyph_for(name).unwrap().color, GLYPH_YML, "{name}");
        }
        assert_eq!(glyph_for("Cargo.toml").unwrap().color, GLYPH_TOML);
        assert_eq!(glyph_for("pom.xml").unwrap().color, GLYPH_XML);
        for name in ["index.html", "old.htm"] {
            assert_eq!(glyph_for(name).unwrap().color, GLYPH_HTML, "{name}");
        }
        for name in [".env", ".env.local", ".env.production", "prod.env"] {
            assert_eq!(glyph_for(name).unwrap().color, GLYPH_ENV, "{name}");
        }
        for name in ["main.rs", "Makefile", "archive.tar.gz", ".gitignore", "env"] {
            assert!(glyph_for(name).is_none(), "{name}");
        }
    }

    #[test]
    fn every_glyph_is_two_characters_at_most() {
        let names = [
            "a.md", "a.json", "a.yml", "a.toml", "a.xml", "a.html", ".env",
        ];
        for name in names {
            let glyph = glyph_for(name).unwrap();
            assert!(glyph.text.chars().count() <= 2, "{name}");
            assert!((9.0..=11.0).contains(&glyph.size), "{name}");
        }
    }
}
