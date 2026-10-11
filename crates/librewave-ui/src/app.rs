use gpui::{
    App, Bounds, Context, Div, Stateful, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    rgb, size,
};
use librewave_core::{
    Command, DeviceAdmissionSnapshot, DeviceGeneration, DeviceId, DeviceWriteAccess,
    FixedPointValue, Snapshot, Wave3Control,
};
use librewave_ipc::Response;
use librewave_platform_linux::ipc::{Client, default_socket_path};

const BACKGROUND: u32 = 0x000D_0F11;
const PANEL: u32 = 0x0014_171A;
const RAISED: u32 = 0x001A_1E22;
const INK: u32 = 0x00F4_F5F6;
const MUTED: u32 = 0x0099_A1AA;
const WARNING: u32 = 0x00E9_B872;

/// Opens the optional desktop client.
///
/// # Panics
///
/// Panics if GPUI cannot open its initial window.
pub fn run() {
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(740.), px(680.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(600.), px(540.))),
            app_id: Some("dev.librewave.LibreWave".to_owned()),
            ..WindowOptions::default()
        };
        cx.open_window(options, |_, cx| cx.new(Shell::new))
            .expect("failed to open the LibreWave application window");
    });
}

struct Shell {
    snapshot: Option<Snapshot>,
    pending: bool,
    error: Option<String>,
}

