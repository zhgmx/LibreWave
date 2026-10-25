//! Wave:3 hardware controls. Each change is one typed daemon command that
//! carries the observed device generation.
use super::controls::{
    Tone, action_button, activate, caption, icon, icon_button, identified, tabular, tip,
};
use super::theme::{
    BODY, BODY_LINE, DANGER, FAINT, FOCUS, HEADING, HEADING_LINE, ICON, ICON_SMALL, INK, LINE,
    MONITOR, MUTED, RADIUS, RAISED, RAISED_HOVER, SPACE_LG, SPACE_MD, SPACE_SM, SPACE_XS, WARNING,
    alpha,
};
use super::{MixerEdit, Shell};
use gpui::{Context, Div, FontWeight, Stateful, div, prelude::*, px, relative, rgb, rgba};
use librewave_core::{
    Command, DeviceAdmissionSnapshot, DeviceConnection, DeviceGeneration, DeviceId, DeviceSnapshot,
    DeviceWriteAccess, FixedPointValue, Wave3ConfigSnapshot, Wave3Control,
};

type Toggle = fn(bool) -> Wave3Control;

const READ_CAUTION: &str = "Reading may restore saved device settings.";

/// The device identity that every control command needs.
#[derive(Clone, Copy)]
struct Target {
    id: DeviceId,
    generation: DeviceGeneration,
    writable: bool,
}

struct Level {
    name: &'static str,
    key: &'static str,
    value: FixedPointValue,
    /// The value being dragged to, or the device value.
    shown: FixedPointValue,
    minimum: i32,
    maximum: i32,
    control: fn(FixedPointValue) -> Wave3Control,
    glyph: &'static str,
    muted_glyph: &'static str,
    mute_key: &'static str,
    mute_hint: &'static str,
    unmute_hint: &'static str,
    muted: bool,
    mute_control: Toggle,
}

impl Shell {
    pub(super) fn hardware_writable(&self, access: DeviceWriteAccess) -> bool {
        matches!(access, DeviceWriteAccess::Ready)
            && !self.busy()
            && !self.hardware_state_stale
            && self.settings_modal.is_none()
    }

    pub(super) fn hardware_controls(
        &self,
        device: Option<&DeviceSnapshot>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let panel = div()
            .id("device-panel")
            .debug_selector(|| "device-panel".to_owned())
            .w_full()
            .flex()
            .flex_col()
            .gap(px(SPACE_LG));
        let Some(device) = device else {
            return panel.when(self.snapshot.is_some(), |view| {
                view.child(notice("Connect the Wave:3 with its USB cable.".to_owned(), MUTED))
            });
        };
        let connected = device.connection == DeviceConnection::Connected;
        let read = |tone, cx: &mut Context<Self>| {
            let id = device.id;
            div()
                .flex()
                .flex_col()
                .gap(px(SPACE_SM))
                .child(
                    action_button(
                        "inspect",
                        "Read controls",
                        tone,
                        !self.busy() && self.settings_modal.is_none() && connected,
                        cx,
                        move |this, _, cx| this.send(Command::InspectDevice { id }, cx),
                    )
                    .w_full(),
                )
                .child(caption(READ_CAUTION).text_color(rgb(FAINT)).text_center())
        };
        match &device.admission {
            DeviceAdmissionSnapshot::NotInspected => panel.child(read(Tone::Primary, cx)),
            DeviceAdmissionSnapshot::Failed { error } => {
                panel.child(notice(error.to_string(), DANGER)).child(read(Tone::Secondary, cx))
            }
            DeviceAdmissionSnapshot::Admitted { api, config: None, .. } => panel
                .child(notice(format!("Configuration unavailable for API {api}."), WARNING))
                .child(read(Tone::Secondary, cx)),
            DeviceAdmissionSnapshot::Admitted {
                generation,
                write_access,
                config: Some(config),
                ..
            } => {
                let target = Target {
                    id: device.id,
                    generation: *generation,
                    writable: self.hardware_writable(*write_access) && connected,
                };
                let slidable = connected
                    && matches!(write_access, DeviceWriteAccess::Ready)
                    && !self.hardware_state_stale
                    && self.settings_modal.is_none()
                    && !self.busy();
                let shown = |kind: fn(Wave3Control) -> Option<FixedPointValue>, value| {
                    self.shown_edit()
                        .and_then(|edit| match edit {
                            MixerEdit::Hardware { device: id, control } if id == device.id => {
                                kind(control)
                            }
                            _ => None,
                        })
                        .unwrap_or(value)
                };
                let shown = (
                    shown(gain_of, config.input_gain),
                    shown(headphone_of, config.headphone_volume),
                );
                panel
                    .child(levels(config, shown, target, slidable, cx))
                    .child(processing(config, target, cx))
                    .when_some(
                        match write_access {
                            DeviceWriteAccess::Ready => None,
                            DeviceWriteAccess::Locked { reason } => Some(*reason),
                        },
                        |view, reason| {
                            view.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(SPACE_SM))
                                    .child(icon("lock-simple", WARNING, ICON_SMALL))
                                    .child(caption(format!("Read-only: {reason}."))),
                            )
                        },
                    )
            }
        }
    }
}

