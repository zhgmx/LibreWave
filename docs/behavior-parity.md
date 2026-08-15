# Wave Link behavior parity

Status: required compatibility research. The gain-lock contract and the Windows endpoint feedback paths are recovered. Exact physical and user-interface feedback still needs validation.

## Goal

LibreWave should match the user-visible precedent set by Wave Link on Windows and macOS where those products agree. When they differ because of an operating-system audio model, LibreWave should preserve the same separation between hardware controls and software mix controls through Linux audio APIs.

Matching behavior does not mean copying the implementation. macOS uses CoreAudio and a virtual audio driver. Windows uses WASAPI and installed driver facilities. Linux uses PipeWire, WirePlumber, and ALSA.

## Values that must stay distinct

- Microphone preamp gain.
- Hardware microphone mute.
- Wave headphone output level.
- Direct-monitor balance.
- Operating-system source volume.
- Operating-system sink volume.
- Wave Link channel volume.
- Monitor mix volume.
- Stream mix volume.

Two controls may share a displayed percentage and still represent different values. LibreWave will not link them until evidence shows that Wave Link links them.

## Observation method

Each Windows and macOS test records four snapshots:

1. Wave device protocol state.
2. Operating-system endpoint volume and mute state.
3. Wave Link application state.
4. Physical knob mode and displayed level.

For each action, the test changes one control and records which snapshots changed. Restart and reconnect tests then determine which component persists or restores the value.

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
- `inferred`: likely from available evidence but not safe to implement as a compatibility rule.
- `unknown`: no sufficient evidence.

Implementation requires `observed` evidence for exact volume linking and persistence. A `documented` or `recovered` result defines the expected test, but it does not prove how an operating-system slider reports a rejected or synchronized change.

## Recovered control paths

Wave Link keeps physical hardware settings separate from software mixer levels on both supported systems.

### Windows endpoint feedback

The Windows MMDevice endpoint observer registers for master-volume and mute notifications. An incoming notification copies the new scalar or mute state into the endpoint model. A block guard prevents that copy from sending the same value back to the endpoint. Separate flags can also suppress outgoing volume and mute writes.

The physical Wave input model enables the ignore flag for its input endpoint model and its paired hardware-device model. As a result, an MMDevice input-volume or mute notification does not become a Wave Link channel-volume or channel-mute change. Microphone gain, hardware microphone mute, and headphone level use a separate Wave device-settings path. That path also has an origin guard so a setting reported by the device is not written back as a new user change.

The Wave:3 physical output follows the normal endpoint path. An operating-system output-volume or output-mute notification updates the output endpoint model under the block guard. The recovered Wave:3 path does not convert that notification into a mixer fader change or a Wave device-setting write. This does not prove whether the Windows driver changes the physical headphone level or mute state.

Ordinary virtual Wave Link mix-output endpoints keep their MMDevice scalar and mute state separate from mix master faders. These endpoint values are software values. They are not microphone preamp gain or Wave headphone level.

The recovered Windows disposal path only unregisters endpoint and session notifications. It does not reset endpoint volume or mute. This shows that Wave Link is an observer and client of the Windows endpoint while it runs. It does not establish what the driver or physical device does after Wave Link closes.

### macOS property feedback

The macOS code has CoreAudio property listeners and setters for mute, scalar volume, unified volume, per-channel volume, and virtual-main volume. Listener subscriptions are added and removed with the CoreAudio property-listener API. Wave microphone gain, hardware mute, headphone level, and Gain Lock use a separate Wave device-settings model and parameter paths.

The recovered code confirms that these control mechanisms are separate. It does not prove whether a macOS input or output slider changes a Wave hardware field, whether Wave Link reflects that change in its device-settings view, or what happens after Wave Link closes.

The recovered Wave:3 schema defines Gain Lock as a switch that ignores input-volume `SET_CUR` requests from the operating system. Its recovered default is off. The schema description applies to input volume, not to microphone mute. Elgato's [Wave Gain Lock guide](https://help.elgato.com/hc/en-us/articles/360050731352-Elgato-Wave-Link-Wave-Gain-Lock) states that, when the switch is on, applications cannot change microphone gain, while the physical dial and Wave Link can still change it.

This means Gain Lock is an optional hardware policy. LibreWave must show and preserve the device setting. It must not force the setting on during setup. When Gain Lock is enabled, an operating-system slider for the physical Wave capture endpoint may stop changing hardware gain by design. A LibreWave software microphone endpoint can still have a separate, working software level.

## Current evidence

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

Until the parity matrix is complete, LibreWave keeps hardware gain and headphone level separate from ordinary software endpoint volume. This avoids the current Linux failure where desktop policy becomes an accidental hardware writer.

The operating-system microphone slider changes software gain on the LibreWave microphone endpoint. Gain in the device settings and on the physical dial changes the Wave preamp. Gain Lock protects that preamp from outside control requests, but it does not disable LibreWave's software level.

The default system playback target is a LibreWave software input channel, as Wave Link uses virtual inputs for system and application audio. Its operating-system volume is a software channel value. The Wave headphone level remains a hardware setting controlled by the device settings and physical dial. LibreWave will not expose the raw physical Wave sink as a second ordinary desktop output while it manages the device.

Every synchronized path tags the change origin as device, LibreWave client, or operating system. Applying an observed change must not create a second outgoing change to the origin. This is the Linux equivalent of the feedback guards recovered on Windows.

No product documentation may claim exact Wave Link parity while required cells remain `unknown` or `inferred`.
