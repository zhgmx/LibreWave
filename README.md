# LibreWave

LibreWave is a Linux application for Elgato Wave audio hardware. It aims to provide the device controls, software mixer, monitor mix, and stream mix that Wave Link provides on its supported systems.

The project is written in Rust. It starts with the Wave:3 on Linux and keeps the shared device and mixer code separate from Linux audio integration. Other operating systems may be supported later, but the repository will not contain placeholder platform crates.

## Status

LibreWave is at the repository bootstrap stage. It does not control hardware or route audio yet. Do not install it as a replacement for an existing audio setup until the project documents a tested migration path.

The first supported configuration will be:

- Elgato Wave:3
- Fedora 44
- PipeWire 1.6 or later
- WirePlumber 0.5 or later
- KDE Plasma on Wayland or X11

Other Linux distributions and Wave devices will be added only after their behavior is tested.

## Product shape

LibreWave has three user-facing programs:

- `librewaved` is the user-session daemon. It owns the device, audio graph, profiles, and recovery state.
- `librewavectl` is the complete command-line interface. Every management operation must be available here.
- `librewave` is the optional GPUI desktop application. It uses the same local interface as the CLI.

The daemon is the only normal writer to Wave hardware. The CLI and UI do not access USB devices directly.

## Initial goals

- Control Wave:3 gain, mute, headphone output, direct monitoring, filters, and supported lighting settings.
- Stop the desktop audio service from changing hardware gain or headphone volume behind the user's back.
- Replace the current capture-first WirePlumber workaround with daemon-owned recovery and a minimal session-manager rule.
- Expose stable software endpoints for applications, monitor output, and stream output.
- Match established Wave Link behavior when Windows and macOS handle operating-system volume changes differently from physical controls.
- Provide exact protocol-version checks and refuse unsafe writes to unknown layouts.

## Safety

LibreWave does not provide firmware updates, DFU commands, bootloader commands, or device reset tools. Hardware writes must use a reviewed allowlist and readback verification. Unknown firmware remains in read-only diagnostic mode.

Do not use a development build to try undocumented writes on another person's hardware. See `AGENTS.md` before changing device or audio code.

## Building

The repository pins Rust 1.97.0. Crates and build commands will be added in the next implementation milestones.

The GPUI application will be optional. Headless daemon and CLI builds will not depend on graphical libraries.

## Distribution

The first releases will support source builds and GitHub CI. Native RPM, Arch, and Debian packaging may follow. Flatpak and AppImage are outside the project scope.

## Project name and trademarks

LibreWave is an independent project. It is not affiliated with, endorsed by, or supported by Elgato or Corsair. Elgato, Wave, and Wave Link are trademarks of their respective owners.

The project will not copy Wave Link artwork, branding, application binaries, or other proprietary assets.

## License

LibreWave is available under the [MIT License](LICENSE).
