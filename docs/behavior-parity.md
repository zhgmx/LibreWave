# Wave Link behavior parity

Status: compatibility research is required. The exact physical and user-interface feedback of Gain Lock and of the Windows endpoint paths is not verified.

## Goal

LibreWave matches the user-visible behavior of Wave Link on Windows and macOS where those products agree. Where they differ because of the operating-system audio model, LibreWave keeps the same separation between hardware controls and software mix controls through Linux audio APIs.

Matching behavior does not mean copying the implementation. macOS uses CoreAudio and a virtual audio driver. Windows uses WASAPI and installed driver facilities. Linux uses PipeWire, WirePlumber, and ALSA.

## Values that must stay distinct

- Microphone preamp gain.
- Hardware microphone mute.
- Wave headphone output level.
- Direct monitor balance.
- Operating-system source volume.
- Operating-system sink volume.
- Wave Link channel volume.
- Monitor mix volume.
- Stream mix volume.

Two controls can show the same percentage and still be different values. LibreWave does not link them until evidence shows that Wave Link links them.

## Observation method

Each Windows and macOS test records four snapshots:

1. Wave device protocol state.
2. Operating-system endpoint volume and mute state.
3. Wave Link application state.
4. Physical knob mode and displayed level.

For each action, the test changes one control and records which snapshots changed. Restart and reconnect tests then show which component persists or restores the value.

The required actions are:

- Change the operating-system microphone volume.
- Change the operating-system output volume for the Wave device or Wave Link endpoint.
- Change microphone gain in Wave Link.
- Change device output volume in Wave Link.
- Turn the physical knob in microphone, headphone, and monitor modes.
- Mute from the operating system, Wave Link, and the physical control.
- Close Wave Link and repeat the operating-system actions.
- Restart the audio service, sign out, and reconnect the device.

## Evidence states

Every behavior-matrix cell has one of these states:

- `observed`: confirmed on a physical system with before and after state.
- `documented`: stated as user-visible behavior in vendor documentation.
- `recovered`: established by a specific static code path or protocol field.
- `inferred`: likely from available evidence, but not safe to implement as a compatibility rule.
- `unknown`: no sufficient evidence.

Exact volume linking and persistence require `observed` evidence before implementation. A `documented` or `recovered` result defines the expected test. It does not prove how an operating-system slider reports a rejected or synchronized change.

## Recovered control paths

Wave Link keeps physical hardware settings separate from software mixer levels on both supported systems.

### Windows endpoint feedback

The Windows MMDevice endpoint observer registers for master-volume and mute notifications. An incoming notification copies the new scalar or mute state into the endpoint model. A block guard stops that copy from sending the same value back to the endpoint. Separate flags can also suppress outgoing volume and mute writes.

The physical Wave input model sets the ignore flag on its input endpoint model and its paired hardware-device model. So an MMDevice input-volume or mute notification does not become a Wave Link channel-volume or channel-mute change. Microphone gain, hardware microphone mute, and headphone level use a separate Wave device-settings path. That path has an origin guard, so a setting reported by the device is not written back as a new user change.

The Wave:3 physical output follows the normal endpoint path. An operating-system output-volume or output-mute notification updates the output endpoint model under the block guard. The recovered Wave:3 path does not turn that notification into a mixer fader change or a Wave device-setting write. This does not show whether the Windows driver changes the physical headphone level or mute state.

Ordinary virtual Wave Link mix-output endpoints keep their MMDevice scalar and mute state separate from the mix master faders. These endpoint values are software values. They are not microphone preamp gain or Wave headphone level.

The recovered Windows disposal path only unregisters endpoint and session notifications. It does not reset endpoint volume or mute. Wave Link is therefore an observer and client of the Windows endpoint while it runs. This does not establish what the driver or physical device does after Wave Link closes.

### macOS property feedback

The macOS code has CoreAudio property listeners and setters for mute, scalar volume, unified volume, per-channel volume, and virtual-main volume. It adds and removes listener subscriptions with the CoreAudio property-listener API. Wave microphone gain, hardware mute, headphone level, and Gain Lock use a separate Wave device-settings model and parameter paths.

The recovered code shows that these control mechanisms are separate. It does not show whether a macOS input or output slider changes a Wave hardware field, whether Wave Link shows that change in its device-settings view, or what happens after Wave Link closes.

The recovered Wave:3 schema defines Gain Lock as a switch that ignores input-volume `SET_CUR` requests from the operating system. Its recovered default is off. The schema description applies to input volume, not to microphone mute. Elgato's [Wave Gain Lock guide](https://help.elgato.com/hc/en-us/articles/360050731352-Elgato-Wave-Link-Wave-Gain-Lock) states that applications cannot change microphone gain while the switch is on, but the physical dial and Wave Link still can.

Gain Lock is therefore an optional hardware policy. LibreWave must show and preserve the device setting, and it must not force the setting on during setup. With Gain Lock on, an operating-system slider for the physical Wave capture endpoint can stop changing hardware gain by design. A LibreWave software microphone endpoint keeps its own working software level.

