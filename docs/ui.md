# GPUI application

The optional `librewave` application uses the same local IPC interface as
`librewavectl`. It shows the daemon's Wave:3 state and sends hardware control
requests through `librewaved`. The application does not open the USB device.

## Prerequisites

Install the Rust toolchain and native build packages in [Setup](setup.md).
For lifecycle controls, install matching CLI and daemon binaries with `librewavectl setup`. Setup includes an exact udev access operation that requires separate approval. Use **View setup plan** to see the paths and access rule without changes. Audio startup still requires reviewed device access and a running PipeWire and WirePlumber session.

## Start the application

1. Run `cargo run -p librewave`.
2. Select **Start experimental audio**.

The start action launches the installed daemon service and waits for ready audio and selected desktop defaults. If an existing WirePlumber fragment conflicts with ownership, use **Choose fragment…** to select the exact `.conf` file. The lifecycle backs it up and restores it on failure or stop. See [Setup](setup.md) for the full contract.

Select **Stop and restore** to release audio and restore desktop ownership. Closing the window keeps audio running. Lifecycle operations run on the background executor. The application prevents overlapping actions and preserves a restoration error while status polling continues. A deliberate stop clears the active snapshot and suppresses the expected disconnect error.

For device and mixer development without an installation, run `cargo run -p librewaved` separately. The application checks status every two seconds and removes old status after an unexpected disconnect. It shows a new snapshot when the daemon becomes available again. Opening the window does not read hardware controls. `LIBREWAVE_SOCKET` selects a development IPC socket; installed service lifecycle controls use the normal session socket.

The main view places sources on the left and destination mixes on the right.
This follows the horizontal matrix used by [Wave Link 3](https://www.elgato.com/us/en/s/wave-link-app).
The older Wave Link 2 interface used channel strips. LibreWave shows the
Microphone and System routes from the daemon's desired mixer profile. The
runtime badge shows whether the audio host and mixer engine are connected.
The device panel below the matrix shows only state received from the daemon.
The application does not invent meter values.

Select **Read controls** to open the daemon's device connection and read the
Wave:3 configuration. This command can also restore hardware values that
LibreWave saved earlier. The application shows the admitted API version,
write access, hardware values, and transaction errors.

Each enabled control sends one command with the observed device generation.
The daemon checks the exact schema and complete baseline, then reads back a
successful write. A locked or unknown device cannot be changed. The
microphone preamp and headphone hardware level are separate from desktop
volume and software mixer levels.

Each Monitor Mix and Stream Mix cell shows one source's saved on/off state and
fader level. Select **Turn on**, **Turn off**, **+**, or **−** to send one route
change with the current mixer generation. A fader step is 0.5 dB. The daemon
returns the complete mixer snapshot after it saves the change. Routes can be
saved while the audio host is inactive; they apply when an opt-in audio attempt
becomes active. A stale generation error is shown until the next action, while
status polling obtains the current profile.

## Current limits

A fresh daemon leaves the mixer audio path inactive. A daemon restart reloads
a previously accepted audio intent. Experimental start reports ready audio only
after the host owns the audio path. Live meter values are not
published over IPC. Physical write validation is limited to the Low-cut control
on Wave:3 API 5.4. Other hardware controls still require validation.
The new service launch, default restoration, restart recovery, and hotplug
lifecycle remain unverified on physical hardware. GPUI tests check state handling;
they do not validate a native Wayland or X11 window.
Longer stability on the current startup and routing implementation also
requires validation.
The new lifecycle needs a monitoring test that separates software microphone
monitoring from direct device monitoring.

The Linux application uses upstream Zed GPUI and `gpui_platform` from
Zed tag `v0.233.9`, commit `236e6ddf54e0763d0343f4d3e369d2ae00fe9670`,
with Wayland and X11 backends. The GPUI feature remains disabled in the
default workspace build.
