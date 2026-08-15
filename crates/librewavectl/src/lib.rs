#![doc = "The complete headless `LibreWave` command-line client."]

use librewave_core::{
    DeviceAdmissionSnapshot, DeviceGeneration, DeviceId, DeviceSnapshot, FaderGain,
    FixedPointValue, MeterAvailability, MixRoute, MixTarget, MixerGeneration, MixerRuntimeState,
    MixerSnapshot, ServiceState, Snapshot, SourceId, VolumeSelection, Wave3ConfigSnapshot,
    Wave3Control,
};
use librewave_ipc::Response;
use librewave_platform_linux::ipc::{Client, ClientError};
use librewave_protocol::{Wave3GainDb, Wave3HeadphoneDb, Wave3MonitorPercent};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

mod lifecycle;

/// Runs the CLI against one explicit daemon socket.
///
/// # Errors
///
/// Returns an output error when the CLI cannot write its result or diagnostic.
pub fn run<I, O, E>(
    arguments: I,
    socket_path: &Path,
    output: &mut O,
    errors: &mut E,
) -> io::Result<i32>
where
    I: IntoIterator<Item = String>,
    O: Write,
    E: Write,
{
    let arguments = arguments.into_iter().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => {
            write_help(output)?;
            Ok(0)
        }
        [command] if matches!(command.as_str(), "help" | "--help" | "-h") => {
            write_help(output)?;
            Ok(0)
        }
        [command] if command == "status" => request_status(socket_path, output, errors),
        [command] if command == "refresh" => request_refresh(socket_path, output, errors),
        [devices, list] if devices == "devices" && list == "list" => {
            request_devices(socket_path, output, errors)
        }
        [devices, inspect, id] if devices == "devices" && inspect == "inspect" => {
            request_device_inspection(socket_path, id, output, errors)
        }
        [devices, set, id, generation, control, value] if devices == "devices" && set == "set" => {
            request_control_change(socket_path, id, generation, control, value, output, errors)
        }
        [mixer, show] if mixer == "mixer" && show == "show" => {
            request_mixer(socket_path, output, errors)
        }
        [mixer, set, source, generation, target, enabled, level]
            if mixer == "mixer" && set == "set" =>
        {
            request_mixer_change(
                socket_path,
                source,
                generation,
                target,
                enabled,
                level,
                output,
                errors,
            )
        }
        _ => {
            writeln!(errors, "error: invalid command; run `librewavectl help`")?;
            Ok(2)
        }
    }
}

fn request_mixer<O, E>(socket_path: &Path, output: &mut O, errors: &mut E) -> io::Result<i32>
where
    O: Write,
    E: Write,
{
    match request(socket_path, librewave_core::Command::GetMixer) {
        Ok(Response::Mixer { mixer }) => {
            write_mixer(output, &mixer)?;
            Ok(0)
        }
        Ok(_) => {
            writeln!(errors, "error: daemon returned an unexpected mixer response")?;
            Ok(1)
        }
        Err(error) => write_client_error(error, errors),
    }
}

#[allow(clippy::too_many_arguments)]
fn request_mixer_change<O, E>(
    socket_path: &Path,
    source: &str,
    generation: &str,
    target: &str,
    enabled: &str,
    level: &str,
    output: &mut O,
    errors: &mut E,
) -> io::Result<i32>
where
    O: Write,
    E: Write,
{
    let command = match parse_mixer_change(source, generation, target, enabled, level) {
        Ok(command) => command,
        Err(message) => {
            writeln!(errors, "error: {message}")?;
            return Ok(2);
        }
    };
    match request(socket_path, command) {
        Ok(Response::MixerRouteChanged { mixer }) => {
            write_mixer(output, &mixer)?;
            Ok(0)
        }
        Ok(_) => {
            writeln!(errors, "error: daemon returned an unexpected mixer response")?;
            Ok(1)
        }
        Err(error) => write_client_error(error, errors),
    }
}

