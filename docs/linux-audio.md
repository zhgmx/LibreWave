# Linux audio integration

Status: the direct host owns ALSA resources and inspects PipeWire through safe Rust wrappers. Two offline seams now cover separate scheduling boundaries. The capture-clock transport tests bounded engine processing. The PipeWire graph seam tests one graph callback and the four deliberate endpoints. Neither seam is connected to the production host, so the lifecycle cannot report the graph as ready.

## Reference system

The first test system uses Fedora 44, PipeWire 1.6, WirePlumber 0.5, KDE Plasma, and a Wave:3. Tests cover Wayland and X11 where desktop behavior differs.

## Validated ownership boundary

Some Wave microphones produce silent capture when playback starts before capture. A local workaround confirmed the required order by keeping capture active through a null sink and creating replacement playback and capture endpoints. A sanitized read-only PipeWire inspection confirmed that the null sink appears in the ordinary sink list beside the replacement endpoints. That workaround is useful evidence, but it is not the product design. A Lua script owns its lifecycle.

LibreWave uses one physical audio owner. `librewaved` will open the Wave:3 ALSA capture PCM directly, consume capture frames, confirm that capture is active, and then open the playback PCM. WirePlumber must not create or reserve the admitted physical card. PipeWire will carry only the product endpoints that `librewave-core` defines.

`librewave-platform-linux` contains the policy artifacts, ordered lifecycle, and direct host. The host uses `alsa` 0.12.1 and `pipewire` 0.9.2. The PipeWire Rust crate does not wrap `pw_filter`. A small C shim includes the installed PipeWire headers and owns only the ABI-sensitive filter event, hook, and process-position layouts. Rust owns graph policy, lifecycle, validation, and processing. None of this code is connected to daemon startup, setup, or the running graph.

The host correlates an admitted USB candidate to one ALSA card. It requires one stable ALSA card identifier and one PCM device for each direction. It repeats the sysfs and procfs correlation immediately before each open. The open uses the revalidated card identifier, device number, and subdevice zero. A changed card number, identifier, topology, or PCM set stops startup.

The daemon must supply an exact physical PCM format. The host has no default sample rate, channel count, period size, or buffer size. It currently supports interleaved signed 32-bit little-endian samples. ALSA must apply the complete requested format without adjustment.

## WirePlumber policy

The Wave:3 policy renders `80-librewave-wave3.conf` as a WirePlumber 0.5 SPA-JSON fragment. Its one rule matches both exact normal-mode USB properties:

```text
device.vendor.id = "0x0fd9"
device.product.id = "0x0070"
```

The rule sets only `device.disabled = true`, which disables the exact card in the WirePlumber ALSA monitor. The fragment does not set `node.disabled`, create a node, load a component, or run a Lua script.

