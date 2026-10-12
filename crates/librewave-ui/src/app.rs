mod chrome;
mod controls;
mod device_art;
mod mixer;
mod theme;
use controls::{Tone, action_button};
use theme::{
    BODY, BODY_LINE, CAPTION, CAPTION_LINE, HEADING, HEADING_LINE, SPACE_LG, SPACE_MD, SPACE_SM,
    SPACE_XL,
};

use mixer::FaderEdit;

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
    Command, DeviceAdmissionSnapshot, DeviceGeneration, DeviceId, DeviceWriteAccess,
    FixedPointValue, Snapshot, Wave3Control,
};
use librewave_ipc::{IpcErrorKind, Response};
use librewave_platform_linux::ipc::{Client, ClientError, configured_socket_path};
use std::time::Duration;

// Neutral surfaces and semantic mix colors from the user's LibreWave mock.
const BACKGROUND: u32 = 0x000A_0A0A;
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
pub fn run(view: LaunchView) {
    gpui_platform::application().with_assets(crate::assets::Assets).run(move |cx: &mut App| {
        cx.text_system().add_fonts(crate::assets::fonts()).expect("embedded Geist fonts must load");
        let bounds = Bounds::centered(None, size(px(1240.), px(900.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(850.), px(650.))),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("LibreWave".into()),
                ..Default::default()
            }),
            app_id: Some("dev.librewave.LibreWave".to_owned()),
            ..WindowOptions::default()
        };
        cx.open_window(options, move |window, cx| {
            cx.new(|cx| {
                let mut shell = Shell::new(cx);
                if view == LaunchView::Settings {
                    shell.open_settings(window, cx);
                }
                shell
            })
        })
        .expect("failed to open the LibreWave application window");
    });
}

/// The view requested when the native client starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchView {
    Mixer,
    Settings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionExpectation {
    Connect,
    Stopped,
}

struct SettingsModal {
    focus: gpui::FocusHandle,
    close_focus: gpui::FocusHandle,
    restore_focus: Option<gpui::FocusHandle>,
}

struct Shell {
    snapshot: Option<Snapshot>,
    pending: bool,
    in_flight_fader: Option<FaderEdit>,
    queued_fader: Option<FaderEdit>,
    fader_window: Option<gpui::AnyWindowHandle>,
    poll_in_flight: bool,
    request_epoch: u64,
    action_error: Option<String>,
    connection_error: Option<String>,
    hardware_state_stale: bool,
    suspend_config: Option<std::path::PathBuf>,
    lifecycle_message: Option<String>,
    session_expectation: SessionExpectation,
    settings_modal: Option<SettingsModal>,
    settings_trigger_focus: Option<gpui::FocusHandle>,
}

