//! Pointer drags on level controls.
//!
//! A GPUI drag (`on_drag`) redraws the whole window on every pointer move.
//! A level drag instead follows the pointer with window mouse listeners, so
//! the window redraws only when the dragged value reaches a new step.
//!
//! The wheel over a level control changes that level by 1 dB per notch.
use super::{MixerEdit, Shell};
use gpui::{
    AccessibleAction, Bounds, Canvas, CursorStyle, DispatchPhase, Div, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Orientation, Pixels, Point, Role, ScrollDelta, ScrollWheelEvent,
    SharedString, Stateful, WeakEntity, canvas, prelude::*,
};
use librewave_core::{MixTarget, SourceId};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// The level control that a pointer gesture moves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LevelControl {
    Fader(SourceId, MixTarget),
    Output(MixTarget),
    Hardware(&'static str),
}

/// Converts a pointer position inside the control bounds to a level edit.
pub(super) type LevelAt = dyn Fn(Point<Pixels>, Bounds<Pixels>) -> Option<MixerEdit>;

/// Moves a level by whole wheel notches, up being louder. It starts from the
/// value that the control shows, when one is given, and stops at the limits.
pub(super) type LevelStep = dyn Fn(Option<MixerEdit>, i32) -> Option<MixerEdit>;

/// Touchpad pixels that count as one wheel notch.
const NOTCH_PIXELS: f32 = 24.;
/// GPUI on Linux reports one mouse wheel notch as three lines.
const NOTCH_LINES: f32 = 3.;
/// A wheel event this soon after a page or strip scroll continues that scroll.
const SCROLL_CONTINUES: Duration = Duration::from_millis(400);
/// After this pause, partial touchpad travel toward a notch is dropped.
const NOTCH_RESET: Duration = Duration::from_millis(500);

/// The control that an edit changes.
fn edit_control(edit: MixerEdit) -> Option<LevelControl> {
    match edit {
        MixerEdit::Input(input) => Some(LevelControl::Fader(input.source, input.target)),
        MixerEdit::Output { target, .. } => Some(LevelControl::Output(target)),
        MixerEdit::Hardware { control, .. } => match control {
            librewave_core::Wave3Control::InputGain(_) => Some(LevelControl::Hardware("gain")),
            librewave_core::Wave3Control::HeadphoneLevel(_) => {
                Some(LevelControl::Hardware("headphone"))
            }
            _ => None,
        },
    }
}

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

    /// Records a wheel event that scrolled the page or the strip row.
    pub(super) fn note_scroll(&mut self) {
        self.scrolled_at = Some(Instant::now());
    }

    /// Whether a wheel event belongs to a scroll that is already moving the
    /// page or the strip row.
    fn scroll_continues(&self) -> bool {
        self.scrolled_at.is_some_and(|at| at.elapsed() < SCROLL_CONTINUES)
    }

    /// Applies wheel travel over a level control.
    fn wheel_level(
        &mut self,
        control: LevelControl,
        step: &LevelStep,
        delta: ScrollDelta,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.settings_modal.is_some() || self.level_gesture.is_some() {
            return;
        }
        let travel = match delta {
            ScrollDelta::Lines(lines) => {
                (if lines.x.abs() > lines.y.abs() { -lines.x } else { lines.y }) / NOTCH_LINES
            }
            ScrollDelta::Pixels(pixels) => {
                let (x, y) = (f32::from(pixels.x), f32::from(pixels.y));
                (if x.abs() > y.abs() { -x } else { y }) / NOTCH_PIXELS
            }
        };
        if !travel.is_finite() {
            return;
        }
        let now = Instant::now();
        let paused = self.wheel_at.is_none_or(|at| now.duration_since(at) >= NOTCH_RESET);
        self.wheel_at = Some(now);
        if self.wheel_control != Some(control) || paused {
            self.wheel_control = Some(control);
            self.wheel_travel = 0.;
        }
        self.wheel_travel += travel;
        let notches = self.wheel_travel.trunc();
        self.wheel_travel -= notches;
        // Whole notches fit an i32: one event never scrolls that far.
        #[allow(clippy::cast_possible_truncation)]
        let notches = notches as i32;
        if notches != 0 {
            self.nudge_level(control, step, notches, cx);
        }
    }

    /// Moves a level by whole 1 dB steps from the value that it shows.
    fn nudge_level(
        &mut self,
        control: LevelControl,
        step: &LevelStep,
        notches: i32,
        cx: &mut gpui::Context<Self>,
    ) {
        let shown = self.shown_edit().filter(|edit| edit_control(*edit) == Some(control));
        if let Some(edit) = step(shown, notches) {
            self.send_fader(edit, cx);
        }
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
    step: Rc<LevelStep>,
) -> Canvas<Option<gpui::Hitbox>> {
    canvas(
        move |bounds, window, _| {
            enabled.then(|| window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal))
        },
        move |bounds, hitbox, window, _| {
            let Some(hitbox) = hitbox else { return };
            let wheel_hitbox = hitbox.clone();
            let wheel_shell = shell.clone();
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !wheel_hitbox.should_handle_scroll(window) {
                    return;
                }
                // A scroll that reached the level from elsewhere keeps scrolling.
                if wheel_shell.upgrade().is_some_and(|shell| shell.read(cx).scroll_continues()) {
                    return;
                }
                // The level takes the wheel, so the strip row does not scroll.
                cx.stop_propagation();
                let _ = wheel_shell.update(cx, |this, cx| {
                    this.wheel_level(control, &*step, event.delta, cx);
                });
            });
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

