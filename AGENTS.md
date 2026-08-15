# LibreWave agent instructions

## Purpose

Build a dependable Linux control and mixing application for Elgato Wave hardware. Wave:3 is the first supported device. Keep the shared device, domain, and mixer code portable, but implement only the Linux platform adapter until another platform is approved.

The CLI is a complete product interface. The GPUI application is an optional client.

## Read before changing code

Read the root `README.md` and the relevant files in `docs/` before editing a crate. Check sibling repositories for research evidence when they are present locally:

- `wave-link-reverse` contains the recovered protocol catalog and architecture research.
- `elgato-wave3-linux` contains the current local WirePlumber workaround.
- `openwave` contains a separate implementation that can be compared with the recovered protocol.

Do not make builds or tests depend on those sibling repositories. Import reviewed, redistributable material into LibreWave and record its provenance.

## Architecture rules

- `librewaved` is the only normal owner of hardware, the physical audio graph, profiles, and recovery state.
- `librewavectl` and `librewave` use the same versioned local IPC interface.
- The CLI and UI must not access USB, ALSA, PipeWire, or WirePlumber directly.
- Protocol codecs contain no I/O. Device sessions depend on transport traits, not Linux libraries.
- Linux APIs stay in `librewave-platform-linux`.
- Shared crates must not use operating-system conditionals to hide platform dependencies.
- Do not create macOS or Windows crates before those platforms have approved implementations.
- Add a crate only when it owns a clear contract and real behavior.
- Keep one representation of device state and one normal write path.

## Hardware safety

Treat all USB output as a hardware operation.

- Never implement or invoke firmware update, DFU, bootloader, flash, reset, or recovery commands.
- Start new device support in read-only mode.
- Match the device model, API version, payload size, and schema exactly before a write.
- Refuse writes when the schema is unknown or ambiguous.
- Build writes from a complete device baseline. Change only the intended field, preserve reserved bytes, write the complete message, then read it back.
- Validate type, range, step, enum, and bit boundaries before encoding.
- Keep the write allowlist close to the device session code and cover it with tests.
- Serialize hardware integration tests. They must be opt-in and must name every field they can change.
- Restore a changed test value when the test can do so safely.
- Do not detach, replace, or rebind `snd_usb_audio` during normal operation.
- Never record or commit a device serial number.

Read-only USB descriptors, protocol reads, and audio-state inspection are allowed. A write requires an explicit test plan and review.

## Audio rules

- Preserve the required capture-before-playback order for affected Wave devices.
- Do not expose implementation-only PipeWire nodes as ordinary sources or sinks.
- The desktop may change software endpoint volume. It must not silently change the microphone preamp or headphone hardware level.
- Record the observed Windows and macOS Wave Link behavior before claiming parity.
- Keep physical hardware controls and software mix levels as separate domain values.
- Define hotplug, PipeWire restart, WirePlumber restart, daemon restart, sleep, and login behavior as state transitions.

The real-time audio path must not allocate, block, perform USB I/O, acquire an unbounded lock, log, or panic. Move control changes across bounded queues and apply them at safe graph boundaries.

## Protocol evidence

Recovered schemas are evidence, not permission to guess.

- Preserve the source model, API version, message identifier, payload size, field offset, encoding, and access mode.
- Keep generated files reproducible and review the generator change that produced them.
- Store sanitized captures as test fixtures only when their provenance and expected behavior are documented.
- Compare conflicting implementations against the recovered application and a physical device. Do not resolve conflicts by majority vote.
- Do not copy vendor application bundles, drivers, artwork, or proprietary assets into this repository.

## Code quality

- Prefer small modules with narrow interfaces and explicit error types.
- Do not swallow errors or turn protocol mismatches into defaults.
- Avoid parallel state caches, fallback write paths, and conversion shims without a removal plan.
- Keep unsafe code inside the smallest platform boundary and document each safety invariant.
- Add focused tests for every parser, state transition, and regression.
- Use stable Rust and the repository toolchain.

When crates are present, run the checks that apply to the change:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Hardware tests and tests that need a live PipeWire session must use separate, explicit commands. Do not include them in the default test suite.

## Documentation

Write public documentation in plain, controlled English influenced by ASD-STE100.

- Use one term for each concept.
- Prefer short, direct sentences, but vary the rhythm enough to sound natural.
- State prerequisites before instructions.
- Give one action per numbered step.
- Name commands, files, units, and expected results exactly.
- Separate tested behavior from proposals and open questions.
- Do not claim support before the relevant hardware and recovery tests pass.
- Run the humanizer skill on public prose without changing technical facts.

Use a root instruction file for repository-wide rules. Add a nested `AGENTS.md` only when a directory needs different commands or safety rules.

## Git workflow

- Use a dedicated worktree and a `codex/` branch for delegated tasks.
- Preserve unrelated user changes.
- Inspect the diff before staging.
- Keep commits focused.
- Use loose Conventional Commit subjects with no commit body.
- Do not rewrite published history.
- Do not add a remote or push without explicit approval.

## Code review rules

Flag a change when it can:

- write to unknown hardware or an unverified field;
- expose a second hardware writer;
- make the UI or CLI depend on Linux device APIs;
- publish an internal audio node to the desktop;
- allocate or block in the real-time path;
- lose user state during restart or reconnect;
- include a device serial, vendor binary, or proprietary asset;
- weaken a test to accept behavior that contradicts the documented contract.
