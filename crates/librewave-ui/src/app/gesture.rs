//! Pointer drags on level controls.
//!
//! A GPUI drag (`on_drag`) redraws the whole window on every pointer move.
//! A level drag instead follows the pointer with window mouse listeners, so
//! the window redraws only when the dragged value reaches a new step.
use super::{MixerEdit, Shell};
use gpui::{
    Bounds, Canvas, CursorStyle, DispatchPhase, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, WeakEntity, canvas,
};
use librewave_core::{MixTarget, SourceId};
use std::rc::Rc;

/// The level control that a pointer gesture moves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LevelControl {
    Fader(SourceId, MixTarget),
    Output(MixTarget),
    Hardware(&'static str),
}

/// Converts a pointer position inside the control bounds to a level edit.
pub(super) type LevelAt = dyn Fn(Point<Pixels>, Bounds<Pixels>) -> Option<MixerEdit>;

impl Shell {
    fn begin_level_gesture(
        &mut self,
        control: LevelControl,
        edit: Option<MixerEdit>,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.settings_modal.is_some() {
            return;
        }
        self.level_gesture = Some(control);
        if let Some(edit) = edit {
            self.preview_fader(edit, cx);
        }
        cx.notify();
    }

    /// Ends a level drag. The last previewed value is committed once.
    pub(super) fn end_level_gesture(&mut self, cx: &mut gpui::Context<Self>) {
        if self.level_gesture.take().is_some() {
            cx.notify();
        }
        self.release_preview(cx);
    }

    /// Stops a level drag without a commit, after an error or a dialog.
    pub(super) fn cancel_level_gesture(&mut self, cx: &mut gpui::Context<Self>) {
        if self.level_gesture.take().is_some() {
            cx.notify();
        }
    }
}

/// An overlay that fills a level control and handles its pointer gesture.
///
/// A press inside the control starts the gesture. While it runs, the window
/// listeners follow the pointer anywhere in the window and a release ends it.
pub(super) fn level_gesture(
    control: LevelControl,
    enabled: bool,
    active: bool,
    shell: WeakEntity<Shell>,
    at: Rc<LevelAt>,
) -> Canvas<Option<gpui::Hitbox>> {
    canvas(
        move |bounds, window, _| {
            enabled.then(|| window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal))
        },
        move |bounds, hitbox, window, _| {
            let Some(hitbox) = hitbox else { return };
            if active {
                window.set_window_cursor_style(CursorStyle::PointingHand);
                let position_edit = Rc::clone(&at);
                let entity = shell.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    if event.pressed_button != Some(MouseButton::Left) {
                        // The window missed the release, for example outside it.
                        let _ = entity.update(cx, Shell::end_level_gesture);
                    } else if let Some(edit) = position_edit(event.position, bounds) {
                        let _ = entity.update(cx, |this, cx| this.preview_fader(edit, cx));
                    }
                });
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                        let _ = shell.update(cx, Shell::end_level_gesture);
                    }
                });
            } else {
                window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble
                        && event.button == MouseButton::Left
                        && hitbox.is_hovered(window)
                    {
                        let edit = at(event.position, bounds);
                        let _ = shell.update(cx, |this, cx| {
                            this.begin_level_gesture(control, edit, cx);
                        });
                    }
                });
            }
        },
    )
}
