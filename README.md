# Vesper

A fast, bloat-free screen recorder for desktop and gaming, written in Rust. An alternative to OBS Studio focused on recording.

## Goals

- Zero-copy pipeline: frames never touch system RAM.
- Under 1% CPU at 1440p144 / 4K60, no dropped frames.
- A clean, modern UI (Slint).

## Architecture (Linux / Wayland first)

1. **Capture**: `xdg-desktop-portal` ScreenCast (`ashpd`) to a PipeWire stream (`pipewire`) using DMA-BUF. Cursor as metadata.
   **Game capture** (Vulkan, incl. Proton): an implicit Vulkan layer (`vesper-vklayer`, opt-in via `crates/vesper-vklayer/install.sh`) copies each presented image into exported DMA-BUFs and sends them over `$XDG_RUNTIME_DIR/vesper/capture.sock` to `vesper-gamecapture`. Game only, at the game's frame rate, no portal.
2. **Conversion**: RGB to NV12 on the GPU (VA-VPP).
3. **Encoding**: VAAPI hardware encode (AV1 / HEVC / H.264). Prototype with `cros-codecs`, fall back to `ffmpeg-next`.
4. **Audio**: PipeWire desktop monitor + microphone as separate tracks, Opus.
5. **Container**: MKV (crash-safe), optional remux to MP4.

## Game capture layer: what it does when installed

Installing is an explicit opt-in (`install.sh`, removed with `install.sh --uninstall`). Once installed, the layer loads into **every 64-bit Vulkan app** of the user, not only games. While Vesper is not recording it stays a pass-through, but it is not invisible:

- It adds `TRANSFER_SRC` to swapchain image usage (only when the surface supports it) and enables the export extensions the driver lists (`VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf`, `VK_KHR_external_semaphore_fd`, `VK_EXT_image_drm_format_modifier` and their dependencies). If the driver rejects either change, the app's original request is retried unchanged.
- After the first capturable swapchain (640x360 or larger), it starts one background thread that tries to connect to Vesper once per second.
- No GPU memory is allocated and no frame is copied until Vesper selects the game. Vesper only captures Steam games (`SteamGameId`/`SteamAppId` set) and games the user allowed; it identifies them through the kernel (`SO_PEERCRED`), same user only.

Escape hatches: a built-in blocklist keeps compositors, browsers, Electron apps, Steam, gamescope, OBS and Vesper itself a pure pass-through; `VESPER_CAPTURE_DISABLE=1` (e.g. `VESPER_CAPTURE_DISABLE=1 %command%` in Steam) stops the loader from loading the layer for one launch; `install.sh --uninstall` removes it.

Risks and limits: anti-cheats (EAC, BattlEye) may flag or reject unknown Vulkan layers, so use the escape hatches for those games. 32-bit games, OpenGL games and HDR output are not captured.

## v1 scope

- Record a screen or a window.
- Two audio tracks (desktop, mic).
- Global hotkeys (GlobalShortcuts portal).
- Replay buffer: ring buffer of encoded packets, save the last N seconds.
- Slint UI + tray icon.

## Out of scope

Scenes, source compositing, filters, browser sources, streaming, plugins.

## Known risks

- Wayland delivers frames on damage only: constant framerate needs frame duplication or timestamp handling.
- DMA-BUF modifier negotiation between PipeWire and VAAPI.
- Hybrid GPUs: pick the right render node explicitly.
