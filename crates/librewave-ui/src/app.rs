mod mixer;

#[cfg(test)]
use mixer::{
    fader_fraction, route_key_command, route_position_command, route_step_command,
    route_toggle_command,
};

use gpui::{
    App, Bounds, Context, Div, Stateful, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    rgb, size,
};
use librewave_core::{
    Command, ControlAccessState, DeviceAdmissionSnapshot, DeviceGeneration, DeviceId,
    DeviceWriteAccess, FixedPointValue, MeterAvailability, MixerRuntimeState, Snapshot,
    Wave3Control,
};
use librewave_ipc::{IpcErrorKind, Response};
use librewave_platform_linux::ipc::{Client, ClientError, configured_socket_path};
use std::time::Duration;

// Neutral surfaces and semantic mix colors from the user's LibreWave mock.
const BACKGROUND: u32 = 0x000A_0A0A;
const SIDEBAR: u32 = 0x0011_1111;
const PANEL: u32 = 0x0014_1414;
const RAISED: u32 = 0x001F_1F1F;
const BORDER: u32 = 0x0029_2929;
const INK: u32 = 0x00ED_EDED;
const MUTED: u32 = 0x00A1_A1A1;
const ACCENT: u32 = 0x0082_DEB1;
const STREAM: u32 = 0x007F_C5EB;
const WARNING: u32 = 0x00FF_8C91;
const STATUS_INTERVAL: Duration = Duration::from_secs(2);

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionExpectation {
    Connect,
    Stopped,
}

struct Shell {
    snapshot: Option<Snapshot>,
    pending: bool,
    poll_in_flight: bool,
    request_epoch: u64,
    action_error: Option<String>,
    connection_error: Option<String>,
    hardware_state_stale: bool,
    suspend_config: Option<std::path::PathBuf>,
    lifecycle_message: Option<String>,
    session_expectation: SessionExpectation,
}

