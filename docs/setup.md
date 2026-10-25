# Install, run, and remove LibreWave

LibreWave installs from a source checkout into your user account. `librewavectl` owns the complete lifecycle: `setup`, `start`, `stop`, `doctor`, and `uninstall`. The `xtask` development commands build the current checkout and call the same `librewavectl` operations.

## Requirements

- Linux with PipeWire and WirePlumber running in your desktop session.
- systemd user services, systemd-udev, and systemd-logind.
- An active local desktop session. An SSH connection alone does not get device access.
- `pkexec` with a polkit authentication agent, and the `getfacl` and `setfacl` commands. Setup uses these only for the device access rule.
- One Elgato Wave:3 connected over USB.
- The Rust toolchain from `rust-toolchain.toml`.

## Install the build packages

On Fedora:

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

These packages are only needed to build LibreWave. They do not install or configure it.

## Install LibreWave

Run these steps from the root of the checkout.

1. Build the CLI, the daemon, and the desktop application:

   ```text
   cargo build --release -p librewavectl -p librewaved -p librewave
   ```

2. Install them:

   ```text
   target/release/librewavectl setup --desktop --autostart --source-root . --profile release
   ```

3. Read the plan that setup prints, then confirm it. Setup asks for your password once to install the device access rule.

4. Open LibreWave from your application menu, or run `librewave`.

Opening LibreWave starts the mixer and then shows the window. Closing the window keeps the mixer running. To release the Wave:3 and restore your previous audio setup, select **Stop** in the title bar or run `librewavectl stop`.

Omit `--autostart` if LibreWave should not start when you log in. To change this later, run setup again with `--autostart` or `--no-autostart`.

To install only the CLI and the daemon, omit `--desktop`. Then start and stop audio with `librewavectl start` and `librewavectl stop`.

### What setup installs

| Path | Purpose |
| --- | --- |
| `~/.local/share/librewave/` | Versioned build directories and the active build link |
| `~/.local/bin/librewavectl`, `~/.local/bin/librewaved` | Links to the active build |
| `~/.config/systemd/user/librewaved.service` | The daemon unit. Setup leaves it disabled. |
| `/etc/udev/rules.d/70-librewave-wave3.rules` | Gives the logged-in user access to the Wave:3 |

With `--desktop`, setup also installs:

| Path | Purpose |
| --- | --- |
| `~/.local/bin/librewave` | Launcher that starts `librewave-desktop.service` |
| `~/.config/systemd/user/librewave-desktop.service` | Starts the mixer, then the window |
| `~/.local/share/applications/com.zhgmx.LibreWave.desktop` | Application menu entry |
| `~/.config/autostart/com.zhgmx.LibreWave.desktop` | Login entry. It is hidden unless login startup is on. |
| `~/.local/share/icons/hicolor/scalable/apps/com.zhgmx.LibreWave.svg` | Application icon |

Setup does not start the daemon, enable a unit, open ALSA, create PipeWire objects, or change hardware controls.

## Device access

The udev rule tags only USB vendor `0fd9`, product `0070` in normal mode with `uaccess`. systemd-logind then grants the user of the active local session access to the device. The grant follows the device through reconnects and reboots while that session is active. The rule stays installed when LibreWave closes.

Setup records the original numeric ACL of each connected Wave:3 node before it installs the rule. It refuses an ACL that `uaccess` would change for another user or widen for another group. Uninstall removes the rule and restores the recorded ACL when the exact node is still connected. It does not guess the permissions of a node that connected later.

If you manage device access yourself, use `setup --device-access existing`. This mode installs the same files without the udev rule and does not ask for your password. `start` still checks that the current user can open the device. Use the same access mode for every upgrade. To change the mode, uninstall first.

Run `setup --plan` to see the complete plan without making changes.

## Start and stop audio

`librewavectl start`:

1. Verifies the installed files and refuses extra or modified daemon processes.
2. Starts `librewaved.service` if it is not running.
3. Checks PipeWire, WirePlumber, and read-only device admission. This check does not restore saved hardware settings.
4. Saves your current default output and input.
5. Installs the WirePlumber rule `80-librewave-wave3.conf`, which hides the raw Wave:3 card, and restarts WirePlumber.
6. Waits for the mixer, then selects `librewave.system` and `librewave.microphone` as the defaults.

