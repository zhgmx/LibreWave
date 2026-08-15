use std::env;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<i32> {
    let mut arguments = env::args_os();
    let _program = arguments.next();
    let command = arguments.next().and_then(|value| value.into_string().ok());
    if arguments.next().is_some() {
        return Err(invalid("development commands do not accept extra arguments"));
    }
    let lifecycle_command = match command.as_deref() {
        Some("dev-install") => "setup",
        Some("dev-status") => "doctor",
        Some("dev-uninstall") => "uninstall",
        None | Some("help" | "--help" | "-h") => {
            print_help();
            return Ok(0);
        }
        Some(unknown) => return Err(invalid(format!("unknown command `{unknown}`"))),
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| invalid("xtask has no workspace parent"))?
        .to_path_buf();
    let target = rustc_host()?;
    let needs_daemon = lifecycle_command != "uninstall";
    let (cli, daemon) = build_runnable_binaries(&root, &target, needs_daemon)?;
    require_file(&cli)?;
    if let Some(daemon) = daemon.as_ref() {
        require_file(daemon)?;
    }
    let mut command = Command::new(&cli);
    command.arg(lifecycle_command);
    match lifecycle_command {
        "setup" => {
            let daemon = daemon
                .as_ref()
                .ok_or_else(|| invalid("development install requires the daemon artifact"))?;
            command
                .args(["--source-root", root.to_string_lossy().as_ref()])
                .args(["--profile", "debug"])
                .args(["--target", &target])
                .arg("--daemon-path")
                .arg(daemon)
                .arg("--yes");
        }
        "doctor" => {
            let daemon = daemon
                .as_ref()
                .ok_or_else(|| invalid("development status requires the daemon artifact"))?;
            command
                .args(["--source-root", root.to_string_lossy().as_ref()])
                .args(["--profile", "debug"])
                .args(["--target", &target])
                .arg("--daemon-path")
                .arg(daemon)
                .arg("--expect-current");
        }
        "uninstall" => {
            command.arg("--yes");
        }
        _ => unreachable!(),
    }
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

fn build_runnable_binaries(
    root: &Path,
    target: &str,
    needs_daemon: bool,
) -> io::Result<(PathBuf, Option<PathBuf>)> {
    let mut command = Command::new(cargo());
    command.args([
        "build",
        "--message-format=json-render-diagnostics",
        "--target",
        target,
        "-p",
        "librewavectl",
    ]);
    if needs_daemon {
        command.args(["-p", "librewaved"]);
    }
    let output = command.current_dir(root).output()?;
    if !output.status.success() {
        eprint!("{}", rendered_diagnostics(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        return Err(io::Error::other(build_failure_message(needs_daemon)));
    }
    let (cli, daemon) = parse_artifacts(&output.stdout, needs_daemon)?;
    Ok((absolute_artifact(root, cli), daemon.map(|path| absolute_artifact(root, path))))
}

fn rendered_diagnostics(messages: &[u8]) -> String {
    messages
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter_map(|value| {
            value
                .pointer("/message/rendered")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

fn build_failure_message(needs_daemon: bool) -> &'static str {
    if needs_daemon {
        "cargo did not build the current runnable CLI and daemon binaries"
    } else {
        "cargo did not build the current runnable CLI binary"
    }
}

fn absolute_artifact(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() { path } else { root.join(path) }
}

fn parse_artifacts(messages: &[u8], needs_daemon: bool) -> io::Result<(PathBuf, Option<PathBuf>)> {
    let mut artifacts = std::collections::BTreeMap::new();
    for line in messages.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
        let value: serde_json::Value = serde_json::from_slice(line).map_err(io::Error::other)?;
        if value.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        let Some(name) = value.pointer("/target/name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !matches!(name, "librewavectl" | "librewaved") {
            continue;
        }
        if let Some(executable) = value.get("executable").and_then(serde_json::Value::as_str) {
            artifacts.insert(name.to_owned(), PathBuf::from(executable));
        }
    }
    let cli = artifacts
        .remove("librewavectl")
        .ok_or_else(|| invalid("Cargo did not report the librewavectl executable artifact"))?;
    let daemon = artifacts.remove("librewaved");
    if needs_daemon && daemon.is_none() {
        return Err(invalid("Cargo did not report the librewaved executable artifact"));
    }
    Ok((cli, daemon))
}

fn cargo() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

fn require_file(path: &Path) -> io::Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("cargo did not produce {}", path.display()),
        ))
    }
}

fn rustc_host() -> io::Result<String> {
    let output = Command::new("rustc").arg("-vV").output()?;
    if !output.status.success() {
        return Err(io::Error::other("rustc -vV failed"));
    }
    let text = String::from_utf8(output.stdout).map_err(|error| invalid(error.to_string()))?;
    text.lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| invalid("rustc did not report a host triple"))
}

fn print_help() {
    println!("Usage: cargo xtask <command>");
    println!();
    println!("Commands:");
    println!("  dev-install    Build and install this checkout");
    println!("  dev-status     Build and compare this checkout with the installed build");
    println!("  dev-uninstall  Build and remove the development installation");
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_artifact_paths_are_used_exactly() {
        let messages = br#"{"reason":"compiler-artifact","target":{"name":"librewavectl"},"executable":"/custom/target/host/debug/librewavectl"}
{"reason":"compiler-artifact","target":{"name":"librewaved"},"executable":"../relative-target/debug/librewaved"}
{"reason":"build-finished","success":true}
"#;
        let (cli, daemon) = parse_artifacts(messages, true).expect("artifact messages must parse");
        assert_eq!(cli, PathBuf::from("/custom/target/host/debug/librewavectl"));
        assert_eq!(daemon, Some(PathBuf::from("../relative-target/debug/librewaved")));
    }

    #[test]
    fn missing_executable_artifact_is_an_error() {
        let messages = br#"{"reason":"compiler-artifact","target":{"name":"librewavectl"},"executable":"/tmp/librewavectl"}
"#;
        assert_eq!(
            parse_artifacts(messages, true).expect_err("missing daemon artifact must fail").kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn uninstall_build_accepts_only_the_current_cli_artifact() {
        let messages = br#"{"reason":"compiler-artifact","target":{"name":"librewavectl"},"executable":"/tmp/librewavectl"}
"#;
        let (cli, daemon) =
            parse_artifacts(messages, false).expect("CLI-only artifact set must parse");
        assert_eq!(cli, PathBuf::from("/tmp/librewavectl"));
        assert_eq!(daemon, None);
    }

    #[test]
    fn rendered_compiler_diagnostics_are_preserved() {
        let messages =
            br#"{"reason":"compiler-message","message":{"rendered":"error: broken daemon\n"}}
{"reason":"build-finished","success":false}
"#;
        assert_eq!(rendered_diagnostics(messages), "error: broken daemon\n");
    }

    #[test]
    fn build_failure_names_only_required_artifacts() {
        assert_eq!(
            build_failure_message(true),
            "cargo did not build the current runnable CLI and daemon binaries"
        );
        assert_eq!(
            build_failure_message(false),
            "cargo did not build the current runnable CLI binary"
        );
    }
}
