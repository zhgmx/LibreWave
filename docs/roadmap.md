# Roadmap

Work is ordered by safety and dependency. Later work never weakens an unfinished safety contract.

## Implemented

- Reviewed Wave:3 protocol codecs with exact model, API version, and payload checks, and offline fixtures.
- A daemon-owned device session with a serialized write path, a complete baseline, readback, and a reviewed control allowlist.
- GitHub CI that checks formatting, Clippy, the tests, and the headless and GPUI builds.
- Versioned local IPC shared by `librewavectl` and `librewave`, with generation checks on every change.
- Transactional `setup`, `doctor`, and `uninstall` with an installation manifest, a journal, backups, and a narrow udev access rule.
- Audio ownership through `librewavectl start` and `stop`: a WirePlumber card-disable rule, capture-first recovery, and restoration of desktop defaults.
- A software mixer with stable input identities, Monitor and Stream mixes, linked faders, meters, and software output levels.
- Application routing with saved rules, Flatpak identity from the owning PipeWire client, and fallback to System when an input is removed.
- A first-run profile with Microphone, System, Music, Browser, Voice Chat, and Game, with installed applications assigned by desktop entry category, and a confirmed reset to that profile.
- The desktop application: the device stage, hardware sliders and toggles, input strips with drag reordering, the Edit input pop-up, Settings, a login service, and an application icon.

## Remaining

- Sleep, a real logout and login, and long playback runs.
- The Windows and macOS volume behavior matrix, before Linux volume links are frozen. See [Wave Link behavior parity](behavior-parity.md).
- Accessibility labels, scaling, and reduced-motion checks on Wayland and X11.
- Output destination selection and effects.
- A documented list of supported firmware versions.

## Later

Native RPM, Arch, and Debian packages can follow the first source release. Other Wave devices need their own admission rules and physical test records.

macOS and Windows support need approved platform implementations. LibreWave will not create placeholder crates for them.

Plugins are outside the first release. Flatpak and AppImage are outside the project scope.
