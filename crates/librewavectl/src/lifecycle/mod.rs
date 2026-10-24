#[cfg(test)]
mod access_tests;
mod artifacts;
mod desktop;
mod diagnostics;
mod engine;
mod fs_ops;
mod hash;
mod identity;
mod installation_conversion;
mod model;
mod profile_conversion;
mod runtime;
mod system;
mod transaction;
mod validation;

use engine::{CheckState, InstallPaths, InstallRequest, Lifecycle, UninstallOutcome};
use identity::IdentityRequest;
use model::DeviceAccess;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use system::{ProductionProcessInspector, ProductionUdev, ProductionUnitInspector};

pub fn is_command(arguments: &[String]) -> bool {
    matches!(
        arguments.get(1).map(String::as_str),
        Some(
            "setup"
                | "doctor"
                | "uninstall"
                | "start"
                | "stop"
                | "convert-profile"
                | "convert-installation"
        )
    )
}

pub fn run<O: Write, E: Write>(
    arguments: &[String],
    output: &mut O,
    errors: &mut E,
) -> io::Result<i32> {
    let command = arguments.get(1).map(String::as_str);
    if matches!(command, Some("start" | "stop")) {
        return runtime::run(arguments, output);
    }
    if matches!(arguments.get(2).map(String::as_str), Some("--help" | "-h")) && arguments.len() == 3
    {
        let kind = match command {
            Some("convert-profile") => {
                writeln!(
                    output,
                    "Usage: librewavectl convert-profile [--rollback]\nStop the daemon and mixer first. Convert schema 2 to 3 or restore the exact retained backup."
                )?;
                return Ok(0);
            }
            Some("setup") => CommandKind::Setup,
            Some("doctor") => CommandKind::Doctor,
            Some("uninstall") => CommandKind::Uninstall,
            _ => return Ok(2),
        };
        write_command_help(output, kind)?;
        return Ok(0);
    }
    let paths = InstallPaths::from_environment()?;
    let udev = ProductionUdev::new(paths.udev_rule.clone());
    let mut lifecycle =
        Lifecycle::new(paths, udev, ProductionUnitInspector, ProductionProcessInspector);
    if command == Some("convert-installation") {
        return installation_conversion::run(arguments, &lifecycle, output);
    }
    if command == Some("convert-profile") {
        return profile_conversion::run(arguments, &lifecycle, output);
    }
    match command {
        Some("setup") => {
            let options = Options::parse(&arguments[2..], CommandKind::Setup)?;
            run_setup(&options, &mut lifecycle, output)
        }
        Some("doctor") => {
            let options = Options::parse(&arguments[2..], CommandKind::Doctor)?;
            let expected = if options.expect_current {
                let binaries = current_binaries(&options)?;
                Some(current_identity(&options, &binaries)?)
            } else {
                None
            };
            let mut checks = lifecycle.doctor(expected.as_ref());
            let live = runtime::live_checks(&lifecycle.paths);
            if !live.is_empty() {
                checks.retain(|check| check.name != "PipeWire graph objects");
                checks.extend(live);
            }
            let failed = checks.iter().any(|check| check.state == CheckState::Fail);
            for check in checks {
                writeln!(output, "{} {}: {}", state_label(check.state), check.name, check.detail)?;
            }
            Ok(i32::from(failed))
        }
        Some("uninstall") => {
            let options = Options::parse(&arguments[2..], CommandKind::Uninstall)?;
            writeln!(
                output,
                "LibreWave will remove only paths that still match its installation manifest."
            )?;
            if options.purge {
                writeln!(output, "Your LibreWave profiles will also be removed.")?;
            } else {
                writeln!(output, "Your LibreWave profiles will remain in place.")?;
            }
            if !options.yes && !confirm("Continue with uninstall? [y/N] ")? {
                writeln!(output, "Uninstall canceled.")?;
                return Ok(0);
            }
            match lifecycle.uninstall(options.purge)? {
                UninstallOutcome::Removed => {
                    writeln!(
                        output,
                        "LibreWave was removed. Profiles were {}.",
                        if options.purge { "removed" } else { "preserved" }
                    )?;
                    Ok(0)
                }
                UninstallOutcome::NotInstalled => {
                    writeln!(output, "LibreWave is not installed. No files were changed.")?;
                    Ok(0)
                }
                UninstallOutcome::Modified(modified) => {
                    writeln!(errors, "Uninstall stopped because these owned paths were changed:")?;
                    for path in modified {
                        writeln!(errors, "  {}", path.display())?;
                    }
                    writeln!(errors, "LibreWave left the changed paths and manifest in place.")?;
                    Ok(1)
                }
            }
        }
        _ => Ok(2),
    }
}