fn notice(body: String, color: u32) -> Div {
    div()
        .text_size(px(BODY))
        .line_height(px(BODY_LINE))
        .text_color(rgb(color))
        .text_center()
        .child(body)
}

fn levels(
    config: &Wave3ConfigSnapshot,
    (gain, headphone): (FixedPointValue, FixedPointValue),
    target: Target,
    slidable: bool,
    cx: &mut Context<Shell>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(SPACE_MD))
        .child(level_row(
            &Level {
                name: "Microphone gain",
                key: "gain",
                value: config.input_gain,
                shown: gain,
                minimum: 0,
                maximum: 40 * 256,
                control: Wave3Control::InputGain,
                glyph: "mic",
                muted_glyph: "microphone-slash",
                mute_key: "mute-microphone",
                mute_hint: "Mute the microphone",
                unmute_hint: "Unmute the microphone",
                muted: config.input_mute,
                mute_control: Wave3Control::MicrophoneMute,
            },
            target,
            slidable,
            cx,
        ))
        .child(level_row(
            &Level {
                name: "Headphone level",
                key: "headphone",
                value: config.headphone_volume,
                shown: headphone,
                minimum: -60 * 256,
                maximum: 0,
                control: Wave3Control::HeadphoneLevel,
                glyph: "headphones",
                muted_glyph: "speaker-slash",
                mute_key: "mute-headphones",
                mute_hint: "Mute the headphones",
                unmute_hint: "Unmute the headphones",
                muted: config.headphone_mute,
                mute_control: Wave3Control::HeadphoneMute,
            },
            target,
            slidable,
            cx,
        ))
}

/// One hardware level: its mute button, name, value, range bar, and steppers.
fn level_row(level: &Level, target: Target, slidable: bool, cx: &mut Context<Shell>) -> Div {
    let down = stepped(level.value, -128, level.minimum, level.maximum).map(level.control);
    let up = stepped(level.value, 128, level.minimum, level.maximum).map(level.control);
    let ink = if level.muted { FAINT } else { INK };
    let (muted, mute_control) = (level.muted, level.mute_control);
    let mute = icon_button(
        level.mute_key,
        if muted { level.muted_glyph } else { level.glyph },
        if muted { level.unmute_hint } else { level.mute_hint },
        Tone::Secondary,
        if muted { DANGER } else { INK },
        target.writable,
        cx,
        move |this, _, cx| this.send(mute_command(target, muted, mute_control), cx),
    )
    .size(px(40.))
    .when(muted, |view| view.bg(rgba(alpha(DANGER, 0x24))));
    div()
        .flex()
        .items_center()
        .gap(px(SPACE_MD))
        .child(mute)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .justify_between()
                        .child(div().text_color(rgb(MUTED)).child(level.name))
                        .child(
                            div()
                                .text_size(px(HEADING))
                                .line_height(px(HEADING_LINE))
                                .font_weight(FontWeight::SEMIBOLD)
                                .font_features(tabular())
                                .text_color(rgb(ink))
                                .child(format!("{} dB", decibels(level.shown))),
                        ),
                )
                .child(level_slider(level, ink, target, slidable, cx)),
        )
        .child(
            div()
                .flex()
                .gap(px(SPACE_XS))
                .child(step_button(&format!("{}-down", level.key), "minus", down, target, cx))
                .child(step_button(&format!("{}-up", level.key), "plus", up, target, cx)),
        )
}

