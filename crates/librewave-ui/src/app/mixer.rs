use super::{
    ACCENT, BACKGROUND, BORDER, INK, MUTED, PANEL, RAISED, STREAM, Shell, WARNING, button,
    section_label,
};
use gpui::{Bounds, Context, Div, MouseButton, Stateful, canvas, div, prelude::*, px, rgb};
use librewave_core::{
    Command, FaderGain, MixRoute, MixTarget, MixerGeneration, MixerRuntimeState, SourceId,
};

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
                                    self.pending,
                                    cx,
                                ))
                                .child(fader_lane(
                                    controls.source(),
                                    MixTarget::Stream,
                                    controls.stream(),
                                    snapshot.mixer.generation(),
                                    self.pending,
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
    pending: bool,
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
    let track = fader_track(&key, source, target, route, generation, pending, color, cx);

    div()
        .w(px(72.))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(12.))
        .child(div().text_xs().text_color(rgb(color)).child(label))
        .child(mixer_button(
            &format!("{key}-reset"),
            &format!("{:.1} dB", route.fader().decibels()),
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
    pending: bool,
    color: u32,
    cx: &mut Context<Shell>,
) -> Stateful<Div> {
    let position = fader_fraction(route.fader());
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
                .bg(rgb(if pending { MUTED } else { INK })),
        )
        .child(
            canvas(move |area, _, _| painted_bounds.set(Some(area)), |_, (), _, _| {})
                .absolute()
                .size_full(),
        );
    if !pending {
        track = track
            .cursor_pointer()
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
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    if let Some(area) = bounds.get() {
                        let distance = f32::from(event.position.y - area.top()) - 5.;
                        let height = f32::from(area.size.height) - 10.;
                        if height > 0.
                            && let Some(command) = route_position_command(
                                source,
                                target,
                                route,
                                generation,
                                1. - distance / height,
                            )
                        {
                            this.send(command, cx);
                        }
                    }
                }),
            );
    }
    track
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
    if !fraction.is_finite() {
        return None;
    }
    let minimum = FaderGain::MIN.half_decibel_steps();
    let span = FaderGain::MAX.half_decibel_steps() - minimum;
    // Clamping proves the rounded value fits the exact fader step interval.
    let steps = (fraction.clamp(0., 1.) * f32::from(span)).round() as i16 + minimum;
    let fader = FaderGain::from_half_decibel_steps(steps).ok()?;
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
