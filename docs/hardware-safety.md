# Hardware safety

This document defines the operations LibreWave can perform on connected Wave hardware.

## Operation classes

### Host inspection

These operations do not send a request to the USB device:

- Read sysfs and udev properties.
- Read ALSA control descriptions and values through the kernel.
- Inspect PipeWire and WirePlumber state.
- Read saved LibreWave state and sanitized fixtures.

Host inspection can run by default.

### Device reads

These operations send a read request through a reviewed transport:

- Read the product and API version.
- Read a known configuration or status message.
- Subscribe to a known event message.

A device read must have an exact expected request and a maximum response length. Short, long, or malformed responses are errors.

### Reversible control writes

The initial write allowlist can include normal controls such as gain, mute, headphone volume, direct monitoring, Clipguard, low cut, lighting, and gain lock. A field enters the allowlist only after its exact device and API schema is verified.

Every write must be reversible through another normal control write. The implementation must preserve the original value during a physical integration test.

### Prohibited operations

LibreWave must not implement or invoke:

- Firmware download or upload.
- Flash-memory writes.
- DFU or bootloader entry.
- Device reset or recovery commands.
- Kernel-driver replacement or rebinding.
- Undocumented requests that could change persistent firmware state.

Production binaries must not contain a hidden command for these operations.

## Device admission

A session becomes writable only when all of these values match a reviewed schema:

1. USB vendor and product identifier.
2. Device family and model.
3. Protocol API major and minor version where available.
4. Message identifier.
5. Payload size.
6. Field encoding and access mode.

An unknown value keeps the session in diagnostic read-only mode. Similar model names and nearby firmware versions are not sufficient evidence.

## Write transaction

A normal control write follows one path:

1. Read the complete message into the device state shadow.
2. Confirm that the baseline has the exact expected size and schema.
3. Validate the requested semantic value.
4. Encode only the selected field in a copy of the baseline.
5. Preserve reserved and unknown bytes.
6. Send the complete message.
7. Read the message again.
8. Compare the selected field and all protected bytes.
9. Report success or a precise mismatch.

The session serializes writes per device. It rejects stale transactions instead of merging them with a newer baseline.

## Physical test rules

Hardware tests are separate from the default test suite. A test command must state whether it is read-only or writable before it opens the device.

A writable test must:

- Name each field it can change.
- Show the original and requested semantic values.
- Reject prohibited or out-of-range values before USB I/O.
- Change one field at a time.
- Restore the original value after verification when restoration is safe.
- Stop after an unexpected disconnect, reset, schema mismatch, or readback failure.

Tests must not run in parallel against one physical device.

## Sensitive data

Logs and fixtures must remove USB serial numbers, user names, application titles, and unrelated audio metadata. A fixture should use a stable test identifier that cannot be traced back to one physical microphone.
