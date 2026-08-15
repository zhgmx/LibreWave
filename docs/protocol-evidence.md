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
| Microphone gain | Bytes 0 to 1, signed little-endian Q8.8 | 0 to 40 dB, 0.5 dB steps | Recovered schema and independent implementation |
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

Physical validation of each writable field is still required. The test plan must name the field, original value, requested value, readback, and safe restoration. Product documentation must not claim that physical write validation is complete until those tests pass.

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
