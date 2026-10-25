//! The sideways scrollbar and edge fades of the input strip row.
//!
//! Both are painted after the row has its layout, so they always match the
//! current scroll position. Neither one takes layout space.
use super::Shell;
use super::device_art::Glow;
use super::theme::{BACKGROUND, FAINT, LINE, MUTED, alpha};
use gpui::{
    Bounds, Canvas, DispatchPhase, Hitbox, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, ScrollHandle, ScrollWheelEvent, WeakEntity, canvas, fill,
    linear_color_stop, linear_gradient, point, px, rgb, rgba, size,
};
use std::cell::Cell;
use std::rc::Rc;

/// The visible bar height. The hit area is the full canvas height.
const BAR: f32 = 6.;
const MIN_THUMB: f32 = 40.;
/// The width of the fade at an edge where strips are hidden, and the scroll
/// distance over which it grows in.
const FADE: f32 = 72.;
/// The height of one fade band. The glow changes little over it.
const BAND: f32 = 8.;

/// The track and thumb of a horizontal scroll position.
#[derive(Clone, Copy)]
pub(super) struct Geometry {
    track: Bounds<Pixels>,
    thumb: Bounds<Pixels>,
    max: Pixels,
}

impl Geometry {
    fn of(handle: &ScrollHandle, area: Bounds<Pixels>) -> Option<Self> {
        let max = handle.max_offset().x;
        let viewport = handle.bounds().size.width;
        if max <= px(0.) || viewport <= px(0.) {
            return None;
        }
        let track = Bounds::new(
            point(area.left(), area.center().y - px(BAR / 2.)),
            size(area.size.width, px(BAR)),
        );
        let width = (track.size.width * (viewport / (viewport + max))).max(px(MIN_THUMB));
        let travel = (track.size.width - width).max(px(0.));
        let fraction = (-handle.offset().x / max).clamp(0., 1.);
        let thumb =
            Bounds::new(point(track.left() + travel * fraction, track.top()), size(width, px(BAR)));
        Some(Self { track, thumb, max })
    }

    /// The scroll offset that puts the thumb's left edge at `left`.
    fn offset_for(&self, left: Pixels) -> Pixels {
        let travel = self.track.size.width - self.thumb.size.width;
        if travel <= px(0.) {
            return px(0.);
        }
        let fraction = ((left - self.track.left()) / travel).clamp(0., 1.);
        -self.max * fraction
    }
}

fn scroll_to(handle: &ScrollHandle, x: Pixels) {
    let offset = handle.offset();
    handle.set_offset(point(x.clamp(-handle.max_offset().x, px(0.)), offset.y));
}

/// Ends a scrollbar drag when the left button is released.
fn release_grab_on_mouse_up(window: &mut gpui::Window, shell: WeakEntity<Shell>) {
    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
        if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
            let _ = shell.update(cx, |this, cx| {
                this.scrollbar_grab = None;
                cx.notify();
            });
        }
    });
}

/// A scrollbar for the strip row. It shows only while strips are hidden.
/// Drag the thumb, click the track, or use the wheel over it.
pub(super) fn input_scrollbar(
    handle: ScrollHandle,
    grab: Option<Pixels>,
    shell: WeakEntity<Shell>,
) -> Canvas<Option<(Geometry, Hitbox)>> {
    canvas(
        {
            let handle = handle.clone();
            move |area, window, _| {
                Geometry::of(&handle, area).map(|geometry| {
                    (geometry, window.insert_hitbox(area, gpui::HitboxBehavior::Normal))
                })
            }
        },
        move |_, state, window, _| {
            let Some((geometry, hitbox)) = state else {
                // The bar went away during its drag: all strips fit now.
                if grab.is_some() {
                    release_grab_on_mouse_up(window, shell);
                }
                return;
            };
            let lit = grab.is_some() || hitbox.is_hovered(window);
            // The thumb lights up while the pointer is over the bar.
            let hover_hitbox = hitbox.clone();
            window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, _| {
                if phase == DispatchPhase::Bubble && hover_hitbox.is_hovered(window) != lit {
                    window.refresh();
                }
            });
            window.paint_quad(
                fill(geometry.track, rgba(alpha(LINE, 0x99))).corner_radii(px(BAR / 2.)),
            );
            window.paint_quad(
                fill(geometry.thumb, rgb(if lit { MUTED } else { FAINT }))
                    .corner_radii(px(BAR / 2.)),
            );
            if let Some(grab) = grab {
                let drag_handle = handle.clone();
                let drag_shell = shell.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let _ = drag_shell.update(cx, |this, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            scroll_to(&drag_handle, geometry.offset_for(event.position.x - grab));
                        } else {
                            this.scrollbar_grab = None;
                        }
                        cx.notify();
                    });
                });
                release_grab_on_mouse_up(window, shell);
                return;
            }
            let press_hitbox = hitbox.clone();
            let press_handle = handle.clone();
            let press_shell = shell.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !press_hitbox.is_hovered(window)
                {
                    return;
                }
                // A press on the thumb keeps its grip point. A press on the
                // track centers the thumb under the pointer first.
                let x = event.position.x;
                let grab = if geometry.thumb.left() <= x && x <= geometry.thumb.right() {
                    x - geometry.thumb.left()
                } else {
                    let grab = geometry.thumb.size.width / 2.;
                    scroll_to(&press_handle, geometry.offset_for(x - grab));
                    grab
                };
                cx.stop_propagation();
                let _ = press_shell.update(cx, |this, cx| {
                    this.scrollbar_grab = Some(grab);
                    cx.notify();
                });
            });
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.should_handle_scroll(window) {
                    return;
                }
                let delta = event.delta.pixel_delta(window.line_height());
                let step = if delta.x.abs() > delta.y.abs() { delta.x } else { delta.y };
                scroll_to(&handle, handle.offset().x + step);
                cx.stop_propagation();
                let _ = shell.update(cx, |this, cx| {
                    this.note_scroll();
                    cx.notify();
                });
            });
        },
    )
}

