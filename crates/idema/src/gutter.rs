//! La gouttière entre deux panes côte à côte. tmux ne réserve qu'une cellule entre deux panes : large de quelques
//! pixels, elle laisse deux colonnes de texte bien plus proches que deux panes empilés (une hauteur de cellule).
//! Idema demande donc à tmux une fenêtre plus étroite et ajoute à chaque séparateur vertical de quoi porter
//! l'écart à une hauteur de cellule, le trait de 1 px restant au milieu. Les panes se placent en pixels.
//!
//! Les nombres de gouttières viennent de `idema_tmux::layout` (`column_gaps`, `gapped_panes`,
//! `gapped_separators`) ; ce module en fait des cadres en pixels.

use idema_tmux::layout::{GappedSeparator, Orientation, PaneRect};

/// Un rectangle en pixels, depuis l'origine de la zone utile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Frame {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

/// Ce qu'une gouttière ajoute à la cellule que tmux réserve : l'écart entre deux zones de texte vaut alors une
/// hauteur de cellule. Rien pour une cellule plus large que haute.
pub(crate) fn gutter_extra(cell: (f32, f32)) -> f32 {
    (cell.1 - cell.0).max(0.)
}

/// Colonnes à demander à tmux pour une zone utile de `content_width` pixels qui porte `gaps` gouttières. Au moins
/// 2, ce que l'émulateur accepte.
pub(crate) fn session_columns(content_width: f32, gaps: u16, cell: (f32, f32)) -> usize {
    let room = content_width - f32::from(gaps) * gutter_extra(cell);
    ((room / cell.0).floor() as usize).max(2)
}

/// L'emplacement d'un pane : son rectangle de cellules à l'échelle de la cellule, décalé de ses gouttières.
pub(crate) fn pane_frame(rect: &PaneRect, gaps_before: u16, cell: (f32, f32)) -> Frame {
    Frame {
        x: f32::from(rect.x) * cell.0 + f32::from(gaps_before) * gutter_extra(cell),
        y: f32::from(rect.y) * cell.1,
        width: f32::from(rect.width) * cell.0,
        height: f32::from(rect.height) * cell.1,
    }
}

/// Le trait d'un séparateur : 1 px au milieu de l'espace entre deux panes. Un trait vertical se place au milieu de
/// la cellule de tmux élargie d'une gouttière ; un trait horizontal s'allonge des gouttières de son nœud.
pub(crate) fn separator_frame(gapped: &GappedSeparator, cell: (f32, f32)) -> Frame {
    let separator = &gapped.separator;
    let extra = gutter_extra(cell);
    let at = f32::from(separator.at);
    let from = f32::from(separator.from);
    let length = f32::from(separator.length);
    let before = f32::from(gapped.gaps_before) * extra;
    match separator.orientation {
        Orientation::Vertical => Frame {
            x: at * cell.0 + before + ((cell.0 + extra) / 2.).floor(),
            y: from * cell.1,
            width: 1.,
            height: length * cell.1,
        },
        Orientation::Horizontal => Frame {
            x: from * cell.0 + before,
            y: at * cell.1 + (cell.1 / 2.).floor(),
            width: length * cell.0 + f32::from(gapped.gaps_within) * extra,
            height: 1.,
        },
    }
}

#[cfg(test)]
mod tests {
    use idema_tmux::layout::{Preset, Separator, WindowLayout, preset_cell};
    use idema_tmux::protocol::PaneId;

    use super::*;

    const CELL: (f32, f32) = (10., 24.);

    fn layout_of(preset: Preset) -> WindowLayout {
        WindowLayout {
            root: preset_cell(preset, 120, 40).unwrap(),
            active: None,
        }
    }

    #[test]
    fn two_columns_are_one_cell_height_apart() {
        let layout = layout_of(Preset::Columns2);
        let panes = layout.gapped_panes();
        let left = pane_frame(&panes[0].0, panes[0].1, CELL);
        let right = pane_frame(&panes[1].0, panes[1].1, CELL);
        assert_eq!((left.x, left.width), (0., 600.));
        assert_eq!(right.x, 61. * 10. + 14.);
        let separator = separator_frame(&layout.gapped_separators()[0], CELL);
        assert_eq!(separator.x, 600. + 12.);
        assert_eq!(separator.width, 1.);
        // L'écart entre les zones de texte vaut une hauteur de cellule, et le trait est dans cet écart.
        assert_eq!(right.x - (left.x + left.width), CELL.1);
        assert!(separator.x >= left.x + left.width && separator.x < right.x);
    }

