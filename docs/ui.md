# GPUI application

The optional `librewave` application uses the same local IPC interface as
`librewavectl`. It shows the daemon's Wave:3 state and sends hardware control
requests through `librewaved`. The application does not open the USB device.

## Prerequisites

Install the Rust toolchain and native build packages in [Setup](setup.md).
For lifecycle controls, install matching CLI and daemon binaries with `librewavectl setup`. Setup uses persistent device access by default, which requires separate approval. Use `setup --device-access existing` for separately approved temporary access. In Settings, use **Show plan** on the **Setup** page to see the paths and access rule without changes. Audio startup still requires reviewed device access and a running PipeWire and WirePlumber session.

## Start the application

1. Run `cargo run -p librewave`.
2. Select **Start mixer** in the title bar.

The start action launches the installed daemon service and waits for ready audio and selected desktop defaults. If an existing WirePlumber fragment conflicts with ownership, open **Settings**, select **Audio**, and use **Choose file…** to select the exact `.conf` file. The lifecycle backs it up and restores it on failure or stop. See [Setup](setup.md) for the full contract.

While audio runs, the title bar shows **Mixer ready** and a **Stop** button. **Stop** releases audio and restores desktop ownership. Closing the window keeps audio running. Lifecycle operations run on the background executor. The application prevents overlapping actions and preserves a restoration error while status polling continues. A deliberate stop clears the active snapshot and suppresses the expected disconnect error.

For device and mixer development without an installation, run `cargo run -p librewaved` separately. The application checks status every 500 ms and removes old status after an unexpected disconnect. It shows a new snapshot when the daemon becomes available again. The daemon reads hardware controls automatically while it runs. Opening the window requests its latest snapshot. `LIBREWAVE_SOCKET` selects a development IPC socket; installed service lifecycle controls use the normal session socket.

## Main view

The main view has a title bar, a device stage, and a mixer. The title bar is
the only place on the main view that shows and changes the audio state. In a
window too narrow for the stage and two input strips side by side, the stage
sits above the mixer. The default window is 1280 by 840 px. The minimum is
850 by 650 px.

In the side-by-side layout, the stage and the mixer use the same 32 px inset.
The device name and the **Inputs** label share a top edge. The faders use
the available window height. When the controls need more height, the mixer
scrolls. Output rows wrap in a narrow mixer.

The application uses Geist type on dark surfaces. Surfaces differ by fill.
Outlines show keyboard focus only. Monitor is mint and Stream is blue in every
place they appear. Red marks a muted state or an error. Amber marks an
experimental or read-only state. Icon-only buttons show their name when the
pointer rests on them. Commands, paths, and command output use Geist Mono.

Every text field uses the same control. It has the button height and corner
radius. It supports Ctrl+A, Ctrl+C, Ctrl+X, Ctrl+V, Shift+Insert, Ctrl+Insert,
Ctrl+Z, and Ctrl+Shift+Z or Ctrl+Y. Ctrl+Left and Ctrl+Right move by word, and
Shift extends the selection. Ctrl+Backspace and Ctrl+Delete delete a word.
Drag to select text, double-click to select a word, and triple-click to select
all. Enter saves the field where saving applies.

When the daemon is unavailable, the stage dims the illustration and the mixer
area shows one notice with **Start mixer** and **Open Settings**. The main
view does not expose socket diagnostics or invent device or channel state.
Live peak meters appear on input strips and output rows while audio runs.
The mixer has linked channel levels, application assignment, and software
output level and mute controls. Effects and output destination selection
remain unavailable.

### Device stage

The stage shows the Wave:3 illustration, its connection state, and the
admitted API version. The build draws the illustration once, so it appears
with the first frame. The lights are drawn over it from the daemon snapshot.

The mode light shows the reported knob target. The ring is white when reported
microphone mute is off and red when it is on. The light behind the
illustration follows the ring. All lights are dark when controls are
unavailable, the device is disconnected, or hardware LEDs are disabled.
The knob face is drawn in front of the ring. In Microphone mode on API 5.4,
the level dots follow the measured gain brackets. The seventh dot lights at
40 dB; it stays dark at 39.5 dB. At exactly 16 dB, the three left dots stay
lit when hardware microphone mute is on. Other muted gain values remain
unverified. In Headphone mode on API 5.4, the dots fill from left to right
using the measured headphone-level brackets. In Mix mode, one dot moves from
the right at 0 percent to the left at 100 percent. Each 5-percent value has a
verified position. Unmeasured fractional transitions, other APIs, flipped
patterns, and other mute states stay dark. Hover over the illustration to see
when the level mapping is unverified. See [Protocol evidence](protocol-evidence.md)
for the observations and limits.

The daemon reads the Wave:3 configuration every 500 ms while connected.
The mode light, mute ring, level dots, and controls follow its observed state.
Polling never changes hardware values. Select **Read controls** to retry admission.
This command can restore managed Clipguard, low-cut, LED, and gain-lock settings. Physical gain, mute, headphone level, direct mix, and knob mode follow the device baseline.
After a read, the same command is the circular arrow beside the device name.

The hardware controls sit under the illustration:

