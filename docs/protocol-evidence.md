# Protocol evidence

## Evidence records

LibreWave keeps reviewed schemas, generators, architecture notes, and sanitized fixtures in this repository. Each imported record identifies its evidence class and collection method without depending on a developer's local directory layout.

Vendor application bundles, drivers, artwork, and copied UI assets do not move into LibreWave.

## Evidence hierarchy

Use evidence in this order:

1. A reproducible physical observation with exact device and API identification.
2. An exact static code path in the recovered Wave Link application or device library.
3. A recovered schema with matching model, version, message identifier, and payload size.
4. An independent implementation.
5. A hypothesis based on a related device.

The lower items help to design a test. They do not override a conflicting higher item.

## Known Wave:3 baseline

The recovered Wave:3 API 5 configuration message is 16 bytes. It includes microphone gain, mute, Clipguard, low cut, headphone volume and mute, direct monitor balance, knob selection, LED controls, and gain lock.

The gain-lock field states that it ignores input-volume `SET_CUR` requests from the operating system. The recovered schema default is off. Vendor documentation describes the same behavior on Windows and macOS. LibreWave still needs a reversible physical validation before it writes the field.

One compared implementation agrees with several core offsets but omits fields from the same message and treats one status message as a meter message. LibreWave will use the exact recovered schema and physical reads instead of selecting a layout by USB product identifier alone.

## Wave:3 control semantics

The reviewed API 5.3 and 5.4 `/config` messages use the same 16-byte layout. The following values have enough evidence for typed read-only decoding:

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

LibreWave keeps these hardware values separate from desktop endpoint levels. The typed protocol view is derived from a complete configuration read. The raw baseline stays available so a future reviewed transaction can preserve reserved bytes.

The Wave:3 configuration does not contain Wave Link mixer faders. Mixer channel values belong to the routing and engine layers. LibreWave therefore does not pretend that a hardware configuration read provides mixer state.

LibreWave now has a typed transaction path for these reviewed API 5.3 and 5.4 fields. It admits the exact device and API, changes one semantic field in a complete baseline, preserves protected bytes, reads the complete message before and after the write, and attempts restoration after a failure. Default tests use fake transports and do not write to live hardware.

Physical Wave:3 API 5.4 reads can report microphone gain between the reviewed 0.5 dB write steps, including raw Q8.8 value 4726 (about 18.46 dB). LibreWave accepts in-range Q8.8 values on reads and preserves their exact bytes when it writes another field. Authored gain writes still require 0.5 dB steps.

Physical Wave:3 API 5.4 tests verified Low-cut readback and the control
transactions used for microphone gain, microphone mute, headphone level,
monitor balance, and knob selection. LED observations establish display
behavior for the conditions below. They do not establish audio attenuation,
blend feed weights, or mute timing. Other writable fields still require
physical validation. Each test plan must name the field, original value,
requested value, readback, and safe restoration.

## Wave:3 level LEDs

The seven level LEDs do not have a pattern or count field in the recovered
Wave:3 API 5.3 or 5.4 schemas. These APIs have three messages: `/config`
(message 0, 16 bytes), `/status` (message 1, 8 bytes), and `/version`
(message 10, 54 bytes). The two status fields are touch duration and touch
signal. They are not audio meters or LED states.

The configuration supplies these candidate display values:

| Selected mode | Reported value | Reviewed range |
| --- | --- | --- |
| Microphone | `/config/input_gain` | 0 to 40 dB |
| Headphone | `/config/headphone_volume` | -60 to 0 dB |
| Mix | `/config/direct_monitor` | 0 to 100 percent |

`/config/all_leds_off` is at byte 13. `/config/leds_flip` is at byte 14.
The recovered flip description reverses the LEDs and knob for an inverted
microphone. This does not establish the segment thresholds or mix direction.
The reserved bytes at offsets 2 and 3 remain reserved. Do not interpret them
as an LED pattern.

The evidence is pinned to the WaveAPI framework from Wave Link 3.2.2 build
2896, SHA-256 `0c9541ff37662b4e35538bd7040d0706783cce132d42fca2c531322e85149860`.
A static x86-64 inspection of `LWT::Wave3Device::open(bool)` at VM address
`0xb16120` confirms that it passes the three-entry `LWT::Wave3::known_apis`
table at `0xc5bdc0` to `lookupSessionAPI`, then initializes tick handling.
The normalized message and field catalogs describe those API entries.
The framework was not loaded or executed during this inspection.

