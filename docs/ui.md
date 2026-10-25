# Desktop application

`librewave` is the optional desktop application. It uses the same local IPC interface as `librewavectl` and sends every hardware and mixer change through `librewaved`. It never opens the USB device or the audio services itself.

## Open the application

Install LibreWave with `--desktop` as described in [Install, run, and remove LibreWave](setup.md). Then open LibreWave from the application menu or run `librewave`. This starts the mixer, then the window.

During development, run `cargo run -p librewave`. Select **Start mixer** in the title bar to start audio through the installed lifecycle. To work on the device or mixer without an installation, run `cargo run -p librewaved` in another terminal. `LIBREWAVE_SOCKET` selects a development IPC socket. Lifecycle actions always use the normal session socket.

`cargo run -p librewave -- --settings` opens Settings at launch.

## Layout

The window has a title bar, a device stage on the left, and the mixer on the right. When the window is too narrow for the stage and two input strips, the stage moves above the mixer. The default window is 1280 by 840 px and the minimum is 850 by 650 px.

The title bar is the only place in the main view that shows and changes the audio state. While audio runs, it shows **Mixer ready** and **Stop**. **Stop** releases audio and restores the desktop defaults. Closing the window keeps audio running. **Mixer ready** means the daemon has admitted the device and activated its graph. It does not mean that an application is playing.

When the daemon is unavailable, the stage dims the illustration and the mixer area shows one notice with **Start mixer** and **Open Settings**.

The application uses Geist on dark surfaces and Geist Mono for commands and paths. Surfaces differ by fill, and outlines show only keyboard focus. Monitor is mint and Stream is blue everywhere. Red marks a muted state, an error, or a destructive action. Amber marks a read-only state. Icon-only buttons name themselves when the pointer rests on them.

## Device stage

The stage shows the Wave:3 illustration, its connection state, and the admitted API version. The build renders the illustration ahead of time, so it appears with the first frame. The lights on it follow the daemon snapshot:

- The mode light shows the knob target.
- The ring is white when the microphone is live and red when it is muted. The glow behind the illustration follows the ring.
- On API 5.4, the level dots follow the measured brackets for microphone gain, headphone level, and the Mix position. Values outside the measured brackets stay dark. Hover over the illustration to see when the mapping is unverified. [Protocol evidence](protocol-evidence.md) has the measurements.
- All lights are dark when controls are unavailable, the device is disconnected, or the device LEDs are off.

The daemon reads the device configuration every 500 ms while it is connected. Polling never changes hardware values. **Read controls** retries admission. It can restore the managed Clipguard, Low-cut, LED, and Gain Lock settings. After the first read, the same action is the circular arrow beside the device name.

The hardware controls sit under the illustration:

- **Microphone gain** (0 to 40 dB) and **Headphone level** (−60 to 0 dB) each have a slider. Drag the slider or click its track to set the level. The level snaps to the device's 0.5 dB steps. With the slider focused, the arrow keys move it by 0.5 dB, and Home and End select the limits. The **−** and **+** buttons also step by 0.5 dB.
- The icon button before each level mutes it. A muted level is dim and its button is red.
- **Clipguard**, **Low-cut**, and **Gain Lock** are lit when on. Select one to change it. Hover over one to see what it does.

Each change is one `SetWave3Control` request with the observed device generation. The daemon checks the schema and the complete baseline, writes, and reads the value back. While a slider is dragged, one request is in flight and only the latest position waits. That position is sent with the device generation from the previous reply. An error ends the drag and drops the waiting position.

When the daemon reports locked write access, the stage shows the reason and disables every control. The microphone preamp and headphone level are hardware values, separate from desktop volume and the software mixer.

## Mixer

### Inputs

**Inputs** has one strip for each input. Microphone and System are permanent. Up to ten more software inputs receive application audio. Each strip is 212 px wide. A long name ends with an ellipsis instead of moving other controls. The strips scroll sideways when they do not fit.