- **Microphone gain** and **Headphone level** each show a level in dB and a bar
  for its position in the reviewed range. Physical readings use two decimal places.
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

**Inputs** has one strip for each input. Microphone and System are permanent
inputs. Up to ten additional software inputs can receive application audio.
Every strip is 212 px wide. A long input name or source ends with an ellipsis
instead of moving other controls. Each strip has a stereo input peak meter, a
Monitor fader, and a Stream fader with a dB scale between them. Input meters
observe samples before the faders.
Output meters observe each final mix after clipping. The meter scale is
-60 to 0 dBFS. A source peak above full scale appears red. A dim track means
that no recent reading is available, not measured silence. A line on each track marks 0 dB.

Drag a fader or click its track to set the level. Select the level under a
fader to reset it to 0 dB. The speaker button under each fader mutes that
input in that mix. A muted fader is grey and its button is red. Setting or
resetting a level preserves its mute state. Levels changed while audio is
stopped are saved.

A headphones icon marks the Monitor fader and a broadcast icon marks the
Stream fader. The plugs button between the two icons links the two fader
levels in the current window. When linked, either fader sets both levels in one
daemon transaction. Each mix keeps its own mute state. Unlinking permits
independent levels again. The link preference lasts until the window closes.

An **Add input** panel follows the last strip. Add or remove software inputs
while the mixer is ready or cleanly stopped. **Add input** creates an input
with a new stable identity. The
small close button on a software input removes it. The daemon saves each
complete layout before it acknowledges the change. Built-in inputs cannot
be removed. The strips scroll sideways when they do not fit.

The CLI accepts a name: `librewavectl mixer add <generation> "Music"`.
Each input has a stable public sink name, such as `librewave.input.3`.
Select its LibreWave device in an application's audio settings. While audio is
ready, the application button on a software input opens the active playback
application list. Select an application's plus button to save its assignment and move its current
streams to that input. The panel shows pending, verified, or failed movement.
Protected streams and disabled WirePlumber movement prevent an immediate move.
Saved rules remain editable while the mixer is stopped. Live layout changes
require a ready mixer; starting or incomplete cleanup blocks them.

The CLI lists groups with `librewavectl apps list`. To move a group, use
`librewavectl apps route desktop "org.example.Player" 3`, or replace `desktop`
with `executable` for an executable identity from the list. The clients use
the same daemon commands. Application grouping and routing passed private
software tests and a bounded disposable-player desktop test. Native assignment
and remembered-target recovery on the desktop remain unverified. The list
identifies applications, not individual browser tabs.
LibreWave sends the observed input serial as `target.object`. WirePlumber
resolves it and stores the stable input name when target restoration is enabled.
A private test confirmed restoration after stream recreation, input recreation,
and a WirePlumber restart. LibreWave saves explicit assignments in the mixer profile and applies them to new stream incarnations. It does not repeatedly override later external routing changes
after restart. A stream's first audio can reach its previous destination before
movement completes.

**Outputs** is under the inputs and has one row for each mix. A row names the
mix and where it goes. The Monitor mix plays in the Wave:3 headphones. The
Stream mix is a source for other applications. Drag or click the horizontal
level control to change a software output. Arrow keys change it by 0.5 dB.
Home and End select the level limits. The speaker button mutes that output,
and the dB button resets it to 0 dB while preserving mute. Output controls are
saved even when audio is stopped. They do not change hardware headphone level
or microphone gain. Destination selection remains unavailable.

The CLI accepts `librewavectl mixer output <generation> monitor on -6.5`.
Replace `monitor` with `stream` for the other output, or use `off` to mute it.
`librewavectl mixer faders <source> <generation> -6.5 -6.5` changes both channel
faders atomically without changing mute states. These controls passed software
tests and a bounded Wave:3 audio run through the CLI. A native Wave:3 check
verified channel fader movement, linked levels, separate mutes, and Monitor
output level and mute. Native keyboard control and Stream output interaction
remain unverified.

## Settings

Open **Settings** with the gear button or Ctrl+,. Use
`cargo run -p librewave -- --settings` to open Settings at launch.
Settings takes keyboard focus. Tab and Shift+Tab stay inside the dialog.
Escape, the close button, or a click outside the dialog closes it and restores
the previous focus. Mixer and hardware input are blocked while Settings is
open.

Settings has three pages:

- **Audio** shows whether audio runs and why it is stopped. Its **Engine**
  section has **Start mixer** and **Stop and restore**. Its **Troubleshooting**
  section has the WirePlumber conflict file.
- **Setup** has **Show plan**, which lists the paths and the access rule that
  setup installs without changes. Its **Terminal commands** section names the
  `librewavectl setup`, `doctor`, and `uninstall` commands. The button after
  each command copies it.
- **Connection** shows the daemon connection and the IPC socket under
  **Daemon**, and the state of PipeWire and WirePlumber under
  **Audio services**.

The Audio and Setup pages show the last lifecycle result under **Result**.

## Mixer requests

Dragging keeps one request in flight and only the latest pending pointer level.
After the daemon acknowledges a request, the next request uses its new mixer
generation and current mute state. A stale generation, disconnect, or command
failure ends the gesture, cancels the pending level, and shows the error.
Pointer previews are not saved until the daemon accepts the change.

