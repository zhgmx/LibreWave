# Contributing to LibreWave

LibreWave is in early development. Changes to USB control or the Linux audio graph can affect a live microphone, so the project uses stricter review rules for those areas.

Read `AGENTS.md` before starting. It defines the crate boundaries, hardware safeguards, required checks, and commit style.

## Before opening a change

1. Confirm that the issue belongs in the current Linux and Wave:3 scope.
2. Read the relevant architecture and safety documentation.
3. Describe any hardware fields, PipeWire nodes, or persistent files that the change can modify.
4. Add a test plan before running a new hardware write.

Do not add support by guessing from a similar device or firmware version. Unknown layouts must remain read-only.

## Development workflow

Use the Rust toolchain from `rust-toolchain.toml`. Keep changes focused and include tests for the behavior you change.

The normal checks are:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Some crates do not exist yet. Run these commands once the relevant workspace members are present.

Hardware tests are opt-in. A hardware test must document the device model, accepted API versions, fields it can change, original-value capture, and restoration behavior.

Use the documented development install command before a live integration test. It must report the current build identity and confirm that no stale LibreWave service or policy path remains.

## Pre-release changes

LibreWave has no public compatibility contract yet. Update the current configuration, IPC, and command formats directly. Do not add a migration, deprecated alias, fallback parser, or parallel implementation only to preserve earlier development builds.

If a format changes, update the tests and development reset path in the same change.

## Commits

Use a clear Conventional Commit style subject when practical.

Examples:

```text
feat(device): read Wave:3 configuration
fix(linux): open capture before playback
docs: record volume ownership behavior
```

## Documentation

Use direct, consistent language. Keep procedures separate from explanation and reference material. State whether behavior is tested, inferred from reverse engineering, or still proposed.

Do not add screenshots, icons, application bundles, drivers, or other assets copied from Wave Link.

## Reporting a problem

Include the LibreWave revision, Linux distribution, PipeWire version, WirePlumber version, device model, firmware version, and exact steps to reproduce the problem. Remove device serial numbers and other identifiers from logs before sharing them.

For a possible security issue, follow `SECURITY.md` instead of opening a public report.