fn run_setup(
    options: &Options,
    lifecycle: &mut Lifecycle<ProductionUdev, ProductionUnitInspector, ProductionProcessInspector>,
    output: &mut impl Write,
) -> io::Result<i32> {
    let desktop_config = options
        .desktop
        .clone()
        .map(|config| {
            Ok::<_, io::Error>(desktop::resolve_config(
                config,
                lifecycle.read_manifest()?.and_then(|m| m.desktop),
                options.login_startup,
            ))
        })
        .transpose()?;
    write_setup_plan(output, &lifecycle.paths, options.device_access)?;
    if let Some(config) = &desktop_config {
        for (path, _, _) in desktop::files(&lifecycle.paths, config)? {
            writeln!(output, "Desktop artifact: {}", path.display())?;
        }
        writeln!(
            output,
            "Login startup: {}. Launching the desktop starts the experimental mixer.",
            config.autostart
        )?;
    }
    if options.setup_mode == SetupMode::Plan {
        return Ok(0);
    }
    let binaries = current_binaries(options)?;
    let identity = current_identity(options, &binaries)?;
    writeln!(output, "LibreWave will install the current CLI, daemon, and inactive user unit.")?;
    writeln!(
        output,
        "Setup leaves audio inactive. Use `start --experimental` after access and live-test approval."
    )?;
    if !options.yes && !confirm("Continue with setup? [y/N] ")? {
        writeln!(output, "Setup canceled.")?;
        return Ok(0);
    }
    let desktop = desktop_config.map(|config| engine::DesktopInstall {
        gui_source: binaries.gui.as_deref().expect("desktop binary resolved"),
        config,
    });
    let manifest = lifecycle.setup(&InstallRequest {
        device_access: options.device_access,
        identity,
        cli_source: &binaries.cli,
        daemon_source: &binaries.daemon,
        desktop,
    })?;
    writeln!(output, "Installed build {}.", manifest.identity.stable_key())?;
    writeln!(output, "Run `librewavectl doctor` to inspect the installation.")?;
    Ok(0)
}

fn write_setup_plan(
    output: &mut impl Write,
    paths: &InstallPaths,
    access: DeviceAccess,
) -> io::Result<()> {
    writeln!(output, "Setup plan (no changes applied):")?;
    writeln!(output, "  Executable links: {}", paths.bin_root.display())?;
    writeln!(output, "  Inactive user unit: {}", paths.unit().display())?;
    if access == DeviceAccess::Existing {
        writeln!(
            output,
            "  Use existing device access. Do not install a udev rule or change USB permissions."
        )?;
        writeln!(
            output,
            "  Start checks current access. A temporary grant ends when its node is replaced."
        )?;
    } else {
        writeln!(output, "  Privileged access rule: {}", paths.udev_rule.display())?;
        write!(
            output,
            "  {}",
            librewave_platform_linux::audio_policy::Wave3AudioPolicy::new().render_udev()
        )?;
        writeln!(
            output,
            "  Reload udev rules and refresh only USB devices with vendor 0fd9 and product 0070."
        )?;
    }
    writeln!(output, "  Do not start the daemon or change WirePlumber configuration.")
}

fn write_command_help(output: &mut impl Write, command: CommandKind) -> io::Result<()> {
    match command {
        CommandKind::Setup => {
            writeln!(output, "Usage: librewavectl setup [OPTIONS]")?;
            writeln!(
                output,
                "  --desktop                 Install the GUI, launcher, and desktop service"
            )?;
            writeln!(output, "  --gui-path <PATH>         GUI executable (implies --desktop)")?;
            writeln!(output, "  --autostart / --no-autostart  Enable or disable login startup")?;
            writeln!(
                output,
                "  --suspend-config <PATH>   Reviewed legacy loader to suspend at mixer startup"
            )?;
            writeln!(output)?;
            writeln!(output, "Install the current CLI and daemon. Leave audio inactive.")?;
            writeln!(output)?;
            writeln!(output, "Options:")?;
            writeln!(
                output,
                "  --plan                     Show the exact access and installation plan without changes"
            )?;
            writeln!(
                output,
                "  --source-root <PATH>       Source checkout used for build identity"
            )?;
            writeln!(output, "  --profile <debug|release> Rust build profile")?;
            writeln!(
                output,
                "  --device-access <persistent|existing> Access mode (default: persistent)"
            )?;
            writeln!(output, "  --target <TRIPLE>          Rust target triple")?;
            writeln!(output, "  --daemon-path <PATH>       Exact daemon executable to install")?;
            writeln!(output, "  --yes                      Skip the confirmation prompt")?;
        }
        CommandKind::Doctor => {
            writeln!(output, "Usage: librewavectl doctor [OPTIONS]")?;
            writeln!(output)?;
            writeln!(output, "Check the installation and report unsafe or stale state.")?;
            writeln!(output)?;
            writeln!(output, "Options:")?;
            writeln!(output, "  --expect-current           Compare with the current source build")?;
            writeln!(output, "  Build comparison options require --expect-current.")?;
            writeln!(output, "  --source-root <PATH>       Source checkout used for comparison")?;
            writeln!(output, "  --profile <debug|release> Rust build profile")?;
            writeln!(output, "  --target <TRIPLE>          Rust target triple")?;
            writeln!(output, "  --daemon-path <PATH>       Exact daemon executable to compare")?;
            writeln!(output, "  --desktop --gui-path <PATH> Compare the GUI build too")?;
        }
        CommandKind::Uninstall => {
            writeln!(output, "Usage: librewavectl uninstall [OPTIONS]")?;
            writeln!(output)?;
            writeln!(output, "Remove files that still match the installation manifest.")?;
            writeln!(output)?;
            writeln!(output, "Options:")?;
            writeln!(output, "  --purge  Remove LibreWave profiles")?;
            writeln!(output, "  --yes    Skip the confirmation prompt")?;
        }
    }
    writeln!(output, "  -h, --help                 Show this help")
}

