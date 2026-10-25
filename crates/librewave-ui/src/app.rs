mod applications;
mod chrome;
mod confirm;
mod controls;
mod device_art;
mod hardware;
mod meters;
mod mixer;
mod settings;
mod stage;
mod text_field;
mod theme;

#[cfg(test)]
use mixer::FaderEdit;
use mixer::MixerEdit;
use settings::SettingsPane;

#[cfg(test)]
use hardware::stepped;
#[cfg(test)]
use mixer::{
    fader_fraction, route_key_command, route_position_command, route_step_command,
    route_toggle_command,
};

use gpui::{App, Bounds, Context, Window, WindowBounds, WindowOptions, prelude::*, px, size};
use librewave_core::{Command, Snapshot};
use librewave_ipc::{IpcErrorKind, Response};
use librewave_platform_linux::ipc::{Client, ClientError, configured_socket_path};
use std::time::Duration;

const STATUS_INTERVAL: Duration = Duration::from_millis(500);
const TITLEBAR: f32 = 52.;

/// Opens the optional desktop client.
///
/// # Panics
///
/// Panics if GPUI cannot open its initial window.
pub fn run(view: LaunchView) {
    gpui_platform::application().with_assets(crate::assets::Assets).run(move |cx: &mut App| {
        cx.text_system().add_fonts(crate::assets::fonts()).expect("embedded Geist fonts must load");
        let bounds = Bounds::centered(None, size(px(1280.), px(840.)), cx);
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
    applications: Option<librewave_core::ApplicationSnapshot>,
    applications_received: Option<std::time::Instant>,
    application_error: Option<String>,
    application_request_epoch: Option<u64>,
    application_input: Option<librewave_core::SourceId>,
    channel_name: Option<gpui::Entity<text_field::TextField>>,
    assignment_name: Option<gpui::Entity<text_field::TextField>>,
    /// A rule identity chosen with the file picker, with the kind it was chosen as.
    picked_rule: Option<(String, applications::RuleKind)>,
    installed_applications: applications::Installed,
    meters: Option<librewave_core::MeterFrame>,
    meter_error: Option<String>,
    meter_request_epoch: Option<u64>,
    meters_received: Option<std::time::Instant>,
    pending: bool,
    lifecycle_pending: Option<&'static str>,
    in_flight_fader: Option<MixerEdit>,
    queued_fader: Option<MixerEdit>,
    linked_faders: std::collections::BTreeSet<librewave_core::SourceId>,
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
    settings_pane: SettingsPane,
    settings_trigger_focus: Option<gpui::FocusHandle>,
    confirmation: Option<confirm::Confirmation>,
}

impl Shell {
    fn linked_command(&self, command: Command) -> Command {
        if let Command::SetMixerRoute { expected_generation, source, target, route } = command
            && self.linked_faders.contains(&source)
            && self
                .snapshot
                .as_ref()
                .and_then(|s| s.mixer.profile.source(source))
                .is_some_and(|s| s.controls.route(target).fader() != route.fader())
        {
            return Command::SetMixerFaders {
                expected_generation,
                source,
                monitor: route.fader(),
                stream: route.fader(),
            };
        }
        command
    }
    fn new(cx: &mut Context<Self>) -> Self {
        let mut shell = Self {
            snapshot: None,
            applications: None,
            applications_received: None,
            application_error: None,
            application_request_epoch: None,
            application_input: None,
            channel_name: None,
            assignment_name: None,
            picked_rule: None,
            installed_applications: applications::Installed::NotRead,
            meters: None,
            meter_error: None,
            meter_request_epoch: None,
            meters_received: None,
            pending: false,
            lifecycle_pending: None,
            in_flight_fader: None,
            queued_fader: None,
            linked_faders: std::collections::BTreeSet::new(),
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
            settings_pane: SettingsPane::Audio,
            settings_trigger_focus: None,
            confirmation: None,
        };
        shell.send(Command::GetStatus, cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                if this.update(cx, Shell::poll_meters).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(STATUS_INTERVAL).await;
                if this
                    .update(cx, |this, cx| {
                        this.poll_status(cx);
                        this.poll_applications(cx);
                    })
                    .is_err()
                {
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
        if self.application_input.is_some() {
            self.close_channel(cx);
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
        let command = self.linked_command(command);
        if self.pending
            || (self.settings_modal.is_some()
                && matches!(
                    command,
                    Command::SetMixerRoute { .. }
                        | Command::SetMixerFaders { .. }
                        | Command::SetMixerOutput { .. }
                        | Command::AddMixerInput { .. }
                        | Command::RemoveMixerInput { .. }
                        | Command::InspectDevice { .. }
                        | Command::SetWave3Control { .. }
                ))
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

    fn edit_command(&self, edit: MixerEdit) -> Option<Command> {
        let snapshot = self.snapshot.as_ref()?;
        if let MixerEdit::Input(input) = edit
            && self.linked_faders.contains(&input.source)
        {
            let controls = snapshot.mixer.profile.source(input.source)?.controls;
            return (controls.monitor().fader() != input.fader
                || controls.stream().fader() != input.fader)
                .then_some(Command::SetMixerFaders {
                    expected_generation: snapshot.mixer.generation(),
                    source: input.source,
                    monitor: input.fader,
                    stream: input.fader,
                });
        }
        edit.command(snapshot)
    }

    fn send_fader(&mut self, edit: impl Into<MixerEdit>, cx: &mut Context<Self>) {
        let edit = edit.into();
        if self.defer_fader(edit) {
            cx.notify();
            return;
        }
        if let Some(command) = self.edit_command(edit) {
            self.send(command, cx);
            self.in_flight_fader = Some(edit);
        }
    }

    fn defer_fader(&mut self, edit: impl Into<MixerEdit>) -> bool {
        let edit = edit.into();
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

    fn finish_fader(&mut self, succeeded: bool) -> Option<MixerEdit> {
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
                let meters_cleared = snapshot.mixer.runtime
                    != librewave_core::MixerRuntimeState::Active
                    && self.meters.take().is_some();
                let changed = self.snapshot.as_ref() != Some(&snapshot)
                    || self.connection_error.is_some()
                    || self.hardware_state_stale
                    || meters_cleared;
                self.snapshot = Some(snapshot);
                self.session_expectation = SessionExpectation::Connect;
                self.connection_error = None;
                self.hardware_state_stale = false;
                return changed;
            }
            Ok(_) => {
                self.snapshot = None;
                self.meters = None;
                self.meter_error = None;
                self.meters_received = None;
                self.connection_error =
                    Some("The daemon returned an unexpected status response.".to_owned());
            }
            Err(error) => {
                self.snapshot = None;
                self.meters = None;
                self.meter_error = None;
                self.meters_received = None;
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
            (Command::RouteApplication { .. }, Response::Applications { applications }) => {
                self.applications = Some(applications);
                self.applications_received = Some(std::time::Instant::now());
            }
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
            | (
                Command::SetMixerRoute { .. }
                | Command::SetMixerFaders { .. }
                | Command::SetMixerOutput { .. }
                | Command::AddMixerInput { .. }
                | Command::RemoveMixerInput { .. }
                | Command::RenameMixerInput { .. }
                | Command::MoveMixerInput { .. }
                | Command::ResetMixer { .. }
                | Command::SetApplicationAssignment { .. },
                Response::MixerChanged { mixer },
            ) => {
                if let Some(snapshot) = &mut self.snapshot {
                    snapshot.mixer = mixer;
                }
            }
            _ => {
                self.action_error = Some("The daemon returned an unexpected response.".to_owned());
                return;
            }
        }
        if let Some(snapshot) = &self.snapshot {
            self.linked_faders.retain(|id| snapshot.mixer.profile.source(*id).is_some());
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
            self.meters = None;
            self.meter_error = None;
            self.meters_received = None;
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.settings_trigger_focus.is_none() {
            let focus = cx.focus_handle();
            if window.focused(cx).is_none() {
                focus.focus(window, cx);
            }
            self.settings_trigger_focus = Some(focus);
        }
        self.frame(window.viewport_size(), cx)
    }
}

impl Shell {
    fn run_lifecycle(&mut self, action: librewavectl::SessionAction, cx: &mut Context<Self>) {
        if self.pending {
            return;
        }
        self.lifecycle_pending = Some(match &action {
            librewavectl::SessionAction::Start { .. } => "Starting mixer…",
            librewavectl::SessionAction::Stop => "Stopping mixer…",
            librewavectl::SessionAction::SetupPlan => "Reading setup plan…",
        });
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
                this.lifecycle_pending = None;
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
                    self.meters = None;
                    self.meter_error = None;
                    self.meters_received = None;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use librewave_core::{DeviceGeneration, DeviceId, DeviceWriteAccess, FixedPointValue};
    use librewave_core::{FaderGain, MixRoute, MixTarget, MixerGeneration, MixerRuntimeState};
    use librewave_core::{MixerProfile, MixerSnapshot, SYSTEM_SOURCE_ID};
    use librewave_ipc::IpcError;
    use std::path::PathBuf;

    pub(super) fn shell_with(snapshot: Snapshot) -> Shell {
        Shell {
            snapshot: Some(snapshot),
            applications: None,
            applications_received: None,
            application_error: None,
            application_request_epoch: None,
            application_input: None,
            channel_name: None,
            assignment_name: None,
            picked_rule: None,
            // Tests never read the host's desktop entries.
            installed_applications: applications::Installed::Ready(vec![
                librewave_platform_linux::desktop_entries::InstalledApplication {
                    id: "org.test.Player".to_owned(),
                    name: "Test Player".to_owned(),
                    executable: Some("player".to_owned()),
                    icon: None,
                    categories: Vec::new(),
                },
            ]),
            meters: None,
            meter_error: None,
            meter_request_epoch: None,
            meters_received: None,
            pending: false,
            lifecycle_pending: None,
            in_flight_fader: None,
            queued_fader: None,
            linked_faders: std::collections::BTreeSet::new(),
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
            settings_pane: SettingsPane::Audio,
            settings_trigger_focus: None,
            confirmation: None,
        }
    }

    #[cfg(feature = "gpui-tests")]
    fn snapshot_with_controls(write_access: DeviceWriteAccess) -> Snapshot {
        use librewave_core::{
            ApiVersion, DeviceAdmissionSnapshot, DeviceConnection, DeviceModel, DeviceSnapshot,
            VolumeSelection, Wave3ConfigSnapshot,
        };
        let value = |raw| FixedPointValue { raw, fractional_bits: 8 };
        let mut snapshot = Snapshot::empty();
        snapshot.devices.push(DeviceSnapshot {
            id: DeviceId(1),
            model: DeviceModel::Wave3,
            connection: DeviceConnection::Connected,
            audio_cards: Vec::new(),
            admission: DeviceAdmissionSnapshot::Admitted {
                api: ApiVersion::new(5, 4),
                generation: DeviceGeneration::new([1; 16], 7),
                write_access,
                config: Some(Wave3ConfigSnapshot {
                    input_gain: value(4_726),
                    input_mute: false,
                    clipguard_enable: true,
                    lowcut_enable: false,
                    headphone_volume: value(-3_072),
                    headphone_mute: true,
                    direct_monitor: value(0),
                    volume_select: VolumeSelection::Microphone,
                    all_leds_off: false,
                    leds_flip: false,
                    gain_lock: false,
                }),
            },
        });
        snapshot
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn microphone_dots_follow_snapshots_and_the_knob_face_is_centered_in_the_ring(
        cx: &mut gpui::TestAppContext,
    ) {
        use librewave_core::VolumeSelection;
        const DOTS: [&str; 7] = [
            "device-level-0",
            "device-level-1",
            "device-level-2",
            "device-level-3",
            "device-level-4",
            "device-level-5",
            "device-level-6",
        ];
        let (view, cx) =
            cx.add_window_view(|_, _| shell_with(snapshot_with_controls(DeviceWriteAccess::Ready)));
        for selector in &DOTS[..3] {
            assert!(cx.debug_bounds(selector).is_some());
        }
        assert!(cx.debug_bounds("device-level-3").is_none());
        let ring = cx.debug_bounds("knob-ring").expect("observed ring");
        let cap = cx.debug_bounds("knob-face").expect("foreground cap");
        assert!(cap.top() > ring.top() && cap.bottom() < ring.bottom());
        assert!(cap.left() > ring.left() && cap.right() < ring.right());
        assert!((cap.center().x - ring.center().x).abs() < px(0.5));
        assert!((cap.center().y - ring.center().y).abs() < px(0.5));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut snapshot = snapshot_with_controls(DeviceWriteAccess::Ready);
                let librewave_core::DeviceAdmissionSnapshot::Admitted {
                    config: Some(config), ..
                } = &mut snapshot.devices[0].admission
                else {
                    panic!("loaded fixture");
                };
                config.input_gain.raw = 10240;
                view.snapshot = Some(snapshot);
                cx.notify();
            });
        });
        for selector in DOTS {
            assert!(cx.debug_bounds(selector).is_some());
        }
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let librewave_core::DeviceAdmissionSnapshot::Admitted {
                    config: Some(config), ..
                } = &mut view.snapshot.as_mut().expect("snapshot").devices[0].admission
                else {
                    panic!("loaded fixture");
                };
                config.volume_select = VolumeSelection::Headphone;
                cx.notify();
            });
        });
        for selector in DOTS {
            assert!(cx.debug_bounds(selector).is_none());
        }
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn hardware_led_positions_follow_observed_headphone_mix_and_mute_snapshots(
        cx: &mut gpui::TestAppContext,
    ) {
        use librewave_core::VolumeSelection;
        const DOTS: [&str; 7] = [
            "device-level-0",
            "device-level-1",
            "device-level-2",
            "device-level-3",
            "device-level-4",
            "device-level-5",
            "device-level-6",
        ];
        let (view, cx) =
            cx.add_window_view(|_, _| shell_with(snapshot_with_controls(DeviceWriteAccess::Ready)));
        for (target, microphone_mute, blend_raw, expected) in [
            (VolumeSelection::Microphone, true, 0, vec![0, 1, 2]),
            (VolumeSelection::Headphone, false, 0, vec![0, 1, 2, 3, 4]),
            (VolumeSelection::Mix, false, 0, vec![6]),
            (VolumeSelection::Mix, false, 12800, vec![3]),
            (VolumeSelection::Mix, false, 25600, vec![0]),
        ] {
            cx.update(|_, cx| {
                view.update(cx, |view, cx| {
                    let mut snapshot = snapshot_with_controls(DeviceWriteAccess::Ready);
                    let librewave_core::DeviceAdmissionSnapshot::Admitted {
                        config: Some(config),
                        ..
                    } = &mut snapshot.devices[0].admission
                    else {
                        panic!("loaded fixture");
                    };
                    config.volume_select = target;
                    config.input_gain.raw = 4096;
                    config.input_mute = microphone_mute;
                    config.headphone_volume.raw = -1152;
                    config.headphone_mute = false;
                    config.direct_monitor.raw = blend_raw;
                    view.snapshot = Some(snapshot);
                    cx.notify();
                });
            });
            for (index, selector) in DOTS.into_iter().enumerate() {
                assert_eq!(
                    cx.debug_bounds(selector).is_some(),
                    expected.contains(&index),
                    "{target:?}, blend {blend_raw}, LED {index}"
                );
            }
        }
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn loaded_controls_fit_the_short_window_and_settings_remains_modal(
        cx: &mut gpui::TestAppContext,
    ) {
        use librewave_core::DeviceWriteLockReason;
        for access in [
            DeviceWriteAccess::Ready,
            DeviceWriteAccess::Locked { reason: DeviceWriteLockReason::NoOwnedConnection },
        ] {
            let (view, cx) = cx.add_window_view(|_, _| {
                let mut shell = shell_with(snapshot_with_controls(access));
                shell.action_error = Some("Restoration pending: retry Stop.".to_owned());
                shell
            });
            for width in [1200., 850.] {
                cx.simulate_resize(gpui::size(px(width), px(650.)));
                let stage = cx.debug_bounds("device-stage").expect("stage");
                for key in [
                    "gain-down",
                    "gain-up",
                    "headphone-down",
                    "headphone-up",
                    "mute-microphone",
                    "mute-headphones",
                    "clipguard",
                    "low-cut",
                    "gain-lock",
                ] {
                    let control = cx.debug_bounds(key).expect(key);
                    assert!(
                        control.top() >= stage.top() && control.bottom() <= stage.bottom(),
                        "{key} is outside the stage: {control:?} versus {stage:?}"
                    );
                }
            }
            cx.simulate_keystrokes("ctrl-,");
            for page in ["pane-setup", "pane-connection", "pane-audio"] {
                let bounds = cx.debug_bounds(page).expect(page);
                cx.simulate_click(bounds.center(), gpui::Modifiers::none());
                assert!(cx.debug_bounds("action-error").is_some(), "error hidden on {page}");
                cx.simulate_keystrokes("tab");
                cx.update(|window, cx| {
                    view.update(cx, |view, cx| {
                        assert!(
                            view.settings_modal
                                .as_ref()
                                .expect("modal")
                                .focus
                                .contains_focused(window, cx)
                        );
                        assert!(!view.hardware_writable(DeviceWriteAccess::Ready));
                    });
                });
            }
            cx.simulate_keystrokes("escape");
            assert!(view.read_with(cx, |view, _| view.settings_modal.is_none()));
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
    fn linked_level_edits_align_both_faders_and_output_edits_share_the_latest_intent_slot() {
        let mut snapshot = Snapshot::empty();
        snapshot.mixer.profile = snapshot
            .mixer
            .profile
            .with_route(SYSTEM_SOURCE_ID, MixTarget::Stream, MixRoute::new(false, FaderGain::MAX))
            .expect("different muted stream");
        let mut shell = shell_with(snapshot);
        shell.linked_faders.insert(SYSTEM_SOURCE_ID);
        let input = FaderEdit {
            source: SYSTEM_SOURCE_ID,
            target: MixTarget::Monitor,
            fader: FaderGain::UNITY,
        };
        assert_eq!(
            shell.edit_command(input.into()),
            Some(Command::SetMixerFaders {
                expected_generation: MixerGeneration(1),
                source: SYSTEM_SOURCE_ID,
                monitor: FaderGain::UNITY,
                stream: FaderGain::UNITY
            })
        );
        let mute = route_toggle_command(
            SYSTEM_SOURCE_ID,
            MixTarget::Monitor,
            MixRoute::new(true, FaderGain::UNITY),
            MixerGeneration(1),
        );
        assert_eq!(shell.linked_command(mute), mute, "linking does not couple mute states");
        shell.pending = true;
        shell.in_flight_fader = Some(input.into());
        let output = MixerEdit::Output { target: MixTarget::Monitor, fader: FaderGain::MIN };
        assert!(shell.defer_fader(output));
        assert_eq!(shell.queued_fader, Some(output));
        assert_eq!(shell.finish_fader(true), Some(output));
        assert!(
            matches!(shell.edit_command(output),Some(Command::SetMixerOutput {route,..}) if route.enabled() && route.fader()==FaderGain::MIN)
        );
        assert!(shell.in_flight_fader.is_none() && shell.queued_fader.is_none());
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
        shell.in_flight_fader = Some(edit.into());
        for step in -120..=24 {
            assert!(shell.defer_fader(FaderEdit {
                fader: FaderGain::from_half_decibel_steps(step).expect("step"),
                ..edit
            }));
        }
        let latest = FaderEdit { fader: FaderGain::MAX, ..edit };
        assert_eq!(shell.queued_fader, Some(latest.into()));
        assert_eq!(shell.snapshot, before, "pointer intent cannot replace daemon state");
        assert_eq!(shell.finish_fader(true), Some(latest.into()));
        assert!(shell.queued_fader.is_none() && shell.in_flight_fader.is_none());
        shell.in_flight_fader = Some(edit.into());
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
    fn unchanged_status_does_not_redraw_and_changed_status_does() {
        let snapshot = Snapshot::empty();
        let mut shell = shell_with(snapshot.clone());
        assert!(!shell.apply_poll_result(0, Ok(Response::Status { snapshot: snapshot.clone() })));
        let mut changed = snapshot;
        changed.generation = 1;
        assert!(shell.apply_poll_result(0, Ok(Response::Status { snapshot: changed.clone() })));
        assert_eq!(shell.snapshot, Some(changed));
        assert!(!shell.poll_in_flight);
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
            Response::MixerChanged { mixer: mixer.clone() },
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
                assert!(!view.hardware_writable(DeviceWriteAccess::Ready));
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
                assert!(view.hardware_writable(DeviceWriteAccess::Ready));
            });
        });
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn connected_view_shows_the_stage_hardware_and_both_mixes(cx: &mut gpui::TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| shell_with(Snapshot::empty()));
        let stage = cx.debug_bounds("device-stage").expect("stage bounds");
        let mixer = cx.debug_bounds("mixer").expect("mixer bounds");
        assert!(stage.size.width > px(0.) && stage.size.height > px(0.));
        assert!(cx.debug_bounds("device-panel").is_some());
        // The stage and the mixer never overlap in either layout.
        assert!(mixer.left() >= stage.right() || mixer.top() >= stage.bottom());
        // Only a small handle at the top of a strip starts a channel drag.
        let handle = cx.debug_bounds("drag-channel-1").expect("drag handle");
        let fader = cx.debug_bounds("fader-1-monitor").expect("fader");
        assert!(handle.size.height <= px(16.) && handle.size.width <= px(48.));
        assert!(handle.bottom() < fader.top());
        // Each input has a fader for each mix.
        assert!(cx.debug_bounds("add-input").is_some());
        for fader in ["fader-1-monitor", "fader-1-stream", "fader-2-monitor", "fader-2-stream"] {
            assert!(cx.debug_bounds(fader).is_some(), "{fader} is missing");
        }
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn settings_copy_commands_survive_repeated_open_and_close(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| shell_with(Snapshot::empty()));
        for _ in 0..2 {
            cx.simulate_keystrokes("ctrl-,");
            let setup = cx.debug_bounds("pane-setup").expect("Setup page");
            cx.simulate_click(setup.center(), gpui::Modifiers::none());
            for (command, selector) in [
                ("setup", "copy-setup"),
                ("doctor", "copy-doctor"),
                ("uninstall", "copy-uninstall"),
            ] {
                let copy = cx.debug_bounds(selector).expect("copy button");
                let panel = cx.debug_bounds("settings-panel").expect("Settings panel");
                assert!(copy.left() >= panel.left() && copy.right() <= panel.right());
                cx.simulate_click(copy.center(), gpui::Modifiers::none());
                cx.update(|_, cx| {
                    assert_eq!(
                        cx.read_from_clipboard().and_then(|item| item.text()),
                        Some(format!("librewavectl {command}"))
                    );
                });
            }
            cx.simulate_keystrokes("escape");
            assert!(view.read_with(cx, |view, _| view.settings_modal.is_none()));
        }
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn opening_settings_cancels_queued_fader_intent(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut shell = shell_with(Snapshot::empty());
            let edit = MixerEdit::Output { target: MixTarget::Monitor, fader: FaderGain::MIN };
            shell.pending = true;
            shell.in_flight_fader = Some(edit);
            shell.queued_fader = Some(edit);
            shell
        });
        cx.simulate_keystrokes("ctrl-,");
        cx.update(|_, cx| {
            view.update(cx, |view, _| {
                assert!(view.queued_fader.is_none());
                assert!(view.finish_fader(true).is_none());
            });
        });
        cx.simulate_keystrokes("escape");
        assert!(view.read_with(cx, |view, _| view.queued_fader.is_none()));
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn mixer_output_controls_stay_inside_the_resized_work_area(cx: &mut gpui::TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| shell_with(Snapshot::empty()));
        for width in [850., 928., 1000., 1280., 1920.] {
            cx.simulate_resize(gpui::size(px(width), px(840.)));
            let mixer = cx.debug_bounds("mixer").expect("mixer");
            for key in [
                "output-monitor-level",
                "output-monitor-mute",
                "output-monitor-reset",
                "output-stream-level",
                "output-stream-mute",
                "output-stream-reset",
            ] {
                let control = cx.debug_bounds(key).expect(key);
                assert!(
                    control.left() >= mixer.left() && control.right() <= mixer.right(),
                    "{key} exceeds the mixer at {width}px: {control:?} versus {mixer:?}"
                );
            }
        }
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn offline_view_offers_settings_instead_of_invented_state(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut shell = shell_with(Snapshot::empty());
            shell.snapshot = None;
            shell
        });
        assert!(cx.debug_bounds("mixer").is_none());
        assert!(cx.debug_bounds("fader-1-monitor").is_none());
        let open = cx.debug_bounds("offline-settings").expect("settings action");
        cx.simulate_click(open.center(), gpui::Modifiers::none());
        assert!(view.read_with(cx, |view, _| view.settings_modal.is_some()));
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn modal_backdrop_click_dismisses_without_activating_background(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| shell_with(Snapshot::empty()));
        let before = view.read_with(cx, |view, _| view.snapshot.clone());
        let settings = cx.debug_bounds("settings").expect("settings bounds");
        cx.simulate_click(settings.center(), gpui::Modifiers::none());
        assert!(view.read_with(cx, |view, _| view.settings_modal.is_some()));
        // This point is outside the modal, over the device stage.
        cx.simulate_click(gpui::point(px(70.), px(180.)), gpui::Modifiers::none());
        view.read_with(cx, |view, _| {
            assert!(view.settings_modal.is_none());
            assert_eq!(view.snapshot, before);
            assert!(!view.pending);
            assert!(view.in_flight_fader.is_none() && view.queued_fader.is_none());
        });
    }
}