/// A pointer and key drag along a hardware level. Each position snaps to
/// the 0.5 dB device step inside the reviewed range.
#[derive(Clone)]
struct LevelDrag {
    key: &'static str,
}

const THUMB: f32 = 14.;

fn level_slider(
    level: &Level,
    ink: u32,
    target: Target,
    enabled: bool,
    cx: &mut Context<Shell>,
) -> Stateful<Div> {
    let (minimum, maximum, control, key) = (level.minimum, level.maximum, level.control, level.key);
    let current = level.value;
    let shown = level.shown;
    let fraction = span_fraction(shown, minimum, maximum);
    let bounds = std::rc::Rc::new(std::cell::Cell::new(None::<gpui::Bounds<gpui::Pixels>>));
    let painted = std::rc::Rc::clone(&bounds);
    let edit = move |raw: Option<FixedPointValue>| {
        raw.map(|value| MixerEdit::Hardware { device: target.id, control: control(value) })
    };
    let mut slider = identified(&format!("{key}-slider"))
        .w_full()
        .h(px(THUMB + 2.))
        .relative()
        .focusable()
        .tab_index(0)
        .tab_stop(enabled)
        .rounded_full()
        .border_1()
        .border_color(rgba(0))
        .children(slider_marks(fraction, ink, enabled))
        .child(
            gpui::canvas(move |area, _, _| painted.set(Some(area)), |_, (), _, _| {})
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
        );
    if !enabled {
        return slider;
    }
    slider = slider
        .cursor_pointer()
        .focus_visible(|style| style.border_color(rgb(FOCUS)))
        .on_drag(LevelDrag { key }, |_, _, _, cx| cx.new(|_| gpui::Empty))
        .on_drag_move(cx.listener(
            move |this, event: &gpui::DragMoveEvent<LevelDrag>, window, cx| {
                if event.drag(cx).key != key {
                    return;
                }
                let value = level_at_pixel(
                    f32::from(event.event.position.x),
                    f32::from(event.bounds.left()),
                    f32::from(event.bounds.size.width),
                    minimum,
                    maximum,
                    current,
                );
                if let Some(edit) = edit(value) {
                    this.preview_fader(edit, window, cx);
                }
            },
        ))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                let Some(area) = bounds.get() else {
                    return;
                };
                let value = level_at_pixel(
                    f32::from(event.position.x),
                    f32::from(area.left()),
                    f32::from(area.size.width),
                    minimum,
                    maximum,
                    current,
                );
                if let Some(edit) = edit(value) {
                    this.preview_fader(edit, window, cx);
                }
            }),
        )
        .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
            let value = match event.keystroke.key.as_str() {
                "left" | "down" => stepped(shown, -128, minimum, maximum),
                "right" | "up" => stepped(shown, 128, minimum, maximum),
                "home" => Some(FixedPointValue { raw: minimum, ..shown }),
                "end" => Some(FixedPointValue { raw: maximum, ..shown }),
                _ => return,
            };
            cx.stop_propagation();
            if let Some(edit) = edit(value) {
                this.send_fader(edit, cx);
            }
        }));
    slider
}

