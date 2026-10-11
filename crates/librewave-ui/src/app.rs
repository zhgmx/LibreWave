use gpui::{
    App, Bounds, Context, Div, Stateful, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    rgb, size,
};
use librewave_core::{
    Command, ControlAccessState, DeviceAdmissionSnapshot, DeviceGeneration, DeviceId,
    DeviceWriteAccess, FixedPointValue, Snapshot, Wave3Control,
};
use librewave_ipc::Response;
use librewave_platform_linux::ipc::{Client, default_socket_path};

// Neutral planes keep status, source routing, and physical controls distinct.
const BACKGROUND: u32 = 0x000A_0E13;
const SIDEBAR: u32 = 0x000F_151C;
const PANEL: u32 = 0x0013_1B23;
const RAISED: u32 = 0x001B_2630;
const BORDER: u32 = 0x0028_3540;
const INK: u32 = 0x00F2_F6F8;
const MUTED: u32 = 0x0095_A7B5;
const ACCENT: u32 = 0x006B_DACB;
const WARNING: u32 = 0x00E9_B872;

/// Opens the optional desktop client.
///
/// # Panics
///
/// Panics if GPUI cannot open its initial window.
pub fn run() {
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1080.), px(780.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(850.), px(650.))),
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
        div()
            .size_full()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(INK))
            .flex()
            .child(sidebar(device.is_some()))
            .child(self.studio_content(device.as_ref(), cx))
    }
}

impl Shell {
    fn studio_content(
        &self,
        device: Option<&librewave_core::DeviceSnapshot>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let daemon_online = self.snapshot.is_some();
        let services = self.snapshot.as_ref().map_or_else(
            || "The daemon is unavailable".to_owned(),
            |snapshot| {
                format!(
                    "PipeWire {}  ·  WirePlumber {}",
                    snapshot.audio.pipewire.status_word(),
                    snapshot.audio.wireplumber.status_word()
                )
            },
        );
        div()
            .id("studio-content")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .p(px(28.))
            .flex()
            .flex_col()
            .gap(px(20.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(div().text_2xl().child("Mixer"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(MUTED))
                                    .child("Sources on the left. Destination mixes on the right."),
                            ),
                    )
                    .child(
                        button("refresh", if self.pending { "Working…" } else { "Refresh status" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.send(Command::GetStatus, cx);
                            })),
                    ),
            )
            .child(service_panel(daemon_online, services))
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
            .child(mixer_matrix())
            .child(self.device_panel(device, cx))
    }

    #[allow(clippy::too_many_lines)]
    fn device_panel(
        &self,
        device: Option<&librewave_core::DeviceSnapshot>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut panel = div()
            .w_full()
            .p(px(20.))
            .rounded(px(10.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .gap(px(12.));
        let Some(device) = device else {
            return panel
                .child(section_label("DEVICE"))
                .child(div().text_color(rgb(MUTED)).child("No Wave:3 is available."));
        };
        panel = panel
            .child(section_label("DEVICE"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_xl().child("Wave:3 hardware"))
                    .child(status_badge(
                        &device.admission.control_access().to_string(),
                        if matches!(device.admission.control_access(), ControlAccessState::Writable)
                        {
                            ACCENT
                        } else {
                            WARNING
                        },
                    )),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(format!("Connection: {}", device.connection)),
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
            .child(div().h(px(1.)).w_full().bg(rgb(BORDER)))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("Physical hardware values. Desktop and mixer volume remain separate."),
            )
            .child(
                button("inspect", if self.pending { "Working…" } else { "Read controls" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.send(Command::InspectDevice { id }, cx);
                    })),
            )
    }
}

fn sidebar(device_available: bool) -> impl IntoElement {
    div()
        .w(px(210.))
        .h_full()
        .flex_none()
        .bg(rgb(SIDEBAR))
        .border_r_1()
        .border_color(rgb(BORDER))
        .p(px(22.))
        .flex()
        .flex_col()
        .gap(px(20.))
        .child(div().text_xl().child("LibreWave"))
        .child(section_label("WORKSPACE"))
        .child(
            div()
                .p(px(11.))
                .rounded(px(8.))
                .bg(rgb(RAISED))
                .text_color(rgb(ACCENT))
                .text_sm()
                .child("Mixer"),
        )
        .child(section_label("HARDWARE"))
        .child(div().text_sm().child(if device_available { "Wave:3" } else { "No Wave:3" }))
        .child(div().flex_1())
        .child(
            div()
                .text_xs()
                .text_color(rgb(MUTED))
                .child("Hardware levels stay separate from desktop volume."),
        )
}

fn service_panel(daemon_online: bool, services: String) -> impl IntoElement {
    div()
        .p(px(14.))
        .rounded(px(9.))
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
        .flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(div().text_sm().child(if daemon_online {
                    "Audio engine waiting for production host"
                } else {
                    "Connect librewaved to see device state"
                }))
                .child(div().text_xs().text_color(rgb(MUTED)).child(services)),
        )
        .child(status_badge(if daemon_online { "INACTIVE" } else { "OFFLINE" }, WARNING))
}

fn section_label(label: &'static str) -> impl IntoElement {
    div().text_xs().text_color(rgb(MUTED)).child(label)
}

fn status_badge(label: &str, color: u32) -> impl IntoElement {
    div()
        .px(px(9.))
        .py(px(5.))
        .rounded(px(5.))
        .bg(rgb(RAISED))
        .text_xs()
        .text_color(rgb(color))
        .child(label.to_owned())
}

fn mixer_matrix() -> impl IntoElement {
    div()
        .w_full()
        .rounded(px(10.))
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
        .p(px(18.))
        .flex()
        .flex_col()
        .gap(px(13.))
        .child(section_label("SOURCE ROUTING"))
        .child(
            div()
                .flex()
                .gap(px(12.))
                .child(matrix_cell("SOURCE", true))
                .child(matrix_cell("MONITOR MIX", false))
                .child(matrix_cell("STREAM MIX", false)),
        )
        .child(matrix_row("Microphone", "Wave:3 capture"))
        .child(matrix_row("System", "Desktop audio"))
        .child(
            div()
                .text_xs()
                .text_color(rgb(MUTED))
                .child("Routing and live meters become available when the audio host is ready."),
        )
}

fn matrix_cell(label: &'static str, source: bool) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .text_xs()
        .text_color(rgb(if source { INK } else { MUTED }))
        .child(label)
}

fn matrix_row(source: &'static str, detail: &'static str) -> impl IntoElement {
    div()
        .flex()
        .gap(px(12.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .p(px(11.))
                .rounded(px(7.))
                .bg(rgb(RAISED))
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(div().text_sm().child(source))
                .child(div().text_xs().text_color(rgb(MUTED)).child(detail)),
        )
        .child(matrix_unavailable())
        .child(matrix_unavailable())
}

fn matrix_unavailable() -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .p(px(11.))
        .rounded(px(7.))
        .border_1()
        .border_color(rgb(BORDER))
        .text_xs()
        .text_color(rgb(MUTED))
        .child("Unavailable")
}

fn button(id: &str, label: &str) -> Stateful<Div> {
    div()
        .id(id.to_owned())
        .px(px(12.))
        .py(px(8.))
        .rounded(px(7.))
        .bg(rgb(RAISED))
        .border_1()
        .border_color(rgb(BORDER))
        .hover(|style| style.bg(rgb(0x002B_3A45)))
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