impl Shell {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut shell = Self { snapshot: None, pending: false, error: None };
        shell.send(Command::GetStatus, cx);
        shell
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if self.pending {
            return;
        }
        self.pending = true;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { Client::new(default_socket_path()).request(command) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.pending = false;
                match result {
                    Ok(Response::Status { snapshot } | Response::Refreshed { snapshot }) => {
                        this.snapshot = Some(snapshot);
                    }
                    Ok(
                        Response::DeviceInspection { device } | Response::ControlChanged { device },
                    ) => {
                        if let Some(snapshot) = &mut this.snapshot
                            && let Some(current) =
                                snapshot.devices.iter_mut().find(|current| current.id == device.id)
                        {
                            *current = device;
                        }
                    }
                    Ok(_) => {
                        this.error = Some("The daemon returned an unexpected response.".to_owned());
                    }
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let device = self.snapshot.as_ref().and_then(|snapshot| snapshot.devices.first()).cloned();
        let services = self.snapshot.as_ref().map(|snapshot| {
            format!(
                "PipeWire: {}  ·  WirePlumber: {}  ·  Mixer: inactive",
                snapshot.audio.pipewire.status_word(),
                snapshot.audio.wireplumber.status_word()
            )
        });
        div()
            .size_full()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(INK))
            .p(px(24.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(div().text_2xl().child("LibreWave"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(services.unwrap_or_else(|| "Connecting to librewaved…".to_owned())),
            )
            .child(
                button("refresh", if self.pending { "Working…" } else { "Refresh status" })
                    .on_click(cx.listener(|this, _, _, cx| this.send(Command::GetStatus, cx))),
            )
            .when_some(self.error.clone(), |view, error| {
                view.child(
                    div()
                        .p(px(12.))
                        .rounded(px(8.))
                        .bg(rgb(RAISED))
                        .text_color(rgb(WARNING))
                        .child(error),
                )
            })
            .child(self.device_panel(device.as_ref(), cx))
    }
}

impl Shell {
    #[allow(clippy::too_many_lines)]
    fn device_panel(
        &self,
        device: Option<&librewave_core::DeviceSnapshot>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut panel = div()
            .flex_1()
            .min_h_0()
            .p(px(18.))
            .rounded(px(10.))
            .bg(rgb(PANEL))
            .flex()
            .flex_col()
            .gap(px(10.));
        let Some(device) = device else {
            return panel.child(div().text_color(rgb(MUTED)).child("No Wave:3 is available."));
        };
        panel = panel.child(div().text_xl().child("Wave:3 hardware controls")).child(
            div().text_sm().text_color(rgb(MUTED)).child(format!(
                "Connection: {}  ·  Access: {}",
                device.connection,
                device.admission.control_access()
            )),
        );
        match &device.admission {
            DeviceAdmissionSnapshot::NotInspected => {
                panel = panel.child(div().text_sm().text_color(rgb(MUTED)).child(
                    "Read controls to open the daemon connection. This can restore saved hardware values.",
                ));
            }
            DeviceAdmissionSnapshot::Failed { error } => {
                panel =
                    panel.child(div().text_sm().text_color(rgb(WARNING)).child(error.to_string()));
            }
            DeviceAdmissionSnapshot::Admitted { api, config: None, .. } => {
                panel = panel.child(
                    div()
                        .text_sm()
                        .text_color(rgb(WARNING))
                        .child(format!("API {api}: the configuration is unavailable.")),
                );
            }
            DeviceAdmissionSnapshot::Admitted {
                api,
                generation,
                write_access,
                config: Some(config),
            } => {
                let writable = matches!(write_access, DeviceWriteAccess::Ready) && !self.pending;
                panel = panel
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child(format!("API {api}  ·  Generation {generation}")),
                    )
                    .child(level_row(
                        "Microphone preamp",
                        config.input_gain,
                        0,
                        40 * 256,
                        Wave3Control::InputGain,
                        device.id,
                        *generation,
                        writable,
                        cx,
                    ))
                    .child(level_row(
                        "Headphone hardware level",
                        config.headphone_volume,
                        -60 * 256,
                        0,
                        Wave3Control::HeadphoneLevel,
                        device.id,
                        *generation,
                        writable,
                        cx,
                    ))
                    .child(toggle_row(
                        "Microphone mute",
                        config.input_mute,
                        Wave3Control::MicrophoneMute,
                        device.id,
                        *generation,
                        writable,
                        cx,
                    ))
                    .child(toggle_row(
                        "Headphone mute",
                        config.headphone_mute,
                        Wave3Control::HeadphoneMute,
                        device.id,
                        *generation,
                        writable,
                        cx,
                    ))
                    .child(toggle_row(
                        "Clipguard",
                        config.clipguard_enable,
                        Wave3Control::Clipguard,
                        device.id,
                        *generation,
                        writable,
                        cx,
                    ))
                    .child(toggle_row(
                        "Low-cut filter",
                        config.lowcut_enable,
                        Wave3Control::LowCut,
                        device.id,
                        *generation,
                        writable,
                        cx,
                    ))
                    .child(toggle_row(
                        "Gain Lock",
                        config.gain_lock,
                        Wave3Control::GainLock,
                        device.id,
                        *generation,
                        writable,
                        cx,
                    ));
            }
        }
        let id = device.id;
        panel
            .child(div().flex_1())
            .child(div().text_xs().text_color(rgb(MUTED)).child(
                "These are physical hardware values. Desktop and mixer volume remain separate.",
            ))
            .child(
                button("inspect", if self.pending { "Working…" } else { "Read controls" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.send(Command::InspectDevice { id }, cx);
                    })),
            )
    }
}

fn button(id: &str, label: &str) -> Stateful<Div> {
    div()
        .id(id.to_owned())
        .px(px(12.))
        .py(px(8.))
        .rounded(px(7.))
        .bg(rgb(RAISED))
        .hover(|style| style.bg(rgb(0x002B_3035)))
        .cursor_pointer()
        .text_sm()
        .child(label.to_owned())
}

fn control_button(
    key: &str,
    label: &str,
    control: Option<Wave3Control>,
    id: DeviceId,
    generation: DeviceGeneration,
    writable: bool,
    cx: &mut Context<Shell>,
) -> Stateful<Div> {
    let mut element = button(key, label);
    if writable && let Some(control) = control {
        element = element.on_click(cx.listener(move |this, _, _, cx| {
            this.send(
                Command::SetWave3Control { id, expected_generation: generation, control },
                cx,
            );
        }));
    } else {
        element = element.text_color(rgb(MUTED));
    }
    element
}

#[allow(clippy::too_many_arguments)]
fn level_row(
    label: &'static str,
    value: FixedPointValue,
    minimum: i32,
    maximum: i32,
    constructor: fn(FixedPointValue) -> Wave3Control,
    id: DeviceId,
    generation: DeviceGeneration,
    writable: bool,
    cx: &mut Context<Shell>,
) -> impl IntoElement {
    let down = stepped(value, -128, minimum, maximum).map(constructor);
    let up = stepped(value, 128, minimum, maximum).map(constructor);
    div()
        .flex()
        .items_center()
        .justify_between()
        .child(div().text_sm().child(format!("{label}: {:.1} dB", f64::from(value.raw) / 256.)))
        .child(
            div()
                .flex()
                .gap(px(5.))
                .child(control_button(
                    &format!("{label}-down"),
                    "−",
                    down,
                    id,
                    generation,
                    writable,
                    cx,
                ))
                .child(control_button(
                    &format!("{label}-up"),
                    "+",
                    up,
                    id,
                    generation,
                    writable,
                    cx,
                )),
        )
}

#[allow(clippy::too_many_arguments)]
fn toggle_row(
    label: &'static str,
    value: bool,
    constructor: fn(bool) -> Wave3Control,
    id: DeviceId,
    generation: DeviceGeneration,
    writable: bool,
    cx: &mut Context<Shell>,
) -> impl IntoElement {
    div().flex().items_center().justify_between().child(div().text_sm().child(label)).child(
        control_button(
            label,
            if value { "On" } else { "Off" },
            Some(constructor(!value)),
            id,
            generation,
            writable,
            cx,
        ),
    )
}

fn stepped(
    value: FixedPointValue,
    delta: i32,
    minimum: i32,
    maximum: i32,
) -> Option<FixedPointValue> {
    let raw = value.raw.checked_add(delta)?;
    (minimum..=maximum).contains(&raw).then_some(FixedPointValue { raw, ..value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_steps_stop_at_reviewed_boundaries() {
        let gain = FixedPointValue { raw: 0, fractional_bits: 8 };
        assert_eq!(stepped(gain, -128, 0, 40 * 256), None);
        assert_eq!(stepped(gain, 128, 0, 40 * 256).map(|value| value.raw), Some(128));
        let headphone = FixedPointValue { raw: -60 * 256, fractional_bits: 8 };
        assert_eq!(stepped(headphone, -128, -60 * 256, 0), None);
    }
}