fn parse_mixer_change(
    source: &str,
    generation: &str,
    target: &str,
    enabled: &str,
    level: &str,
) -> Result<librewave_core::Command, String> {
    let source = source
        .parse::<u16>()
        .map(SourceId::new)
        .map_err(|_| "source ID must be an integer from 0 through 65535".to_owned())?;
    let expected_generation = generation
        .parse::<u64>()
        .map(MixerGeneration)
        .map_err(|_| "mixer generation must be a nonnegative integer".to_owned())?;
    let target = match target {
        "monitor" => MixTarget::Monitor,
        "stream" => MixTarget::Stream,
        _ => return Err("mix target must be monitor or stream".to_owned()),
    };
    let enabled = parse_switch(enabled)?;
    let decibels =
        level.parse::<f32>().map_err(|_| "fader level must be a decimal number".to_owned())?;
    let fader = FaderGain::from_decibels(decibels).map_err(|error| error.to_string())?;
    Ok(librewave_core::Command::SetMixerRoute {
        expected_generation,
        source,
        target,
        route: MixRoute::new(enabled, fader),
    })
}

fn request_control_change<O, E>(
    socket_path: &Path,
    id: &str,
    generation: &str,
    control: &str,
    value: &str,
    output: &mut O,
    errors: &mut E,
) -> io::Result<i32>
where
    O: Write,
    E: Write,
{
    let Ok(id) = id.parse::<u32>() else {
        writeln!(errors, "error: device ID must be a number")?;
        return Ok(2);
    };
    let id = DeviceId(id);
    let Ok(generation) = generation.parse::<u64>() else {
        writeln!(errors, "error: device generation must be a number")?;
        return Ok(2);
    };
    let expected_generation = DeviceGeneration(generation);
    let control = match parse_control(control, value) {
        Ok(control) => control,
        Err(message) => {
            writeln!(errors, "error: {message}")?;
            return Ok(2);
        }
    };
    match request(
        socket_path,
        librewave_core::Command::SetWave3Control { id, expected_generation, control },
    ) {
        Ok(Response::ControlChanged { device }) => {
            write_device_inspection(output, &device)?;
            Ok(0)
        }
        Ok(_) => {
            writeln!(errors, "error: daemon returned an unexpected control response")?;
            Ok(1)
        }
        Err(error) => write_client_error(error, errors),
    }
}

fn parse_control(name: &str, value: &str) -> Result<Wave3Control, String> {
    match name {
        "microphone-gain" => parse_half_decibels(value)
            .and_then(|value| validate_fixed(value, Wave3GainDb::from_raw_q8_8, "microphone gain"))
            .map(Wave3Control::InputGain),
        "microphone-mute" => parse_switch(value).map(Wave3Control::MicrophoneMute),
        "clipguard" => parse_switch(value).map(Wave3Control::Clipguard),
        "low-cut" => parse_switch(value).map(Wave3Control::LowCut),
        "headphone-level" => parse_half_decibels(value)
            .and_then(|value| {
                validate_fixed(value, Wave3HeadphoneDb::from_raw_q8_8, "headphone level")
            })
            .map(Wave3Control::HeadphoneLevel),
        "headphone-mute" => parse_switch(value).map(Wave3Control::HeadphoneMute),
        "monitor-mix" => parse_percent(value)
            .and_then(|value| {
                validate_fixed(value, Wave3MonitorPercent::from_raw_q8_8, "monitor mix")
            })
            .map(Wave3Control::MonitorMix),
        "knob-target" => match value {
            "microphone" => Ok(Wave3Control::KnobTarget(VolumeSelection::Microphone)),
            "headphone" => Ok(Wave3Control::KnobTarget(VolumeSelection::Headphone)),
            "mix" => Ok(Wave3Control::KnobTarget(VolumeSelection::Mix)),
            _ => Err("knob target must be microphone, headphone, or mix".to_owned()),
        },
        "all-leds-off" => parse_switch(value).map(Wave3Control::AllLedsOff),
        "leds-flip" => parse_switch(value).map(Wave3Control::LedsFlip),
        "gain-lock" => parse_switch(value).map(Wave3Control::GainLock),
        _ => Err(format!("unknown Wave:3 control `{name}`")),
    }
}

fn parse_switch(value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("switch value must be on or off".to_owned()),
    }
}

