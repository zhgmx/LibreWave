# Setup, development installs, and removal

## Goals

Setup and removal are product features. They must be safe for a first source build and quick enough for repeated hardware development.

The lifecycle has one implementation in `librewavectl`. `xtask` builds the current checkout and invokes that implementation. It does not copy or remove installation files itself.

## Installed build identity

Each installation records:

- The source commit.
- Whether the source tree had uncommitted changes.
- A deterministic content fingerprint for a dirty build.
- The Rust profile and target triple.
- The installed CLI and daemon hashes.
- The installation time.
- The manifest schema version.

`librewavectl doctor --expect-current` compares this identity with the current checkout. A live test report can name the exact code that ran.

## Installation manifest and journal

The manifest is the source of truth for paths that LibreWave owns. It records:

- Versioned executable files and stable links.
- The inactive systemd user unit.
- The exact Wave:3 udev access rule.
- The active build directory.
- User files that setup replaced.
- Original file hashes and exact backup paths.

The persistent journal records setup or removal progress. Setup validates the complete manifest and journal before it changes an owned path. Recovery either rolls back a build before the active switch or finishes cleanup after the new manifest becomes active.

Removal validates every owned path before its first deletion. If one path was already changed, removal stops and leaves all paths in place. It also revalidates each path at its mutation boundary. If a path changes after removal starts, removal stops there; earlier owned removals can already be complete, and the journal keeps the operation resumable. LibreWave never removes an extra path because it is under a broad directory or matches a filename pattern.

## Source setup

Build both runnable binaries before setup. A test build does not guarantee that the plain executable files are current.

```text
cargo build --release -p librewaved -p librewavectl
./target/release/librewavectl setup --source-root . --profile release
```

The setup command shows its plan and asks for confirmation before its first mutation. It requests elevation only for the exact system udev operation.

Setup then:

1. Validates the exact daemon and CLI files that it will install.
2. Reads the current manifest and any interrupted journal.
3. Records exact backups for user files that it must replace.
4. Stages the complete installation in a versioned build directory.
5. Installs the inactive user unit and exact udev access rule.
6. Reloads the udev rules and sends a change event only to connected normal-mode Wave:3 devices.
7. Switches the active link atomically.
8. Verifies and stores the new manifest.
9. Removes superseded, unmodified LibreWave build files.
10. Leaves production audio ownership blocked.

A failure before the manifest switch restores the previous paths byte for byte. A failure after the switch leaves a journal so the next setup can finish exact cleanup.

Setup does not install or reload the WirePlumber card-disable rule. It does not enable the unit, start the daemon, open ALSA, or publish PipeWire objects. The production ALSA and PipeWire host does not exist yet, so setup cannot take ownership of the physical Wave card.

The udev refresh matches USB vendor `0fd9` and product `0070`. It does not trigger another USB product, a device interface, or firmware mode. If the privileged install or refresh operation fails, setup fails and does not claim that access is current. Rollback or journal recovery preserves or restores the prior rule state.

## Development loop

Use these commands during development:

```text
cargo xtask dev-install
cargo xtask dev-status
cargo xtask dev-uninstall
```

`dev-install` and `dev-status` first ask Cargo to build the runnable `librewavectl` and `librewaved` binaries. `xtask` reads the compiler artifact messages, hashes those exact files, and invokes the exact CLI path that Cargo returned. It does not search `PATH` or call an older installed CLI.

`dev-install` records the source revision and dirty fingerprint. `dev-status` compares the current source identity and exact binary hashes with the manifest. A mismatch is an error. `dev-uninstall` builds only the current CLI and uses it for cleanup. Broken daemon work cannot prevent removal.

## Doctor checks

`librewavectl doctor` reports:

- The manifest and interrupted journal.
- CLI and daemon file hashes.
- Stable link targets and the active build directory.
- The inactive unit and running daemon executable paths.
- The exact udev rule and any stale WirePlumber rule.
- Stale build directories, temporary files, and backups.
- The blocked production-audio prerequisite.
- Graph inspection as not implemented until a production graph exists.

Doctor also scans fixed LibreWave paths when the manifest is missing. It does not claim that an unavailable PipeWire check passed.

## Uninstall

`librewavectl uninstall` shows its plan and asks for confirmation. It then:

1. Validates the manifest and journal schema.
2. Checks all owned paths for local changes.
3. Stops before any deletion if one owned path was changed.
4. Restores replaced files from verified backups.
5. Removes the exact manifest-owned udev rule, reloads udev, and refreshes only connected normal-mode Wave:3 devices.
6. Removes executable links, the inactive unit, and exact build files.
7. Removes its exact backup and transaction files.
8. Verifies that no manifest-owned path, running daemon, active unit, stale policy, or build directory remains.

Normal uninstall preserves profiles and says where they remain. `librewavectl uninstall --purge` removes profiles only after explicit confirmation. The journal records this choice, so an interrupted removal cannot resume with different profile behavior.

Daemon-owned desired hardware state is stored at `librewave/profiles/device-state.json` below the selected configuration root. It uses serial-free USB topology keys and contains only controls explicitly managed through LibreWave. Normal uninstall preserves this file with the profile directory. `uninstall --purge` removes it as part of that exact directory.

Setup never changes Gain Lock. Uninstall and unmanage do not restore a saved Gain Lock value.

## Pre-release policy

Development installations support only the current manifest, configuration, and IPC formats. The lifecycle refuses an unknown manifest or journal schema. Remove an incompatible pre-release installation explicitly before the next setup.

Do not add compatibility readers, deprecated commands, dual writers, or fallback installation paths before the first public release.
