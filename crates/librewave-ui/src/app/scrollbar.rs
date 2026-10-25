//! The sideways scrollbar and edge fades of the input strip row.
//!
//! Both are painted after the row has its layout, so they always match the
//! current scroll position. Neither one takes layout space.
use super::Shell;
use super::theme::{BACKGROUND, FAINT, LINE, MUTED, alpha};
use gpui::{
    Bounds, Canvas, DispatchPhase, Hitbox, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, ScrollHandle, ScrollWheelEvent, WeakEntity, canvas, fill,
    linear_color_stop, linear_gradient, point, px, rgb, rgba, size,
};

/// The visible bar height. The hit area is the full canvas height.
const BAR: f32 = 6.;
const MIN_THUMB: f32 = 40.;
/// The width of the fade at an edge where strips are hidden.
const FADE: f32 = 72.;

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
            let Some((geometry, hitbox)) = state else { return };
            let lit = grab.is_some() || hitbox.is_hovered(window);
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
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                        let _ = shell.update(cx, |this, cx| {
                            this.scrollbar_grab = None;
                            cx.notify();
                        });
                    }
                });
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
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                let delta = event.delta.pixel_delta(window.line_height());
                let step = if delta.x.abs() > delta.y.abs() { delta.x } else { delta.y };
                scroll_to(&handle, handle.offset().x + step);
                cx.stop_propagation();
                let _ = shell.update(cx, |_, cx| cx.notify());
            });
        },
    )
}

/// Black fades over the edges of the strip row where strips are hidden.
/// They paint over the row and never take pointer input.
pub(super) fn input_fades(handle: ScrollHandle) -> Canvas<()> {
    canvas(
        |_, _, _| {},
        move |area, (), window, _| {
            let max = handle.max_offset().x;
            if max <= px(0.) {
                return;
            }
            let offset = -handle.offset().x;
            let opaque = rgb(BACKGROUND);
            let clear = rgba(alpha(BACKGROUND, 0));
            let width = px(FADE).min(area.size.width / 2.);
            if offset > px(0.5) {
                let left = Bounds::new(area.origin, size(width, area.size.height));
                window.paint_quad(fill(
                    left,
                    linear_gradient(
                        90.,
                        linear_color_stop(opaque, 0.),
                        linear_color_stop(clear, 1.),
                    ),
                ));
            }
            if offset < max - px(0.5) {
                let right = Bounds::new(
                    point(area.right() - width, area.top()),
                    size(width, area.size.height),
                );
                window.paint_quad(fill(
                    right,
                    linear_gradient(
                        90.,
                        linear_color_stop(clear, 0.),
                        linear_color_stop(opaque, 1.),
                    ),
                ));
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