fn parse_half_decibels(value: &str) -> Result<FixedPointValue, String> {
    let (negative, magnitude) =
        value.strip_prefix('-').map_or((false, value), |value| (true, value));
    let (whole, fraction) = magnitude.split_once('.').unwrap_or((magnitude, "0"));
    let whole = whole.parse::<i32>().map_err(|_| "level must use 0.5 dB steps".to_owned())?;
    let half = match fraction {
        "0" | "00" => 0,
        "5" | "50" => 128,
        _ => return Err("level must use 0.5 dB steps".to_owned()),
    };
    let magnitude = whole
        .checked_mul(256)
        .and_then(|whole| whole.checked_add(half))
        .ok_or_else(|| "level is outside the supported numeric range".to_owned())?;
    Ok(FixedPointValue { raw: if negative { -magnitude } else { magnitude }, fractional_bits: 8 })
}

fn parse_percent(value: &str) -> Result<FixedPointValue, String> {
    let percent =
        value.parse::<i32>().map_err(|_| "monitor mix must be a whole percentage".to_owned())?;
    let raw = percent
        .checked_mul(256)
        .ok_or_else(|| "monitor mix is outside the supported numeric range".to_owned())?;
    Ok(FixedPointValue { raw, fractional_bits: 8 })
}

fn validate_fixed<T>(
    value: FixedPointValue,
    validate: impl FnOnce(i32) -> Result<T, librewave_protocol::ValueError>,
    name: &str,
) -> Result<FixedPointValue, String> {
    validate(value.raw).map(|_| value).map_err(|error| format!("{name} {error}"))
}

/// Runs the CLI using `LIBREWAVE_SOCKET` or the default user runtime path.
///
/// # Errors
///
/// Returns an output error when the CLI cannot write its result or diagnostic.
pub fn run_default<I, O, E>(arguments: I, output: &mut O, errors: &mut E) -> io::Result<i32>
where
    I: IntoIterator<Item = String>,
    O: Write,
    E: Write,
{
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if lifecycle::is_command(&arguments) {
        return lifecycle::run(&arguments, output, errors);
    }
    let socket_path = configured_socket_path();
    run(arguments, &socket_path, output, errors)
}

fn request_status<O, E>(socket_path: &Path, output: &mut O, errors: &mut E) -> io::Result<i32>
where
    O: Write,
    E: Write,
{
    let response = request(socket_path, librewave_core::Command::GetStatus);
    match response {
        Ok(Response::Status { snapshot }) => {
            write_status(output, &snapshot)?;
            Ok(0)
        }
        Ok(_) => {
            writeln!(errors, "error: daemon returned an unexpected status response")?;
            Ok(1)
        }
        Err(error) => write_client_error(error, errors),
    }
}

fn request_refresh<O, E>(socket_path: &Path, output: &mut O, errors: &mut E) -> io::Result<i32>
where
    O: Write,
    E: Write,
{
    request_refresh_with(socket_path, output, errors, request)
}

fn request_refresh_with<O, E>(
    socket_path: &Path,
    output: &mut O,
    errors: &mut E,
    mut send: impl FnMut(&Path, librewave_core::Command) -> Result<Response, ClientError>,
) -> io::Result<i32>
where
    O: Write,
    E: Write,
{
    match send(socket_path, librewave_core::Command::Refresh) {
        Ok(Response::Refreshed { snapshot }) => {
            write_status(output, &snapshot)?;
            Ok(0)
        }
        Ok(_) => {
            writeln!(errors, "error: daemon returned an unexpected refresh response")?;
            Ok(1)
        }
        Err(error) => write_client_error(error, errors),
    }
}

fn request_devices<O, E>(socket_path: &Path, output: &mut O, errors: &mut E) -> io::Result<i32>
where
    O: Write,
    E: Write,
{
    let response = request(socket_path, librewave_core::Command::ListDevices);
    match response {
        Ok(Response::Devices { devices }) => {
            write_devices(output, &devices)?;
            Ok(0)
        }
        Ok(_) => {
            writeln!(errors, "error: daemon returned an unexpected devices response")?;
            Ok(1)
        }
        Err(error) => write_client_error(error, errors),
    }
}

fn request_device_inspection<O, E>(
    socket_path: &Path,
    id: &str,
    output: &mut O,
    errors: &mut E,
) -> io::Result<i32>
where
    O: Write,
    E: Write,
{
    let id = if let Ok(id) = id.parse::<u32>() {
        DeviceId(id)
    } else {
        writeln!(errors, "error: device ID must be a number")?;
        return Ok(2);
    };
    match request(socket_path, librewave_core::Command::InspectDevice { id }) {
        Ok(Response::DeviceInspection { device }) => {
            write_device_inspection(output, &device)?;
            Ok(inspection_exit_code(&device))
        }
        Ok(_) => {
            writeln!(errors, "error: daemon returned an unexpected device response")?;
            Ok(1)
        }
        Err(error) => write_client_error(error, errors),
    }
}

