//! La colonne à poignée des volets droits : la règle de largeur (une préférence rognée par le plancher de la colonne et
//! par celui de l'autre côté), le glisser de la poignée et la poignée elle-même. L'arbre du volet Fichiers (jalon 4 de P2)
//! et la liste du volet Changes (P3-10) s'en servent : chaque volet garde sa largeur voulue et ses planchers, ce module
//! garde le geste.

use gpui::{
    Context, CursorStyle, DispatchPhase, Entity, HitboxBehavior, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, canvas, prelude::*, px,
};

use crate::theme::Theme;
use crate::workspace::HANDLE_WIDTH;

/// La largeur que la colonne montre : la préférence (ou `default`), rognée pour laisser `other_min` à l'autre côté dans
/// un corps de `body`, sans jamais passer sous `floor`. La préférence n'est pas touchée : un volet rétréci puis rélargi
/// rend la colonne à la largeur voulue.
pub(crate) fn width_shown(
    wanted: Option<f32>,
    body: f32,
    default: f32,
    floor: f32,
    other_min: f32,
) -> f32 {
    wanted.unwrap_or(default).min(body - other_min).max(floor)
}

/// La largeur que le glisser écrit : celle de départ plus le déplacement (unités de la maquette), telle que la colonne la
/// montre, donc bornée par les deux planchers. C'est la valeur gardée, jamais celle de la souris.
pub(crate) fn dragged_width(
    start_width: f32,
    delta: f32,
    body: f32,
    default: f32,
    floor: f32,
    other_min: f32,
) -> f32 {
    width_shown(Some(start_width + delta), body, default, floor, other_min)
}

/// Le glisser de la poignée : l'abscisse de l'appui et la largeur montrée alors, plus un drapeau qui garde un simple clic
/// de réécrire `state.json` (comme les bords de `Workspace`).
#[derive(Clone, Copy)]
pub(crate) struct ColumnDrag {
    pub start_x: Pixels,
    pub start_width: f32,
    /// La largeur voulue avant l'appui : un glisser abandonné la rend, rien n'ayant été écrit.
    pub start_wanted: Option<f32>,
    pub moved: bool,
}

/// Le glisser s'arrête sans relâché : la largeur voulue d'avant l'appui revient et rien n'est écrit. Rend `true` s'il y
/// avait un glisser en cours.
pub(crate) fn cancel_drag(drag: &mut Option<ColumnDrag>, wanted: &mut Option<f32>) -> bool {
    match drag.take() {
        Some(drag) => {
            *wanted = drag.start_wanted;
            true
        }
        None => false,
    }
}

/// Ce que la poignée demande au volet qui la porte : le début du glisser (abscisse de l'appui), le mouvement (abscisse
/// courante), la fin (relâché ou relâché perdu) et le retour à la largeur par défaut (double-clic).
pub(crate) struct HandleActions<T> {
    pub begin: fn(&mut T, Pixels, &mut Context<T>),
    pub drag: fn(&mut T, Pixels, &mut Context<T>),
    pub end: fn(&mut T, &mut Context<T>),
    pub reset: fn(&mut T, &mut Context<T>),
}

impl<T> Clone for HandleActions<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for HandleActions<T> {}