/// The rail with its filled part, and the thumb when the slider is usable.
fn slider_marks(fraction: f32, ink: u32, enabled: bool) -> [Div; 2] {
    [
        div()
            .absolute()
            .left(px(THUMB / 2.))
            .right(px(THUMB / 2.))
            .top(px(THUMB / 2. - 1.))
            .h(px(4.))
            .rounded_full()
            .bg(rgb(LINE))
            .child(div().h_full().w(relative(fraction)).rounded_full().bg(rgb(ink))),
        div().absolute().left_0().right(px(THUMB)).top_0().h(px(THUMB)).child(
            div()
                .absolute()
                .left(relative(fraction))
                .size(px(THUMB))
                .rounded_full()
                .bg(rgb(ink))
                .when(!enabled, |view| view.opacity(0.)),
        ),
    ]
}

/// The snapped level under a pointer position on a slider.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn level_at_pixel(
    x: f32,
    left: f32,
    width: f32,
    minimum: i32,
    maximum: i32,
    current: FixedPointValue,
) -> Option<FixedPointValue> {
    const HALF_DB_RAW: i32 = 128;
    let travel = width - THUMB;
    if travel.is_nan() || travel <= 0. || !x.is_finite() || !left.is_finite() {
        return None;
    }
    let fraction = ((x - left - THUMB / 2.) / travel).clamp(0., 1.);
    // Reviewed ranges are a few thousand raw steps, far inside exact f32 integers.
    let steps = ((maximum - minimum) / HALF_DB_RAW) as f32;
    let raw = minimum + (fraction * steps).round() as i32 * HALF_DB_RAW;
    Some(FixedPointValue { raw: raw.clamp(minimum, maximum), ..current })
}

/// The request for a level, or `None` when the device already has it.
pub(super) fn level_command(
    snapshot: &librewave_core::Snapshot,
    device: DeviceId,
    control: Wave3Control,
) -> Option<Command> {
    let device = snapshot.devices.iter().find(|candidate| candidate.id == device)?;
    let DeviceAdmissionSnapshot::Admitted { generation, config: Some(config), .. } =
        &device.admission
    else {
        return None;
    };
    let current = match control {
        Wave3Control::InputGain(_) => config.input_gain,
        Wave3Control::HeadphoneLevel(_) => config.headphone_volume,
        _ => return None,
    };
    (gain_of(control).or_else(|| headphone_of(control)) != Some(current)).then_some(
        Command::SetWave3Control { id: device.id, expected_generation: *generation, control },
    )
}

const fn gain_of(control: Wave3Control) -> Option<FixedPointValue> {
    match control {
        Wave3Control::InputGain(value) => Some(value),
        _ => None,
    }
}

const fn headphone_of(control: Wave3Control) -> Option<FixedPointValue> {
    match control {
        Wave3Control::HeadphoneLevel(value) => Some(value),
        _ => None,
    }
}

fn processing(config: &Wave3ConfigSnapshot, target: Target, cx: &mut Context<Shell>) -> Div {
    let mut row = div().flex().gap(px(SPACE_SM));
    let controls: [(&str, &str, &'static str, &'static str, bool, Toggle); 3] = [
        (
            "Clipguard",
            "clipguard",
            "shield-check",
            "Prevents distortion from sudden loud sounds by lowering the microphone gain \
             before the signal clips.",
            config.clipguard_enable,
            Wave3Control::Clipguard,
        ),
        (
            "Low-cut",
            "low-cut",
            "wave-sine",
            "Removes low rumble, such as desk bumps, fans, and traffic noise.",
            config.lowcut_enable,
            Wave3Control::LowCut,
        ),
        (
            "Gain Lock",
            "gain-lock",
            "lock-simple",
            "Stops other applications from changing the microphone gain. The dial and LibreWave can still change it.",
            config.gain_lock,
            Wave3Control::GainLock,
        ),
    ];
    for (name, key, glyph, hint, on, control) in controls {
        row = row.child(toggle_tile(name, key, glyph, on, control, target, cx).tooltip(tip(hint)));
    }
    row
}