/// Fades over the edges of the strip row where strips are hidden.
///
/// Each fade ends in the backdrop color beside the row, including the stage
/// glow that reaches behind it, so no edge shows. A fade grows in over the
/// first [`FADE`] pixels that the row scrolls past, instead of appearing at
/// full strength.
pub(super) fn input_fades(
    handle: ScrollHandle,
    glow: Glow,
    glow_area: Rc<Cell<Option<Bounds<Pixels>>>>,
) -> Canvas<()> {
    canvas(
        |_, _, _| {},
        move |area, (), window, _| {
            let max = handle.max_offset().x;
            if max <= px(0.) {
                return;
            }
            let hidden_left = -handle.offset().x;
            let hidden_right = max - hidden_left;
            let width = px(FADE).min(area.size.width / 2.);
            let glow_area = glow_area.get();
            for (hidden, edge, angle) in
                [(hidden_left, area.left(), 270.), (hidden_right, area.right() - width, 90.)]
            {
                let strength = ease((hidden / px(FADE)).clamp(0., 1.));
                if strength <= 0. {
                    continue;
                }
                let outer = if angle > 180. { area.left() } else { area.right() };
                // Bands follow the glow as it changes down the edge.
                let mut top = area.top();
                while top < area.bottom() {
                    let height = px(BAND).min(area.bottom() - top);
                    let sample = point(outer, top + height / 2.);
                    let backdrop = glow_area.map_or_else(
                        || rgb(BACKGROUND),
                        |lit| glow.backdrop(lit, sample, BACKGROUND),
                    );
                    let solid = gpui::Rgba { a: strength, ..backdrop };
                    let clear = gpui::Rgba { a: 0., ..backdrop };
                    window.paint_quad(fill(
                        Bounds::new(point(edge, top), size(width, height)),
                        linear_gradient(
                            angle,
                            linear_color_stop(clear, 0.),
                            linear_color_stop(solid, 1.),
                        ),
                    ));
                    top += height;
                }
            }
        },
    )
}

/// Smooth start and end for the fade strength.
fn ease(t: f32) -> f32 {
    t * t * (3. - 2. * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fade_grows_in_smoothly_from_nothing() {
        assert!(ease(0.).abs() < f32::EPSILON);
        assert!((ease(1.) - 1.).abs() < f32::EPSILON);
        assert!((ease(0.5) - 0.5).abs() < f32::EPSILON);
        // Slow at both ends: the first pixels of scroll add almost nothing.
        assert!(ease(0.1) < 0.1 && ease(0.9) > 0.9);
    }

    #[test]
    fn the_thumb_travel_maps_to_the_whole_scroll_range() {
        let geometry = Geometry {
            track: Bounds::new(point(px(100.), px(0.)), size(px(400.), px(BAR))),
            thumb: Bounds::new(point(px(100.), px(0.)), size(px(100.), px(BAR))),
            max: px(900.),
        };
        assert_eq!(geometry.offset_for(px(100.)), px(0.));
        assert_eq!(geometry.offset_for(px(400.)), px(-900.));
        assert_eq!(geometry.offset_for(px(250.)), px(-450.));
        // The thumb stays on the track.
        assert_eq!(geometry.offset_for(px(-50.)), px(0.));
        assert_eq!(geometry.offset_for(px(900.)), px(-900.));
    }
}