    #[test]
    fn stacked_panes_keep_today_s_geometry() {
        let layout = layout_of(Preset::Rows2);
        for (rect, gaps) in layout.gapped_panes() {
            assert_eq!(gaps, 0);
            assert_eq!(
                pane_frame(&rect, gaps, CELL),
                Frame {
                    x: f32::from(rect.x) * CELL.0,
                    y: f32::from(rect.y) * CELL.1,
                    width: f32::from(rect.width) * CELL.0,
                    height: f32::from(rect.height) * CELL.1,
                }
            );
        }
        let separator = separator_frame(&layout.gapped_separators()[0], CELL);
        assert_eq!(
            separator,
            Frame {
                x: 0.,
                y: 20. * CELL.1 + 12.,
                width: 1200.,
                height: 1.,
            }
        );
    }

    #[test]
    fn a_horizontal_separator_spans_the_gutters_of_its_row() {
        assert!(gutter_extra(CELL) > 0.);
        let layout = layout_of(Preset::ThreeTwo);
        let horizontal = layout
            .gapped_separators()
            .into_iter()
            .find(|gapped| gapped.separator.orientation == Orientation::Horizontal)
            .unwrap();
        let frame = separator_frame(&horizontal, CELL);
        // La rangée du haut a deux gouttières : le trait couvre ses trois zones de texte et leurs écarts.
        assert_eq!(frame.width, 120. * CELL.0 + 2. * gutter_extra(CELL));
        let last_top = layout.gapped_panes()[2];
        let last = pane_frame(&last_top.0, last_top.1, CELL);
        assert_eq!(frame.x + frame.width, last.x + last.width);
    }

    #[test]
    fn session_columns_leave_room_for_the_gutters() {
        assert_eq!(session_columns(1000., 2, CELL), 97);
        assert_eq!(session_columns(1000., 0, CELL), 100);
    }

    #[test]
    fn a_cell_wider_than_tall_adds_nothing() {
        assert_eq!(gutter_extra((10., 8.)), 0.);
        assert_eq!(session_columns(1000., 2, (10., 8.)), 100);
    }

    fn ungapped(separator: Separator) -> GappedSeparator {
        GappedSeparator {
            separator,
            gaps_before: 0,
            gaps_within: 0,
        }
    }

    #[test]
    fn panes_and_separators_are_placed_on_the_cell_grid() {
        let cell = (10., 20.);
        let rect = PaneRect {
            pane: PaneId(2),
            x: 61,
            y: 5,
            width: 59,
            height: 35,
        };
        assert_eq!(
            pane_frame(&rect, 0, cell),
            Frame {
                x: 610.,
                y: 100.,
                width: 590.,
                height: 700.
            }
        );
        // Le trait vertical passe au milieu de la cellule de tmux élargie de la gouttière (10 + 10 px), sur toute
        // la hauteur du groupe.
        let vertical = ungapped(Separator {
            orientation: Orientation::Vertical,
            at: 60,
            from: 0,
            length: 40,
        });
        assert_eq!(
            separator_frame(&vertical, cell),
            Frame {
                x: 610.,
                y: 0.,
                width: 1.,
                height: 800.
            }
        );
        // Une cellule plus large que haute n'ajoute rien : le trait garde sa place d'avant.
        assert_eq!(separator_frame(&vertical, (10., 8.)).x, 605.);
        let horizontal = ungapped(Separator {
            orientation: Orientation::Horizontal,
            at: 20,
            from: 0,
            length: 120,
        });
        assert_eq!(
            separator_frame(&horizontal, (8., 25.)),
            Frame {
                x: 0.,
                y: 512.,
                width: 960.,
                height: 1.
            }
        );
    }
}
