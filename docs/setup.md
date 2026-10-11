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
- Original numeric USB ACLs and the exact connected node identities.
- The active build directory.
- User files that setup replaced.
- Original file hashes and exact backup paths.

The persistent journal records setup or removal progress. Setup validates the complete manifest and journal before it changes an owned path. Recovery either rolls back a build before the active switch or finishes cleanup after the new manifest becomes active.

Removal validates every owned path before its first deletion. If one path was already changed, removal stops and leaves all paths in place. It also revalidates each path at its mutation boundary. If a path changes after removal starts, removal stops there; earlier owned removals can already be complete, and the journal keeps the operation resumable. LibreWave never removes an extra path because it is under a broad directory or matches a filename pattern.

## Build prerequisites

Install the Rust toolchain from `rust-toolchain.toml` and the native development files before you run a Cargo or `xtask` build.

On Ubuntu 24.04, install the same packages that CI uses:

```text
sudo apt-get update
sudo apt-get install --no-install-recommends \
  acl \
  clang \
  fontconfig \
  libasound2-dev \
  libfontconfig1-dev \
  libpipewire-0.3-dev \
  libusb-1.0-0-dev \
  libx11-dev \
  libxcb1-dev \
  libxcb-render0-dev \
  libxcb-shape0-dev \
  libxcb-xfixes0-dev \
  libxkbcommon-dev \
  libxkbcommon-x11-dev \
  libwayland-dev \
  pkg-config
```

On Fedora, install these packages:

```text
sudo dnf install \
  acl \
  alsa-lib-devel \
  clang \
  fontconfig \
  fontconfig-devel \
  libusb1-devel \
  libX11-devel \
  libxcb-devel \
  libxkbcommon-devel \
  libxkbcommon-x11-devel \
  pipewire-devel \
  pkgconf-pkg-config \
  wayland-devel
```

These packages are prerequisites for builds from source. They do not install or configure LibreWave.

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
10. Leaves audio inactive.

A failure before the manifest switch restores the previous paths byte for byte. A failure after the switch leaves a journal so the next setup can finish exact cleanup.

Setup does not install or reload the WirePlumber card-disable rule. It does not enable the unit, start the daemon, open ALSA, or publish PipeWire objects. The separate experimental start operation composes the endpoint engine and physical workers. The new application lifecycle still needs controlled hardware and recovery tests. Setup alone cannot take ownership of the physical Wave card.

The udev refresh matches USB vendor `0fd9` and product `0070`. It does not trigger another USB product, a device interface, or firmware mode. If the privileged install or refresh operation fails, setup fails and does not claim that access is current. Rollback or journal recovery preserves or restores the prior rule state.

Setup records the original numeric ACL for each connected Wave:3 node before it installs the access rule. The manifest also records the boot ID, USB topology, node inode, and filesystem identity. It contains no device serial. An upgrade retains the original baseline. Setup refuses an ACL that `uaccess` would change for another user or widen for another group.

An upgrade reuses an unchanged owned access rule and verified grant. It does not install or refresh that rule again. A new installation or a missing owned rule still requires the privileged operation.

Removal of the udev rule alone can leave its ACL on a connected node. Uninstall therefore restores the recorded ACL after it removes and refreshes the rule. Restoration accepts only the exact recorded connection and an unchanged owned grant. It pins that node while it restores the original ACL and verifies the result. An interrupted restoration retains the manifest, journal, and binaries so uninstall can resume.

A changed ACL is preserved. A node connected after setup, after reconnect, or after a reboot has no original baseline unless the recorded node identity still matches. Uninstall refuses to guess its permissions. Keep the installation records and review that exact connection before cleanup. Reconnect and login permission recovery remain acceptance gates.

## Experimental application lifecycle

Prerequisites: install matching CLI and daemon binaries with the current manifest schema. Connect exactly one supported Wave:3. PipeWire and WirePlumber must run in the same user session. The device must already have reviewed USB access. A start operation cannot grant access. Approve any persistent access rule separately before you run setup.

Keep the installed unit disabled. Use the normal session socket for service control. Explicitly configured default names derived from USB identities cannot be saved because they can contain a device serial. Automatic defaults and stable virtual defaults are supported.

1. Run `librewavectl start --experimental`.
2. Run `librewavectl doctor`.
3. Run `librewavectl stop` when you want to release audio ownership.

If an existing user WirePlumber fragment creates competing Wave endpoints, select that exact file in the application or use an absolute path:

```text
librewavectl start --experimental --suspend-config /absolute/path/to/wireplumber.conf.d/51-elgato.conf
```

Start verifies the installed artifacts and refuses extra, stale, or modified daemon processes. It reloads the user units and starts `librewaved.service` when necessary. It checks host services and read-only device admission before it changes the audio policy.

The daemon saves the configured default selections. The lifecycle backs up the selected regular `.conf` file, including its mode, and suspends only that file. It creates the exact `80-librewave-wave3.conf` policy and restarts WirePlumber. It waits for ready audio before selecting `librewave.system` and `librewave.microphone`. Success requires both configured and effective defaults to select those endpoints.

The generated unit quotes executable paths and pins the installation's configuration root so the daemon uses the same profiles as the clients.

The runtime journal records every owned file and whether the daemon service was already active. Repeated starts reuse that record. A repeated start without `--suspend-config` uses the recorded file. Setup and uninstall refuse to run until the ownership record is cleared. One process lock serializes all lifecycle operations.

