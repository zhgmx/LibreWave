#![doc = "The complete headless `LibreWave` command-line client."]

use librewave_core::{DeviceSnapshot, ServiceState, Snapshot};
use librewave_ipc::Response;
use librewave_platform_linux::ipc::{Client, ClientError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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
    let mut arguments = arguments.into_iter();
    let _program = arguments.next();
    let command = arguments.next().unwrap_or_default();
    let subcommand = arguments.next();
    if arguments.next().is_some() {
        writeln!(errors, "error: unexpected command arguments")?;
        return Ok(2);
    }
    match (command.as_str(), subcommand.as_deref()) {
        ("" | "help" | "--help" | "-h", None) => {
            write_help(output)?;
            Ok(0)
        }
        ("status", None) => request_status(socket_path, output, errors),
        ("devices", Some("list")) => request_devices(socket_path, output, errors),
        ("devices", None) => {
            writeln!(errors, "error: expected `librewavectl devices list`")?;
            Ok(2)
        }
        (unknown, _) => {
            writeln!(errors, "error: unknown command `{unknown}`")?;
            Ok(2)
        }
    }
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

fn request(socket_path: &Path, command: librewave_core::Command) -> Result<Response, ClientError> {
    Client::new(socket_path).request(command)
}

fn write_status<W: Write>(output: &mut W, snapshot: &Snapshot) -> io::Result<()> {
    writeln!(output, "Daemon: connected")?;
    writeln!(output, "Protocol: {}", librewave_ipc::PROTOCOL_VERSION)?;
    writeln!(output, "State generation: {}", snapshot.generation)?;
    writeln!(output, "Devices: {}", snapshot.devices.len())?;
    write_service(output, "PipeWire", &snapshot.audio.pipewire)?;
    write_service(output, "WirePlumber", &snapshot.audio.wireplumber)
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
    writeln!(output, "  devices list    List supported devices")
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
            }],
            audio: AudioSnapshot {
                pipewire: ServiceState::Available,
                wireplumber: ServiceState::Unavailable { reason: ServiceFailureReason::NotRunning },
            },
        }
    }

    #[test]
    fn status_output_is_stable_and_human_readable() {
        let mut output = Vec::new();
        write_status(&mut output, &snapshot()).expect("render status");
        assert_eq!(
            String::from_utf8(output).expect("UTF-8"),
            "Daemon: connected\nProtocol: 1\nState generation: 4\nDevices: 1\nPipeWire: available\nWirePlumber: unavailable (not running)\n"
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
    fn help_omits_unimplemented_setup_commands() {
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
        assert!(!String::from_utf8(output).expect("UTF-8").contains("setup"));
    }
}
