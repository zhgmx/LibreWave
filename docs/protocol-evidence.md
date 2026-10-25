# Protocol evidence

## Evidence records

This repository holds the reviewed schema, this evidence record, and sanitized fixtures. Each record names its evidence class and collection method. It does not depend on a developer's local directory layout.

Vendor application bundles, drivers, artwork, and copied UI assets stay out of LibreWave.

## Evidence hierarchy

Use evidence in this order:

1. A reproducible physical observation with exact device and API identification.
2. An exact static code path in the recovered Wave Link application or device library.
3. A recovered schema with matching model, version, message identifier, and payload size.
4. An independent implementation.
5. A hypothesis based on a related device.

The lower items help to design a test. They do not override a conflicting higher item.

## Wave:3 configuration baseline

The recovered Wave:3 API 5 configuration message is 16 bytes. It contains microphone gain, microphone mute, Clipguard, Low-cut, headphone level and mute, direct monitor balance, knob target, LED controls, and Gain Lock.

The recovered Gain Lock description says that the device ignores input-volume `SET_CUR` requests from the operating system when the field is on. The recovered default is off. Vendor documentation describes the same behavior on Windows and macOS.

One compared implementation agrees with several core offsets. It omits fields from the same message and treats one status message as a meter message. LibreWave uses the exact recovered schema and the API version that the device reports. It does not select a layout from the USB product identifier alone.

## Wave:3 control semantics

The reviewed API 5.3 and 5.4 `/config` messages use the same 16-byte layout:

| Control | Wire location | Unit and limits | Confidence |
| --- | --- | --- | --- |
| Microphone gain | Bytes 0 to 1, signed little-endian Q8.8 | 0 to 40 dB; reviewed write step is 0.5 dB | Recovered schema, independent implementation, and physical read |
| Microphone mute | Byte 4 | Boolean | Recovered schema |
| Clipguard | Byte 5 | Boolean | Recovered schema |
| Low-cut filter | Byte 6 | Boolean | Recovered schema |
| Headphone output | Bytes 7 to 8, signed little-endian Q8.8 | -60 to 0 dB, 0.5 dB steps | Recovered schema and independent implementation |
| Headphone mute | Byte 9 | Boolean | Recovered schema |
| Direct monitor balance | Bytes 10 to 11, signed little-endian Q8.8 | 0 to 100 percent, 5 percent steps | Recovered schema and independent implementation |
| Physical knob target | Byte 12 | Microphone, headphone, or mix | Recovered schema |
| Gain Lock | Byte 15 | Boolean. This is a device policy, not a LibreWave software-volume lock. | Recovered schema and vendor behavior description |

Bytes 2 and 3 are reserved. Byte 13 is `/config/all_leds_off` and byte 14 is `/config/leds_flip`. The knob target uses wire values 1 (Microphone), 2 (Headphone), and 3 (Mix). Any other value is a decode error.

These hardware values stay separate from desktop endpoint levels. The typed view comes from a complete configuration read. The raw 16-byte baseline is kept so that a write preserves the reserved bytes.

The Wave:3 configuration does not contain Wave Link mixer faders. Mixer channel values belong to the routing and engine layers, so a hardware configuration read does not supply mixer state.

The transaction path admits only Wave:3 API 5.3 and 5.4. It changes one semantic field in a complete baseline and preserves the other bytes. It reads the complete message before and after the write and makes one restoration attempt after a failure. Default tests use fake transports and do not write to live hardware.

Physical Wave:3 API 5.4 reads can report microphone gain between the 0.5 dB write steps. One observed raw Q8.8 value is 4726 (about 18.46 dB). LibreWave accepts any in-range Q8.8 gain on a read and keeps its exact bytes when it writes another field. A gain write must be on a 0.5 dB step.

These physical effects are not verified:

- The all-LEDs-off flag. With it on, the LEDs showed no visible change.
- The LED flip effect on the knob direction.
- The audio effect of Clipguard. The device has no Clipguard indicator.
- Gain Lock rejection of input-volume requests from the operating system.
- Audio attenuation, monitor feed weights, and mute timing.

The LED observations below apply to the stated conditions only. Each physical test plan must name the field, original value, requested value, readback, and safe restoration.

## Wave:3 level LEDs

The recovered Wave:3 API 5.3 and 5.4 schemas have no pattern or count field for the seven level LEDs. These APIs have three messages: `/config` (message 0, 16 bytes), `/status` (message 1, 8 bytes), and `/version` (message 10, 54 bytes). The two status fields are touch duration and touch signal. They are not audio meters or LED states.

The configuration supplies these candidate display values:

| Selected mode | Reported value | Reviewed range |
| --- | --- | --- |
| Microphone | `/config/input_gain` | 0 to 40 dB |
| Headphone | `/config/headphone_volume` | -60 to 0 dB |
| Mix | `/config/direct_monitor` | 0 to 100 percent |