A failed start runs the same restoration path as stop. Stop cancels durable audio recovery, releases the audio resources, and waits for all LibreWave nodes to disappear. Cleanup failure retains the policy and backup. After successful cleanup, the daemon restores each configured default only while it still selects LibreWave. A later user selection is preserved. The lifecycle restores the exact fragment, removes its policy, restarts WirePlumber, and verifies the defaults. The daemon retains the baseline until this verification is complete. The lifecycle stops a service that it launched and removes its journal and backup.

The daemon records the confirmed restoration selections before the policy restart. If WirePlumber reloads an earlier LibreWave selection, the daemon restores that selection again. A later user choice remains in place.

If restoration fails, keep the journal and backup and retry `librewavectl stop`. A modified file is preserved. The error names the conflicting artifact or failed operation. Do not delete the journal to bypass that check. An interrupted final service stop resumes without starting a new daemon or restarting WirePlumber again.

The daemon stores active audio intent in `librewave/profiles/audio-state.json` below the configuration root. A daemon restart reloads that intent. Hotplug and host-service recovery use the existing capture-first supervisor. After graph recovery, the daemon reselects missing defaults and preserves explicit user choices. Stop saves disabled audio intent before cleanup. An interrupted or failed persistence operation reports an error instead of claiming that stop completed.

These commands do not enable login startup, change Gain Lock, or change an ALSA saved state. Closing the application window keeps the daemon running.

## Controlled live validation

The new lifecycle has isolated regression tests. Native GUI behavior and real-device recovery remain unverified for this code.

Before a live run, review `setup --plan`, the selected fragment, the current installation manifest, and the saved hardware profile. Record the current configured and effective defaults, fragment bytes and mode, unit state, daemon path, product nodes, and hardware gain without recording a serial. Confirm the exact access grant separately. Existing read-only USB access is sufficient for takeover; the plan includes no hardware control write.

1. Install the reviewed build after approving `/etc/udev/rules.d/70-librewave-wave3.rules` and its exact normal-mode Wave:3 refresh.
2. Run `librewavectl start --experimental --suspend-config <absolute-fragment-path>`.
3. Verify application audio, software microphone monitoring, independent mixes, and the four deliberate desktop endpoints.
4. Run `librewavectl start --experimental` again.
5. Verify that one daemon and one audio graph remain.
6. Change one software endpoint volume.
7. Verify that hardware gain is unchanged.
8. Run `systemctl --user restart librewaved.service`.
9. Verify that accepted audio intent recovers.
10. Run `systemctl --user restart pipewire.service`.
11. Verify audio recovery.
12. Run `systemctl --user restart wireplumber.service`.
13. Verify audio recovery.
14. Test reconnect only after approval for the physical action and the exact temporary access grant.
15. Run `librewavectl stop`.
16. Verify exact fragment restoration, preserved user default changes, original service state, and zero LibreWave graph nodes.
17. Run `librewavectl uninstall`.
18. Verify every manifest-owned path, service, access rule, process, and graph object.

Stop immediately on unexpected hardware changes or a cleanup failure. Preserve the ownership files and use the normal stop path for recovery. Do not retire the existing workaround until this sequence passes. Sleep and login recovery require separate acceptance runs before production support.

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
- Unit state and running daemon executable paths.
- The exact udev rule, temporary WirePlumber policy, and audio ownership journal.
- Stale build directories, temporary files, and backups.
- The remaining production validation gate.
- Live product nodes and audio readiness through the installed daemon while ownership is recorded.

An unavailable graph observation is a blocked check. Doctor does not report it as a successful cleanup. A changed policy, backup, stale daemon, or incomplete restoration is a failed check.

Doctor also scans fixed LibreWave paths when the manifest is missing. It does not claim that an unavailable PipeWire check passed.

## Uninstall

`librewavectl uninstall` shows its plan and asks for confirmation. It then:

1. Validates the manifest and journal schema.
2. Checks all owned paths for local changes.
3. Stops before any deletion if one owned path was changed.
4. Restores replaced files from verified backups.
5. Removes the exact manifest-owned udev rule, reloads udev, and refreshes only connected normal-mode Wave:3 devices.
6. Restores and verifies the original USB ACLs for the recorded connected nodes.
7. Removes executable links, the inactive unit, and exact build files.
8. Removes its exact backup and transaction files.
9. Verifies that no manifest-owned path, running daemon, active unit, stale policy, or build directory remains.

Normal uninstall preserves profiles and says where they remain. `librewavectl uninstall --purge` removes profiles only after explicit confirmation. The journal records this choice, so an interrupted removal cannot resume with different profile behavior.

Daemon-owned desired hardware state is stored at `librewave/profiles/device-state.json` below the selected configuration root. It uses serial-free USB topology keys and contains only controls explicitly managed through LibreWave. Normal uninstall preserves this file with the profile directory. `uninstall --purge` removes it as part of that exact directory.

Setup never changes Gain Lock. Uninstall and unmanage do not restore a saved Gain Lock value.

## Pre-release policy

Development installations support only the current manifest, configuration, and IPC formats. The lifecycle refuses an unknown manifest or journal schema. Remove an incompatible pre-release installation explicitly before the next setup.

Do not add compatibility readers, deprecated commands, dual writers, or fallback installation paths before the first public release.
