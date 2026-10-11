use super::{
    ACCENT, BACKGROUND, BORDER, INK, MUTED, PANEL, RAISED, STREAM, Shell, WARNING, button,
    section_label,
};
use gpui::{
    Bounds, Context, Div, DragMoveEvent, MouseButton, Stateful, canvas, div, prelude::*, px, rgb,
};
use librewave_core::{
    Command, FaderGain, MixRoute, MixTarget, MixerGeneration, MixerRuntimeState, Snapshot, SourceId,
};

/// A transient pointer intent, never an authoritative or persisted mixer state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FaderEdit {
    pub(super) source: SourceId,
    pub(super) target: MixTarget,
    pub(super) fader: FaderGain,
}

impl FaderEdit {
    pub(super) fn command(self, snapshot: &Snapshot) -> Option<Command> {
        let controls = snapshot.mixer.profile.source(self.source)?.controls;
        let route = match self.target {
            MixTarget::Monitor => controls.monitor(),
            MixTarget::Stream => controls.stream(),
        };
        if self.fader == route.fader() {
            return None;
        }
        Some(Command::SetMixerRoute {
            expected_generation: snapshot.mixer.generation(),
            source: self.source,
            target: self.target,
            route: MixRoute::new(route.enabled(), self.fader),
        })
    }
}

#[derive(Clone)]
struct FaderDrag {
    source: SourceId,
    target: MixTarget,
}

impl Shell {
    pub(super) fn mixer_channels(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut mixer = div()
            .w_full()
            .p(px(24.))
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(BACKGROUND))
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(section_label("INPUTS"));
        if let Some(snapshot) = &self.snapshot {
            let mut channels = div().id("input-channels").flex().gap(px(8.)).overflow_x_scroll();
            for source in &snapshot.mixer.profile.sources {
                let controls = source.controls;
                channels = channels.child(
                    div()
                        .w(px(208.))
                        .flex_shrink_0()
                        .p(px(12.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(PANEL))
                        .flex()
                        .flex_col()
                        .gap(px(16.))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.))
                                .child(div().text_sm().child(source.name.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child(source.role.to_string()),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_center()
                                .gap(px(16.))
                                .child(fader_lane(
                                    controls.source(),
                                    MixTarget::Monitor,
                                    controls.monitor(),
                                    snapshot.mixer.generation(),
                                    self,
                                    cx,
                                ))
                                .child(fader_lane(
                                    controls.source(),
                                    MixTarget::Stream,
                                    controls.stream(),
                                    snapshot.mixer.generation(),
                                    self,
                                    cx,
                                )),
                        ),
                );
            }
            mixer = mixer.child(channels).child(div().text_xs().text_color(rgb(MUTED)).child(
                match snapshot.mixer.runtime {
                    MixerRuntimeState::Active => "Live meters are not available yet.",
                    MixerRuntimeState::Inactive { .. } => {
                        "Audio is inactive. Saved levels apply when audio starts."
                    }
                },
            ));
        } else {
            mixer = mixer.child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("Connect to the daemon to see inputs."),
            );
        }
        mixer
    }
}

fn fader_lane(
    source: SourceId,
    target: MixTarget,
    route: MixRoute,
    generation: MixerGeneration,
    shell: &Shell,
    cx: &mut Context<Shell>,
) -> impl IntoElement {
    let color = match target {
        MixTarget::Monitor => ACCENT,
        MixTarget::Stream => STREAM,
    };
    let key = format!("fader-{}-{target}", source.get());
    let label = match target {
        MixTarget::Monitor => "Monitor",
        MixTarget::Stream => "Stream",
    };
    let shown = shell
        .queued_fader
        .or(shell.in_flight_fader)
        .filter(|edit| edit.source == source && edit.target == target)
        .map_or(route.fader(), |edit| edit.fader);
    let track = fader_track(
        &key,
        source,
        target,
        route,
        generation,
        shown,
        !shell.pending || shell.in_flight_fader.is_some(),
        color,
        cx,
    );
    let pending = shell.pending;

    div()
        .w(px(72.))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(12.))
        .child(div().text_xs().text_color(rgb(color)).child(label))
        .child(mixer_button(
            &format!("{key}-reset"),
            &format!("{:.1} dB", shown.decibels()),
            route_position_command(
                source,
                target,
                route,
                generation,
                fader_fraction(FaderGain::UNITY),
            ),
            pending,
            cx,
        ))
        .child(track)
        .child(
            div()
                .flex()
                .gap(px(4.))
                .child(mixer_button(
                    &format!("{key}-down"),
                    "−",
                    route_step_command(source, target, route, generation, -1),
                    pending,
                    cx,
                ))
                .child(mixer_button(
                    &format!("{key}-up"),
                    "+",
                    route_step_command(source, target, route, generation, 1),
                    pending,
                    cx,
                )),
        )
        .child(
            mixer_button(
                &format!("{key}-toggle"),
                if route.enabled() { "Mute" } else { "Unmute" },
                Some(route_toggle_command(source, target, route, generation)),
                pending,
                cx,
            )
            .text_color(rgb(if route.enabled() { color } else { WARNING })),
        )
}