impl Shell {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut shell = Self {
            snapshot: None,
            pending: false,
            poll_in_flight: false,
            request_epoch: 0,
            action_error: None,
            connection_error: None,
            hardware_state_stale: false,
            suspend_config: None,
            lifecycle_message: None,
            session_expectation: SessionExpectation::Connect,
        };
        shell.send(Command::GetStatus, cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(STATUS_INTERVAL).await;
                if this.update(cx, Shell::poll_status).is_err() {
                    break;
                }
            }
        })
        .detach();
        shell
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if self.pending {
            return;
        }
        self.pending = true;
        self.request_epoch = self.request_epoch.wrapping_add(1);
        self.action_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { Client::new(configured_socket_path()).request(command) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.pending = false;
                let refresh_hardware = match result {
                    Ok(response) => {
                        this.apply_response(command, response);
                        false
                    }
                    Err(error) => this.apply_error(&error),
                };
                if refresh_hardware {
                    this.poll_status(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn poll_status(&mut self, cx: &mut Context<Self>) {
        if self.pending || self.poll_in_flight {
            return;
        }
        self.poll_in_flight = true;
        let epoch = self.request_epoch;
        cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        Client::new(configured_socket_path()).request(Command::GetStatus)
                    })
                    .await;
            let _ = this.update(cx, |this, cx| {
                if this.apply_poll_result(epoch, result) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn apply_poll_result(&mut self, epoch: u64, result: Result<Response, ClientError>) -> bool {
        self.poll_in_flight = false;
        if self.pending || self.request_epoch != epoch {
            return false;
        }
        match result {
            Ok(Response::Status { snapshot }) => {
                self.snapshot = Some(snapshot);
                self.session_expectation = SessionExpectation::Connect;
                self.connection_error = None;
                self.hardware_state_stale = false;
            }
            Ok(_) => {
                self.snapshot = None;
                self.connection_error =
                    Some("The daemon returned an unexpected status response.".to_owned());
            }
            Err(error) => {
                self.snapshot = None;
                self.connection_error = if self.session_expectation == SessionExpectation::Stopped {
                    None
                } else {
                    Some(error.to_string())
                };
            }
        }
        true
    }

    fn apply_response(&mut self, command: Command, response: Response) {
        match (command, response) {
            (Command::GetStatus, Response::Status { snapshot })
            | (Command::Refresh, Response::Refreshed { snapshot }) => {
                self.snapshot = Some(snapshot);
                self.hardware_state_stale = false;
            }
            (Command::InspectDevice { .. }, Response::DeviceInspection { device })
            | (Command::SetWave3Control { .. }, Response::ControlChanged { device }) => {
                if let Some(snapshot) = &mut self.snapshot
                    && let Some(current) =
                        snapshot.devices.iter_mut().find(|current| current.id == device.id)
                {
                    *current = device;
                }
                self.hardware_state_stale = false;
            }
            (Command::GetMixer, Response::Mixer { mixer })
            | (Command::SetMixerRoute { .. }, Response::MixerRouteChanged { mixer }) => {
                if let Some(snapshot) = &mut self.snapshot {
                    snapshot.mixer = mixer;
                }
            }
            _ => {
                self.action_error = Some("The daemon returned an unexpected response.".to_owned());
                return;
            }
        }
        self.connection_error = None;
    }

    fn apply_error(&mut self, error: &ClientError) -> bool {
        if let ClientError::Remote(remote) = error {
            self.action_error = Some(error.to_string());
            if matches!(
                remote.kind,
                IpcErrorKind::StaleDeviceGeneration { .. }
                    | IpcErrorKind::StaleDeviceBaseline
                    | IpcErrorKind::DeviceDisconnected { .. }
                    | IpcErrorKind::DeviceWriteLocked { .. }
                    | IpcErrorKind::PersistenceAmbiguous
                    | IpcErrorKind::TransactionFailed
            ) {
                self.hardware_state_stale = true;
                return true;
            }
        } else {
            self.snapshot = None;
            self.action_error = None;
            self.connection_error = if self.session_expectation == SessionExpectation::Stopped {
                None
            } else {
                Some(error.to_string())
            };
            self.hardware_state_stale = false;
        }
        false
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
    fn run_lifecycle(&mut self, action: librewavectl::SessionAction, cx: &mut Context<Self>) {
        if self.pending {
            return;
        }
        self.pending = true;
        self.request_epoch = self.request_epoch.wrapping_add(1);
        self.action_error = None;
        self.lifecycle_message = None;
        self.connection_error = None;
        let stopped = matches!(action, librewavectl::SessionAction::Stop);
        let starting = matches!(action, librewavectl::SessionAction::Start { .. });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { librewavectl::session_action(action) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.pending = false;
                if this.apply_lifecycle_result(stopped, starting, result) {
                    this.poll_status(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_lifecycle_result(
        &mut self,
        stopped: bool,
        starting: bool,
        result: std::io::Result<String>,
    ) -> bool {
        match result {
            Ok(message) => {
                self.lifecycle_message = Some(message);
                if stopped {
                    self.session_expectation = SessionExpectation::Stopped;
                    self.snapshot = None;
                    self.connection_error = None;
                } else if starting {
                    self.session_expectation = SessionExpectation::Connect;
                    return true;
                }
            }
            Err(error) => self.action_error = Some(error.to_string()),
        }
        false
    }

    fn choose_fragment(&mut self, cx: &mut Context<Self>) {
        if self.pending {
            return;
        }
        let selection = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select the WirePlumber .conf fragment to suspend temporarily".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = selection.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(Some(paths))) => this.suspend_config = paths.into_iter().next(),
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => this.action_error = Some(error.to_string()),
                    Err(error) => this.action_error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn lifecycle_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().p(px(16.)).rounded(px(8.)).bg(rgb(PANEL)).flex().flex_col().gap(px(10.))
            .child(div().text_sm().child("Experimental audio ownership"))
            .child(div().text_sm().text_color(rgb(MUTED)).child(
                "Start requires setup and USB access. The selected fragment is backed up and restored on stop. Closing this window keeps audio running."))
            .child(div().flex().gap(px(8.))
                .child(button("session-start", if self.pending { "Working…" } else { "Start experimental audio" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.run_lifecycle(librewavectl::SessionAction::Start { suspend_config: this.suspend_config.clone() }, cx);
                    })))
                .child(button("session-stop", "Stop and restore")
                    .on_click(cx.listener(|this, _, _, cx| this.run_lifecycle(librewavectl::SessionAction::Stop, cx))))
                .child(button("setup-plan", "View setup plan")
                    .on_click(cx.listener(|this, _, _, cx| this.run_lifecycle(librewavectl::SessionAction::SetupPlan, cx)))))
            .child(div().flex().gap(px(8.))
                .child(button("choose-fragment", "Choose fragment…").on_click(cx.listener(|this, _, _, cx| this.choose_fragment(cx))))
                .child(button("clear-fragment", "Clear selection").on_click(cx.listener(|this, _, _, cx| {
                    if !this.pending { this.suspend_config = None; cx.notify(); }
                }))))
            .child(div().text_sm().text_color(rgb(MUTED)).child(self.suspend_config.as_ref().map_or_else(
                || "No user fragment selected.".to_owned(), |path| format!("Temporarily suspend: {}", path.display()))))
            .when_some(self.lifecycle_message.clone(), |view, message| view.child(div().text_sm().child(message)))
    }

    fn hardware_writable(&self, access: DeviceWriteAccess) -> bool {
        matches!(access, DeviceWriteAccess::Ready) && !self.pending && !self.hardware_state_stale
    }

    fn studio_content(
        &self,
        device: Option<&librewave_core::DeviceSnapshot>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                                    .child("Independent Monitor and Stream levels for each input."),
                            ),
                    )
                    .child(
                        button("refresh", if self.pending { "Working…" } else { "Refresh status" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.send(Command::GetStatus, cx);
                            })),
                    ),
            )
            .child(service_panel(self.snapshot.as_ref()))
            .child(self.lifecycle_controls(cx))
            .when_some(self.connection_error.clone(), |view, error| {
                view.child(
                    div()
                        .p(px(12.))
                        .rounded(px(8.))
                        .bg(rgb(RAISED))
                        .text_color(rgb(WARNING))
                        .child(error),
                )
            })
            .when_some(self.action_error.clone(), |view, error| {
                view.child(
                    div()
                        .p(px(12.))
                        .rounded(px(8.))
                        .bg(rgb(RAISED))
                        .text_color(rgb(WARNING))
                        .child(error),
                )
            })
            .child(self.mixer_channels(cx))
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
                let writable = self.hardware_writable(*write_access);
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

fn service_panel(snapshot: Option<&Snapshot>) -> impl IntoElement {
    let (headline, badge, color) = match snapshot.map(|state| state.mixer.runtime) {
        Some(MixerRuntimeState::Active) => {
            ("Audio host and mixer engine connected", "ACTIVE", ACCENT)
        }
        Some(MixerRuntimeState::Inactive { .. }) => {
            ("Audio host and mixer engine are not connected", "INACTIVE", WARNING)
        }
        None => ("Connect librewaved to see device state", "OFFLINE", WARNING),
    };
    let services = snapshot.map_or_else(
        || "The daemon is unavailable".to_owned(),
        |state| {
            let meters = match state.mixer.meters {
                MeterAvailability::Unavailable { .. } => "meters unavailable",
                MeterAvailability::Unpublished => "meters not published over IPC",
            };
            format!(
                "PipeWire {}  ·  WirePlumber {}  ·  {meters}",
                state.audio.pipewire.status_word(),
                state.audio.wireplumber.status_word(),
            )
        },
    );
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
                .child(div().text_sm().child(headline))
                .child(div().text_xs().text_color(rgb(MUTED)).child(services)),
        )
        .child(status_badge(badge, color))
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

fn button(id: &str, label: &str) -> Stateful<Div> {
    div()
        .id(id.to_owned())
        .px(px(12.))
        .py(px(8.))
        .rounded(px(7.))
        .bg(rgb(RAISED))
        .border_1()
        .border_color(rgb(BORDER))
        .hover(|style| style.bg(rgb(0x0026_2626)))
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
    const HALF_DB_RAW: i32 = 128;
    let target = value.raw.checked_add(delta)?;
    let raw = target.checked_add(HALF_DB_RAW / 2)?.div_euclid(HALF_DB_RAW) * HALF_DB_RAW;
    (minimum..=maximum).contains(&raw).then_some(FixedPointValue { raw, ..value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use librewave_core::{FaderGain, MixRoute, MixTarget, MixerGeneration, MixerRuntimeState};
    use librewave_core::{MixerProfile, MixerSnapshot, SYSTEM_SOURCE_ID};
    use librewave_ipc::IpcError;
    use std::path::PathBuf;

    fn shell_with(snapshot: Snapshot) -> Shell {
        Shell {
            snapshot: Some(snapshot),
            pending: false,
            poll_in_flight: false,
            request_epoch: 0,
            action_error: None,
            connection_error: None,
            hardware_state_stale: false,
            suspend_config: None,
            lifecycle_message: None,
            session_expectation: SessionExpectation::Connect,
        }
    }

    #[test]
    fn lifecycle_stop_clears_active_status_and_expected_disconnect_keeps_the_result() {
        let mut snapshot = Snapshot::empty();
        snapshot.mixer = MixerSnapshot::active(MixerProfile::default());
        let mut shell = shell_with(snapshot);
        assert!(!shell.apply_lifecycle_result(true, false, Ok("Restored".into())));
        assert!(shell.snapshot.is_none());
        assert!(shell.apply_poll_result(
            0,
            Err(ClientError::Connection {
                path: PathBuf::from("/tmp/stopped.sock"),
                message: "stopped".into()
            })
        ));
        assert!(shell.connection_error.is_none());
        assert_eq!(shell.lifecycle_message.as_deref(), Some("Restored"));
        assert!(shell.apply_lifecycle_result(false, true, Ok("Ready".into())));
        assert_eq!(shell.session_expectation, SessionExpectation::Connect);
    }
    #[test]
    fn lifecycle_restoration_errors_survive_status_polling() {
        let mut shell = shell_with(Snapshot::empty());
        assert!(!shell.apply_lifecycle_result(
            false,
            true,
            Err(std::io::Error::other("restoration pending; retry stop"))
        ));
        shell.apply_poll_result(
            0,
            Err(ClientError::Connection {
                path: PathBuf::from("/tmp/failed.sock"),
                message: "unreachable".into(),
            }),
        );
        assert_eq!(shell.action_error.as_deref(), Some("restoration pending; retry stop"));
    }
    #[test]
    fn hardware_steps_stop_at_reviewed_boundaries() {
        let gain = FixedPointValue { raw: 0, fractional_bits: 8 };
        assert_eq!(stepped(gain, -128, 0, 40 * 256), None);
        assert_eq!(stepped(gain, 128, 0, 40 * 256).map(|value| value.raw), Some(128));
        let headphone = FixedPointValue { raw: -60 * 256, fractional_bits: 8 };
        assert_eq!(stepped(headphone, -128, -60 * 256, 0), None);
    }

    #[test]
    fn hardware_steps_round_physical_gain_to_reviewed_write_values() {
        let physical_gain = FixedPointValue { raw: 4_726, fractional_bits: 8 };
        assert_eq!(stepped(physical_gain, 128, 0, 40 * 256).map(|value| value.raw), Some(4_864));
        assert_eq!(stepped(physical_gain, -128, 0, 40 * 256).map(|value| value.raw), Some(4_608));
        let physical_headphone = FixedPointValue { raw: -1_153, fractional_bits: 8 };
        assert_eq!(
            stepped(physical_headphone, 128, -60 * 256, 0).map(|value| value.raw),
            Some(-1_024)
        );
        assert_eq!(
            stepped(physical_headphone, -128, -60 * 256, 0).map(|value| value.raw),
            Some(-1_280)
        );
    }

    #[test]
    fn software_route_commands_keep_source_target_and_other_route_values() {
        let source = SYSTEM_SOURCE_ID;
        let route = MixRoute::new(false, FaderGain::from_decibels(-3.5).expect("test level"));
        let generation = MixerGeneration(9);
        assert_eq!(
            route_toggle_command(source, MixTarget::Monitor, route, generation),
            Command::SetMixerRoute {
                expected_generation: generation,
                source,
                target: MixTarget::Monitor,
                route: MixRoute::new(true, route.fader()),
            }
        );
        assert_eq!(
            route_step_command(source, MixTarget::Stream, route, generation, 1),
            Some(Command::SetMixerRoute {
                expected_generation: generation,
                source,
                target: MixTarget::Stream,
                route: MixRoute::new(false, FaderGain::from_decibels(-3.0).expect("next level")),
            })
        );
        assert!(
            route_step_command(
                source,
                MixTarget::Stream,
                MixRoute::new(true, FaderGain::MAX),
                generation,
                1,
            )
            .is_none()
        );
    }

    #[test]
    fn fader_positions_round_trip_every_step_and_preserve_muted_route_identity() {
        let source = SYSTEM_SOURCE_ID;
        let generation = MixerGeneration(25);
        for target in [MixTarget::Monitor, MixTarget::Stream] {
            let old = MixRoute::new(false, FaderGain::MIN);
            for steps in -120..=24 {
                let fader = FaderGain::from_half_decibel_steps(steps).expect("valid step");
                let command =
                    route_position_command(source, target, old, generation, fader_fraction(fader));
                if fader == old.fader() {
                    assert!(command.is_none());
                } else {
                    assert_eq!(
                        command,
                        Some(Command::SetMixerRoute {
                            expected_generation: generation,
                            source,
                            target,
                            route: MixRoute::new(false, fader)
                        })
                    );
                }
            }
            for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                assert!(route_position_command(source, target, old, generation, invalid).is_none());
            }
            assert_eq!(
                route_position_command(source, target, old, generation, 2.),
                Some(Command::SetMixerRoute {
                    expected_generation: generation,
                    source,
                    target,
                    route: MixRoute::new(false, FaderGain::MAX)
                })
            );
            assert!(route_position_command(source, target, old, generation, -1.).is_none());
        }
    }

    #[test]
    fn keyboard_fader_changes_use_checked_levels_without_enabling_muted_audio() {
        let source = SYSTEM_SOURCE_ID;
        let generation = MixerGeneration(8);
        let route = MixRoute::new(false, FaderGain::MIN);
        for (key, steps) in [("up", -119), ("right", -119), ("end", 24), ("enter", 0)] {
            assert_eq!(
                route_key_command(source, MixTarget::Stream, route, generation, key),
                Some(Command::SetMixerRoute {
                    expected_generation: generation,
                    source,
                    target: MixTarget::Stream,
                    route: MixRoute::new(
                        false,
                        FaderGain::from_half_decibel_steps(steps).expect("checked level")
                    ),
                })
            );
        }
        for key in ["down", "left", "home", "unknown"] {
            assert!(route_key_command(source, MixTarget::Stream, route, generation, key).is_none());
        }
    }

    #[test]
    fn connection_loss_removes_stale_active_status_until_reconnect() {
        let mut snapshot = Snapshot::empty();
        snapshot.mixer = MixerSnapshot::active(MixerProfile::default());
        let mut shell = shell_with(snapshot);
        shell.apply_error(&ClientError::Connection {
            path: PathBuf::from("/tmp/librewave-test.sock"),
            message: "daemon stopped".to_owned(),
        });
        assert!(shell.snapshot.is_none());
        assert!(shell.connection_error.is_some());
        shell.apply_response(Command::GetStatus, Response::Status { snapshot: Snapshot::empty() });
        assert_eq!(
            shell.snapshot.as_ref().map(|snapshot| snapshot.mixer.runtime),
            Some(MixerRuntimeState::Inactive {
                reason: librewave_core::MixerInactiveReason::AudioHostEngineNotConnected,
            })
        );
        assert!(shell.connection_error.is_none());
    }

    #[test]
    fn status_poll_recovers_after_restart_and_cannot_overwrite_a_newer_action() {
        let mut active = Snapshot::empty();
        active.mixer = MixerSnapshot::active(MixerProfile::default());
        let mut shell = shell_with(active);
        shell.poll_in_flight = true;
        assert!(shell.apply_poll_result(
            0,
            Err(ClientError::Connection {
                path: PathBuf::from("/tmp/librewave-test.sock"),
                message: "daemon stopped".to_owned(),
            }),
        ));
        assert!(shell.snapshot.is_none());
        assert!(shell.connection_error.is_some());

        shell.poll_in_flight = true;
        shell.request_epoch = 1;
        assert!(!shell.apply_poll_result(0, Ok(Response::Status { snapshot: Snapshot::empty() }),));
        assert!(shell.snapshot.is_none());

        shell.poll_in_flight = true;
        assert!(shell.apply_poll_result(1, Ok(Response::Status { snapshot: Snapshot::empty() }),));
        assert!(shell.snapshot.is_some());
        assert!(shell.connection_error.is_none());
    }

    #[test]
    fn stale_hardware_error_blocks_old_controls_until_fresh_status() {
        let mut shell = shell_with(Snapshot::empty());
        assert!(shell.hardware_writable(DeviceWriteAccess::Ready));
        assert!(shell.apply_error(&ClientError::Remote(IpcError {
            kind: IpcErrorKind::StaleDeviceGeneration {
                expected: DeviceGeneration::new([0; 16], 1),
                actual: DeviceGeneration::new([0; 16], 2),
            },
            message: "device generation is stale".to_owned(),
        })));
        assert!(shell.hardware_state_stale);
        assert!(!shell.hardware_writable(DeviceWriteAccess::Ready));
        assert!(shell.action_error.is_some());

        shell.poll_in_flight = true;
        assert!(shell.apply_poll_result(0, Ok(Response::Status { snapshot: Snapshot::empty() }),));
        assert!(!shell.hardware_state_stale);
        assert!(shell.hardware_writable(DeviceWriteAccess::Ready));
        assert!(shell.action_error.is_some());

        assert!(shell.apply_error(&ClientError::Remote(IpcError {
            kind: IpcErrorKind::DeviceDisconnected { id: DeviceId(1) },
            message: "device disconnected".to_owned(),
        })));
        assert!(shell.hardware_state_stale);
        assert!(!shell.hardware_writable(DeviceWriteAccess::Ready));
    }

    #[test]
    fn accepted_route_response_replaces_desired_mixer_state() {
        let mut shell = shell_with(Snapshot::empty());
        let profile = MixerProfile::default()
            .with_route(
                SYSTEM_SOURCE_ID,
                MixTarget::Monitor,
                MixRoute::new(true, FaderGain::from_decibels(-0.5).expect("route level")),
            )
            .expect("valid route");
        let mixer = MixerSnapshot::inactive(profile);
        shell.apply_response(
            Command::SetMixerRoute {
                expected_generation: MixerGeneration(0),
                source: SYSTEM_SOURCE_ID,
                target: MixTarget::Monitor,
                route: MixRoute::new(true, FaderGain::from_decibels(-0.5).expect("route level")),
            },
            Response::MixerRouteChanged { mixer: mixer.clone() },
        );
        assert_eq!(shell.snapshot.as_ref().map(|snapshot| &snapshot.mixer), Some(&mixer));
    }
}
