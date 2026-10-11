# GPUI application

The optional `librewave` application uses the same local IPC interface as
`librewavectl`. It shows the daemon's Wave:3 state and sends hardware control
requests through `librewaved`. The application does not open the USB device.

## Prerequisites

Install the Rust toolchain and native build packages in [Setup](setup.md).
The `librewaved` daemon must run in the same user session. The current
development build does not start it automatically.

## Start the application

1. Run `cargo run -p librewaved` in one terminal.
2. Run `cargo run -p librewave` in another terminal.

The application shows a connection error if it cannot reach the daemon. It
checks daemon status every two seconds and removes old status when the daemon
disconnects. It shows the new snapshot after the daemon starts again. It does
not read the device configuration when the window opens. Set `LIBREWAVE_SOCKET`
for both processes to use a development socket outside the normal session.

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

Normal daemon startup leaves the mixer audio path inactive. The opt-in host can
report an active mixer after it owns the audio path. Live meter values are not
published over IPC. Hardware controls still require physical write validation.
Default-route selection, restart recovery, and hotplug remain unverified.
Longer stability on the current startup and routing implementation also
requires validation.

The Linux application uses upstream Zed GPUI and `gpui_platform` from
Zed tag `v0.233.9`, commit `236e6ddf54e0763d0343f4d3e369d2ae00fe9670`,
with Wayland and X11 backends. The GPUI feature remains disabled in the
default workspace build.