fn request(socket_path: &Path, command: librewave_core::Command) -> Result<Response, ClientError> {
    Client::new(socket_path).request(command)
}

fn write_status<W: Write>(output: &mut W, snapshot: &Snapshot) -> io::Result<()> {
    writeln!(output, "Daemon: connected")?;
    writeln!(output, "Protocol: {}", librewave_ipc::PROTOCOL_VERSION)?;
    writeln!(output, "State generation: {}", snapshot.generation)?;
    writeln!(output, "Devices: {}", snapshot.devices.len())?;
    writeln!(
        output,
        "Mixer: generation {}, inactive, {} sources",
        snapshot.mixer.generation(),
        snapshot.mixer.profile.sources.len()
    )?;
    write_service(output, "PipeWire", &snapshot.audio.pipewire)?;
    write_service(output, "WirePlumber", &snapshot.audio.wireplumber)
}

fn write_mixer<W: Write>(output: &mut W, mixer: &MixerSnapshot) -> io::Result<()> {
    writeln!(output, "Mixer generation: {}", mixer.generation())?;
    match mixer.runtime {
        MixerRuntimeState::Inactive { reason } => {
            writeln!(output, "Runtime: inactive ({reason})")?;
        }
    }
    match mixer.meters {
        MeterAvailability::Unavailable { reason } => {
            writeln!(output, "Meters: unavailable ({reason})")?;
        }
    }
    writeln!(output, "Microphone source: {}", mixer.profile.microphone_source)?;
    writeln!(output, "Sources:")?;
    for source in &mixer.profile.sources {
        writeln!(output, "  {}  {}  role={}", source.controls.source(), source.name, source.role)?;
        write_mix_route(output, "Monitor", source.controls.monitor())?;
        write_mix_route(output, "Stream", source.controls.stream())?;
    }
    Ok(())
}

fn write_mix_route<W: Write>(output: &mut W, name: &str, route: MixRoute) -> io::Result<()> {
    writeln!(output, "    {name}: {}, {} dB", on_off(route.enabled()), format_fader(route.fader()))
}

fn format_fader(fader: FaderGain) -> String {
    let steps = i32::from(fader.half_decibel_steps());
    if steps % 2 == 0 {
        return (steps / 2).to_string();
    }
    let sign = if steps < 0 { "-" } else { "" };
    format!("{sign}{}.5", steps.abs() / 2)
}

fn write_service<W: Write>(output: &mut W, name: &str, state: &ServiceState) -> io::Result<()> {
    match state {
        ServiceState::Available => writeln!(output, "{name}: available"),
        ServiceState::Unavailable { reason } => writeln!(output, "{name}: unavailable ({reason})"),
    }
}

