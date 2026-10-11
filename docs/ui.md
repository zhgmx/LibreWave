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
does not read the device configuration when the window opens.

The main view places sources on the left and destination mixes on the right.
This follows the horizontal matrix used by [Wave Link 3](https://www.elgato.com/us/en/s/wave-link-app).
The older Wave Link 2 interface used channel strips. LibreWave shows its
Microphone and System rows as unavailable until the production audio host
provides real routes and meters. The device panel below the matrix shows only
state received from the daemon. It does not invent meter or route values.

Select **Read controls** to open the daemon's device connection and read the
Wave:3 configuration. This command can also restore hardware values that
LibreWave saved earlier. The application shows the admitted API version,
write access, hardware values, and transaction errors.

Each enabled control sends one command with the observed device generation.
The daemon checks the exact schema and complete baseline, then reads back a
successful write. A locked or unknown device cannot be changed. The
microphone preamp and headphone hardware level are separate from desktop
volume and software mixer levels.

## Current limits

The mixer audio path remains inactive. The application does not yet show
live meters or change software routes. Its hardware controls have not
passed physical write validation on the reference Wave:3. Do not treat an
offline build or test result as a live hardware test.

The Linux application uses upstream Zed GPUI and `gpui_platform` from
Zed tag `v0.233.9`, commit `236e6ddf54e0763d0343f4d3e369d2ae00fe9670`,
with Wayland and X11 backends. The GPUI feature remains disabled in the
default workspace build.