/// What assistive technology reports for a level control.
pub(super) struct LevelDescription {
    pub(super) label: SharedString,
    pub(super) decibels: f64,
    pub(super) minimum: f64,
    pub(super) maximum: f64,
    pub(super) orientation: Orientation,
}

/// Exposes a level control as a slider. Assistive technology can move it by
/// the same 1 dB steps as the wheel.
pub(super) fn accessible_level(
    element: Stateful<Div>,
    control: LevelControl,
    description: LevelDescription,
    enabled: bool,
    step: &Rc<LevelStep>,
    shell: &WeakEntity<Shell>,
) -> Stateful<Div> {
    let mut element = element
        .role(Role::Slider)
        .aria_label(description.label)
        .aria_orientation(description.orientation)
        .aria_numeric_value(description.decibels)
        .aria_min_numeric_value(description.minimum)
        .aria_max_numeric_value(description.maximum)
        .aria_numeric_value_step(1.);
    if !enabled {
        return element;
    }
    for (action, notches) in [(AccessibleAction::Increment, 1), (AccessibleAction::Decrement, -1)] {
        let step = Rc::clone(step);
        let shell = shell.clone();
        element = element.on_a11y_action(action, move |_, _, cx| {
            let _ = shell.update(cx, |this, cx| this.nudge_level(control, &*step, notches, cx));
        });
    }
    element
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn one_wheel_notch_is_one_step(cx: &mut gpui::TestAppContext) {
        use super::*;
        use crate::app::tests::shell_with;
        use std::cell::RefCell;
        let (view, cx) = cx.add_window_view(|_, _| shell_with(librewave_core::Snapshot::empty()));
        let notches = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&notches);
        // The step records the notches and sends nothing.
        let step = move |_: Option<MixerEdit>, count: i32| {
            seen.borrow_mut().push(count);
            None
        };
        let control = LevelControl::Output(MixTarget::Monitor);
        let lines = |y: f32| ScrollDelta::Lines(gpui::point(0., y));
        view.update(cx, |view, cx| {
            // GPUI reports one Linux wheel notch as three lines.
            view.wheel_level(control, &step, lines(3.), cx);
            view.wheel_level(control, &step, lines(-3.), cx);
            // Smooth wheels and touchpads add up to whole notches.
            view.wheel_level(control, &step, lines(1.), cx);
            view.wheel_level(control, &step, lines(1.), cx);
            view.wheel_level(control, &step, lines(1.), cx);
            view.wheel_level(control, &step, lines(f32::NAN), cx);
            view.wheel_level(control, &step, lines(2.), cx);
            // A pause drops the partial travel.
            view.wheel_at = Instant::now().checked_sub(NOTCH_RESET);
            view.wheel_level(control, &step, lines(1.), cx);
        });
        assert_eq!(*notches.borrow(), vec![1, -1, 1]);
        view.read_with(cx, |view, _| assert!((view.wheel_travel - 1. / 3.).abs() < 1e-6));
    }
}