## Evidence matrix

| Behavior | macOS | Windows | Linux target |
| --- | --- | --- | --- |
| Third-party input-volume request with Gain Lock off | Documented to allow hardware gain changes | Documented to allow hardware gain changes | Keep hardware gain separate from LibreWave software levels |
| Third-party input-volume request with Gain Lock on | Documented and recovered as blocked | Documented and recovered as blocked | Preserve the option and keep software endpoint volume usable |
| Exact OS slider display after Gain Lock rejects a request | Unknown | Unknown | Do not promise snap-back behavior yet |
| OS input mute on the physical Wave endpoint | CoreAudio and Wave setting paths recovered; physical and UI results unknown | Input endpoint notification is ignored as a mixer change; physical and UI results unknown | Keep software source mute separate from hardware microphone mute |
| OS output volume or mute on the physical Wave endpoint | CoreAudio and Wave setting paths recovered; physical and UI results unknown | Endpoint model updates without a mixer or device-setting write; physical result unknown | Pending physical parity test |
| OS volume or mute on an ordinary virtual Wave Link mix output | Recovered as software endpoint state; exact mixer coupling unknown | Recovered as software endpoint state, separate from the mix master fader | Keep it a software value |
| Recovered shutdown behavior | Property listeners have a removal path; post-close device behavior unknown | Endpoint observers unregister without resetting endpoint volume or mute; post-close physical behavior unknown | Define and test daemon shutdown behavior explicitly |
| Wave Link output control changes the headphone port | [Documented](https://help.elgato.com/hc/en-us/articles/360057309711-Wave-Link-First-time-setup-with-macOS) | [Documented](https://help.elgato.com/hc/en-us/articles/360044566172-Wave-Link-First-Time-Setup-for-Windows-10) | Required hardware control |
| Virtual mix endpoints have software levels | Recovered from architecture | Recovered from architecture | Required |
| Physical control events update application state | Recovered from architecture | Recovered from architecture | Required and must be observed on Wave:3 |

## Provisional Linux rule

Until the parity matrix is complete, LibreWave keeps hardware gain and headphone level separate from ordinary software endpoint volume. Without this separation, Linux desktop policy can become an accidental hardware writer.

The operating-system microphone slider changes software gain on the LibreWave microphone endpoint. The device settings and the physical dial change the Wave preamp gain. Gain Lock protects that preamp from outside control requests. It does not disable the LibreWave software level.

The default system playback target is a LibreWave software input channel, because Wave Link uses virtual inputs for system and application audio. Its operating-system volume is a software channel value. The Wave headphone level is a hardware setting that the device settings and the physical dial control. While LibreWave manages the device, it does not expose the raw physical Wave sink as a second ordinary desktop output.

Every synchronized path tags the change origin as device, LibreWave client, or operating system. Applying an observed change must not send a second change back to its origin. This is the Linux equivalent of the feedback guards recovered on Windows.

An explicit LibreWave inspection or refresh reads the complete Wave:3 configuration on the daemon's retained connection. If a physical setting changed, the observed device state wins for that read. LibreWave updates its portable snapshot and does not write the managed value back to the device. Sparse desired state stays unchanged.

The recovered feedback guards show that a device setting report must not cause a write back to its origin. They do not show whether Wave Link updates, clears, or keeps saved intent for each physical control. LibreWave therefore keeps the observed state and leaves saved intent unchanged. Reconnect follows the managed-control contract in [Hardware safety](hardware-safety.md). Physical tests on Windows and macOS must settle this persistence rule before LibreWave claims exact parity.

No product documentation may claim exact Wave Link parity while required cells are `unknown` or `inferred`.

## Read-only Linux hardware-control observation

A read-only live inspection of the Wave:3 ALSA card found two simple control groups. `Mic Capture Volume` has integer steps from 0 to 80, mapped from 0.00 to 40.00 dB. `PCM Playback Volume` has integer steps from 0 to 120, mapped from -60.00 to 0.00 dB. Each group also has a switch and a read-only channel map.

These ranges match the Wave:3 microphone gain and headphone level ranges. LibreWave treats these card controls as hardware aliases. They are not software endpoint volumes, and LibreWave endpoints must not use them for desktop volume or mute.

## Levels across reconnect

`alsa-state.service` runs `alsactl` to save the card controls and to restore them when a card is added. For the Wave:3, `Mic Capture Volume` and `PCM Playback Volume` are the microphone gain and the headphone level.

On the reference system, the saved values are 32 (16 dB) and 73 (−23.5 dB). Before a reconnect, the device reported a headphone level of −6 dB. About 1.6 s after the device returned, the first daemon read reported −23.5 dB. An earlier reconnect changed a microphone gain of 18.4609375 dB to 16 dB. ALSA restoration is the writer.

LibreWave does not change ALSA persistence. Before it writes back the saved levels, the daemon locks the four level controls on the card, so a later ALSA write fails. [Hardware safety](hardware-safety.md) describes the restore.