`librewavectl stop` releases the audio resources, waits until every LibreWave node is gone, and removes the WirePlumber rule. It restores each saved default only where LibreWave still owns it, so a default you changed yourself stays. A failed start runs the same restoration.

If another WirePlumber fragment creates competing Wave endpoints, start reports it. Name that file with `--suspend-config` and an absolute path:

```text
librewavectl start --suspend-config /absolute/path/to/wireplumber.conf.d/51-elgato.conf
```

LibreWave backs up that file, suspends it while audio runs, and restores it on stop. In the application, choose the file in **Settings > Audio > WirePlumber conflict**.

The daemon stores the audio intent in `~/.config/librewave/profiles/audio-state.json`. After a daemon, PipeWire, or WirePlumber restart, the daemon restores audio and reselects missing defaults. Explicit user choices are kept. Hotplug uses the same recovery path.

A runtime journal records every file that start changed. Setup and uninstall refuse to run while this record exists. If restoration fails, keep the journal and the backup and run `librewavectl stop` again. Do not delete the journal to bypass the check.

Start and stop never change Gain Lock, the hardware gain, the headphone level, or ALSA saved state.

## Check the installation

Run `librewavectl doctor`. It reports:

- The manifest and any interrupted transaction.
- The hashes of the installed files and the targets of the links.
- The unit state and the running daemon executables.
- The udev rule, the WirePlumber rule, and the audio ownership journal.
- Stale build directories, temporary files, and backups.
- Live product nodes, through the installed daemon, while audio runs.

`doctor --expect-current --desktop` also compares the installed build with the current checkout. A missing PipeWire observation is reported as blocked, never as passed. A changed file, a stale daemon, or an incomplete restoration is a failure.

## Uninstall

1. Run `librewavectl stop` if audio is running.
2. Run `librewavectl uninstall`.
3. Read the plan and confirm it.

Uninstall checks every owned path first and stops before the first deletion if one was changed. It restores replaced files from backups, removes the udev rule, restores the recorded USB ACLs, and removes the links, units, desktop files, and build directories. It then verifies that nothing owned remains.

Your profiles stay in `~/.config/librewave/profiles/`. To remove them too, run `librewavectl uninstall --purge` and confirm.

## Installation records

The manifest, `~/.local/state/librewave/install-manifest.json`, lists every path that LibreWave owns, with its hash, mode, and any backup of a file it replaced. LibreWave never removes a path that is not in the manifest. Setup and uninstall write a journal while they run. An interrupted setup rolls back or finishes on the next run, and an interrupted uninstall resumes.

Each installation records its build identity: the source commit, whether the tree had uncommitted changes, a content fingerprint for a dirty tree, the Rust profile and target, and the hashes of the installed executables.

LibreWave supports only the current manifest, profile, and IPC formats. The lifecycle refuses an unknown manifest or journal schema. Remove an incompatible installation with the CLI that installed it, then run setup again. Do not edit a manifest to bypass validation.

`librewaved --check-config` validates the profiles without starting a service, opening hardware, or changing files. `start` runs this check before it changes anything.

## Profiles

Profiles live in `~/.config/librewave/profiles/`:

- `mixer-state.json` holds the inputs, levels, output levels, and application assignments.
- `device-state.json` holds the hardware controls that LibreWave manages, keyed by USB topology. It never contains a device serial.
- `audio-state.json` holds the audio intent.

With no saved mixer profile, the daemon creates the inputs Microphone, System, Music, Browser, Voice Chat, and Game. It assigns installed applications to them from their desktop entry categories. Setup never changes an existing profile.

For an old schema-2 mixer profile, stop audio and the daemon, then run `librewavectl convert-profile`. It backs up the original file in the LibreWave state directory. `librewavectl convert-profile --rollback` restores that backup when the profile has not been edited since.

## Development installs

```text
cargo xtask dev-install
cargo xtask dev-status
cargo xtask dev-uninstall
```

`dev-install` builds the current checkout in debug mode, hashes the exact files that Cargo produced, and runs that CLI's `setup`. Add `--device-access existing` to skip the udev rule. `dev-status` compares the source identity and binary hashes with the manifest and fails on a mismatch. `dev-uninstall` builds only the CLI and uses it for removal.

`xtask` does not install the desktop application. If a desktop installation exists, use the release steps above.

## Limits

Recovery after sleep and after a logout and login is not verified. Long playback runs are not verified.
