//! The software mixer: one strip for each input with a fader for each mix.
use super::Shell;
use super::controls::{
    Tone, activate, button_face, caption, card, icon, icon_button, icon_tile, identified, label,
    tabular, tip,
};
use super::theme::{
    CAPTION, DANGER, FAINT, HEADING, HEADING_LINE, ICON_SMALL, INK, LINE, MONITOR, MUTED, PANEL,
    RADIUS, RADIUS_CARD, RAISED, SPACE_LG, SPACE_MD, SPACE_SM, SPACE_XL, SPACE_XS, STREAM, alpha,
};
use gpui::{
    Bounds, Context, Div, DragMoveEvent, FontWeight, MouseButton, Stateful, canvas, div,
    prelude::*, px, relative, rgb, rgba,
};
use librewave_core::{
    Command, FaderGain, MixRoute, MixTarget, MixerGeneration, MixerSourceSnapshot, Snapshot,
    SourceId, SourceRole,
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

const STRIP_MIN: f32 = 200.;
const ADD_MIN: f32 = 140.;
const STRIP_MAX: f32 = 320.;
const FADER_WIDTH: f32 = 44.;
const CAP: f32 = 16.;
const FADER_MIN: f32 = 180.;
const FADER_MAX: f32 = 340.;
// Title bar, padding, section labels, strip chrome, and the output row.
const MIXER_FIXED: f32 = 536.;
// Levels named on the scale between the two faders of a strip.
const SCALE: [i16; 5] = [12, 0, -20, -40, -60];

/// The widest the mixer grows for a number of inputs and the add-input panel.
#[allow(clippy::cast_precision_loss)]
pub(super) fn mixer_width(inputs: usize) -> f32 {
    let inputs = inputs.max(2) as f32;
    (inputs + 1.) * STRIP_MAX + inputs * SPACE_MD
}

/// The narrowest the mixer can be before its strips scroll sideways.
#[allow(clippy::cast_precision_loss)]
pub(super) fn mixer_min_width(inputs: usize) -> f32 {
    let inputs = inputs.max(2) as f32;
    inputs * (STRIP_MIN + SPACE_MD) + ADD_MIN
}

fn fader_height_for_viewport(viewport_height: f32) -> f32 {
    (viewport_height - MIXER_FIXED).clamp(FADER_MIN, FADER_MAX)
}

const fn mix_identity(target: MixTarget) -> (u32, &'static str, &'static str) {
    match target {
        MixTarget::Monitor => (MONITOR, "headphones", "Monitor"),
        MixTarget::Stream => (STREAM, "stream", "Stream"),
    }
}

impl Shell {
    pub(super) fn mixer_section(
        &self,
        snapshot: &Snapshot,
        viewport_height: f32,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let fader_height = fader_height_for_viewport(viewport_height);
        let mut strips =
            div().id("input-strips").w_full().flex().gap(px(SPACE_MD)).overflow_x_scroll();
        for source in &snapshot.mixer.profile.sources {
            strips = strips.child(self.channel_strip(
                source,
                snapshot.mixer.generation(),
                fader_height,
                cx,
            ));
        }
        let strips = strips.child(add_input());
        div()
            .id("mixer")
            .debug_selector(|| "mixer".to_owned())
            .w_full()
            .max_w(px(mixer_width(snapshot.mixer.profile.sources.len())))
            .flex()
            .flex_col()
            .gap(px(SPACE_XL))
            .child(div().flex().flex_col().gap(px(SPACE_MD)).child(label("Inputs")).child(strips))
            .child(
                div().flex().flex_col().gap(px(SPACE_MD)).child(label("Outputs")).child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(SPACE_SM))
                        .child(output_row(MixTarget::Monitor, "headphones", "Wave:3 headphones"))
                        .child(output_row(MixTarget::Stream, "system", "Source for applications")),
                ),
            )
    }

    fn channel_strip(
        &self,
        source: &MixerSourceSnapshot,
        generation: MixerGeneration,
        fader_height: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        let microphone = source.role == SourceRole::Microphone;
        card()
            .flex_1()
            .min_w(px(STRIP_MIN))
            .max_w(px(STRIP_MAX))
            .p(px(SPACE_LG))
            .flex()
            .flex_col()
            .gap(px(SPACE_XL))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(SPACE_MD))
                    .child(icon_tile(if microphone { "mic" } else { "system" }, INK, 40.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(HEADING))
                                    .line_height(px(HEADING_LINE))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(source.name.clone()),
                            )
                            .child(caption(if microphone { "Wave:3" } else { "Desktop audio" })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .justify_center()
                    .gap(px(SPACE_XS))
                    .child(self.lane(source, MixTarget::Monitor, generation, fader_height, cx))
                    .child(scale(fader_height))
                    .child(self.lane(source, MixTarget::Stream, generation, fader_height, cx)),
            )
    }

    /// One source in one mix: the mix marker, fader, level, and mute.
    fn lane(
        &self,
        source: &MixerSourceSnapshot,
        target: MixTarget,
        generation: MixerGeneration,
        fader_height: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = source.controls.source();
        let route = match target {
            MixTarget::Monitor => source.controls.monitor(),
            MixTarget::Stream => source.controls.stream(),
        };
        let (color, glyph, name) = mix_identity(target);
        let key = format!("fader-{}-{target}", id.get());
        let shown = self
            .queued_fader
            .or(self.in_flight_fader)
            .filter(|edit| edit.source == id && edit.target == target)
            .map_or(route.fader(), |edit| edit.fader);
        let open = self.settings_modal.is_none();
        let muted = !route.enabled();
        let reset =
            route_position_command(id, target, route, generation, fader_fraction(FaderGain::UNITY));
        let toggle = route_toggle_command(id, target, route, generation);
        div()
            .w(px(64.))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(SPACE_MD))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(icon(glyph, if muted { FAINT } else { color }, ICON_SMALL))
                    .child(caption(name).text_color(rgb(if muted { FAINT } else { color }))),
            )
            .child(fader_track(
                &key,
                id,
                target,
                route,
                generation,
                shown,
                fader_height,
                if muted { FAINT } else { color },
                open && (!self.pending || self.in_flight_fader.is_some()),
                cx,
            ))
            .child(
                // A busy client ignores the request and a level at its target has
                // no command, so this readout does not flicker while dragging.
                activate(
                    button_face(&format!("{key}-reset"), Tone::Quiet)
                        .h(px(24.))
                        .px(px(SPACE_SM))
                        .text_size(px(CAPTION))
                        .font_features(tabular())
                        .text_color(rgb(if muted { FAINT } else { INK }))
                        .child(format!("{:.1} dB", shown.decibels()).replace('-', "−"))
                        .tooltip(tip("Reset to 0 dB")),
                    open,
                    cx,
                    move |this, _, cx| {
                        if let Some(command) = reset {
                            this.send(command, cx);
                        }
                    },
                ),
            )
            .child(
                icon_button(
                    &format!("{key}-toggle"),
                    if muted { "speaker-slash" } else { "speaker-high" },
                    match (target, muted) {
                        (MixTarget::Monitor, false) => "Mute in the Monitor mix",
                        (MixTarget::Monitor, true) => "Unmute in the Monitor mix",
                        (MixTarget::Stream, false) => "Mute in the Stream mix",
                        (MixTarget::Stream, true) => "Unmute in the Stream mix",
                    },
                    Tone::Secondary,
                    if muted { DANGER } else { MUTED },
                    open && !self.pending,
                    cx,
                    move |this, _, cx| this.send(toggle, cx),
                )
                .w(px(48.))
                .when(muted, |view| view.bg(rgba(alpha(DANGER, 0x24)))),
            )
    }
}

