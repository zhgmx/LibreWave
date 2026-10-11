# GPUI application

The optional `librewave` application uses the same local IPC interface as
`librewavectl`. It shows the daemon's Wave:3 state and sends hardware control
requests through `librewaved`. The application does not open the USB device.

## Prerequisites

Install the Rust toolchain and native build packages in [Setup](setup.md).
For lifecycle controls, install matching CLI and daemon binaries with `librewavectl setup`. Setup includes an exact udev access operation that requires separate approval. Use **View setup plan** to see the paths and access rule without changes. Audio startup still requires reviewed device access and a running PipeWire and WirePlumber session.

## Start the application

1. Run `cargo run -p librewave`.
2. Open **Settings** with the gear button.
3. Select **Start experimental audio**.

The start action launches the installed daemon service and waits for ready audio and selected desktop defaults. If an existing WirePlumber fragment conflicts with ownership, use **Choose fragment…** to select the exact `.conf` file. The lifecycle backs it up and restores it on failure or stop. See [Setup](setup.md) for the full contract.

Select **Stop and restore** to release audio and restore desktop ownership. Closing the window keeps audio running. Lifecycle operations run on the background executor. The application prevents overlapping actions and preserves a restoration error while status polling continues. A deliberate stop clears the active snapshot and suppresses the expected disconnect error.

For device and mixer development without an installation, run `cargo run -p librewaved` separately. The application checks status every two seconds and removes old status after an unexpected disconnect. It shows a new snapshot when the daemon becomes available again. Opening the window does not read hardware controls. `LIBREWAVE_SOCKET` selects a development IPC socket; installed service lifecycle controls use the normal session socket.

The main view has one mixer canvas with a slim header, input channel strips,
and output rows. Each input has paired vertical Monitor and Stream faders.
The application uses Geist type, neutral surfaces, mint Monitor icons, and
blue Stream icons. The header shows whether audio is running, stopped, or
disconnected. Only the Microphone and System inputs are currently supported.

Setup actions, device controls, and connection details are in Settings. When
the daemon is unavailable, the mixer shows a concise connection message. It
does not expose socket diagnostics on the main canvas or invent channel state.
Live meters, effects, linked levels, additional inputs, and output master
controls remain unavailable. Their inactive controls do not change audio.

In Settings, select **Read controls** to open the daemon's device connection and read the
Wave:3 configuration. This command can also restore hardware values that
LibreWave saved earlier. The application shows the admitted API version,
write access, hardware values, and transaction errors.

Each enabled control sends one command with the observed device generation.
The daemon checks the exact schema and complete baseline, then reads back a
successful write. A locked or unknown device cannot be changed. The
microphone preamp and headphone hardware level are separate from desktop
volume and software mixer levels.

Each input has separate Monitor and Stream levels. Drag a fader or click its
track to set the level. Select the displayed level to reset it to 0 dB. The
headphone and broadcast icons toggle that source in the Monitor and Stream
mixes. A muted icon is red. Setting or resetting a level preserves its mute state.

Dragging keeps one request in flight and only the latest pending pointer level.
After the daemon acknowledges a request, the next request uses its new mixer
generation and current mute state. A stale generation, disconnect, or command
failure ends the gesture, cancels the pending level, and shows the error.
Pointer previews are not saved until the daemon accepts the change.

Focus a fader with the Tab key. Use the arrow keys for 0.5 dB steps, Home for
-60 dB, End for +12 dB, and Enter for 0 dB. Each action sends one route
change with the current mixer generation. The daemon returns the complete mixer
snapshot after it saves the change. Routes can be
saved while the audio host is inactive; they apply when an opt-in audio attempt
becomes active. A stale generation error is shown until the next action, while
status polling obtains the current profile.

## Current limits

A fresh daemon leaves the mixer audio path inactive. A daemon restart reloads
a previously accepted audio intent. Experimental start reports ready audio only
after the host owns the audio path. Live meter values are not
published over IPC. Physical write validation is limited to the Low-cut control
on Wave:3 API 5.4. Other hardware controls still require validation.
Service launch, default restoration, service restart, and removal checks have
passed on the reference Wave:3. Reconnect, sleep, and login recovery still need
acceptance runs. GPUI tests check command mapping and state handling. Native
window captures check the connected, audio-stopped view and disconnected
view. These captures do not validate mouse or keyboard interaction, active
audio, or cross-desktop behavior.
Longer stability on the current startup and routing implementation also
requires validation.
The fader tracks support drag, click, and keyboard input. Linked
faders, application-specific inputs, effects, and output selection remain work
for later increments.

The Linux application uses upstream Zed GPUI and `gpui_platform` from
Zed tag `v0.233.9`, commit `236e6ddf54e0763d0343f4d3e369d2ae00fe9670`,
with Wayland and X11 backends. The GPUI feature remains disabled in the
default workspace build.

Geist fonts and Phosphor icons are bundled with their required license notices
under `crates/librewave-ui/assets/`. No vendor application artwork is bundled.