fn state_label(state: CheckState) -> &'static str {
    match state {
        CheckState::Pass => "PASS",
        CheckState::Fail => "FAIL",
        CheckState::Blocked => "BLOCKED",
    }
}

fn confirm(prompt: &str) -> io::Result<bool> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SetupMode {
    Apply,
    Plan,
}

struct Options {
    setup_mode: SetupMode,
    device_access: DeviceAccess,
    yes: bool,
    purge: bool,
    expect_current: bool,
    source_root: Option<PathBuf>,
    profile: Option<String>,
    target: Option<String>,
    daemon_path: Option<PathBuf>,
    gui_path: Option<PathBuf>,
    desktop: Option<model::DesktopConfig>,
    login_startup: Option<bool>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CommandKind {
    Setup,
    Doctor,
    Uninstall,
}

impl Options {
    fn parse_desktop(
        &mut self,
        arguments: &[String],
        index: &mut usize,
        command: CommandKind,
    ) -> io::Result<bool> {
        match arguments[*index].as_str() {
            "--desktop" if matches!(command, CommandKind::Setup | CommandKind::Doctor) => {
                self.desktop
                    .get_or_insert(model::DesktopConfig { autostart: false, suspend_config: None });
            }
            "--autostart" | "--no-autostart" if command == CommandKind::Setup => {
                self.login_startup = Some(arguments[*index] == "--autostart");
                self.desktop
                    .get_or_insert(model::DesktopConfig { autostart: false, suspend_config: None })
                    .autostart = self.login_startup == Some(true);
            }
            "--suspend-config" if command == CommandKind::Setup => {
                *index += 1;
                let path = PathBuf::from(required_value(arguments, *index, "--suspend-config")?);
                self.desktop
                    .get_or_insert(model::DesktopConfig { autostart: false, suspend_config: None })
                    .suspend_config = Some(path);
            }
            "--gui-path" if matches!(command, CommandKind::Setup | CommandKind::Doctor) => {
                *index += 1;
                self.gui_path = Some(required_value(arguments, *index, "--gui-path")?.into());
                self.desktop
                    .get_or_insert(model::DesktopConfig { autostart: false, suspend_config: None });
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn parse(arguments: &[String], command: CommandKind) -> io::Result<Self> {
        let mut options = Self {
            setup_mode: SetupMode::Apply,
            device_access: DeviceAccess::Persistent,
            yes: false,
            purge: false,
            expect_current: false,
            source_root: None,
            profile: None,
            target: None,
            daemon_path: None,
            gui_path: None,
            desktop: None,
            login_startup: None,
        };
        let mut index = 0;
        while index < arguments.len() {
            if options.parse_desktop(arguments, &mut index, command)? {
                index += 1;
                continue;
            }
            match arguments[index].as_str() {
                "--plan" if command == CommandKind::Setup => options.setup_mode = SetupMode::Plan,
                "--device-access" if command == CommandKind::Setup => {
                    index += 1;
                    options.device_access =
                        match required_value(arguments, index, "--device-access")? {
                            "persistent" => DeviceAccess::Persistent,
                            "existing" => DeviceAccess::Existing,
                            _ => {
                                return Err(invalid(
                                    "--device-access must be `persistent` or `existing`",
                                ));
                            }
                        };
                }
                "--yes" if matches!(command, CommandKind::Setup | CommandKind::Uninstall) => {
                    options.yes = true;
                }
                "--purge" if command == CommandKind::Uninstall => options.purge = true,
                "--expect-current" if command == CommandKind::Doctor => {
                    options.expect_current = true;
                }
                "--source-root" if matches!(command, CommandKind::Setup | CommandKind::Doctor) => {
                    index += 1;
                    options.source_root =
                        Some(required_value(arguments, index, "--source-root")?.into());
                }
                "--profile" if matches!(command, CommandKind::Setup | CommandKind::Doctor) => {
                    index += 1;
                    let profile = required_value(arguments, index, "--profile")?.to_owned();
                    if !matches!(profile.as_str(), "debug" | "release") {
                        return Err(invalid("--profile must be `debug` or `release`"));
                    }
                    options.profile = Some(profile);
                }
                "--target" if matches!(command, CommandKind::Setup | CommandKind::Doctor) => {
                    index += 1;
                    options.target = Some(required_value(arguments, index, "--target")?.to_owned());
                }
                "--daemon-path" if matches!(command, CommandKind::Setup | CommandKind::Doctor) => {
                    index += 1;
                    options.daemon_path =
                        Some(required_value(arguments, index, "--daemon-path")?.into());
                }
                unknown => return Err(invalid(format!("unknown lifecycle option `{unknown}`"))),
            }
            index += 1;
        }
        let has_context = options.source_root.is_some()
            || options.profile.is_some()
            || options.target.is_some()
            || options.daemon_path.is_some()
            || options.desktop.is_some();
        if command == CommandKind::Doctor && has_context && !options.expect_current {
            return Err(invalid("build context options require `doctor --expect-current`"));
        }
        if options.setup_mode == SetupMode::Plan && options.yes {
            return Err(invalid("--plan cannot be combined with --yes"));
        }
        if (command == CommandKind::Setup && options.setup_mode == SetupMode::Apply)
            || options.expect_current
        {
            if options.source_root.is_none() {
                options.source_root = Some(std::env::current_dir()?);
            }
            if options.profile.is_none() {
                options.profile = Some(infer_profile()?);
            }
            if options.target.is_none() {
                options.target = Some(rustc_host()?);
            }
        }
        Ok(options)
    }
}

fn required_value<'a>(arguments: &'a [String], index: usize, option: &str) -> io::Result<&'a str> {
    arguments
        .get(index)
        .map(String::as_str)
        .ok_or_else(|| invalid(format!("{option} requires a value")))
}

fn current_identity(options: &Options, binaries: &Binaries) -> io::Result<model::BuildIdentity> {
    identity::discover(&IdentityRequest {
        source_root: options
            .source_root
            .as_deref()
            .ok_or_else(|| invalid("source root is unavailable"))?,
        profile: options.profile.as_deref().ok_or_else(|| invalid("profile is unavailable"))?,
        target: options.target.as_deref().ok_or_else(|| invalid("target is unavailable"))?,
        cli_path: &binaries.cli,
        daemon_path: &binaries.daemon,
        gui_path: binaries.gui.as_deref(),
    })
}

struct Binaries {
    cli: PathBuf,
    daemon: PathBuf,
    gui: Option<PathBuf>,
}

fn current_binaries(options: &Options) -> io::Result<Binaries> {
    let cli = std::env::current_exe()?;
    let daemon = match options.daemon_path.as_deref() {
        Some(path) => path.to_path_buf(),
        None => cli
            .parent()
            .ok_or_else(|| invalid("the current CLI path has no parent directory"))?
            .join("librewaved"),
    };
    if !daemon.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "the matching daemon does not exist at {}; build both runnable binaries first",
                daemon.display()
            ),
        ));
    }
    let gui = if options.desktop.is_some() {
        Some(
            options
                .gui_path
                .clone()
                .unwrap_or_else(|| cli.parent().expect("CLI parent").join("librewave")),
        )
    } else {
        None
    };
    if let Some(path) = &gui
        && !path.is_file()
    {
        return Err(invalid(format!("GUI binary missing at {}", path.display())));
    }
    Ok(Binaries { cli, daemon, gui })
}

fn infer_profile() -> io::Result<String> {
    let executable = std::env::current_exe()?;
    let parent = executable.parent().and_then(Path::file_name).and_then(|name| name.to_str());
    Ok(if parent == Some("release") { "release" } else { "debug" }.to_owned())
}

fn rustc_host() -> io::Result<String> {
    let output = std::process::Command::new("rustc").arg("-vV").output()?;
    if !output.status.success() {
        return Err(io::Error::other("rustc -vV failed"));
    }
    let text = String::from_utf8(output.stdout).map_err(|error| invalid(error.to_string()))?;
    text.lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| invalid("rustc did not report a host triple"))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests;