#[allow(clippy::too_many_arguments)]
fn fader_track(
    key: &str,
    source: SourceId,
    target: MixTarget,
    route: MixRoute,
    generation: MixerGeneration,
    shown: FaderGain,
    enabled: bool,
    color: u32,
    cx: &mut Context<Shell>,
) -> Stateful<Div> {
    let position = fader_fraction(shown);
    let bounds = std::rc::Rc::new(std::cell::Cell::new(None::<Bounds<gpui::Pixels>>));
    let painted_bounds = std::rc::Rc::clone(&bounds);
    let mut track = div()
        .id(key.to_owned())
        .w(px(32.))
        .h(px(200.))
        .relative()
        .focusable()
        .focus_visible(|style| style.bg(rgb(RAISED)))
        .child(div().absolute().left(px(14.)).w(px(4.)).h_full().rounded(px(2.)).bg(rgb(BORDER)))
        .child(
            div()
                .absolute()
                .left(px(14.))
                .bottom_0()
                .w(px(4.))
                .h(px(position * 200.))
                .bg(rgb(color)),
        )
        .child(
            div()
                .absolute()
                .top(px((1. - position) * 190.))
                .w_full()
                .h(px(10.))
                .rounded(px(3.))
                .bg(rgb(if enabled { INK } else { MUTED })),
        )
        .child(
            canvas(move |area, _, _| painted_bounds.set(Some(area)), |_, (), _, _| {})
                .absolute()
                .size_full(),
        );
    if enabled {
        track = track
            .cursor_pointer()
            .on_drag(FaderDrag { source, target }, |_, _, _, cx| cx.new(|_| gpui::Empty))
            .on_drag_move(cx.listener(move |this, event: &DragMoveEvent<FaderDrag>, window, cx| {
                let drag = event.drag(cx);
                if drag.source != source || drag.target != target {
                    return;
                }
                if let Some(fader) = fader_at_pixel(
                    f32::from(event.event.position.y),
                    f32::from(event.bounds.top()),
                    f32::from(event.bounds.size.height),
                ) {
                    this.fader_window = Some(window.window_handle());
                    this.send_fader(FaderEdit { source, target, fader }, cx);
                }
            }))
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                if let Some(command) =
                    route_key_command(source, target, route, generation, &event.keystroke.key)
                {
                    cx.stop_propagation();
                    this.send(command, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if let Some(area) = bounds.get()
                        && let Some(fader) = fader_at_pixel(
                            f32::from(event.position.y),
                            f32::from(area.top()),
                            f32::from(area.size.height),
                        )
                    {
                        this.fader_window = Some(window.window_handle());
                        this.send_fader(FaderEdit { source, target, fader }, cx);
                    }
                }),
            );
    }
    track
}

fn fader_at_pixel(y: f32, top: f32, height: f32) -> Option<FaderGain> {
    if !y.is_finite() || !top.is_finite() || !height.is_finite() || height <= 10. {
        return None;
    }
    fader_from_fraction(1. - (y - top - 5.) / (height - 10.))
}