impl Shell {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut shell = Self {
            snapshot: None,
            pending: false,
            in_flight_fader: None,
            queued_fader: None,
            fader_window: None,
            poll_in_flight: false,
            request_epoch: 0,
            action_error: None,
            connection_error: None,
            hardware_state_stale: false,
            suspend_config: None,
            lifecycle_message: None,
            session_expectation: SessionExpectation::Connect,
            settings_modal: None,
            settings_trigger_focus: None,
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

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_modal.is_some() {
            return;
        }
        let modal = SettingsModal {
            focus: cx.focus_handle(),
            close_focus: cx.focus_handle(),
            restore_focus: window.focused(cx).or_else(|| self.settings_trigger_focus.clone()),
        };
        self.queued_fader = None;
        cx.stop_active_drag(window);
        modal.close_focus.focus(window, cx);
        self.settings_modal = Some(modal);
        cx.notify();
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(modal) = self.settings_modal.take() {
            let restore = if self.snapshot.is_some() { modal.restore_focus } else { None };
            if let Some(focus) = restore.or_else(|| self.settings_trigger_focus.clone()) {
                focus.focus(window, cx);
            }
            cx.notify();
        }
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if self.pending
            || (self.settings_modal.is_some() && matches!(command, Command::SetMixerRoute { .. }))
        {
            return;
        }
        self.in_flight_fader = None;
        self.queued_fader = None;
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
                let succeeded = result.is_ok();
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
                if !succeeded
                    && this.in_flight_fader.is_some()
                    && let Some(window) = this.fader_window
                {
                    // A closed window has no remaining pointer gesture.
                    let _ = cx.update_window(window, |_, window, cx| cx.stop_active_drag(window));
                }
                if let Some(edit) = this.finish_fader(succeeded) {
                    this.send_fader(edit, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn send_fader(&mut self, edit: FaderEdit, cx: &mut Context<Self>) {
        if self.defer_fader(edit) {
            cx.notify();
            return;
        }
        if let Some(command) = self.snapshot.as_ref().and_then(|snapshot| edit.command(snapshot)) {
            self.send(command, cx);
            self.in_flight_fader = Some(edit);
        }
    }

    fn defer_fader(&mut self, edit: FaderEdit) -> bool {
        if self.settings_modal.is_some() {
            return true;
        }
        if !self.pending {
            return false;
        }
        if self.in_flight_fader.is_some() {
            // One pending user intent. Mouse events never create a request backlog.
            self.queued_fader = Some(edit);
        }
        true
    }

    fn finish_fader(&mut self, succeeded: bool) -> Option<FaderEdit> {
        self.in_flight_fader = None;
        let queued = self.queued_fader.take();
        // Never replay a drag after a stale generation, disconnect or write error.
        succeeded.then_some(queued).flatten()
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
        if self.settings_trigger_focus.is_none() {
            self.settings_trigger_focus = Some(cx.focus_handle());
        }
        self.mixer_frame(cx)
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
        let enabled = !self.pending;
        div()
            .border_t_1()
            .border_color(rgb(BORDER))
            .pt(px(SPACE_XL))
            .flex()
            .flex_col()
            .gap(px(SPACE_LG))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(HEADING))
                            .line_height(px(HEADING_LINE))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child("Audio"),
                    )
                    .child(
                        div()
                            .text_size(px(CAPTION))
                            .line_height(px(CAPTION_LINE))
                            .text_color(rgb(MUTED))
                            .child("Experimental"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(SPACE_SM))
                    .child(action_button(
                        "session-start",
                        "Start audio",
                        Tone::Primary,
                        enabled,
                        cx,
                        |this, _, cx| {
                            this.run_lifecycle(
                                librewavectl::SessionAction::Start {
                                    suspend_config: this.suspend_config.clone(),
                                },
                                cx,
                            );
                        },
                    ))
                    .child(action_button(
                        "session-stop",
                        "Stop and restore",
                        Tone::Secondary,
                        enabled,
                        cx,
                        |this, _, cx| this.run_lifecycle(librewavectl::SessionAction::Stop, cx),
                    ))
                    .child(action_button(
                        "setup-plan",
                        "Setup plan",
                        Tone::Quiet,
                        enabled,
                        cx,
                        |this, _, cx| {
                            this.run_lifecycle(librewavectl::SessionAction::SetupPlan, cx);
                        },
                    )),
            )
            .child(self.audio_configuration(cx))
            .when_some(self.lifecycle_message.clone(), |view, message| {
                view.child(div().child(message))
            })
    }

    fn audio_configuration(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = !self.pending;
        div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().text_color(rgb(MUTED)).child("Audio configuration"))
            .child(
                div()
                    .flex()
                    .gap(px(SPACE_SM))
                    .child(action_button(
                        "choose-fragment",
                        "Choose file…",
                        Tone::Secondary,
                        enabled,
                        cx,
                        |this, _, cx| this.choose_fragment(cx),
                    ))
                    .child(action_button(
                        "clear-fragment",
                        "Clear",
                        Tone::Quiet,
                        enabled && self.suspend_config.is_some(),
                        cx,
                        |this, _, cx| {
                            this.suspend_config = None;
                            cx.notify();
                        },
                    )),
            )
            .when_some(self.suspend_config.as_ref(), |view, path| {
                view.child(
                    div()
                        .text_size(px(CAPTION))
                        .line_height(px(CAPTION_LINE))
                        .text_color(rgb(MUTED))
                        .child(path.display().to_string()),
                )
            })
    }

    fn hardware_writable(&self, access: DeviceWriteAccess) -> bool {
        matches!(access, DeviceWriteAccess::Ready) && !self.pending && !self.hardware_state_stale
    }