The recovered `leds_flip` description reverses the LEDs and knob for an inverted microphone. With the flag on, the seven level LEDs show the same pattern mirrored, starting from the other side. Do not interpret the reserved bytes at offsets 2 and 3 as an LED pattern.

The evidence is pinned to the WaveAPI framework from Wave Link 3.2.2 build 2896, SHA-256 `0c9541ff37662b4e35538bd7040d0706783cce132d42fca2c531322e85149860`. A static x86-64 inspection of `LWT::Wave3Device::open(bool)` at VM address `0xb16120` shows that it passes the three-entry `LWT::Wave3::known_apis` table at `0xc5bdc0` to `lookupSessionAPI` and then initializes tick handling. The normalized message and field catalogs from this inspection describe those API entries. The inspection did not load or execute the framework.

The application executable from the same build, SHA-256 `6435476d1c041831a09bc5501ec0820603c1b9f9b57cbe1025868405605d197c`, contains indicator paths for newer devices. Those paths are absent from the original Wave:3 schemas and cannot define its seven level LEDs. A bounded static search did not find an original Wave:3 segment calculation. The hashes identify the analyzed inputs. They do not verify vendor signatures or physical behavior.

These facts support the inference that the device derives the display from the selected value and does not report a segment pattern. Physical Wave:3 API 5.4 tests bracket the Microphone-mode and Headphone-mode count transitions on the 0.5 dB write grid. Mix-mode tests give one LED position at each 5 percent value. Fractional thresholds are unverified. The application shows the measured brackets and positions. It leaves the dots dark inside unmeasured fractional intervals and for unverified states. Software audio peaks cannot replace this evidence.

### Observed Microphone-mode LED counts

Device and API: Wave:3 API 5.4. Control path: daemon `InputGain`. Measurement: exact gain readback and a manual count of fully lit LEDs. Conditions: audio inactive, playback paused, IEMs out, Microphone selector, microphone mute and both LED flags off. Other configuration fields unchanged.

There are 33 rising observations and 14 falling confirmations. Rising intermediate values are approached from 0 dB and falling values from 40 dB. Both directions agree at every tested boundary. At 0 dB, the count is 0. At 40 dB, the count is 7.

| New lit count | Lower tested gain and count | Upper tested gain and count |
| --- | --- | --- |
| 1 | 2.5 dB: 0 | 3.0 dB: 1 |
| 2 | 8.5 dB: 1 | 9.0 dB: 2 |
| 3 | 14.5 dB: 2 | 15.0 dB: 3 |
| 4 | 20.5 dB: 3 | 21.0 dB: 4 |
| 5 | 26.5 dB: 4 | 27.0 dB: 5 |
| 6 | 32.5 dB: 5 | 33.0 dB: 6 |
| 7 | 39.5 dB: 6 | 40.0 dB: 7 |

The rightmost-dot data has 12 observations. At 38.0, 38.5, 39.0, and 39.5 dB, six dots are fully lit and the rightmost dot is off. At 40.0 dB, all seven dots are fully lit in three observations. No dim or blinking state was reported. Final gain: 15.5 dB. Every other hardware field is unchanged.

These are transition brackets, not exact fractional thresholds. Do not infer the seventh transition from the spacing of the first six. The sanitized [observations](../crates/librewave-ui/tests/fixtures/wave3-api5-4-microphone-led-counts.json) record raw Q8.8 gain and manual counts. They apply to the tested device and API only.

### Observed microphone mute

Device: Wave:3, USB `0fd9:0070`, USB `bcdDevice` `0x0124`, API 5.4. Configuration message: `/config`, identifier 0, 16 bytes. Fixed-point values use signed little-endian Q8.8.

| Condition | Raw value | Semantic value |
| --- | --- | --- |
| Microphone gain | 4096 | 16 dB |
| Headphone level | -1152 | -4.5 dB |
| Monitor balance | 0 | 0 percent |
| Knob target | 0 | Microphone |
| Clipguard | 1 | On |
| Low cut, headphone mute, LED flags, Gain Lock | 0 | Off |

| Hardware microphone mute | Ring | Lit level positions, left to right |
| --- | --- | --- |
| Off | White | 1, 2, 3 |
| On | Red | 1, 2, 3 |

The daemon observes a physical tap-mute. CLI hardware-mute commands use the same daemon path as the UI and change the physical ring. PipeWire source volume stays at 0.40 without software mute. Hardware mute and software mute are separate values. The final hardware mute is off, and the raw baseline above is preserved.

The [mute data](../crates/librewave-ui/tests/fixtures/wave3-api5-4-mute-led-positions.json) has three LED observations at exact gain raw 4096.