Each input has its own icon. Microphone and System have fixed icons. A software input whose name states a kind, such as Music, Browser, Chat, Game, or Video, gets that kind's icon. Other inputs get one of the remaining icons by identity, so neighbors differ.

A strip has a stereo peak meter, a Monitor fader, and a Stream fader with a dB scale between them. Input meters measure the signal before the faders. Output meters measure each final mix after clipping. The meter scale is −60 to 0 dBFS. A peak above full scale is red. A dim track means no recent reading, not silence.

- Drag a fader or click its track to set the level. Select the level under a fader to reset it to 0 dB.
- The speaker button under a fader mutes the input in that mix. A muted fader is grey and its button is red.
- The link button between the mix icons links the two faders of a strip. Either fader then sets both levels in one request. Each mix keeps its own mute. The link lasts until the window closes.
- With a fader focused, the arrow keys step by 0.5 dB, Home selects −60 dB, End selects +12 dB, and Enter resets to 0 dB.

### Edit an input

Select the pencil button in a strip heading to open **Edit input**. The pop-up opens over the mixer and does not move other controls. A click outside it, the close button, or Escape closes it and discards unsaved text.

- The name field at the top renames a software input. Press Enter or select the check button to save.
- **Assigned here** lists the saved application rules for this input, including rules for closed applications.
- **Active** lists playing applications that the input does not have yet.
- **All applications** lists applications with a visible desktop entry. LibreWave reads entries from the XDG data directories and the Flatpak export directories, once, when the first pop-up opens. Each row shows the icon from the current icon theme, then Breeze, Adwaita, `hicolor`, or the pixmap directories. A row without an icon shows a generic glyph.
- Select a row to assign that application to this input and move its current streams here. Select a row under **Assigned here** to forget its rule. A row assigned to another input names that input.
- The field at the bottom filters the list. To save a rule the list does not show, type the identity and press Enter or select the plus button. Text that names an installed or playing application takes that application's kind. Other text with a dot is an app ID, and other text is an executable name. The icon before the field shows the kind. The folder button opens a file picker: a `.desktop` file gives its desktop ID, and any other file gives its file name as an executable.
- The line under the field shows routing progress and counts playing streams that name no application.
- **Remove input**, at the bottom of a software input's pop-up, asks for confirmation. Removing an input deletes its levels and moves its assigned applications to System. Microphone and System cannot be removed.

The pop-up keeps one height while you filter it. Adding and removing inputs needs a ready or cleanly stopped mixer.

### Reorder inputs

Drag the six dots at the top of a strip onto another strip. The input takes that strip's position. Only the dots start a drag. With the dots focused, Left and Right move the input by one place.

### Add an input

The **Add input** panel after the last strip creates a software input with a new identity. Each input has a stable sink name, such as `librewave.input.3`, that applications can select directly.

### Outputs

**Outputs** has one row for each mix. The Monitor mix plays in the Wave:3 headphones. The Stream mix is a source for other applications. Drag or click the level control to change a software output, or use the arrow keys, Home, and End. The speaker button mutes the output. The dB button resets it to 0 dB. Output levels do not change the hardware headphone level or microphone gain.

### Requests

A fader drag keeps one request in flight and only the latest waiting position. Each request uses the mixer generation from the previous reply. A stale generation, a disconnect, or an error ends the drag and shows the error. Changes made while audio is stopped are saved and apply when audio starts.

## Application routing

LibreWave groups playback streams by application identity: the desktop app ID, or the executable name when no app ID exists. For a Flatpak stream without its own identity, the daemon reads the app ID from the PipeWire client that owns the stream. The list identifies applications, not browser tabs.

A saved rule moves every current stream of the application and applies to new streams when they appear. LibreWave sends the input's serial as `target.object`. WirePlumber stores the stable input name when its target restoration is on. A stream's first audio can reach its previous destination before the move completes. Protected streams and disabled stream movement in WirePlumber prevent a move. Forgetting a rule does not move a current stream back.

