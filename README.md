# Vesper

A fast, bloat-free screen recorder for desktop and gaming, written in Rust. An alternative to OBS Studio focused on recording.

## Goals

- Zero-copy pipeline: frames never touch system RAM.
- Under 1% CPU at 1440p144 / 4K60, no dropped frames.
- A clean, modern UI (Slint).

## Architecture (Linux / Wayland first)

1. **Capture**: `xdg-desktop-portal` ScreenCast (`ashpd`) to a PipeWire stream (`pipewire`) using DMA-BUF. Cursor as metadata.
2. **Conversion**: RGB to NV12 on the GPU (VA-VPP).
3. **Encoding**: VAAPI hardware encode (AV1 / HEVC / H.264). Prototype with `cros-codecs`, fall back to `ffmpeg-next`.
4. **Audio**: PipeWire desktop monitor + microphone as separate tracks, Opus.
5. **Container**: MKV (crash-safe), optional remux to MP4.

## v1 scope

- Record a screen or a window.
- Two audio tracks (desktop, mic).
- Global hotkeys (GlobalShortcuts portal).
- Replay buffer: ring buffer of encoded packets, save the last N seconds.
- Slint UI + tray icon.

## Install

Global shortcuts need an app id, and the portal only accepts one with an installed desktop file whose `Exec` binary it can find:

```sh
cargo build --release
install -Dm755 target/release/vesper ~/.local/bin/vesper
install -Dm644 crates/vesper-app/data/io.github.PaulCarouge.Vesper.desktop \
    ~/.local/share/applications/io.github.PaulCarouge.Vesper.desktop
```

If `~/.local/bin` is not in the session `PATH`, set `Exec=` to the full path. Default global shortcuts: Alt+F9 starts or stops recording, Alt+F10 saves the replay. KDE asks to confirm them on first launch; change them later in System Settings > Shortcuts.

Settings are kept in `~/.config/vesper/settings`.

## Out of scope

Scenes, source compositing, filters, browser sources, streaming, plugins.

## Known risks

- Wayland delivers frames on damage only: constant framerate needs frame duplication or timestamp handling.
- DMA-BUF modifier negotiation between PipeWire and VAAPI.
- Hybrid GPUs: pick the right render node explicitly.