fn write_devices<W: Write>(output: &mut W, devices: &[DeviceSnapshot]) -> io::Result<()> {
    if devices.is_empty() {
        return writeln!(output, "No supported devices found.");
    }
    writeln!(output, "ID  MODEL  CONNECTION  AUDIO CARDS")?;
    for device in devices {
        let cards = if device.audio_cards.is_empty() {
            "none".to_owned()
        } else {
            device
                .audio_cards
                .iter()
                .map(|card| match &card.name {
                    Some(name) => format!("card {} ({name})", card.number),
                    None => format!("card {}", card.number),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        writeln!(output, "{}  {}  {}  {cards}", device.id, device.model, device.connection)?;
    }
    Ok(())
}

fn write_device_inspection<W: Write>(output: &mut W, device: &DeviceSnapshot) -> io::Result<()> {
    writeln!(output, "Device {}", device.id)?;
    writeln!(output, "Model: {}", device.model)?;
    writeln!(output, "Connection: {}", device.connection)?;
    writeln!(output, "ALSA cards: {}", format_audio_cards(device))?;
    write_admission(output, &device.admission)
}

fn format_audio_cards(device: &DeviceSnapshot) -> String {
    if device.audio_cards.is_empty() {
        return "none".to_owned();
    }
    device
        .audio_cards
        .iter()
        .map(|card| match &card.name {
            Some(name) => format!("card {} ({name})", card.number),
            None => format!("card {}", card.number),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn write_admission<W: Write>(
    output: &mut W,
    admission: &DeviceAdmissionSnapshot,
) -> io::Result<()> {
    writeln!(output, "Control access: {}", admission.control_access())?;
    match admission.admitted_api() {
        Some(api) => writeln!(output, "Admitted API: {api}")?,
        None => writeln!(output, "Admitted API: none")?,
    }
    if let Some(error) = admission.error() {
        writeln!(output, "Admission: {error}")?;
    } else if matches!(admission, librewave_core::DeviceAdmissionSnapshot::NotInspected) {
        writeln!(output, "Admission: not inspected")?;
    }
    if let Some(config) = admission.config() {
        write_config(output, config)?;
    }
    if let Some(generation) = admission.generation() {
        writeln!(output, "Device generation: {generation}")?;
    }
    Ok(())
}

fn inspection_exit_code(device: &DeviceSnapshot) -> i32 {
    match &device.admission {
        librewave_core::DeviceAdmissionSnapshot::Admitted { .. } => 0,
        librewave_core::DeviceAdmissionSnapshot::NotInspected
        | librewave_core::DeviceAdmissionSnapshot::Failed { .. } => 1,
    }
}

fn write_config<W: Write>(output: &mut W, config: &Wave3ConfigSnapshot) -> io::Result<()> {
    writeln!(output, "Configuration:")?;
    writeln!(output, "  Microphone gain: {}", fixed_point(config.input_gain))?;
    writeln!(output, "  Microphone mute: {}", on_off(config.input_mute))?;
    writeln!(output, "  Clipguard: {}", on_off(config.clipguard_enable))?;
    writeln!(output, "  Low cut: {}", on_off(config.lowcut_enable))?;
    writeln!(output, "  Headphone level: {}", fixed_point(config.headphone_volume))?;
    writeln!(output, "  Headphone mute: {}", on_off(config.headphone_mute))?;
    writeln!(output, "  Monitor mix: {}", fixed_point(config.direct_monitor))?;
    writeln!(output, "  Knob target: {}", config.volume_select)?;
    writeln!(output, "  All LEDs off: {}", on_off(config.all_leds_off))?;
    writeln!(output, "  LEDs flip: {}", on_off(config.leds_flip))?;
    writeln!(output, "  Gain lock: {}", on_off(config.gain_lock))
}

fn fixed_point(value: librewave_core::FixedPointValue) -> String {
    let scale = 2_f64.powi(i32::from(value.fractional_bits));
    format!("{:.2}", f64::from(value.raw) / scale)
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn write_client_error<W: Write>(error: ClientError, output: &mut W) -> io::Result<i32> {
    match error {
        ClientError::PermissionDenied => {
            writeln!(output, "error: permission denied by librewaved")?;
        }
        ClientError::VersionMismatch { expected, actual } => writeln!(
            output,
            "error: protocol version mismatch (client {expected}, daemon {actual})"
        )?,
        ClientError::Connection { path, message } => writeln!(
            output,
            "error: cannot connect to librewaved at {}: {message}",
            path.display()
        )?,
        other => writeln!(output, "error: {other}")?,
    }
    Ok(1)
}

fn write_help<W: Write>(output: &mut W) -> io::Result<()> {
    writeln!(output, "Usage: librewavectl <command>")?;
    writeln!(output)?;
    writeln!(output, "Commands:")?;
    writeln!(output, "  status          Show daemon, device, and audio status")?;
    writeln!(output, "  refresh         Refresh inventory and retained device state")?;
    writeln!(output, "  devices list    List supported devices")?;
    writeln!(output, "  devices inspect <id>  Inspect one device")?;
    writeln!(
        output,
        "  devices set <id> <generation> <control> <value>  Set one Wave:3 hardware control"
    )?;
    writeln!(output, "  mixer show      Show desired mixer state and runtime availability")?;
    writeln!(output, "  mixer set <source-id> <generation> <monitor|stream> <on|off> <level-db>")?;
    writeln!(output)?;
    writeln!(output, "Wave:3 controls:")?;
    writeln!(output, "  microphone-gain  0 to 40 dB in 0.5 dB steps")?;
    writeln!(output, "  headphone-level  -60 to 0 dB in 0.5 dB steps")?;
    writeln!(output, "  monitor-mix      0 to 100 percent in 5 percent steps")?;
    writeln!(
        output,
        "  microphone-mute, headphone-mute, clipguard, low-cut, all-leds-off, leds-flip, gain-lock  on or off"
    )?;
    writeln!(output, "  knob-target      microphone, headphone, or mix")?;
    writeln!(output, "Use the device generation shown by `librewavectl devices inspect <id>`.")?;
    writeln!(output, "Use the mixer generation shown by `librewavectl mixer show`.")?;
    writeln!(output, "  setup           Install the current build")?;
    writeln!(output, "  doctor          Check the installation")?;
    writeln!(output, "  uninstall       Remove manifest-owned files")
}

fn configured_socket_path() -> PathBuf {
    std::env::var_os("LIBREWAVE_SOCKET")
        .map_or_else(librewave_platform_linux::ipc::default_socket_path, PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use librewave_core::{
        AudioSnapshot, DeviceConnection, DeviceId, DeviceModel, ServiceFailureReason,
    };

    fn snapshot() -> Snapshot {
        Snapshot {
            generation: 4,
            devices: vec![DeviceSnapshot {
                id: DeviceId(1),
                model: DeviceModel::Wave3,
                connection: DeviceConnection::Connected,
                audio_cards: vec![],
                admission: DeviceAdmissionSnapshot::not_inspected(),
            }],
            audio: AudioSnapshot {
                pipewire: ServiceState::Available,
                wireplumber: ServiceState::Unavailable { reason: ServiceFailureReason::NotRunning },
            },
            mixer: librewave_core::MixerSnapshot::default(),
        }
    }

    #[test]
    fn status_output_is_stable_and_human_readable() {
        let mut output = Vec::new();
        write_status(&mut output, &snapshot()).expect("render status");
        assert_eq!(
            String::from_utf8(output).expect("UTF-8"),
            "Daemon: connected\nProtocol: 4\nState generation: 4\nDevices: 1\nMixer: generation 0, inactive, 2 sources\nPipeWire: available\nWirePlumber: unavailable (not running)\n"
        );
    }

    #[test]
    fn devices_output_has_no_serial_or_topology_fields() {
        let mut output = Vec::new();
        write_devices(&mut output, &snapshot().devices).expect("render devices");
        assert_eq!(
            String::from_utf8(output).expect("UTF-8"),
            "ID  MODEL  CONNECTION  AUDIO CARDS\n1  Wave:3  connected  none\n"
        );
    }

    #[test]
    fn connection_error_is_explicit() {
        let mut output = Vec::new();
        let exit = write_client_error(
            ClientError::Connection {
                path: PathBuf::from("/run/user/1000/librewave.sock"),
                message: "No such file or directory".to_owned(),
            },
            &mut output,
        )
        .expect("render connection error");
        assert_eq!(exit, 1);
        assert!(String::from_utf8(output).expect("UTF-8").contains("cannot connect to librewaved"));
    }

    #[test]
    fn inspection_output_renders_safe_config_and_explicit_failure_without_identity_data() {
        let device = DeviceSnapshot {
            id: DeviceId(1),
            model: DeviceModel::Wave3,
            connection: DeviceConnection::Connected,
            audio_cards: vec![],
            admission: DeviceAdmissionSnapshot::Admitted {
                api: librewave_core::ApiVersion::new(5, 4),
                generation: librewave_core::DeviceGeneration(1),
                write_access: librewave_core::DeviceWriteAccess::Ready,
                config: Some(librewave_core::Wave3ConfigSnapshot {
                    input_gain: librewave_core::FixedPointValue { raw: 10_240, fractional_bits: 8 },
                    input_mute: false,
                    clipguard_enable: true,
                    lowcut_enable: false,
                    headphone_volume: librewave_core::FixedPointValue {
                        raw: -15_360,
                        fractional_bits: 8,
                    },
                    headphone_mute: false,
                    direct_monitor: librewave_core::FixedPointValue {
                        raw: 12_800,
                        fractional_bits: 8,
                    },
                    volume_select: librewave_core::VolumeSelection::Headphone,
                    all_leds_off: false,
                    leds_flip: true,
                    gain_lock: true,
                }),
            },
        };
        let mut output = Vec::new();
        write_device_inspection(&mut output, &device).expect("render inspection");
        let output = String::from_utf8(output).expect("UTF-8");
        assert_eq!(inspection_exit_code(&device), 0);
        assert!(output.contains("Control access: writable"));
        assert!(output.contains("Admitted API: 5.4"));
        assert!(output.contains("Microphone gain: 40.00"));
        assert!(output.contains("Knob target: headphone"));
        assert!(!output.contains("serial"));
        assert!(!output.contains("topology"));

        let failed = DeviceSnapshot {
            admission: DeviceAdmissionSnapshot::Failed {
                error: librewave_core::AdmissionError::Disconnected,
            },
            connection: DeviceConnection::Disconnected,
            ..device.clone()
        };
        let mut failed_output = Vec::new();
        write_device_inspection(&mut failed_output, &failed).expect("render failed inspection");
        assert_eq!(inspection_exit_code(&failed), 1);
        assert!(
            String::from_utf8(failed_output)
                .expect("UTF-8")
                .contains("Admission: device disconnected")
        );

        let not_inspected =
            DeviceSnapshot { admission: DeviceAdmissionSnapshot::NotInspected, ..device };
        let mut pending_output = Vec::new();
        write_device_inspection(&mut pending_output, &not_inspected)
            .expect("render pending inspection");
        assert_eq!(inspection_exit_code(&not_inspected), 1);
        assert!(
            String::from_utf8(pending_output).expect("UTF-8").contains("Admission: not inspected")
        );
    }

    #[test]
    fn refreshed_inspection_and_stale_client_diagnostic_show_the_new_generation() {
        let mut device = snapshot().devices.remove(0);
        device.admission = DeviceAdmissionSnapshot::Admitted {
            api: librewave_core::ApiVersion::new(5, 4),
            generation: DeviceGeneration(2),
            write_access: librewave_core::DeviceWriteAccess::Ready,
            config: Some(Wave3ConfigSnapshot {
                input_gain: FixedPointValue { raw: 0, fractional_bits: 8 },
                input_mute: true,
                clipguard_enable: false,
                lowcut_enable: false,
                headphone_volume: FixedPointValue { raw: 0, fractional_bits: 8 },
                headphone_mute: false,
                direct_monitor: FixedPointValue { raw: 0, fractional_bits: 8 },
                volume_select: VolumeSelection::Microphone,
                all_leds_off: false,
                leds_flip: false,
                gain_lock: false,
            }),
        };
        let mut output = Vec::new();
        write_device_inspection(&mut output, &device).expect("render refreshed inspection");
        let output = String::from_utf8(output).expect("UTF-8");
        assert!(output.contains("Microphone mute: on"));
        assert!(output.contains("Device generation: 2"));

        let mut errors = Vec::new();
        let exit = write_client_error(
            ClientError::Remote(librewave_ipc::IpcError {
                kind: librewave_ipc::IpcErrorKind::StaleDeviceGeneration {
                    expected: DeviceGeneration(1),
                    actual: DeviceGeneration(2),
                },
                message: "device generation 1 is stale; current generation is 2".to_owned(),
            }),
            &mut errors,
        )
        .expect("render stale diagnostic");
        assert_eq!(exit, 1);
        assert_eq!(
            String::from_utf8(errors).expect("UTF-8"),
            "error: device generation 1 is stale; current generation is 2\n"
        );
    }

    #[test]
    fn refresh_command_uses_the_semantic_daemon_request() {
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let exit = request_refresh_with(
            Path::new("unused.sock"),
            &mut output,
            &mut errors,
            |path, command| {
                assert_eq!(path, Path::new("unused.sock"));
                assert_eq!(command, librewave_core::Command::Refresh);
                Ok(Response::Refreshed { snapshot: snapshot() })
            },
        )
        .expect("run refresh command");

        assert_eq!(exit, 0);
        assert!(errors.is_empty());
        assert!(String::from_utf8(output).expect("UTF-8").contains("State generation: 4"));
    }

    #[test]
    fn help_lists_installation_lifecycle_commands() {
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let exit = run(
            vec!["librewavectl".to_owned(), "help".to_owned()],
            Path::new("/unused"),
            &mut output,
            &mut errors,
        )
        .expect("run CLI");
        assert_eq!(exit, 0);
        let output = String::from_utf8(output).expect("UTF-8");
        assert!(output.contains("refresh"));
        assert!(output.contains("setup"));
        assert!(output.contains("doctor"));
        assert!(output.contains("uninstall"));
        assert!(output.contains("mixer show"));
        assert!(output.contains("mixer set <source-id> <generation>"));
    }

    #[test]
    fn mixer_parser_uses_the_core_fader_validator() {
        let command =
            parse_mixer_change("2", "7", "stream", "off", "-0.5").expect("valid mixer command");
        assert_eq!(
            command,
            librewave_core::Command::SetMixerRoute {
                expected_generation: MixerGeneration(7),
                source: SourceId::new(2),
                target: MixTarget::Stream,
                route: MixRoute::new(
                    false,
                    FaderGain::from_half_decibel_steps(-1).expect("valid fader")
                ),
            }
        );
        for level in ["NaN", "inf", "-inf", "-60.5", "12.5", "0.25"] {
            assert!(
                parse_mixer_change("1", "0", "monitor", "on", level).is_err(),
                "accepted {level}"
            );
        }
        assert!(parse_mixer_change("1", "0", "auxiliary", "on", "0").is_err());
        assert!(parse_mixer_change("1", "0", "monitor", "yes", "0").is_err());
    }

    #[test]
    fn invalid_mixer_level_exits_before_ipc_through_the_public_run_path() {
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let exit = run(
            ["librewavectl", "mixer", "set", "1", "0", "monitor", "on", "0.25"].map(str::to_owned),
            Path::new("/unused"),
            &mut output,
            &mut errors,
        )
        .expect("render invalid mixer command");
        assert_eq!(exit, 2);
        assert!(output.is_empty());
        assert_eq!(
            String::from_utf8(errors).expect("UTF-8"),
            "error: fader gain must use exact 0.5 dB steps\n"
        );
    }

    #[test]
    fn mixer_output_is_exact_deterministic_and_explicitly_inactive() {
        let mut mixer = MixerSnapshot::default();
        mixer.profile.generation = MixerGeneration(3);
        mixer.profile.sources[0].controls = mixer.profile.sources[0].controls.with_route(
            MixTarget::Monitor,
            MixRoute::new(true, FaderGain::from_half_decibel_steps(-1).expect("valid fader")),
        );
        let mut output = Vec::new();
        write_mixer(&mut output, &mixer).expect("render mixer");
        assert_eq!(
            String::from_utf8(output).expect("UTF-8"),
            "Mixer generation: 3\nRuntime: inactive (audio host and mixer engine are not connected)\nMeters: unavailable (audio host and mixer engine are not connected)\nMicrophone source: 1\nSources:\n  1  Microphone  role=microphone\n    Monitor: on, -0.5 dB\n    Stream: on, 0 dB\n  2  System  role=system\n    Monitor: on, 0 dB\n    Stream: on, 0 dB\n"
        );
        assert_eq!(format_fader(FaderGain::MAX), "12");
        assert_eq!(format_fader(FaderGain::MIN), "-60");
    }

    #[test]
    fn control_parser_enforces_reviewed_units_ranges_and_steps() {
        for (name, value) in [
            ("microphone-gain", "0"),
            ("microphone-gain", "40"),
            ("microphone-gain", "0.5"),
            ("headphone-level", "-60"),
            ("headphone-level", "0"),
            ("headphone-level", "-59.5"),
            ("monitor-mix", "0"),
            ("monitor-mix", "100"),
            ("monitor-mix", "5"),
        ] {
            assert!(parse_control(name, value).is_ok(), "{name} {value}");
        }
        for (name, value) in [
            ("microphone-gain", "-0.5"),
            ("microphone-gain", "40.5"),
            ("microphone-gain", "1.25"),
            ("headphone-level", "-60.5"),
            ("headphone-level", "0.5"),
            ("headphone-level", "-1.25"),
            ("monitor-mix", "-5"),
            ("monitor-mix", "105"),
            ("monitor-mix", "1"),
        ] {
            assert!(parse_control(name, value).is_err(), "{name} {value}");
        }
    }
}
