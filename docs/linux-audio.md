# Linux audio integration

Status: design baseline. The physical graph implementation still requires a focused PipeWire and ALSA spike.

## Reference system

The first test system uses Fedora 44, PipeWire 1.6, WirePlumber 0.5, KDE Plasma, and a Wave:3. Tests cover Wayland and X11 where desktop behavior differs.

## Current failure and workaround

Some Wave microphones produce silent capture when playback starts before capture. The current local workaround disables the automatic playback node, keeps capture active through a null sink, and creates a replacement playback sink after the capture link exists.

That workaround proves the required order, but the null sink appears as another audio device and the lifecycle lives in a user-maintained Lua script. LibreWave will replace the Lua orchestration.

## Required desktop result

LibreWave may publish deliberate product endpoints, such as microphone, monitor mix, stream mix, and configured input channels. It must not publish a keepalive sink, raw helper source, duplicate physical Wave node, or other implementation detail as an ordinary desktop device.

KDE, `wpctl status`, and PulseAudio-compatible device listings form the visibility acceptance test. Low-level debug tools may still show internal PipeWire objects when they are needed to inspect the graph.

## WirePlumber integration

The setup command installs a small declarative rule for admitted Wave devices. The rule uses `node.disabled` to stop WirePlumber from creating the conflicting automatic physical nodes. It does not create a replacement node and does not run a Lua lifecycle script.

WirePlumber documents `node.disabled` as removal of the matched node from its node list. It also documents `api.alsa.soft-mixer` as the setting that prevents ordinary node volume and mute changes from writing the hardware mixer. LibreWave will use these properties only where the physical graph design requires them.

The match includes the vendor, product, and admitted device family. It must not disable unrelated USB audio hardware.

## Physical graph ownership

The Linux platform spike will choose the smallest reliable method that satisfies these requirements:

- The daemon owns capture and playback lifecycle.
- Capture opens before playback.
- The daemon confirms active capture before it starts playback.
- Normal KDE volume controls affect software endpoint volume.
- The hardware preamp and headphone level change only through the device control path or an accepted physical event.
- No implementation-only node appears as a desktop device.
- PipeWire or WirePlumber restart uses the same deterministic recovery path.

Direct ALSA ownership and daemon-created PipeWire endpoints are the preferred design if the Rust APIs provide safe lifecycle and timing control. A daemon-owned PipeWire ALSA node is also acceptable when it meets the same tests. The project will not keep a visible null sink to avoid doing the integration work.

## Startup state machine

1. Detect an admitted USB and ALSA device.
2. Confirm that the automatic conflicting nodes are absent.
3. Open or create the physical capture path.
4. Start capture consumption inside the daemon.
5. Confirm that capture produces valid timing and does not stall.
6. Open or create the physical playback path.
7. Create the deliberate user-facing endpoints.
8. Restore saved routes and software levels.
9. Report the graph as ready.

If a step fails, the daemon removes graph objects from that attempt and enters a named degraded state. It does not reverse the order or create an alternate graph silently.

## Volume ownership

The Linux audio graph separates these concepts:

- Microphone preamp gain is a Wave hardware value.
- Headphone output level is a Wave hardware value.
- LibreWave source, sink, channel, and mix volumes are software values.
- A raw ALSA hardware mixer value is not a LibreWave endpoint value.

KDE can change the software values that LibreWave exposes. It must not use the ALSA hardware controls as those endpoint values. The raw Wave capture and playback nodes stay out of the ordinary desktop device list while LibreWave manages them.

Gain Lock remains an optional Wave hardware setting. LibreWave reads and preserves it, and the user can change it through the hardware settings. It is not enabled automatically. When it is on, it blocks outside requests to change hardware microphone gain. It does not disable the software volume on the LibreWave microphone endpoint.

Volume events carry an origin. A physical knob event can update hardware state and the UI without being written back to the device. A KDE software-volume event can update the matching LibreWave endpoint without reaching the ALSA hardware mixer. This prevents feedback loops and makes one owner responsible for each value.

The final mapping must also match the observed Wave Link behavior on Windows and macOS. See [Wave Link behavior parity](behavior-parity.md).

## Setup and removal

`librewavectl setup` will show its plan, back up the current user workaround, install the declarative WirePlumber rule, install the systemd user service, and request elevation for the narrow udev change.

`librewavectl unmanage` will stop the daemon graph and restore the previous WirePlumber files. It will restore the saved gain-lock value when the connected device and exact schema permit a safe write.

`librewavectl uninstall` includes unmanage, then removes every LibreWave-owned service, executable, rule, and state file listed in the installation manifest. It reloads the affected user services and checks that no LibreWave node remains.

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
- Uninstall and confirm that the original audio configuration is restored without a LibreWave process, unit, rule, or node.