/// A tile that is lit when its hardware option is on.
fn toggle_tile(
    name: &str,
    key: &str,
    glyph: &'static str,
    on: bool,
    control: Toggle,
    target: Target,
    cx: &mut Context<Shell>,
) -> Stateful<Div> {
    let tile = identified(key)
        .flex_1()
        .min_w_0()
        .h(px(68.))
        .p(px(SPACE_MD))
        .rounded(px(RADIUS))
        .border_1()
        .border_color(rgba(0))
        .flex()
        .flex_col()
        .justify_between()
        .text_size(px(BODY))
        .line_height(px(BODY_LINE))
        .font_weight(FontWeight::MEDIUM)
        .map(|view| {
            if on {
                view.bg(rgba(alpha(MONITOR, 0x24)))
                    .text_color(rgb(INK))
                    .hover(|style| style.bg(rgba(alpha(MONITOR, 0x38))))
            } else {
                view.bg(rgb(RAISED))
                    .text_color(rgb(MUTED))
                    .hover(|style| style.bg(rgb(RAISED_HOVER)))
            }
        })
        .child(icon(glyph, if on { MONITOR } else { MUTED }, ICON))
        .child(name.to_owned());
    activate(tile, target.writable, cx, move |this, _, cx| {
        this.send(control_command(target, control(!on)), cx);
    })
}

fn step_button(
    key: &str,
    glyph: &'static str,
    control: Option<Wave3Control>,
    target: Target,
    cx: &mut Context<Shell>,
) -> Stateful<Div> {
    icon_button(
        key,
        glyph,
        if glyph == "minus" { "Lower by 0.5 dB" } else { "Raise by 0.5 dB" },
        Tone::Secondary,
        INK,
        target.writable && control.is_some(),
        cx,
        move |this, _, cx| {
            if let Some(control) = control {
                this.send(control_command(target, control), cx);
            }
        },
    )
}

const fn control_command(target: Target, control: Wave3Control) -> Command {
    Command::SetWave3Control { id: target.id, expected_generation: target.generation, control }
}

fn mute_command(target: Target, muted: bool, control: Toggle) -> Command {
    control_command(target, control(!muted))
}

fn decibels(value: FixedPointValue) -> String {
    format!("{:.2}", f64::from(value.raw) / 256.).replace('-', "−")
}

#[allow(clippy::cast_precision_loss)]
fn span_fraction(value: FixedPointValue, minimum: i32, maximum: i32) -> f32 {
    // Reviewed ranges are a few thousand raw steps, far inside exact f32 integers.
    let offset = i64::from(value.raw) - i64::from(minimum);
    let span = i64::from(maximum) - i64::from(minimum);
    (offset as f32 / span as f32).clamp(0., 1.)
}

