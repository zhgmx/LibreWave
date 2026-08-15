# Setup, development installs, and removal

Status: accepted lifecycle contract. Commands will become available with the daemon and CLI milestones.

## Goals

Setup and removal are product features. They must be safe for a first-time source user and fast enough for repeated hardware development.

The lifecycle has one implementation in `librewavectl`. `xtask` can build the current checkout and invoke that implementation, but it must not maintain its own file-copy or cleanup logic.

## Installed build identity

Each installation records:

- The source commit.
- Whether the source tree had uncommitted changes.
- A content fingerprint for a dirty development build.
- The Rust profile and target triple.
- The installed executable hashes.
- The installation time.
- The manifest schema version.

`librewavectl status` and `librewavectl doctor` show this identity. A live test report can therefore name the exact code that ran.

## Installation manifest

The manifest is the source of truth for files LibreWave owns. It records:

- Installed executables and links.
- The systemd user unit.
- The WirePlumber rule.
- The udev rule.
- LibreWave state directories.
- User files that setup replaced.
- Backup paths and original hashes.
- The active build directory.

Every installed file includes an ownership marker or expected hash where the file format permits it. Removal does not rely on a broad filename pattern.

## Source setup

The source workflow will be:

```text
cargo build --release --workspace
cargo run --release -p librewavectl -- setup
```

The setup command shows the files and services it will change. It asks for confirmation before the first mutation and requests elevation only for the narrow system udev operation.

Setup then:

1. Validates the built daemon, CLI, optional UI, and policy assets.
2. Reads any existing LibreWave manifest.
3. Backs up user-owned files that it must replace.
4. Stages the complete installation in a new build directory.
5. Writes and validates the new manifest.
6. Stops the old LibreWave daemon if one is running.
7. Switches executable links and service paths to the new build.
8. Reloads udev, systemd user units, and WirePlumber as required.
9. Starts the new daemon.
10. Runs the same checks as `librewavectl doctor`.
11. Removes superseded LibreWave-owned build directories.

A failure before the switch leaves the old installation active. A failure after the switch attempts one rollback to the prior manifest and reports any remaining manual action.

## Development loop

The intended development commands are:

```text
cargo xtask dev-install
cargo xtask dev-status
cargo xtask dev-uninstall
```

`dev-install` builds the current checkout, includes dirty-source identity, and invokes that newly built `librewavectl setup` in development mode. It never calls an older installed CLI by accident.

`dev-status` compares the current checkout identity with the installed manifest and running daemon. A mismatch is an error, not a warning.

`dev-uninstall` builds or locates the current lifecycle client, invokes `librewavectl uninstall`, and verifies cleanup. It remains useful when the daemon does not start.

## Doctor checks

`librewavectl doctor` checks:

- The resolved paths and hashes of `librewavectl`, `librewaved`, and the optional UI.
- The systemd user unit contents and active process executable.
- Duplicate or stale LibreWave processes.
- Installed udev and WirePlumber rule paths.
- The active installation manifest and backups.
- PipeWire and WirePlumber connectivity.
- User-visible and internal LibreWave graph objects.
- The admitted Wave device and protocol read-only status.

The command prints a direct repair action for each failed check. `librewavectl doctor --repair` can perform safe, manifest-scoped repairs after confirmation.

## Uninstall

`librewavectl uninstall` shows the removal plan before changing the system. It then:

1. Stops and disables the user service.
2. Tells the daemon to release the physical and virtual audio graph when possible.
3. Restores saved hardware policy, such as the original gain-lock value, only when the connected device and schema are exact.
4. Restores user-owned WirePlumber files from verified backups.
5. Removes manifest-owned WirePlumber and udev rules.
6. Reloads affected services.
7. Removes executable links, units, state, and build directories recorded in the manifest.
8. Checks for stale files, processes, units, rules, and PipeWire nodes.
9. Reports whether the original configuration was fully restored.

The default uninstall preserves user profiles only after it tells the user where they remain. A separate `--purge` option can remove them after confirmation.

## Pre-release policy

Development installations support only the current manifest, configuration, and IPC formats. When one changes, the development installer can back up and reset the old test state instead of migrating it.

Do not add compatibility shims, deprecated commands, dual readers, dual writers, or fallback installation paths before the first public release. The only supported development state is the state produced by the current checkout.