/// La poignée du bord droit d'une colonne, sur toute la hauteur du corps : une bande de `HANDLE_WIDTH` pixels d'écran
/// centrée sur le bord (`edge_x`), même geste que les bords de `Workspace`. Le curseur de redimensionnement la survole ;
/// l'appui démarre le glisser (`capture_pointer`), le relâché l'écrit s'il y a eu mouvement, un double-clic rend la
/// largeur par défaut. Son fond bloque la souris : la ligne dessous ne reçoit pas l'appui. À poser en dernier enfant du
/// corps, en positionnement absolu.
pub(crate) fn render_handle<T: 'static>(
    panel: Entity<T>,
    dragging: bool,
    edge_x: Pixels,
    actions: HandleActions<T>,
) -> impl IntoElement + use<T> {
    canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::BlockMouse),
        move |_, hitbox, window, _| {
            if dragging {
                window.set_window_cursor_style(CursorStyle::ResizeLeftRight);
            } else {
                window.set_cursor_style(CursorStyle::ResizeLeftRight, &hitbox);
            }
            let target = panel.clone();
            let handle = hitbox.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !handle.is_hovered(window)
                {
                    return;
                }
                cx.stop_propagation();
                if event.click_count >= 2 {
                    target.update(cx, |this, cx| (actions.reset)(this, cx));
                } else {
                    window.capture_pointer(handle.id);
                    let x = event.position.x;
                    target.update(cx, |this, cx| (actions.begin)(this, x, cx));
                }
            });
            if dragging {
                let target = panel.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    let x = event.position.x;
                    // Un relâché perdu (hors de la fenêtre) : la souris n'appuie plus, le glisser s'arrête.
                    if event.pressed_button == Some(MouseButton::Left) {
                        target.update(cx, |this, cx| (actions.drag)(this, x, cx));
                    } else {
                        target.update(cx, |this, cx| (actions.end)(this, cx));
                    }
                });
                let target = panel;
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                        target.update(cx, |this, cx| (actions.end)(this, cx));
                    }
                });
            }
        },
    )
    .absolute()
    .left(edge_x - px(HANDLE_WIDTH / 2.))
    .top(px(0.))
    .w(px(HANDLE_WIDTH))
    .h_full()
}

/// La mesure du corps du volet, derrière tout : elle dit au volet de quelle largeur il dispose, ce qui décide de ce que
/// le plancher de l'autre côté laisse à la colonne. `note` rend `true` quand la valeur a changé.
pub(crate) fn measure_body<T: 'static>(
    panel: Entity<T>,
    note: fn(&mut T, f32) -> bool,
) -> impl IntoElement + use<T> {
    canvas(
        move |bounds, window, cx| {
            let scale = cx.global::<Theme>().scale;
            let width = f32::from(bounds.size.width) / scale;
            let changed = panel.update(cx, |this, _| note(this, width));
            // Pendant le dessin, `notify` est ignoré : sans cette demande, le plafond n'agirait qu'à la prochaine image venue
            // d'ailleurs. La seconde image mesure la même largeur et s'arrête là.
            if changed {
                window.request_animation_frame();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_width_shown_defaults_to_the_default() {
        assert_eq!(width_shown(None, 600., 236., 180., 200.), 236.);
    }

    #[test]
    fn column_width_shown_leaves_the_other_side_its_floor() {
        assert_eq!(width_shown(Some(500.), 600., 236., 180., 200.), 400.);
    }

    #[test]
    fn column_width_shown_never_goes_under_the_floor() {
        assert_eq!(width_shown(Some(100.), 600., 236., 180., 200.), 180.);
        // Un corps trop étroit pour les deux planchers : la colonne garde le sien, l'autre côté prend le reste.
        assert_eq!(width_shown(Some(300.), 350., 236., 180., 200.), 180.);
    }

    #[test]
    fn a_cancelled_drag_restores_the_previous_wanted_width() {
        // Un glisser en cours (largeur déjà tirée), puis la colonne cesse d'être peinte : la largeur voulue d'avant l'appui
        // revient, le glisser est fini, et rien n'est à écrire.
        let mut drag = Some(ColumnDrag {
            start_x: px(100.),
            start_width: 236.,
            start_wanted: Some(250.),
            moved: true,
        });
        let mut wanted = Some(310.);
        assert!(cancel_drag(&mut drag, &mut wanted));
        assert!(drag.is_none());
        assert_eq!(wanted, Some(250.));
        // Sans glisser en cours, rien ne bouge.
        assert!(!cancel_drag(&mut drag, &mut wanted));
        assert_eq!(wanted, Some(250.));
    }

    #[test]
    fn a_drag_writes_the_shown_width_not_the_mouse() {
        assert_eq!(dragged_width(236., 500., 600., 236., 180., 200.), 400.);
        assert_eq!(dragged_width(236., -300., 600., 236., 180., 200.), 180.);
        assert_eq!(dragged_width(236., 40., 600., 236., 180., 200.), 276.);
    }
}