Visual observations on the same device give the effect of each hardware mute on the level LEDs. They have no recorded raw values.

| Selector | Microphone mute | Headphone mute |
| --- | --- | --- |
| Microphone | Ring red, level LEDs unchanged | Level LEDs unchanged |
| Headphone | Ring red, level LEDs unchanged | All level LEDs off |
| Mix | Ring red, level LEDs unchanged | Level LEDs unchanged |

Audio attenuation, mute timing, and native UI pointer input are unverified. The tests generated no audio.

### Observed Headphone-mode LED counts

Device and API: the Wave:3 API 5.4 above. Selector: Headphone. Microphone mute, headphone mute, and both LED flags: off. Microphone gain: raw 4096 (16 dB). Monitor balance: raw 0 (0 percent). Write grid: 0.5 dB, raw step 128. LEDs fill from left to right.

| New lit count | Lower tested level and count | Upper tested level and count |
| --- | --- | --- |
| 1 | -54.5 dB: 0 | -54.0 dB: 1 |
| 2 | -26.0 dB: 1 | -25.5 dB: 2 |
| 3 | -17.0 dB: 2 | -16.5 dB: 3 |
| 4 | -11.0 dB: 3 | -10.5 dB: 4 |
| 5 | -6.5 dB: 4 | -6.0 dB: 5 |
| 6 | -3.5 dB: 5 | -3.0 dB: 6 |
| 7 | -0.5 dB: 6 | 0.0 dB: 7 |

At -60 dB, the count is 0. At -4.5 dB, the count is 5. At 0 dB, the count is 7. Rising and falling observations agree at every tested boundary. These are transition brackets, not exact fractional thresholds. Fractional cutoffs are unverified.

The [headphone data](../crates/librewave-ui/tests/fixtures/wave3-api5-4-headphone-led-counts.json) has 50 observations with raw values and LED positions. Final headphone level: raw -1152 (-4.5 dB). Final selector: Microphone. Unrelated controls match the baseline. The test generated no audio.

### Observed Mix-mode LED positions

Device and API: the Wave:3 API 5.4 above. Selector: Mix. Microphone mute, headphone mute, and both LED flags: off. Microphone gain: raw 4096 (16 dB). Headphone level: raw -1152 (-4.5 dB). Write grid: 5 percent, raw step 1280.

Mix lights one level LED. The LED moves from right to left as `/config/direct_monitor` increases. Positions are numbered 1 through 7 from left to right. The rightmost mode LED is lit.

| Tested monitor balance | Lit level LED |
| --- | --- |
| 0 percent | 7 (rightmost) |
| 5, 10, 15 percent | 6 |
| 20, 25, 30 percent | 5 |
| 35, 40, 45, 50 percent | 4 (center) |
| 55, 60, 65 percent | 3 |
| 70, 75, 80 percent | 2 |
| 85, 90, 95, 100 percent | 1 (leftmost) |

Rising and falling observations agree at every tested position change. Values between the tested 5 percent points and audio feed weights at 0 and 100 percent are unverified. The test generated no audio.

The [blend data](../crates/librewave-ui/tests/fixtures/wave3-api5-4-blend-led-positions.json) has 33 observations. Final monitor balance: raw 0 (0 percent). Final selector: Microphone. Unrelated controls match the baseline. Software source and sink volumes stay at 0.40 and 0.72.

### Further physical observations

Each further LED or threshold test needs separate approval. The plan must name each physical value it can change, protect headphone listening levels, and define restoration before the test starts. Keep LibreWave audio off and use one daemon with an isolated profile. Stop on disconnect, unsupported data, or an unintended level change. Report a changed physical value and do not restore it with an unapproved USB write.

Gain writes use 0.5 dB steps. If the observed baseline gain is between two steps, agree on the final gain before the first write. Do not round it silently.

## Schema catalog

The reviewed schema is the hand-written field table in `crates/librewave-protocol/src/wave3.rs`. The repository has no schema generator. Each schema records:

- Device model.
- Protocol API version.
- Message path and identifier.
- Payload size.
- Read and write access.
- Field offset and size.
- Scalar encoding, fractional bits, or enum mapping.
- Range and step.

This document records the evidence source and confidence for each field. Generated protocol code, if added, must be reproducible from checked-in normalized evidence, and a generator change and its output belong in the same commit.

The schema access mode does not decide whether a field is safe to write. The write allowlist is the separate reviewed `Wave3ControlChange` set, which an admitted `Wave3Session` applies.

## Fixtures

A fixture must not contain device serials or unrelated user data. Its metadata must name the device model, API version, collection method, expected schema, and whether the data came from a read-only session.

Tests must cover short payloads, long payloads, unsupported versions, invalid booleans, invalid enums, fixed-point boundaries, reserved-byte preservation, stale baselines, and failed readback.