    #[allow(clippy::too_many_lines)]
    fn device_panel(
        &self,
        device: Option<&librewave_core::DeviceSnapshot>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut panel = div().w_full().flex().flex_col().gap(px(SPACE_MD));
        let Some(device) = device else {
            return panel
                .child(section_label("Device"))
                .child(div().text_color(rgb(MUTED)).child("No Wave:3 connected."));
        };
        panel = panel
            .child(
                div()
                    .text_size(px(HEADING))
                    .line_height(px(HEADING_LINE))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child("Wave:3"),
            )
            .child(
                div()
                    .text_size(px(CAPTION))
                    .line_height(px(CAPTION_LINE))
                    .text_color(rgb(MUTED))
                    .child(device.connection.to_string()),
            );
        match &device.admission {
            DeviceAdmissionSnapshot::NotInspected => {
                panel = panel.child(
                    div()
                        .text_size(px(BODY))
                        .line_height(px(BODY_LINE))
                        .text_color(rgb(MUTED))
                        .child("Controls not loaded."),
                );
            }
            DeviceAdmissionSnapshot::Failed { error } => {
                panel = panel.child(
                    div()
                        .text_size(px(BODY))
                        .line_height(px(BODY_LINE))
                        .text_color(rgb(WARNING))
                        .child(error.to_string()),
                );
            }
            DeviceAdmissionSnapshot::Admitted { api, config: None, .. } => {
                panel = panel.child(
                    div()
                        .text_size(px(BODY))
                        .line_height(px(BODY_LINE))
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
                            .text_size(px(BODY))
                            .line_height(px(BODY_LINE))
                            .text_color(rgb(MUTED))
                            .child(format!("API {api}  ·  Generation {generation}")),
                    )
                    .child(level_row(
                        "Microphone gain",
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
                        "Headphone level",
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
        let controls = panel
            .child(
                div()
                    .text_size(px(CAPTION))
                    .line_height(px(CAPTION_LINE))
                    .text_color(rgb(MUTED))
                    .child("Reading may restore saved device settings."),
            )
            .child(action_button(
                "inspect",
                "Read controls",
                Tone::Primary,
                !self.pending,
                cx,
                move |this, _, cx| {
                    this.send(Command::InspectDevice { id }, cx);
                },
            ));
        div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(SPACE_XL))
            .child(
                div()
                    .w(px(200.))
                    .h(px(300.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        gpui::img(device_art::image(device.admission.config().filter(|_| {
                            device.connection == librewave_core::DeviceConnection::Connected
                        })))
                        .w(px(180.))
                        .h(px(270.)),
                    ),
            )
            .child(controls.flex_1().min_w_0())
    }
}

fn section_label(label: &'static str) -> impl IntoElement {
    div().text_size(px(CAPTION)).line_height(px(CAPTION_LINE)).text_color(rgb(MUTED)).child(label)
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
    action_button(
        key,
        label,
        Tone::Secondary,
        writable && control.is_some(),
        cx,
        move |this, _, cx| {
            if let Some(control) = control {
                this.send(
                    Command::SetWave3Control { id, expected_generation: generation, control },
                    cx,
                );
            }
        },
    )
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
        .child(
            div()
                .text_size(px(BODY))
                .line_height(px(BODY_LINE))
                .child(format!("{label}: {:.2} dB", f64::from(value.raw) / 256.)),
        )
        .child(
            div()
                .flex()
                .gap(px(SPACE_SM))
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
    div()
        .flex()
        .items_center()
        .justify_between()
        .child(div().text_size(px(BODY)).line_height(px(BODY_LINE)).child(label))
        .child(action_button(
            label,
            if value { "On" } else { "Off" },
            if value { Tone::Selected } else { Tone::Secondary },
            writable,
            cx,
            move |this, _, cx| {
                this.send(
                    Command::SetWave3Control {
                        id,
                        expected_generation: generation,
                        control: constructor(!value),
                    },
                    cx,
                );
            },
        ))
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
            in_flight_fader: None,
            queued_fader: None,
            fader_window: None,
            poll_in_flight: false,
            request_epoch: 0,
            action_error: None,
            connection_error: None,
            hardware_state_stale: false,
            suspend_config: None,
            lifecycle_message: None,
            session_expectation: SessionExpectation::Connect,
            settings_modal: None,
            settings_trigger_focus: None,
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
    fn repeated_drag_motion_keeps_one_latest_intent_and_cancels_it_on_failure() {
        let mut shell = shell_with(Snapshot::empty());
        let before = shell.snapshot.clone();
        let edit = FaderEdit {
            source: SYSTEM_SOURCE_ID,
            target: MixTarget::Stream,
            fader: FaderGain::MIN,
        };
        shell.pending = true;
        // An unrelated in-flight request does not acquire a fader backlog.
        assert!(shell.defer_fader(edit));
        assert!(shell.queued_fader.is_none());
        shell.in_flight_fader = Some(edit);
        for step in -120..=24 {
            assert!(shell.defer_fader(FaderEdit {
                fader: FaderGain::from_half_decibel_steps(step).expect("step"),
                ..edit
            }));
        }
        let latest = FaderEdit { fader: FaderGain::MAX, ..edit };
        assert_eq!(shell.queued_fader, Some(latest));
        assert_eq!(shell.snapshot, before, "pointer intent cannot replace daemon state");
        assert_eq!(shell.finish_fader(true), Some(latest));
        assert!(shell.queued_fader.is_none() && shell.in_flight_fader.is_none());
        shell.in_flight_fader = Some(edit);
        shell.defer_fader(latest);
        assert!(shell.finish_fader(false).is_none());
        assert!(shell.queued_fader.is_none() && shell.in_flight_fader.is_none());
    }

    #[test]
    fn queued_drag_uses_acknowledged_generation_and_preserves_both_mix_mute_states() {
        let mut snapshot = Snapshot::empty();
        let edited = snapshot
            .mixer
            .profile
            .with_route(SYSTEM_SOURCE_ID, MixTarget::Stream, MixRoute::new(false, FaderGain::MIN))
            .expect("route");
        snapshot.mixer = MixerSnapshot::inactive(edited);
        let edit = FaderEdit {
            source: SYSTEM_SOURCE_ID,
            target: MixTarget::Stream,
            fader: FaderGain::MAX,
        };
        let command = edit.command(&snapshot).expect("latest intent");
        assert_eq!(
            command,
            Command::SetMixerRoute {
                expected_generation: MixerGeneration(1),
                source: SYSTEM_SOURCE_ID,
                target: MixTarget::Stream,
                route: MixRoute::new(false, FaderGain::MAX),
            }
        );
        let applied = snapshot
            .mixer
            .profile
            .with_route(SYSTEM_SOURCE_ID, MixTarget::Stream, MixRoute::new(false, FaderGain::MAX))
            .expect("acknowledged route");
        assert_eq!(
            applied.source(SYSTEM_SOURCE_ID).expect("source").controls.monitor(),
            snapshot.mixer.profile.source(SYSTEM_SOURCE_ID).expect("source").controls.monitor()
        );
        snapshot.mixer = MixerSnapshot::inactive(applied);
        assert!(
            edit.command(&snapshot).is_none(),
            "acknowledged final position is not written twice"
        );
        assert!(
            FaderEdit { source: librewave_core::SourceId::new(999), ..edit }
                .command(&snapshot)
                .is_none()
        );
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
    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn modal_keyboard_focus_is_contained_and_restored(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| shell_with(Snapshot::empty()));
        let trigger =
            view.read_with(cx, |view, _| view.settings_trigger_focus.clone().expect("trigger"));
        cx.update(|window, cx| trigger.focus(window, cx));
        cx.simulate_keystrokes("ctrl-,");
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                let modal = view.settings_modal.as_ref().expect("opened");
                assert!(modal.close_focus.is_focused(window));
                assert!(modal.focus.contains_focused(window, cx));
            });
        });
        for key in ["tab", "shift-tab"] {
            for _ in 0..30 {
                cx.simulate_keystrokes(key);
                cx.update(|window, cx| {
                    view.update(cx, |view, cx| {
                        assert!(
                            view.settings_modal
                                .as_ref()
                                .expect("still open")
                                .focus
                                .contains_focused(window, cx)
                        );
                    });
                });
            }
        }
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| {
            view.update(cx, |view, _| {
                assert!(view.settings_modal.is_none());
                assert!(trigger.is_focused(window));
            });
        });
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn modal_backdrop_click_dismisses_without_activating_background(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| shell_with(Snapshot::empty()));
        let before = view.read_with(cx, |view, _| view.snapshot.clone());
        let settings = cx.debug_bounds("settings").expect("settings bounds");
        cx.simulate_click(settings.center(), gpui::Modifiers::none());
        assert!(view.read_with(cx, |view, _| view.settings_modal.is_some()));
        // This point is outside the modal, over the first source strip.
        cx.simulate_click(gpui::point(px(70.), px(180.)), gpui::Modifiers::none());
        view.read_with(cx, |view, _| {
            assert!(view.settings_modal.is_none());
            assert_eq!(view.snapshot, before);
            assert!(!view.pending);
            assert!(view.in_flight_fader.is_none() && view.queued_fader.is_none());
        });
    }
}