The application executable from the same build, SHA-256
`6435476d1c041831a09bc5501ec0820603c1b9f9b57cbe1025868405605d197c`,
contains indicator paths for newer devices. Those paths are absent from the
original Wave:3 schemas and cannot define its seven level LEDs. A bounded
static search did not recover an original Wave:3 segment calculation.
Hash matching identifies the analyzed inputs. It does not verify their vendor
signatures or physical behavior.

These facts support a device-side display derived from the selected value,
rather than a reported segment pattern. This is an inference. A physical
Wave:3 API 5.4 test brackets Microphone-mode and Headphone-mode count
transitions on the reviewed 0.5 dB write grid. Mix-mode observations establish
one LED position at each 5-percent value. Fractional thresholds, flip behavior,
and most mute states remain unverified. The application uses the measured
brackets and positions. It leaves the dots dark inside unmeasured fractional
transition intervals and for unverified states. Software audio peaks cannot
substitute for this evidence.

### Observed Microphone-mode LED counts

Device and API: Wave:3 API 5.4. Control path: daemon `InputGain`.
Measurement: exact gain readback and manual fully lit LED count. Conditions:
audio inactive, playback paused, IEMs out, Microphone selector, microphone mute
and both LED flags off. Other configuration fields unchanged.

There are 33 rising observations and 14 falling confirmations. Rising
intermediate values are approached from 0 dB; falling values from 40 dB.
Both directions agree at every tested boundary. At 0 dB, the count is 0.
At 40 dB, the count is 7.

| New lit count | Lower tested gain and count | Upper tested gain and count |
| --- | --- | --- |
| 1 | 2.5 dB: 0 | 3.0 dB: 1 |
| 2 | 8.5 dB: 1 | 9.0 dB: 2 |
| 3 | 14.5 dB: 2 | 15.0 dB: 3 |
| 4 | 20.5 dB: 3 | 21.0 dB: 4 |
| 5 | 26.5 dB: 4 | 27.0 dB: 5 |
| 6 | 32.5 dB: 5 | 33.0 dB: 6 |
| 7 | 39.5 dB: 6 | 40.0 dB: 7 |

The rightmost-dot data contains 12 observations. At 38.0, 38.5, 39.0, and
39.5 dB, six dots are fully lit and the rightmost dot is off. At 40.0 dB,
all seven dots are fully lit in three observations. No dim or blinking state
is reported. Historical final gain: 15.5 dB. Every other hardware field is
unchanged.

These are transition brackets, not exact fractional thresholds. Do not infer
the seventh transition from the spacing of the first six. The sanitized
[observations](../crates/librewave-ui/tests/fixtures/wave3-api5-4-microphone-led-counts.json)
record raw Q8.8 gain and manual counts. They apply to the tested device and
API only.

### Observed microphone mute

Device: Wave:3, USB `0fd9:0070`, USB `bcdDevice` `0x0124`, API 5.4.
Configuration message: `/config`, identifier 0, 16 bytes. Fixed-point values
use signed little-endian Q8.8.

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

Physical tap-mute is observed by the daemon. CLI hardware-mute commands use
the same daemon path as the UI and change the physical ring. PipeWire source
volume remains 0.40 without software mute. Hardware mute and software mute
are separate values. Final hardware mute is off; the raw baseline above is
preserved.

The [mute data](../crates/librewave-ui/tests/fixtures/wave3-api5-4-mute-led-positions.json)
contains three LED observations. Muted level positions are verified only at
exact gain raw 4096. Audio attenuation, mute timing, and native UI pointer
input are unverified. No audio was generated.

### Observed Headphone-mode LED counts

Device and API: the Wave:3 API 5.4 above. Selector: Headphone. Microphone
mute, headphone mute, and both LED flags: off. Microphone gain: raw 4096
(16 dB). Monitor balance: raw 0 (0 percent). Write grid: 0.5 dB, raw step 128.
LEDs fill from left to right.

