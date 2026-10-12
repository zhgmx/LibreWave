# GPUI application

The optional `librewave` application uses the same local IPC interface as
`librewavectl`. It shows the daemon's Wave:3 state and sends hardware control
requests through `librewaved`. The application does not open the USB device.

## Prerequisites

Install the Rust toolchain and native build packages in [Setup](setup.md).
For lifecycle controls, install matching CLI and daemon binaries with `librewavectl setup`. Setup includes an exact udev access operation that requires separate approval. In Settings, use **Show plan** on the **Setup** page to see the paths and access rule without changes. Audio startup still requires reviewed device access and a running PipeWire and WirePlumber session.

## Start the application

1. Run `cargo run -p librewave`.
2. Select **Start audio** in the title bar.

The start action launches the installed daemon service and waits for ready audio and selected desktop defaults. If an existing WirePlumber fragment conflicts with ownership, open **Settings**, select **Audio**, and use **Choose file…** to select the exact `.conf` file. The lifecycle backs it up and restores it on failure or stop. See [Setup](setup.md) for the full contract.

While audio runs, the title bar shows **Audio running** and a **Stop** button. **Stop** releases audio and restores desktop ownership. Closing the window keeps audio running. Lifecycle operations run on the background executor. The application prevents overlapping actions and preserves a restoration error while status polling continues. A deliberate stop clears the active snapshot and suppresses the expected disconnect error.

For device and mixer development without an installation, run `cargo run -p librewaved` separately. The application checks status every two seconds and removes old status after an unexpected disconnect. It shows a new snapshot when the daemon becomes available again. Opening the window does not read hardware controls. `LIBREWAVE_SOCKET` selects a development IPC socket; installed service lifecycle controls use the normal session socket.

## Main view

The main view has a title bar, a device stage, and a mixer. The title bar is
the only place on the main view that shows and changes the audio state. In a
window too narrow for the stage and two input strips side by side, the stage
sits above the mixer. The default window is 1200 by 820 px. The minimum is
850 by 650 px.

The application uses Geist type on dark surfaces. Surfaces differ by fill.
Outlines show keyboard focus only. Monitor is mint and Stream is blue in every
place they appear. Red marks a muted state or an error. Amber marks an
experimental or read-only state. Icon-only buttons show their name when the
pointer rests on them. Commands, paths, and command output use Geist Mono.

When the daemon is unavailable, the stage dims the illustration and the mixer
area shows one notice with **Start audio** and **Open Settings**. The main
view does not expose socket diagnostics or invent device or channel state.
Live meters, effects, and linked levels remain unavailable and have no
controls. Additional inputs, output selection, and output level are also
unavailable. The mixer shows where they go, as described in Mixer.

### Device stage

The stage shows the Wave:3 illustration, its connection state, and the
admitted API version. The build draws the illustration once, so it appears
with the first frame. The lights are drawn over it from the daemon snapshot.

The mode light shows the reported knob target. The ring is white when reported
microphone mute is off and red when it is on. The light behind the
illustration follows the ring. All lights are dark when controls have not been
read, the device is disconnected, or hardware LEDs are disabled. The seven
level dots stay dark because their hardware mapping is unverified.

Select **Read controls** to open the daemon's device connection and read the
Wave:3 configuration. This command can also restore hardware values that
LibreWave saved earlier. The application shows that caution under the button.
After a read, the same command is the circular arrow beside the device name.

The hardware controls sit under the illustration:

- **Input gain** and **Headphones** each show a level in dB and a bar for its
  position in the reviewed range. Physical readings use two decimal places.
  The **−** and **+** buttons change a level
  by 0.5 dB. The bars are not draggable.
- The icon button before each level mutes or unmutes it. A muted level is dim
  and its button is red.
- The **Clipguard**, **Low-cut**, and **Gain Lock** tiles are lit when the
  option is on. Select a tile to change it.

When the daemon reports locked write access, the stage shows the reason and
disables every control. Transaction errors appear above the mixer and can be
dismissed.

Each enabled control sends one command with the observed device generation.
The daemon checks the exact schema and complete baseline, then reads back a
successful write. A locked or unknown device cannot be changed. The
microphone preamp and headphone hardware level are separate from desktop
volume and software mixer levels.

### Mixer

**Inputs** has one strip for each input. Only the Microphone and System inputs
are currently supported. Each strip has a Monitor fader and a Stream fader
with a dB scale between them. A line on each track marks 0 dB.

Drag a fader or click its track to set the level. Select the level under a
fader to reset it to 0 dB. The speaker button under each fader mutes that
input in that mix. A muted fader is grey and its button is red. Setting or
resetting a level preserves its mute state. Levels changed while audio is
stopped are saved.

An **Add input** panel follows the last strip. The daemon has no command that
adds an input, so the panel does nothing and says so when the pointer rests on
it. The strips scroll sideways when they do not fit.

**Outputs** is under the inputs and has one row for each mix. A row names the
mix and where it goes. The Monitor mix plays in the Wave:3 headphones. The
Stream mix is a source for other applications. The destination and the level
slider in each row are inactive because the daemon has no command for them.
Each says so when the pointer rests on it.

## Settings

Open **Settings** with the gear button or Ctrl+,. Use
`cargo run -p librewave -- --settings` to open Settings at launch.
Settings takes keyboard focus. Tab and Shift+Tab stay inside the dialog.
Escape, the close button, or a click outside the dialog closes it and restores
the previous focus. Mixer and hardware input are blocked while Settings is
open.

Settings has three pages:

- **Audio** shows whether audio runs and why it is stopped. It has
  **Start audio**, **Stop and restore**, and the WirePlumber conflict file.
- **Setup** has **Show plan**, which lists the paths and the access rule that
  setup installs without changes. It also names the `librewavectl setup`,
  `doctor`, and `uninstall` commands.
- **Connection** shows the daemon connection, the IPC socket, and the state of
  PipeWire and WirePlumber.

## Mixer requests

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
acceptance runs. GPUI tests check command mapping, pointer geometry, state
handling, loaded Ready and Locked control layouts, initial keyboard focus,
modal focus containment, restoration errors across Settings pages, and backdrop
dismissal. Loaded-control layout tests use fixtures. Native captures check the
daemon-connected view with controls not loaded and the Settings dialog. These
captures do not validate native mouse or keyboard interaction, active audio,
or cross-desktop behavior.
Longer stability on the current startup and routing implementation also
requires validation.
The fader tracks support drag, click, and keyboard input. The hardware level
bars are not draggable. Linked
faders, application-specific inputs, effects, and output selection remain work
for later increments.

The Linux application uses upstream Zed GPUI and `gpui_platform` from
Zed tag `v0.233.9`, commit `236e6ddf54e0763d0343f4d3e369d2ae00fe9670`,
with Wayland and X11 backends. The GPUI feature remains disabled in the
default workspace build.

Geist fonts and Phosphor icons are bundled with their required license notices
under `crates/librewave-ui/assets/`. No vendor application artwork is bundled.
