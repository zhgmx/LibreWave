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

- Read the `/version` message to get the API version.
- Read the complete `/config` message.

A device read has an exact expected request and a maximum response length. A short, long, or malformed response is an error.

### Reversible control writes

The write allowlist is the `Wave3ControlChange` set in `librewave-protocol`. It covers the eleven fields of the Wave:3 API 5.3 and 5.4 `/config` message: microphone gain, microphone mute, Clipguard, Low-cut, headphone level, headphone mute, direct monitor balance, knob target, all-LEDs-off, LED flip, and Gain Lock. A field enters the allowlist only after its exact device and API schema is verified. [Protocol evidence](protocol-evidence.md) states which fields also have physical validation.

Another normal control write can reverse every allowlisted write. A physical integration test must preserve the original value.

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

1. USB vendor and product identifier (`0fd9:0070` for Wave:3).
2. Device family and model.
3. Protocol API major and minor version (5.3 or 5.4).
4. Message identifier.
5. Payload size.
6. Field encoding and access mode.

An unsupported API version stops the probe before the configuration read. Similar model names and nearby firmware versions are not sufficient evidence.

## Write transaction

A normal control write follows one path:

1. Confirm that the caller's baseline matches the session's complete configuration.
2. Validate the requested semantic value against its range and step.
3. Encode only the selected field in a copy of the baseline. Reserved and unknown bytes stay unchanged.
4. Read the complete message from the device. Reject the write as stale if it differs from the baseline.
5. Send the complete message.
6. Read the complete message again.
7. Compare the readback with the complete expected message.
8. Report success or a precise mismatch.

One daemon-owned session accepts one mutable transaction at a time. The platform transport and daemon session code must keep only one admitted session for each device connection. A session rejects a stale transaction. It does not merge the transaction with a newer baseline.

The local IPC interface accepts semantic controls only. It does not accept setup packets, payload bytes, field offsets, message names, or arbitrary field names. Each `SetWave3Control` command includes the daemon-issued device generation. The daemon validates and persists a pending semantic command before it performs USB I/O. The GUI sends one `SetWave3Control` at a time with the current generation. A slider drag writes nothing until the pointer is released. Then the GUI sends one command for the last value.

After a failed transaction, the daemon clears the pending command. After a verified apply or an unchanged result, it promotes the command to managed desired state. If it cannot persist either step, it locks writes and reports persistence ambiguity. Admission of a topology that still has a pending command also locks writes with persistence ambiguity and restores nothing.

Reconnect restoration uses the same transaction path. Each managed field is a separate transaction, so restoration can make partial progress. Any failure stops the sequence. The daemon locks the retained connection, or drops it when the device disconnected.

If a failure occurs after a write attempt, the transaction makes one restoration attempt. It writes the complete original payload and reads the message again. The result includes the first failure and the restoration result. After any failed transaction, the daemon locks writes on that connection. The lock reason states whether restoration was verified. Writes stay locked until a new admission probe succeeds.

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

Logs and fixtures must not contain USB serial numbers, user names, application titles, or unrelated audio metadata. A fixture uses a stable test identifier that cannot identify one physical microphone.

## Control ownership

The physical device owns microphone gain and mute, headphone level and mute, direct monitor balance, and knob target. Admission and reconnect read these controls and do not restore saved values, so adjustments made while the daemon was stopped stay in place. An explicit client command still uses the generation check and complete readback verification.

On reconnect, LibreWave restores only the non-level settings that a client changed through LibreWave: Clipguard, Low-cut, the LED settings, and Gain Lock. A pending or ambiguous write blocks new hardware writes until recovery resolves it.