| New lit count | Lower tested level and count | Upper tested level and count |
| --- | --- | --- |
| 1 | -54.5 dB: 0 | -54.0 dB: 1 |
| 2 | -26.0 dB: 1 | -25.5 dB: 2 |
| 3 | -17.0 dB: 2 | -16.5 dB: 3 |
| 4 | -11.0 dB: 3 | -10.5 dB: 4 |
| 5 | -6.5 dB: 4 | -6.0 dB: 5 |
| 6 | -3.5 dB: 5 | -3.0 dB: 6 |
| 7 | -0.5 dB: 6 | 0.0 dB: 7 |

At -60 dB, the count is 0. At -4.5 dB, the count is 5. At 0 dB, the count
is 7. Rising and falling observations agree at every tested boundary.
These are transition brackets, not exact fractional thresholds. Fractional
cutoffs, mute effects, and flipped patterns are unverified.

The [headphone data](../crates/librewave-ui/tests/fixtures/wave3-api5-4-headphone-led-counts.json)
contains 50 observations with raw values and LED positions. Final headphone
level: raw -1152 (-4.5 dB). Final selector: Microphone. Unrelated controls match
the baseline. No audio was generated.

### Observed Mix-mode LED positions

Device and API: the Wave:3 API 5.4 above. Selector: Mix. Microphone mute,
headphone mute, and both LED flags: off. Microphone gain: raw 4096 (16 dB).
Headphone level: raw -1152 (-4.5 dB). Write grid: 5 percent, raw step 1280.

Mix lights one level LED. It moves from right to left as
`/config/direct_monitor` increases. Positions are numbered 1 through 7 from
left to right. The rightmost mode LED is lit.

| Tested monitor balance | Lit level LED |
| --- | --- |
| 0 percent | 7 (rightmost) |
| 5, 10, 15 percent | 6 |
| 20, 25, 30 percent | 5 |
| 35, 40, 45, 50 percent | 4 (center) |
| 55, 60, 65 percent | 3 |
| 70, 75, 80 percent | 2 |
| 85, 90, 95, 100 percent | 1 (leftmost) |

Rising and falling observations agree at every tested position change. Values
between the tested 5-percent points, mute effects, flipped patterns, and audio
feed weights at 0 and 100 percent are unverified. No audio was generated.

The [blend data](../crates/librewave-ui/tests/fixtures/wave3-api5-4-blend-led-positions.json)
contains 33 observations. Final monitor balance: raw 0 (0 percent). Final
selector: Microphone. Unrelated controls match the baseline. Software source
and sink volumes remain 0.40 and 0.72.

### Physical observation plan

Prerequisites: approve the observation separately. Keep LibreWave audio off,
use one read-only daemon with an isolated profile, and leave headphones and
hardware levels unchanged. Do not send hardware setting commands.

1. Record the exact API, selector, raw gain, raw headphone level, raw monitor balance, and LED flags.
2. Count the lit LEDs and record their positions in Microphone mode.
3. Press the selector once and record the Headphone pattern and configuration.
4. Press the selector once and record the Mix pattern and configuration.
5. Press the selector once to return to Microphone mode.
6. Verify the original selector and all reported values.

Allow at most two minutes. Stop on disconnect, unsupported data, or an
unintended level change. Preserve a changed physical value and report it;
do not restore it with an unapproved USB write. This observation can identify
which values affect the pattern, but it cannot establish every threshold.
A later approved threshold sweep must name each physical value it can change,
protect headphone listening levels, and review restoration before it starts.
Normal gain writes use 0.5 dB steps. If the observed baseline is between those
steps, agree on the final gain before the first write. Do not round it silently.

## Generated catalog

Generated protocol code must be reproducible from checked-in normalized evidence. A generator change and its output belong in the same commit.

Each schema record includes:

- Device family and model.
- Protocol API version.
- Message path and identifier.
- Payload size.
- Read and write access.
- Field offset and size.
- Scalar encoding, fractional bits, or enum mapping.
- Range and step when known.
- Evidence source and confidence.

Generated code does not decide whether a field is safe to write. The device-session allowlist is a separate reviewed policy.

## Fixtures

A fixture must remove device serials and unrelated user data. Its metadata names the device model, API version, collection method, expected schema, and whether it came from a read-only session.

Tests must cover short payloads, long payloads, unsupported versions, invalid booleans, invalid enums, fixed-point boundaries, reserved-byte preservation, stale baselines, and failed readback.