WirePlumber documents `device.disabled` as the property that removes a matched card or device. See the [WirePlumber ALSA configuration reference](https://pipewire.pages.freedesktop.org/wireplumber/daemon/configuration/alsa.html).

The rule has no serial, name, family, class, or regular-expression match. A different vendor ID or adjacent product ID does not match it. The current setup does not install or activate this rule. A future setup can install it only after the production audio host can take ownership. Hardware validation must then confirm that WirePlumber created no physical nodes and holds no reservation before `librewaved` takes ownership.

## Device access policy

The Wave:3 policy also renders `70-librewave-wave3.rules`. The udev rule matches the USB device object with vendor `0fd9` and product `0070`, then adds `TAG+="uaccess"`.

After it installs or removes this rule, the lifecycle reloads the udev rules. It then sends a change event only to normal-mode Wave:3 USB device objects with the same vendor and product values. It does not use a broad USB trigger. If the privileged rule or refresh operation fails, the lifecycle fails and does not claim that access is current. Rollback or journal recovery preserves or restores the prior rule state.

The rule does not use `MODE="0666"`. It does not match a USB class or interface, and it does not grant access to DFU or another product mode. `snd_usb_audio` stays bound during normal operation.

## Runtime ownership plan

The direct backend must follow this one startup path:

```mermaid
flowchart TD
    ADMIT["Admit exact Wave:3 normal-mode product"] --> UNMANAGED["Confirm WirePlumber did not create or reserve the ALSA card"]
    UNMANAGED --> CAPTURE["Open the physical ALSA capture PCM"]
    CAPTURE --> CONSUME["Consume capture frames in librewaved"]
    CONSUME --> CONFIRM{"At least one capture frame confirmed?"}
    CONFIRM -->|No| DEGRADED["Tear down the attempt and report a degraded state"]
    CONFIRM -->|Yes| PLAYBACK["Open the physical ALSA playback PCM"]
    PLAYBACK --> ENDPOINTS["Publish deliberate PipeWire product endpoints"]
    ENDPOINTS --> RESTORE["Restore software routes and levels"]
    RESTORE --> READY["Report the graph as ready"]
```

The daemon must not open playback when capture is inactive or has produced no frames. A failed step tears down objects from that attempt and reports a named degraded state. Recovery after hotplug or a PipeWire or WirePlumber restart uses the same sequence.

The lifecycle state machine enforces this order through one host trait. The production host configures and starts capture before its first bounded ALSA wait. Before the worker starts, the host allocates one buffer from the checked period size, channel count, and sample width.

The capture loop does not allocate or lock. Atomic counters report frames and recovered xruns. After ALSA recovers an xrun, the host restarts capture only if the PCM is prepared. It accepts a running PCM and fails for any other state. Timeout, disconnect, an unrecoverable PCM error, worker shutdown, and worker join have distinct outcomes. Playback cannot open until the worker has consumed at least one frame.

The host owns its PipeWire core connection, capture worker, and playback PCM. One idempotent teardown path disconnects PipeWire and closes both PCM resources. Its resource snapshot reports whether it owns a PipeWire connection resource. It also reports capture and playback activity, frame and xrun counts, deliberate endpoint identities, and the readiness boundary. The snapshot does not contain ALSA card numbers, PCM names, USB topology, or PipeWire object identifiers.

## Product endpoints

The desktop may see only these deliberate product endpoints:

| Endpoint ID | Display name | PipeWire node name | Flow | PipeWire direction | Media class |
| --- | --- | --- | --- | --- | --- |
| `System` | System | `librewave.system` | Public sink | Input | `Audio/Sink` |
| `Microphone` | Microphone | `librewave.microphone` | Public source | Output | `Audio/Source` |
| `MonitorMix` | Monitor mix | `librewave.monitor-mix` | Public source | Output | `Audio/Source` |
| `StreamMix` | Stream mix | `librewave.stream-mix` | Public source | Output | `Audio/Source` |

All four plans set `node.virtual=true`. Applications render to the System sink. Its frames supply the engine's System logical source. Physical Wave:3 capture supplies the Microphone logical source. The engine produces only Microphone, Monitor Mix, and Stream Mix output buffers. The System sink does not increase the engine output count. Monitor Mix is also the intended signal for physical headphone playback.

The physical Wave capture and playback PCMs are not desktop endpoints. LibreWave must not publish a keepalive sink, null sink, raw helper source, duplicate physical Wave node, or another implementation object as an ordinary device. KDE, `wpctl status`, and PulseAudio-compatible listings form the visibility acceptance test. Low-level diagnostic tools may still show internal objects needed to inspect the graph.

The host connects to the user's existing PipeWire instance and completes a registry round trip. It checks the admitted candidate's exact ALSA card number. A Device global must have the Wave:3 vendor and product values, and its `api.alsa.card` value must identify that card. A Node global matches through the card component of `api.alsa.path`. Node globals do not need vendor or product properties. Startup fails while any matching Device or Node global remains visible.

Endpoint plans come only from `librewave-core`. The production adapter still returns `EndpointStreamTransportUnavailable` before it creates an endpoint and records the matching `GraphNotReady` reason. Playback remains prepared, not active, at this boundary. The adapter does not publish silence or report the graph as ready. The offline graph seam is not a production publication path. Independent ALSA and PipeWire clocks still need ASRC boundaries before the endpoints can carry live hardware audio.

## Offline PipeWire graph seam

The crate contains a private five-node graph seam. It exists to verify PipeWire scheduling and ownership. The seam has no call path from the production host.

The System `Audio/Sink` node is the PipeWire driver. Its permanent monitor links keep the graph active when no application renders to System. Microphone, Monitor Mix, and Stream Mix are `Audio/Source` nodes. A fifth node owns the mixer callback and eight planar F32 ports. That node has no `Audio/Sink` or `Audio/Source` media class. The client creates all five nodes and all eight permanent links while the filter is inactive, validates the observed graph, and activates the filter once. It does not create a hidden driver, timer, server configuration, lingering object, or helper endpoint.

One callback receives the graph driver ID, rate, position, quantum, two System input buffers, and six output buffers. The callback accepts 48 kHz and one fixed quantum for an attempt. A missing buffer, PipeWire error or disconnect, pause, driver change, rate change, quantum change, position discontinuity, overlapping or invalid buffer range, processor error, or panic fails that attempt. A fixed-capacity atomic record carries fault details to the control thread. The callback does not tear down the graph.

System link state uses a generation published from registry events. A producer attach has one priming quantum. A detach has one valid drain quantum, then System becomes semantic silence. The test removes producer links before it deactivates the producer. This order preserves the adapter tail and prevents stale samples from entering a later idle cycle.

The explicit integration test starts its own PipeWire daemon. It uses temporary runtime and configuration directories and a private remote name. It does not use WirePlumber, ALSA, USB, user services, or persistent configuration. The test repeats the full lifecycle at fixed quanta of 64, 128, 256, and 512 frames. It verifies the four endpoint classes, the hidden filter, all eight F32 ports, one System driver ID, continuous graph position, deterministic mixer vectors, idle processing, producer and consumer churn, one priming quantum, one drain quantum, and zero residual LibreWave objects. System input has exactly one quantum of adapter latency. Filter output reaches all three source adapters in the same graph cycle.

A separate test changes the live quantum from 128 to 64 frames. PipeWire pauses the filter before it delivers a block with the new quantum. The seam records that pause as an attempt fault and tears down the graph. It does not claim seamless live quantum changes.

Run the integration gate only on a host with the PipeWire development files, `pipewire`, `pw-cli`, and `pw-metadata`:

```text
LIBREWAVE_PIPEWIRE_INTEGRATION=1 cargo test -p librewave-platform-linux audio_host::pipewire_graph::tests::integration::private_pipewire_graph -- --ignored --exact --test-threads=1
```

The default test suite does not start a daemon and does not silently skip this gate. CI does not run the gate yet because the current runner contract does not install the PipeWire daemon tools. Add it when CI supplies a fixed PipeWire runtime image and runs the command as a separate serial job.

## Portable mixer contract

`librewave-engine` processes stereo interleaved 32-bit float audio at 48 kHz. Construction sets a portable logical source collection with a maximum of 12 sources. It also sets the microphone source and the maximum frames that one call can process. Each call can use a different frame count at or below that maximum. The Linux adapter is responsible for bridging ALSA periods and PipeWire quantum sizes to this contract.

Each source has separate monitor and stream routes. A route has an enabled value and a fader from -60.0 dB through +12.0 dB in 0.5 dB steps. The microphone endpoint receives the configured microphone source at unity. Monitor and stream route values do not change that endpoint. Hardware microphone gain, hardware microphone mute, and headphone level stay outside the engine.

The engine reports separate left and right peak and RMS values for each source and endpoint. It applies one complete pending control snapshot at the boundary before a nonzero block. Processing uses fixed-capacity state and does not allocate, free memory, lock, block, log, perform I/O, or call platform code.

## Offline capture-clock transport seam

This earlier test seam uses physical capture as its only processing clock. It is separate from the PipeWire graph seam and remains offline. One call accepts between one frame and the configured maximum number of complete stereo frames. A zero-length read means that capture made no progress. A partial stereo frame or an oversized block fails the transport attempt. The production ALSA worker is not connected to this seam in this milestone.

The System ingress is a bounded single-producer, single-consumer FIFO. It can join blocks with different frame counts, but it does not correct rate drift between independent clocks. The producer publishes its first frames before it marks the stream active. An idle or unconnected System sink supplies intentional silence, so microphone monitoring and headphone output do not depend on application playback. Once a System producer is active, insufficient frames are an underrun and fail the attempt. An overflow also fails the attempt. The availability count in an underrun report is the snapshot that rejected that block.

The seam converts S32LE capture samples to `f32` by dividing each signed integer by 2147483648. It converts finite `f32` output samples with deterministic rounding. Values at or below -1 map to `i32::MIN`, and values at or above 1 map to `i32::MAX`. All byte assembly uses explicit little-endian operations.

Construction allocates the engine buffers, System FIFO, control handoff, meter handoff, and playback encoding buffer. Block processing uses no allocation, lock, blocking operation, log, or platform call other than the optional playback writer at its owned worker boundary. The playback writer must complete one block or return a classified short-write, xrun-recovery, disconnect, or general failure.

The first accepted capture block is preflight work. It proves capture, conversion, engine processing, and meter delivery while readiness is false, but it does not count as playback or public endpoint delivery. Later test-runtime blocks can reach a fake playback writer. This activity is not graph readiness. A System fault observed before delivery suppresses the meter and playback for that block. If the producer publishes a fault after the final check, a playback write already in progress may finish. The next block observes the sticky failure.

Mixer changes use the engine's existing bounded control stager and consume the exact controls in the current `MixerProfile`. One complete update applies at a block boundary. Meter delivery uses a bounded, nonblocking handoff. No meter is available before the first processed block, and the handoff does not invent a zero observation.

The seam owns the engine, System consumer, meter publisher, and block buffers. The returned handles own the sole System producer, control producer, and meter consumer. A future production worker must own capture, engine, and playback together if it uses this capture-clock design. That worker must stop and join before those resources are released. The current host keeps its existing capture worker and playback object separate because the seam is not connected to live hardware.

## Mixer control plane

The current product profile has two logical sources. Source 1 is Microphone, and source 2 is System. Source 1 supplies the microphone endpoint. Each source starts with its monitor and stream routes enabled at 0 dB. Levels use integer half-decibel steps from -60 dB through +12 dB.

`librewaved` stores the desired profile in `librewave/profiles/mixer-state.json` under the selected configuration root. Schema 1 contains the mixer generation, microphone source, and the complete two-source profile. The daemon rejects unknown fields, unsupported schemas, changed source identities, and invalid fader steps. It does not read an older mixer format or add an Auxiliary source.

`librewavectl mixer show` reads the current snapshot. `librewavectl mixer set <source-id> <generation> <monitor|stream> <on|off> <level-db>` replaces one complete route. The daemon rejects a stale mixer generation or an unknown source. An unchanged route succeeds without a file write or generation change.

For a changed route, the daemon writes the complete desired profile to a temporary file, syncs it, renames it, and syncs the profile directory. It updates retained state only after all these steps succeed. If the directory sync fails after the rename, the command reports durability ambiguity and retained state does not advance. The destination can already contain the next complete profile. A retry with the retained generation writes the same next profile and converges the file and snapshot. On startup, the daemon strictly loads whichever complete file is present.

The mixer snapshot reports that the audio host and mixer engine are not connected. Meter values are unavailable. These controls are durable desired state only. They do not start ALSA, create PipeWire objects, enable the daemon service, or make the Linux audio graph ready.

## Volume ownership

The Linux audio graph keeps these values separate:

- Microphone preamp gain is a Wave hardware value.
- Headphone output level is a Wave hardware value.
- LibreWave endpoint, channel, and mix volumes are software values.
- A raw ALSA hardware mixer value is not a LibreWave endpoint value.

KDE may change the software volumes that LibreWave publishes. It must not use ALSA hardware controls as those endpoint values. The device control path and accepted physical events remain the only routes to the microphone preamp and headphone level.

A sanitized read-only ALSA inspection found `Mic Capture Volume` at 0.00 to 40.00 dB in 0.5 dB steps and `PCM Playback Volume` at -60.00 to 0.00 dB in 0.5 dB steps. Their switches and read-only channel maps belong to the same physical control groups. These controls are hardware aliases for microphone gain and headphone level. LibreWave must keep them out of software endpoint volume and mute semantics.

Gain Lock remains an optional Wave hardware setting. LibreWave reads and preserves it, and the user can change it through the hardware settings. Setup does not enable it. Gain Lock does not disable software volume on the LibreWave microphone endpoint.

Volume events carry an origin. A physical knob event can update hardware state and the UI without a write back to the device. A KDE software-volume event can update the matching LibreWave endpoint without reaching the ALSA hardware mixer. This avoids feedback loops and gives each value one owner.

The final mapping must also match the observed Wave Link behavior on Windows and macOS. See [Wave Link behavior parity](behavior-parity.md).

## Setup and removal

`librewavectl setup` shows its plan before it changes the system. It stages the current CLI and daemon, switches stable links, installs an inactive daemon unit, and requests elevation only for the exact udev access rule.

Setup does not install or reload the WirePlumber fragment. It does not enable the unit, start the daemon, open ALSA, or publish PipeWire objects. `librewavectl doctor` reports production audio ownership as blocked and graph inspection as not implemented. The host resource snapshot is not yet part of daemon reconciliation or doctor output. The lifecycle contract in [Setup, development installs, and removal](setup.md) governs installation, rollback, diagnostics, and removal.

## Work still required

Production composition, ASRC, and the live hardware plan remain required. Completion requires all of these results:

- `librewaved` owns the physical capture and playback PCMs directly.
- Capture consumption produces confirmed frames before playback opens.
- PipeWire source and sink directions come from `librewave-core`, and endpoint audio reaches the daemon-owned mixer.
- No helper, null sink, or physical Wave node appears as a desktop device.
- Restart, reconnect, sleep, login, teardown, and rollback tests pass on the reference system.
- Software volume changes cannot write microphone gain or headphone hardware level.

Native validation must confirm the PCM state transitions after ALSA recovers `EPIPE` and `ESTRPIPE`. The default fake test confirms xrun observation, but it cannot reproduce the driver's recovery state.

Do not claim Linux audio backend support until these tests pass.

## Acceptance tests

- Login with the Wave:3 already connected.
- Login with the Wave:3 disconnected, then connect it.
- Disconnect and reconnect during capture and playback.
- Restart PipeWire.
- Restart WirePlumber.
- Stop and restart `librewaved`.
- Change KDE microphone and output volumes.
- Change the physical Wave:3 knob in each control mode.
- Confirm that only deliberate endpoints appear in KDE and `wpctl status`.
- Confirm that saved microphone gain and headphone level survive every recovery case.
- Install a new development build over an older one and confirm that every running path refers to the new build.
- Uninstall and confirm that manifest-owned paths are removed, replaced files are restored, profiles are preserved, and no LibreWave process, unit, rule, or node remains.
