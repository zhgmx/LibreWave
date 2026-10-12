//! Wave:3 hardware controls. Each change is one typed daemon command that
//! carries the observed device generation.
use super::Shell;
use super::controls::{
    Tone, action_button, activate, caption, icon, icon_button, identified, tabular,
};
use super::theme::{
    BODY, BODY_LINE, DANGER, FAINT, HEADING, HEADING_LINE, ICON, ICON_SMALL, INK, LINE, MONITOR,
    MUTED, RADIUS, RAISED, RAISED_HOVER, SPACE_LG, SPACE_MD, SPACE_SM, SPACE_XS, WARNING, alpha,
};
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
            && !self.pending
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
                        !self.pending && self.settings_modal.is_none() && connected,
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
                panel
                    .child(levels(config, target, cx))
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

fn levels(config: &Wave3ConfigSnapshot, target: Target, cx: &mut Context<Shell>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(SPACE_MD))
        .child(level_row(
            &Level {
                name: "Microphone gain",
                key: "gain",
                value: config.input_gain,
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
            cx,
        ))
        .child(level_row(
            &Level {
                name: "Headphone level",
                key: "headphone",
                value: config.headphone_volume,
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
            cx,
        ))
}

/// One hardware level: its mute button, name, value, range bar, and steppers.
fn level_row(level: &Level, target: Target, cx: &mut Context<Shell>) -> Div {
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
                                .child(format!("{} dB", decibels(level.value))),
                        ),
                )
                .child(
                    div().w_full().h(px(4.)).rounded_full().bg(rgb(LINE)).child(
                        div()
                            .h_full()
                            .w(relative(span_fraction(level.value, level.minimum, level.maximum)))
                            .rounded_full()
                            .bg(rgb(ink)),
                    ),
                ),
        )
        .child(
            div()
                .flex()
                .gap(px(SPACE_XS))
                .child(step_button(&format!("{}-down", level.key), "minus", down, target, cx))
                .child(step_button(&format!("{}-up", level.key), "plus", up, target, cx)),
        )
}

fn processing(config: &Wave3ConfigSnapshot, target: Target, cx: &mut Context<Shell>) -> Div {
    let mut row = div().flex().gap(px(SPACE_SM));
    let controls: [(&str, &str, &'static str, bool, Toggle); 3] = [
        (
            "Clipguard",
            "clipguard",
            "shield-check",
            config.clipguard_enable,
            Wave3Control::Clipguard,
        ),
        ("Low-cut", "low-cut", "wave-sine", config.lowcut_enable, Wave3Control::LowCut),
        ("Gain Lock", "gain-lock", "lock-simple", config.gain_lock, Wave3Control::GainLock),
    ];
    for (name, key, glyph, on, control) in controls {
        row = row.child(toggle_tile(name, key, glyph, on, control, target, cx));
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
    fn extreme_snapshot_values_cannot_overflow_a_hardware_level_bar() {
        let value = |raw| FixedPointValue { raw, fractional_bits: 8 };
        assert!(span_fraction(value(i32::MIN), -60 * 256, 0).abs() < f32::EPSILON);
        assert!((span_fraction(value(i32::MAX), -60 * 256, 0) - 1.).abs() < f32::EPSILON);
    }
}
