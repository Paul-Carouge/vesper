# Vesper

A fast, bloat-free screen recorder for desktop and gaming, written in Rust. An alternative to OBS Studio focused on recording.

## Goals

- Zero-copy pipeline: frames never touch system RAM.
- Under 1% CPU at 1440p144 / 4K60, no dropped frames.
- A clean, modern UI (Slint).

## Architecture (Linux / Wayland first)

1. **Capture**: `xdg-desktop-portal` ScreenCast (`ashpd`) to a PipeWire stream (`pipewire`) using DMA-BUF. Cursor as metadata.
   **Game capture** (Vulkan, incl. Proton): an implicit Vulkan layer (`vesper-vklayer`, opt-in with the *Game capture* switch in the settings) copies each presented image into exported DMA-BUFs and sends them over `/tmp/vesper-<uid>/capture.sock` to `vesper-gamecapture`. The socket is in `/tmp`, not `$XDG_RUNTIME_DIR`, because Proton games run inside the Steam Linux Runtime container (pressure-vessel). That container replaces `$XDG_RUNTIME_DIR` with its own tmpfs but shares the host's `/tmp`. Game only, at the game's frame rate, no portal.
2. **Conversion**: RGB to NV12 on the GPU (VA-VPP).
3. **Encoding**: VAAPI hardware encode (AV1 / HEVC / H.264). Prototype with `cros-codecs`, fall back to `ffmpeg-next`.
4. **Audio**: up to 8 PipeWire tracks (desktop, microphone, a device by node name, or an application's streams, followed as they come and go), each its own named Opus track, plus an optional mix of them all as the first track. Volume and mute per track, applied live. Built-in filters per track (`vesper-dsp`) run on the track's encode thread, never on PipeWire's real-time thread: parameter changes are lock-free, a new filter list is built off that thread and handed over without a lock or an allocation. A filter's delay (noise suppression 20 ms, limiter lookahead) is removed from the track's timeline, so tracks stay in sync with the video and with each other. The mix goes through a true-peak limiter (-1 dBTP). VST3 effects run in the same chains, each in its own host process (see *VST3 plugins*).
5. **Container**: MKV (crash-safe), optional remux to MP4.

## Game capture layer: what it does when installed

Installing is an explicit opt-in: the *Game capture (Vulkan layer)* switch in Vesper's settings (or `crates/vesper-vklayer/install.sh`) copies the layer to `~/.local/share/vesper/` and writes its manifest to `~/.local/share/vulkan/implicit_layer.d/vesper_capture.json`; turning the switch off (or `install.sh --uninstall`) removes both. Vesper never installs it otherwise; when the installed layer differs from the one built with Vesper (release builds), the switch offers an Update button. Once installed, the layer loads into **every 64-bit Vulkan app** of the user, not only games. While Vesper is not recording it stays a pass-through, but it is not invisible:

- It adds `TRANSFER_SRC` to swapchain image usage (only when the surface supports it) and enables the export extensions the driver lists (`VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf`, `VK_KHR_external_semaphore_fd`, `VK_EXT_image_drm_format_modifier` and their dependencies). If the driver rejects either change, the app's original request is retried unchanged.
- After the first capturable swapchain (640x360 or larger), it starts one background thread that tries to connect to Vesper once per second.
- No GPU memory is allocated and no frame is copied until Vesper selects the game. Vesper only captures Steam games (`SteamAppId` set) and games the user allowed; it identifies them through the kernel (`SO_PEERCRED`), same user only.

In the app, pick *Game* in the source card. It shows the captured game (or *Waiting for a game…*) and lets you switch to another running game. A non-Steam game asks first: *Allow Vesper to capture <name>?* Allow adds the name to the allow list (`allow_game=` lines in `~/.config/vesper/settings`), Ignore skips it until Vesper restarts. Games only copy frames while Vesper records or runs the replay buffer. While nothing records, a new game size that lasts over a second reopens the encoder at that size (the replay buffer restarts); during a recording the file keeps its size and the game is scaled to fit on the GPU, with black bars. A recording that gets no frame within 5 s (the game quit) fails instead of waiting.

Consent prevents accidental captures (a launcher, a tool, the wrong window). It is not a barrier against malicious code running under your account: with the layer installed, any process of this account can serve the socket and read Vulkan apps' frames without a portal prompt. The socket's directory must belong to you with mode 0700 and must not be a symlink. Vesper checks this and refuses to start game capture otherwise, with an error naming the directory. Another local user who creates `/tmp/vesper-<uid>` first can therefore block game capture, but cannot read frames. On top of that, both ends check the peer's uid (`SO_PEERCRED`). Flatpak apps get a private `/tmp`, so they cannot reach the socket unless granted host `/tmp` access (`--filesystem=/tmp` or `--filesystem=host`). An abstract socket was avoided for this reason: every Flatpak with network access shares that namespace. The allow list is keyed on the process name as the kernel reports it (`/proc/<pid>/comm`, 15 characters), so never allow generic names such as `java`, `python3`, `game.exe` or `launcher.exe`; keying on the executable path is a follow-up.

Escape hatches: a built-in blocklist keeps compositors, browsers, Electron apps, Steam, gamescope, OBS and Vesper itself a pure pass-through; `VESPER_CAPTURE_DISABLE=1` (e.g. `VESPER_CAPTURE_DISABLE=1 %command%` in Steam) stops the loader from loading the layer for one launch; the settings switch (or `install.sh --uninstall`) removes it.

Risks and limits: anti-cheats (EAC, BattlEye) may flag or reject unknown Vulkan layers, so use the escape hatches for those games. 32-bit games, OpenGL games and HDR output are not captured.

## Done

- Record a screen, a window or a game (Vulkan layer), constant frame rate, AV1 / HEVC / H.264 with constant-quality presets.
- Up to 8 named audio tracks (desktop, mic, devices, applications) with volume and mute, plus a mix track, sample-aligned and drift-corrected. The desktop and microphone devices are chosen in the settings (system default or a specific device, never silently replaced when unplugged), and any track's source can be changed after it was added.
- Built-in filters per track (noise suppression, expander, compressor, de-esser, limiter, high-pass, gain), changed live, with a one-click voice preset and gain reduction meters.
- VST3 effects per track, hosted out of process: a plugin that crashes or stalls leaves its track dry, never the recording down.
- Global hotkeys (GlobalShortcuts portal).
- Replay buffer: ring buffer of encoded packets, save the last N seconds.
- Slint UI, tray icon, start with the session.

## Install

Global shortcuts need an app id, and the portal only accepts one with an installed desktop file whose `Exec` binary it can find.

Arch (and CachyOS): the package builds the last commit of this checkout (uncommitted changes are left out), no GitHub access needed:

```sh
cd packaging/arch && makepkg -si
```

It installs `/usr/bin/vesper`, the desktop file, the icon, and the game capture layer and the VST3 plugin host (`vesper-vst-host`) in `/usr/lib/vesper/`. The layer stays off: only the settings switch enables it, per user. Uninstall with `sudo pacman -R vesper-git`. Turn the switch off first: otherwise your per-user copy of the layer stays installed and keeps loading into Vulkan apps; remove it by hand with `rm ~/.local/share/vulkan/implicit_layer.d/vesper_capture.json ~/.local/share/vesper/libvesper_vklayer.so`. Each `makepkg` rewrites `pkgver=` in the PKGBUILD (normal for a -git package): `git checkout packaging/arch/PKGBUILD` before committing.

Manual alternative, in your home:

```sh
cargo build --release
install -Dm755 target/release/vesper ~/.local/bin/vesper
install -Dm644 target/release/libvesper_vklayer.so ~/.local/lib/vesper/libvesper_vklayer.so
install -Dm755 target/release/vesper-vst-host ~/.local/lib/vesper/vesper-vst-host
install -Dm644 crates/vesper-app/data/io.github.PaulCarouge.Vesper.desktop -t ~/.local/share/applications/
install -Dm644 crates/vesper-app/data/io.github.PaulCarouge.Vesper.svg -t ~/.local/share/icons/hicolor/scalable/apps/
```

Vesper looks for the layer and the plugin host next to its binary, then in `../lib/vesper/`; the settings switch installs that copy (build with `cargo build --release` at the workspace root, not `-p vesper-app`, so the layer is built too). If `~/.local/bin` is not in the session `PATH`, set `Exec=` to the full path. Without the desktop file, global shortcuts and desktop notifications (sent when a recording or replay is saved, or fails, while the window is in the background) are off; the in-window shortcuts still work. The saved source is forgotten once after updating to this version, since it is now tied to the app id: pick it again. Default global shortcuts: Alt+F9 starts or stops recording, Alt+F10 saves the replay. KDE asks to confirm them on first launch; change them later in System Settings > Shortcuts.

Settings are kept in `~/.config/vesper/settings`.

Failure-injection lab (disk full, game crash, portal or PipeWire gone...): `scripts/robustness/go <name> <scenario>.sh` runs a scenario against a debug build in a private user/mount/pid namespace with its own D-Bus, PipeWire (no real device) and headless gamescope; see `scripts/robustness/run.sh`.

## VST3 plugins

*Add filter* lists the VST3 effects installed in `~/.vst3`, `/usr/lib/vst3` and `/usr/local/lib/vst3` (vendor sub-folders included), after the built-in filters. They are enabled, removed and reordered like the built-in ones, and changed live while recording.

- **Out of process.** Each plugin instance runs in its own `vesper-vst-host` process. Audio goes through shared memory: the track's encode thread hands each 20 ms block to the host, wakes it with a futex and waits for the answer without allocating; all the plugins of a track share 8 ms per block. Control (load, parameters, state) goes over a Unix socket. If the host is late, still starting, crashed or was killed, or the plugin fails the block or outputs NaN or infinity, the block goes out dry, delayed by the plugin's latency so the track stays in place; the filter's title says *bypassed* with the reason. The host is restarted with a growing delay (0.5 s, doubling, up to 30 s; reset after 30 s of normal running), with the plugin's last saved state and the parameters set since. Three late blocks in a row count as a stall. A host also dies with Vesper, inherits none of its other files, cannot resize the shared audio region, and is the first process the kernel kills when memory runs out. The host binary is only taken from next to `vesper` or `../lib/vesper/`, never from `PATH`; without it, plugins show *not installed*.
- **Latency.** The latency a plugin reports (also when it changes it) is removed from the track's timeline like the built-in filters' (noise suppression, limiter lookahead): tracks stay in sync. Up to 1 s is followed; a plugin that reports more is bypassed.
- **Scanning** runs when *Add filter* opens, not at startup. Each bundle is opened by its own host process (10 s limit), so a plugin that crashes on load is only skipped. Results, failures included, are cached by the module's modification time in `~/.config/vesper/vst3/scan-cache`.
- **Settings.** The filter list in `~/.config/vesper/settings` only holds `vst3:id=<n>`. The `vst3` folder is private (0700, files 0600) and ids are never reused. Each instance's plugin path, class id and state (component and controller, base64) are in `~/.config/vesper/vst3/<n>.conf`, saved when its settings are. Removing the filter, or its track, stops its host and deletes that file; nothing else deletes it.
- **Parameters** are shown as generic sliders (0-100 % of the plugin's normalized range), the first eight in the card, the others behind *More settings*. Read-only, hidden and program-change parameters are left out.
- **Limits.** Effects with a mono or stereo main bus only (side-chain inputs get silence); 48 kHz, 32-bit float; no MIDI. No plugin editor window yet: plugin GUIs (X11 embedding through Xwayland, or Wayland) are a follow-up, as are plain-value parameter labels (`-6 dB` instead of a percentage) and parameter changes made inside a plugin's own GUI. A plugin that needs the host's `IMessage` objects to link its processor and controller may lose that link.

The host uses the `vst3` crate (bindings generated from the VST3 SDK headers, MIT or Apache-2.0); the VST3 SDK itself is MIT-licensed since version 3.8.

## Background and tray

Vesper shows an icon in the system tray (StatusNotifierItem, as on KDE Plasma). A left click shows or hides the window; its menu starts or stops a recording, saves the replay (while the replay buffer runs), turns the replay buffer on or off, and quits. A red dot marks a recording in progress.

With a tray, closing the window keeps Vesper running in the background, with the replay buffer and global shortcuts; the first time, a notification says so. Quit from the tray menu or with Ctrl+Q in the window (logging out, SIGTERM or SIGINT quit the same way): a recording in progress and replays being saved are finished first. Without a tray (no StatusNotifierWatcher on the session bus), closing the window quits, as before. Only one Vesper runs per session: launching it again shows the running one's window (it owns the D-Bus name `io.github.PaulCarouge.Vesper`). While the window is hidden, messages such as *No game to record yet* come as notifications.

*Start with the session* in the settings (off by default) writes `~/.config/autostart/io.github.PaulCarouge.Vesper.desktop` (under `$XDG_CONFIG_HOME` if set), which runs `vesper --background`; turning it off removes the file. `--background` starts with the window hidden in the tray (shown anyway when there is no tray), and the replay buffer starts if it is on in the settings.

## Roadmap

Every feature below must keep recording quality, game performance and compatibility intact: nothing on the game's render path, compositing and encoding on the GPU, audio processing off the PipeWire real-time thread.

1. **VST3 plugin editors**: the plugins' own windows, next to the generic sliders.
2. **Scenes and profiles**: sources composited on the GPU (VA-VPP / Vulkan) into the encoded picture, saved per profile.
3. **Overlays**: images, text, webcam, and optional browser sources, on the same GPU compositor.
4. **Streaming** to YouTube and Twitch (RTMP / SRT), sharing the recording's encode when settings match.
5. **Game capture**: 32-bit Vulkan games, native OpenGL games.

Not planned: plugin APIs for the video pipeline itself.

## Known risks

- Wayland delivers frames on damage only: constant framerate needs frame duplication or timestamp handling.
- DMA-BUF modifier negotiation between PipeWire and VAAPI.
- Hybrid GPUs: pick the right render node explicitly.
