# Roadmap

This roadmap orders work by safety and dependency. A later milestone does not start by weakening an unfinished earlier contract.

## Milestone 0: repository foundation

- Public README, license, contribution policy, and security policy.
- Agent instructions and architecture documentation.
- Rust workspace policy and pinned toolchain.
- Focused commits and reviewed worktree tasks.
- Transactional setup and removal contract for source and development builds.

Exit condition: the repository explains its product boundary, safety rules, and verification expectations without claiming unsupported behavior.

## Milestone 1: protocol foundation

- Import the reviewed protocol schema and codec foundation.
- Preserve evidence metadata and reproducible generation.
- Add parser, encoder, range, reserved-byte, and unsupported-version tests.
- Add sanitized offline Wave:3 fixtures.

Exit condition: offline tests can identify and decode the exact supported Wave:3 layout without opening a USB device.

## Milestone 2: read-only Wave:3 session

- Discover the connected Wave:3 through the Linux platform adapter.
- Read USB descriptors, protocol version, configuration, and events.
- Validate the non-claiming control-transfer path while `snd_usb_audio` streams normally.
- Add read-only CLI diagnostics and fixture capture.

Exit condition: repeated reads do not disrupt capture or playback, and unknown firmware remains read-only.

## Milestone 3: safe hardware control

- Add the device state shadow and serialized transaction path.
- Validate one reversible field at a time on the connected Wave:3.
- Add readback, restoration, stale-baseline, reconnect, and failure tests.
- Validate gain lock without firmware, reset, or bootloader operations.

Exit condition: the reviewed allowlist can change and restore normal controls while preserving every protected byte.

## Milestone 4: daemon and Linux ownership

- Add versioned local IPC, profiles, persistence, and `librewavectl`.
- Install the user service and narrow udev access through `librewavectl setup`.
- Add installation manifests, atomic development replacement, `librewavectl doctor`, and verified uninstall.
- Replace the current Lua workaround with deterministic capture-first ownership.
- Expose only deliberate desktop endpoints.
- Complete the Windows and macOS volume behavior matrix before freezing Linux volume links.

Exit condition: login, hotplug, service restart, PipeWire restart, and WirePlumber restart restore the same gain, headphone level, and graph without visible helper nodes.

A development install must also prove that the running daemon, CLI link, service unit, WirePlumber policy, and graph belong to the current checkout. Uninstall must leave none of those objects behind.

## Milestone 5: mixer engine

- Add stable input channels and application routing.
- Add monitor and stream mixes.
- Add bounded real-time control transfer, metering, latency measurement, and xrun reporting.
- Test graph changes without allocations or blocking work in the audio callback.

Exit condition: the CLI can configure and observe the complete mixer while recovery tests continue to pass.

## Milestone 6: GPUI application

- Pin GPUI to Zed v1.15.0 commit `e17dc4f9d50db73a458b64dcce50ecd4878b98a3`.
- Build an original, modern channel-strip interface that follows the established Wave Link mental model.
- Keep complete command parity with `librewavectl`.
- Test Wayland, X11, keyboard use, accessible labels, scaling, and reduced motion.

Exit condition: closing the UI does not change daemon behavior, and headless builds do not require GPUI dependencies.

## Milestone 7: source release

- Run formatting, lint, unit, integration, documentation, and GPUI build jobs in GitHub CI.
- Publish source build instructions and a tested migration from the old WirePlumber workaround.
- Document supported firmware and known limitations.

Exit condition: a clean Fedora 44 source build can install, manage, diagnose, unmanage, and rebuild the tested Wave:3 setup.

## Later work

Native RPM, Arch, and Debian packages can follow the source release. Other Wave devices require their own admission and physical test records.

macOS and Windows support require approved platform implementations. LibreWave will not create placeholder crates for them.

Plugins are outside the first release. Flatpak and AppImage are outside the project scope.
