# GPUI application development

The optional `librewave` application is a static presentation shell. It does not access hardware, PipeWire, persistence, or IPC.

The shell uses upstream Zed GPUI and `gpui_platform` at commit `e17dc4f9d50db73a458b64dcce50ecd4878b98a3`. The Linux application enables both the Wayland and X11 backends. The GPUI feature is disabled in the default workspace build.

## Linux prerequisites

On Fedora, install:

```text
clang fontconfig-devel libX11-devel libxcb-devel libxkbcommon-devel wayland-devel
```

## Build

Run this command from the repository root:

```text
cargo build --package librewave
```

The application opens one window with placeholder mixer channels and a `Not connected` status. This status does not claim hardware support.