Focus a fader with the Tab key. Use the arrow keys for 0.5 dB steps, Home for
-60 dB, and End for +12 dB. Enter resets a channel fader to 0 dB. Each action
uses the current mixer generation. Linked channel edits set both levels in
one request. Output edits change one software mix. The daemon returns the complete mixer
snapshot after it saves the change. Routes can be
saved while the audio host is inactive; they apply when an opt-in audio attempt
becomes active. A stale generation error is shown until the next action, while
status polling obtains the current profile.

## Current limits

A fresh daemon leaves the mixer audio path inactive. A daemon restart reloads
a previously accepted audio intent. Experimental start reports ready audio only
after the host owns the audio path. The daemon publishes actual software
peak and RMS values over a separate IPC request. The UI reads meters every
100 ms while audio is active. Readings expire after 350 ms without a block.
Meters report individual processed blocks, not a peak hold over the interval.
Default tests check the signal path with fakes. A bounded Wave:3 run verified
software meters with a quiet disposable test source. Wave:3 API 5.4 control
tests verified Low-cut readback, microphone gain LED counts, hardware microphone
mute in both directions, headphone LED brackets, and Mix LED positions.
The LED and mute tests used no audio. They do not verify audio attenuation,
blend feed weights, or mute timing. Other hardware controls still require
physical validation.
Service launch, default restoration, service restart, and removal are verified
on the reference Wave:3. Reconnect, sleep, login recovery, and longer startup
and routing stability remain unverified.

GPUI fixture tests cover command mapping, pointer geometry, state handling,
Ready and Locked layouts, initial keyboard focus, modal focus containment,
Settings restoration errors, and backdrop dismissal. They also check output
controls from 850 to 1920 px wide, repeated command copying in Settings,
and cancellation of queued fader edits when Settings opens. Native Wave:3 API 5.4
captures cover the connected view, loaded controls, minimum-window device
stage, and Settings dialog with audio inactive and hardware controls unchanged.
Captures alone do not verify native input, active audio, or other desktops.
Physical selector and tap-mute updates in the native UI are verified with
audio inactive. The cause of a small gain change remains uncertain; incidental
knob rotation is possible. Selector and mute results remain valid.

CLI and native tests verify linked and independent channel levels and separate
channel mutes. CLI tests also verify software output mute and attenuation, and
disposable application moves between System and a named input. Native tests
also verify headphone playback, channel fader movement, and Monitor output
level and mute. Native keyboard control, application assignment, and Stream
output interaction remain unverified. Channel and output fader tracks support
drag, click, and keyboard input. Hardware level bars are not draggable.
Effects and output destination selection remain work for later increments.

The Linux application uses upstream Zed GPUI and `gpui_platform` from
Zed tag `v0.233.9`, commit `236e6ddf54e0763d0343f4d3e369d2ae00fe9670`,
with Wayland and X11 backends. The GPUI feature remains disabled in the
default workspace build.

Geist fonts and Phosphor icons are bundled with their required license notices
under `crates/librewave-ui/assets/`. No vendor application artwork is bundled.

## Saved channels and assignments

New profiles contain Microphone, System, Game, Chat, and Music. Microphone monitoring starts muted to avoid duplicate direct monitoring. Existing profiles keep their channels and levels. Channel IDs do not change when a channel is renamed or reordered. A removed ID is not reused.

Select the application button in a channel heading to open the channel menu. The menu opens over the mixer and does not move other controls. A click outside the menu, the close button, or Escape closes it and discards unsaved text.

- The menu heading renames a software channel and moves a channel left or right.
- **Assigned here** lists the saved rules for this channel, including rules for closed applications.
- **Active** lists playing applications that this channel does not have yet.
- **All applications** lists the applications that have a visible desktop entry. LibreWave reads the entries from the XDG data directories and the Flatpak export directories. It reads them once, when a menu first opens.
- Select a row to save a rule for this channel. Select a row under **Assigned here** to forget its rule. A row assigned to another channel names that channel; selecting it moves the rule here.
- The field at the bottom filters the list. To save a rule that the list does not show, type the identity, choose **App ID** or **Executable** with the button before the field, and select the plus button.
- The folder button opens a file picker. A `.desktop` file fills in its desktop ID. Any other file fills in its file name as an executable.

Deleting its channel changes the saved destination to System. Forgetting a rule stops future enforcement; it does not move a current stream. WirePlumber may retain its last stream target. The menu shows routing progress, and it counts playing streams that name no application.

The CLI supports `mixer rename <source> <generation> <name>`, `mixer move <source> <generation> <zero-based-position>`, and `apps assign <generation> <desktop|executable> <identity> <source|forget>`. `mixer show` lists saved rules. Assignment success confirms that the rule was saved. The channel menu reports routing progress separately.

**Mixer ready** means that the daemon has admitted the device and activated its graph. It does not assert that a playback application is streaming or that a signal is audible. The device stage reports device access. The meters report recent signal levels. Lifecycle progress names the start or stop operation; a fader edit does not replace the mixer state label.