pub(super) fn stepped(
    value: FixedPointValue,
    delta: i32,
    minimum: i32,
    maximum: i32,
) -> Option<FixedPointValue> {
    const HALF_DB_RAW: i32 = 128;
    let target = value.raw.checked_add(delta)?;
    let raw = target.checked_add(HALF_DB_RAW / 2)?.div_euclid(HALF_DB_RAW) * HALF_DB_RAW;
    (minimum..=maximum).contains(&raw).then_some(FixedPointValue { raw, ..value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_mute_uses_the_observed_value_and_device_generation() {
        let generation = DeviceGeneration::new([7; 16], 19);
        let target = Target { id: DeviceId(3), generation, writable: true };
        for muted in [false, true] {
            assert_eq!(
                mute_command(target, muted, Wave3Control::MicrophoneMute),
                Command::SetWave3Control {
                    id: target.id,
                    expected_generation: generation,
                    control: Wave3Control::MicrophoneMute(!muted)
                }
            );
            assert_eq!(
                mute_command(target, muted, Wave3Control::HeadphoneMute),
                Command::SetWave3Control {
                    id: target.id,
                    expected_generation: generation,
                    control: Wave3Control::HeadphoneMute(!muted)
                }
            );
        }
    }

    #[test]
    fn level_bar_spans_the_reviewed_range_and_never_overflows_it() {
        let value = |raw| FixedPointValue { raw, fractional_bits: 8 };
        assert!(span_fraction(value(0), 0, 40 * 256).abs() < f32::EPSILON);
        assert!((span_fraction(value(20 * 256), 0, 40 * 256) - 0.5).abs() < f32::EPSILON);
        assert!((span_fraction(value(0), -60 * 256, 0) - 1.).abs() < f32::EPSILON);
        assert!(span_fraction(value(-90 * 256), -60 * 256, 0).abs() < f32::EPSILON);
        assert!((span_fraction(value(90 * 256), 0, 40 * 256) - 1.).abs() < f32::EPSILON);
    }

    #[test]
    fn displayed_physical_levels_preserve_fractional_reads_and_a_true_minus_sign() {
        let value = |raw| FixedPointValue { raw, fractional_bits: 8 };
        assert_eq!(decibels(value(4_726)), "18.46");
        assert_eq!(decibels(value(-4_726)), "−18.46");
        assert_eq!(decibels(value(-3_072)), "−12.00");
    }

    #[test]
    fn slider_positions_snap_to_half_decibel_steps_inside_the_range() {
        let current = FixedPointValue { raw: 4_726, fractional_bits: 8 };
        let at = |x| level_at_pixel(x, 100., 200. + THUMB, 0, 40 * 256, current).map(|v| v.raw);
        assert_eq!(at(100. + THUMB / 2.), Some(0));
        assert_eq!(at(100. + THUMB / 2. + 100.), Some(20 * 256));
        assert_eq!(at(100. + THUMB / 2. + 200.), Some(40 * 256));
        assert_eq!(at(-500.), Some(0));
        assert_eq!(at(5_000.), Some(40 * 256));
        for x in 0..240 {
            let raw = at(100. + f32::from(u8::try_from(x).expect("small"))).expect("level");
            assert_eq!(raw % 128, 0, "{raw} is not a 0.5 dB step");
        }
        assert!(level_at_pixel(f32::NAN, 0., 100., 0, 256, current).is_none());
        assert!(level_at_pixel(10., 0., THUMB, 0, 256, current).is_none());
    }

    #[test]
    fn queued_levels_use_the_current_generation_and_skip_unchanged_values() {
        let snapshot = crate::app::tests::snapshot_with_controls(DeviceWriteAccess::Ready);
        let DeviceAdmissionSnapshot::Admitted { generation, config: Some(config), .. } =
            snapshot.devices[0].admission.clone()
        else {
            panic!("admitted fixture");
        };
        let id = snapshot.devices[0].id;
        let raised = FixedPointValue { raw: config.input_gain.raw + 128, ..config.input_gain };
        assert_eq!(
            level_command(&snapshot, id, Wave3Control::InputGain(raised)),
            Some(Command::SetWave3Control {
                id,
                expected_generation: generation,
                control: Wave3Control::InputGain(raised),
            })
        );
        assert!(level_command(&snapshot, id, Wave3Control::InputGain(config.input_gain)).is_none());
        assert!(level_command(&snapshot, id, Wave3Control::LowCut(true)).is_none());
    }

    #[test]
    fn extreme_snapshot_values_cannot_overflow_a_hardware_level_bar() {
        let value = |raw| FixedPointValue { raw, fractional_bits: 8 };
        assert!(span_fraction(value(i32::MIN), -60 * 256, 0).abs() < f32::EPSILON);
        assert!((span_fraction(value(i32::MAX), -60 * 256, 0) - 1.).abs() < f32::EPSILON);
    }
}
