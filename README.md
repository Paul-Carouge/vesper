# Vesper

A fast, bloat-free screen recorder for desktop and gaming, written in Rust. An alternative to OBS Studio focused on recording.

## Goals

- Zero-copy pipeline: frames never touch system RAM.
- Under 1% CPU at 1440p144 / 4K60, no dropped frames.
- A clean, modern UI (Slint).

## Architecture (Linux / Wayland first)

1. **Capture**: `xdg-desktop-portal` ScreenCast (`ashpd`) to a PipeWire stream (`pipewire`) using DMA-BUF. Cursor as metadata.
   **Game capture** (Vulkan, incl. Proton): an implicit Vulkan layer (`vesper-vklayer`, opt-in with the *Game capture* switch in the settings) copies each presented image into exported DMA-BUFs and sends them over `$XDG_RUNTIME_DIR/vesper/capture.sock` to `vesper-gamecapture`. Game only, at the game's frame rate, no portal.
2. **Conversion**: RGB to NV12 on the GPU (VA-VPP).
3. **Encoding**: VAAPI hardware encode (AV1 / HEVC / H.264). Prototype with `cros-codecs`, fall back to `ffmpeg-next`.
4. **Audio**: PipeWire desktop monitor + microphone as separate tracks, Opus.
5. **Container**: MKV (crash-safe), optional remux to MP4.

## Game capture layer: what it does when installed

Installing is an explicit opt-in: the *Game capture (Vulkan layer)* switch in Vesper's settings (or `crates/vesper-vklayer/install.sh`) copies the layer to `~/.local/share/vesper/` and writes its manifest to `~/.local/share/vulkan/implicit_layer.d/vesper_capture.json`; turning the switch off (or `install.sh --uninstall`) removes both. Vesper never installs it otherwise; an installed layer older than the one built with Vesper is refreshed at launch. Once installed, the layer loads into **every 64-bit Vulkan app** of the user, not only games. While Vesper is not recording it stays a pass-through, but it is not invisible:

- It adds `TRANSFER_SRC` to swapchain image usage (only when the surface supports it) and enables the export extensions the driver lists (`VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf`, `VK_KHR_external_semaphore_fd`, `VK_EXT_image_drm_format_modifier` and their dependencies). If the driver rejects either change, the app's original request is retried unchanged.
- After the first capturable swapchain (640x360 or larger), it starts one background thread that tries to connect to Vesper once per second.
- No GPU memory is allocated and no frame is copied until Vesper selects the game. Vesper only captures Steam games (`SteamAppId` set) and games the user allowed; it identifies them through the kernel (`SO_PEERCRED`), same user only.

In the app, pick *Game* in the source card. It shows the captured game (or *Waiting for a game…*) and lets you switch to another running game. A non-Steam game asks first: *Allow Vesper to capture <name>?* Allow adds the name to the allow list (`allow_game=` lines in `~/.config/vesper/settings`), Ignore skips it until Vesper restarts. When a game resizes or changes resolution, the recording keeps the size of its first frame and the game is scaled to it on the GPU (aspect ratio not kept).

Consent prevents accidental captures (a launcher, a tool, the wrong window). It is not a barrier against malicious code running under your account: with the layer installed, any process of this account can serve the socket and read Vulkan apps' frames without a portal prompt. The allow list is keyed on the process name as the kernel reports it (`/proc/<pid>/comm`, 15 characters), so never allow generic names such as `java`, `python3`, `game.exe` or `launcher.exe`; keying on the executable path is a follow-up.

Escape hatches: a built-in blocklist keeps compositors, browsers, Electron apps, Steam, gamescope, OBS and Vesper itself a pure pass-through; `VESPER_CAPTURE_DISABLE=1` (e.g. `VESPER_CAPTURE_DISABLE=1 %command%` in Steam) stops the loader from loading the layer for one launch; the settings switch (or `install.sh --uninstall`) removes it.

Risks and limits: anti-cheats (EAC, BattlEye) may flag or reject unknown Vulkan layers, so use the escape hatches for those games. 32-bit games, OpenGL games and HDR output are not captured.

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
# Optional, for game capture: the workspace build also produces the layer.
install -Dm644 target/release/libvesper_vklayer.so ~/.local/lib/vesper/libvesper_vklayer.so
install -Dm644 crates/vesper-app/data/io.github.PaulCarouge.Vesper.desktop \
    ~/.local/share/applications/io.github.PaulCarouge.Vesper.desktop
```

Vesper looks for the layer next to its binary, then in `../lib/vesper/`; the settings switch installs that copy (build with `cargo build --release` at the workspace root, not `-p vesper-app`, so the layer is built too). If `~/.local/bin` is not in the session `PATH`, set `Exec=` to the full path. Without the desktop file, global shortcuts are off (the in-window ones still work). The saved source is forgotten once after updating to this version, since it is now tied to the app id: pick it again. Default global shortcuts: Alt+F9 starts or stops recording, Alt+F10 saves the replay. KDE asks to confirm them on first launch; change them later in System Settings > Shortcuts.

Settings are kept in `~/.config/vesper/settings`.

## Out of scope

Scenes, source compositing, filters, browser sources, streaming, plugins.

## Known risks

- Wayland delivers frames on damage only: constant framerate needs frame duplication or timestamp handling.
- DMA-BUF modifier negotiation between PipeWire and VAAPI.
- Hybrid GPUs: pick the right render node explicitly.