/// The panel after the last input. The daemon has no command that adds an
/// input, so this panel only marks where one goes.
fn add_input() -> Stateful<Div> {
    identified("add-input")
        .flex_1()
        .min_w(px(ADD_MIN))
        .max_w(px(STRIP_MAX))
        .rounded(px(RADIUS_CARD))
        .bg(rgba(alpha(PANEL, 0x8C)))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(SPACE_MD))
        .child(icon_tile("plus", MUTED, 40.))
        .child(div().text_color(rgb(FAINT)).child("Add input"))
        .tooltip(tip("More inputs are not available yet"))
}

/// One mix: its name, where it goes, and its level.
///
/// The daemon has no output selection or output level command. Both parts of
/// the row are shown inactive.
fn output_row(target: MixTarget, glyph: &'static str, destination: &'static str) -> Div {
    let (color, mix_glyph, name) = mix_identity(target);
    let key = name.to_lowercase();
    card()
        .w_full()
        .p(px(SPACE_MD))
        .flex()
        .items_center()
        .gap(px(SPACE_LG))
        .child(
            div()
                .w(px(152.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(SPACE_MD))
                .child(icon_tile(mix_glyph, color, 36.))
                .child(div().font_weight(FontWeight::MEDIUM).child(format!("{name} mix"))),
        )
        .child(
            identified(&format!("output-{key}-destination"))
                .flex_1()
                .min_w_0()
                .h(px(36.))
                .px(px(SPACE_MD))
                .rounded(px(RADIUS))
                .bg(rgb(RAISED))
                .flex()
                .items_center()
                .gap(px(SPACE_SM))
                .child(icon(glyph, MUTED, ICON_SMALL))
                .child(div().flex_1().min_w_0().truncate().child(destination))
                .tooltip(tip("Output selection is not available yet")),
        )
        .child(
            identified(&format!("output-{key}-level"))
                .flex_1()
                .min_w_0()
                .h(px(36.))
                .opacity(0.4)
                .flex()
                .items_center()
                .gap(px(SPACE_MD))
                .child(icon("speaker-high", MUTED, ICON_SMALL))
                .child(
                    div()
                        .flex_1()
                        .h(px(4.))
                        .rounded_full()
                        .bg(rgb(LINE))
                        .flex()
                        .items_center()
                        .justify_end()
                        .child(div().w(px(CAP)).h(px(CAP)).rounded_full().bg(rgb(MUTED))),
                )
                .tooltip(tip("Output level is not available yet")),
        )
}

/// The level names between the two faders of a strip.
fn scale(fader_height: f32) -> Div {
    let mut scale = div().w(px(24.)).h(px(fader_height)).mt(px(28.)).relative();
    for level in SCALE {
        let fraction = FaderGain::from_decibels(f32::from(level)).map_or(0., fader_fraction);
        scale = scale.child(
            div()
                .absolute()
                .top(px((1. - fraction) * (fader_height - CAP) + CAP / 2. - 7.))
                .w_full()
                .text_center()
                .text_size(px(10.))
                .line_height(px(14.))
                .text_color(rgb(FAINT))
                .child(level.to_string().replace('-', "−")),
        );
    }
    scale
}

#[allow(clippy::too_many_arguments)]
fn fader_track(
    key: &str,
    source: SourceId,
    target: MixTarget,
    route: MixRoute,
    generation: MixerGeneration,
    shown: FaderGain,
    height: f32,
    color: u32,
    enabled: bool,
    cx: &mut Context<Shell>,
) -> Stateful<Div> {
    let bounds = std::rc::Rc::new(std::cell::Cell::new(None::<Bounds<gpui::Pixels>>));
    let painted_bounds = std::rc::Rc::clone(&bounds);
    let mut track = identified(key)
        .w(px(FADER_WIDTH))
        .h(px(height))
        .py(px(CAP / 2.))
        .rounded(px(RADIUS))
        .relative()
        .flex()
        .justify_center()
        .focusable()
        .tab_index(0)
        .tab_stop(enabled)
        .focus_visible(|style| style.bg(rgb(RAISED)))
        .child(fader_rail(fader_fraction(shown), color))
        .child(
            canvas(move |area, _, _| painted_bounds.set(Some(area)), |_, (), _, _| {})
                .absolute()
                .top_0()
                .left_0()
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

/// The cap center travels between the two inset ends of this rail.
fn fader_rail(position: f32, color: u32) -> Div {
    div()
        .relative()
        .w_full()
        .h_full()
        .child(
            div()
                .absolute()
                .left(px(FADER_WIDTH / 2. - 2.))
                .w(px(4.))
                .h_full()
                .rounded_full()
                .bg(rgb(LINE)),
        )
        .child(
            div()
                .absolute()
                .left(px(FADER_WIDTH / 2. - 2.))
                .bottom_0()
                .w(px(4.))
                .h(relative(position))
                .rounded_full()
                .bg(rgb(color)),
        )
        .child(
            div()
                .absolute()
                .top(relative(1. - fader_fraction(FaderGain::UNITY)))
                .left(px(FADER_WIDTH / 2. - 8.))
                .w(px(16.))
                .h(px(1.))
                .bg(rgb(FAINT)),
        )
        .child(
            div()
                .absolute()
                .top(relative(1. - position))
                .mt(px(-CAP / 2.))
                .left(px(4.))
                .w(px(FADER_WIDTH - 8.))
                .h(px(CAP))
                .rounded(px(5.))
                .bg(rgb(INK))
                .flex()
                .items_center()
                .justify_center()
                .child(div().w(px(14.)).h(px(2.)).rounded_full().bg(rgb(PANEL))),
        )
}

/// Maps a pointer position on a vertical track to the nearest fader step.
fn fader_at_pixel(y: f32, top: f32, height: f32) -> Option<FaderGain> {
    if !y.is_finite() || !top.is_finite() || !height.is_finite() || height <= CAP {
        return None;
    }
    fader_from_fraction(1. - (y - top - CAP / 2.) / (height - CAP))
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
            let y = 100. + CAP / 2. + (1. - fader_fraction(fader)) * (260. - CAP);
            assert_eq!(fader_at_pixel(y, 100., 260.), Some(fader));
        }
        assert_eq!(fader_at_pixel(-100., 100., 260.), Some(FaderGain::MAX));
        assert_eq!(fader_at_pixel(1000., 100., 260.), Some(FaderGain::MIN));
        for (y, top, height) in [
            (f32::NAN, 0., 260.),
            (1., f32::INFINITY, 260.),
            (1., 0., f32::NAN),
            (1., 0., CAP),
            (1., 0., -1.),
        ] {
            assert!(fader_at_pixel(y, top, height).is_none());
        }
    }

    #[test]
    fn faders_fit_the_shortest_window_and_the_scale_names_real_levels() {
        assert!((fader_height_for_viewport(650.) - FADER_MIN).abs() < f32::EPSILON);
        assert!((fader_height_for_viewport(4000.) - FADER_MAX).abs() < f32::EPSILON);
        for level in SCALE {
            assert!(FaderGain::from_decibels(f32::from(level)).is_ok());
        }
        assert!(mixer_min_width(2) < mixer_width(2));
    }
}