A new profile assigns installed applications by their desktop entry categories:

| Input | Categories |
| --- | --- |
| Game | `Game` |
| Voice Chat | `InstantMessaging`, `Chat`, `Telephony`, or `VideoConference` |
| Browser | `WebBrowser` |
| Music | `Music`, or `Audio` with `Player` and without `Video` |

## Settings

Open Settings with the gear button or Ctrl+,. Tab and Shift+Tab stay inside the dialog. Escape, the close button, or a click outside it closes the dialog and restores focus. The mixer and hardware controls do not accept input while Settings is open.

- **Audio** shows whether audio runs and why it is stopped. **Engine** has **Start audio** and **Stop and restore**. **Troubleshooting** chooses a conflicting WirePlumber file. **Configuration** has **Reset…**, which asks for confirmation and then restores the default inputs, levels, and application assignments. Reset does not change the microphone and headphone settings on the device.
- **Setup** has **Show plan**, which lists what setup installs without making changes. **Terminal commands** names `librewavectl setup`, `doctor`, and `uninstall`, each with a copy button.
- **Connection** shows the daemon connection, the IPC socket, and the state of PipeWire and WirePlumber.

The Audio and Setup pages show the last lifecycle result under **Result**.

## Text fields

Every text field uses one shared control with the button height and corner radius. It supports:

- Ctrl+A, Ctrl+C, Ctrl+X, and Ctrl+V, plus Shift+Insert, Ctrl+Insert, and Shift+Delete.
- Ctrl+Z to undo, and Ctrl+Shift+Z or Ctrl+Y to redo.
- Ctrl+Left and Ctrl+Right to move by word, with Shift to extend the selection.
- Ctrl+Backspace and Ctrl+Delete to delete a word.
- Drag to select, double-click to select a word, and triple-click to select all.
- Enter to save where saving applies.

## CLI equivalents

| Action | Command |
| --- | --- |
| Add an input | `librewavectl mixer add <generation> "Music"` |
| Remove an input | `librewavectl mixer remove <source> <generation>` |
| Rename an input | `librewavectl mixer rename <source> <generation> <name>` |
| Move an input | `librewavectl mixer move <source> <generation> <zero-based-position>` |
| Set both faders | `librewavectl mixer faders <source> <generation> -6.5 -6.5` |
| Set an output | `librewavectl mixer output <generation> monitor on -6.5` |
| Reset the configuration | `librewavectl mixer reset <generation>` |
| List playing applications | `librewavectl apps list` |
| Save or forget a rule | `librewavectl apps assign <generation> <desktop\|executable> <identity> <source\|forget>` |
| Move an application now | `librewavectl apps route <desktop\|executable> <identity> <source>` |

`librewavectl mixer show` prints the generation, the inputs, and the saved rules.

## Tests and limits

GPUI fixture tests cover command mapping, pointer geometry, slider snapping, layouts from 850 to 1920 px wide, keyboard focus, the Settings dialog, the confirmation dialog, the Edit input pop-up, and cancellation of queued fader edits.

On a Wave:3 with API 5.4, native checks verified the connected view, channel fader movement, linked levels, separate mutes, Monitor output level and mute, physical selector and tap-mute updates, and headphone playback. CLI tests verified software output mute and level and application moves between System and a named input.

Native drag-and-drop reordering, keyboard control, and Stream output interaction are not yet verified. Effects and output destination selection are not implemented.

The application uses upstream Zed GPUI and `gpui_platform` from Zed tag `v0.233.9`, commit `236e6ddf54e0763d0343f4d3e369d2ae00fe9670`, with Wayland and X11 backends. The GPUI tests are behind the `gpui-tests` feature. Geist fonts and Phosphor icons are bundled with their license notices under `crates/librewave-ui/assets/`. No vendor artwork is bundled.