#[allow(clippy::cast_possible_truncation)]
fn fader_from_fraction(fraction: f32) -> Option<FaderGain> {
    if !fraction.is_finite() {
        return None;
    }
    let minimum = FaderGain::MIN.half_decibel_steps();
    let span = FaderGain::MAX.half_decibel_steps() - minimum;
    // The clamped, rounded value is bounded by the exact fader interval.
    let steps = (fraction.clamp(0., 1.) * f32::from(span)).round() as i16 + minimum;
    FaderGain::from_half_decibel_steps(steps).ok()
}

pub(super) fn fader_fraction(fader: FaderGain) -> f32 {
    f32::from(fader.half_decibel_steps() - FaderGain::MIN.half_decibel_steps())
        / f32::from(FaderGain::MAX.half_decibel_steps() - FaderGain::MIN.half_decibel_steps())
}

pub(super) fn route_key_command(
    source: SourceId,
    target: MixTarget,
    route: MixRoute,
    generation: MixerGeneration,
    key: &str,
) -> Option<Command> {
    match key {
        "up" | "right" => route_step_command(source, target, route, generation, 1),
        "down" | "left" => route_step_command(source, target, route, generation, -1),
        "home" => route_position_command(source, target, route, generation, 0.),
        "end" => route_position_command(source, target, route, generation, 1.),
        "enter" => route_position_command(
            source,
            target,
            route,
            generation,
            fader_fraction(FaderGain::UNITY),
        ),
        _ => None,
    }
}

#[allow(clippy::cast_possible_truncation)]
pub(super) fn route_position_command(
    source: SourceId,
    target: MixTarget,
    route: MixRoute,
    expected_generation: MixerGeneration,
    fraction: f32,
) -> Option<Command> {
    let fader = fader_from_fraction(fraction)?;
    if fader == route.fader() {
        return None;
    }
    Some(Command::SetMixerRoute {
        expected_generation,
        source,
        target,
        route: MixRoute::new(route.enabled(), fader),
    })
}

fn mixer_button(
    key: &str,
    label: &str,
    command: Option<Command>,
    pending: bool,
    cx: &mut Context<Shell>,
) -> Stateful<Div> {
    let mut element =
        button(key, label).focusable().focus_visible(|style| style.border_color(rgb(0x0070_B7FF)));
    if !pending && let Some(command) = command {
        element = element
            .on_click(cx.listener(move |this, _, _, cx| this.send(command, cx)))
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    this.send(command, cx);
                }
            }));
    } else {
        element = element.text_color(rgb(MUTED));
    }
    element
}

pub(super) fn route_toggle_command(
    source: SourceId,
    target: MixTarget,
    route: MixRoute,
    expected_generation: MixerGeneration,
) -> Command {
    Command::SetMixerRoute {
        expected_generation,
        source,
        target,
        route: MixRoute::new(!route.enabled(), route.fader()),
    }
}

pub(super) fn route_step_command(
    source: SourceId,
    target: MixTarget,
    route: MixRoute,
    expected_generation: MixerGeneration,
    delta: i16,
) -> Option<Command> {
    let steps = route.fader().half_decibel_steps().checked_add(delta)?;
    let fader = FaderGain::from_half_decibel_steps(steps).ok()?;
    Some(Command::SetMixerRoute {
        expected_generation,
        source,
        target,
        route: MixRoute::new(route.enabled(), fader),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_geometry_covers_every_step_and_clamps_drag_beyond_the_track() {
        for step in -120..=24 {
            let fader = FaderGain::from_half_decibel_steps(step).expect("step");
            let y = 105. + (1. - fader_fraction(fader)) * 190.;
            assert_eq!(fader_at_pixel(y, 100., 200.), Some(fader));
        }
        assert_eq!(fader_at_pixel(-100., 100., 200.), Some(FaderGain::MAX));
        assert_eq!(fader_at_pixel(1000., 100., 200.), Some(FaderGain::MIN));
        for (y, top, height) in [
            (f32::NAN, 0., 200.),
            (1., f32::INFINITY, 200.),
            (1., 0., f32::NAN),
            (1., 0., 10.),
            (1., 0., -1.),
        ] {
            assert!(fader_at_pixel(y, top, height).is_none());
        }
    }
}
