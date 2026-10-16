# Vesper

Fast, bloat-free screen recorder in Rust. See README.md for goals, architecture and scope.

## Layout

- `crates/vesper-core`: shared types (`VideoFrame`, `EncodedPacket`, `Codec`). Change only with care: every crate depends on it.
- `crates/vesper-capture`: xdg-desktop-portal ScreenCast + PipeWire, DMA-BUF frames; `camera` streams webcams straight from PipeWire (MJPEG decoded by VAAPI into DMA-BUF frames, raw YUY2/NV12 as CPU frames for the compositor to convert).
- `crates/vesper-gamecapture`: game capture server (Unix socket) and its wire protocol, DMA-BUF frames from the layer; `client` is the game side shared by the layer and the GL interposer (connection thread, blocklist).
- `crates/vesper-vklayer`: implicit Vulkan layer (cdylib) loaded into games; copies presented images into exported DMA-BUFs. Must never crash or block the game. `install.sh` installs it per user. Also built for i686 (32-bit games) when the target is installed; optional, CI does not build it.
- `crates/vesper-glcapture`: `LD_PRELOAD` interposer (cdylib) for native OpenGL games, loaded per game by the `vesper-gl` wrapper; blits swapped frames into exported DMA-BUFs. Same rules as the layer.
- `crates/vesper-encode`: VAAPI hardware encoding.
- `crates/vesper-compose`: scene compositor (Vulkan compute through `ash`, libvulkan loaded at run time): imports the main frame's DMA-BUF, draws overlay textures with crop, fit and opacity, and returns an exported XR24 ring frame for `vesper-encode`. Runs on renderD128's async compute queue at low global priority. Its shader's SPIR-V is committed: edit `shaders/compose.comp`, then run `scripts/shaders.sh`.
- `crates/vesper-mux`: Matroska (MKV) muxing of encoded video and Opus audio.
- `crates/vesper-audio`: PipeWire capture (desktop, microphone, devices, applications), mixing, Opus encoding (libopus via ffmpeg-next).
- `crates/vesper-vst`: VST3 effects in a track's filter chain, each run by its own host process (shared-memory audio with a deadline, Unix socket control, supervisor with restarts), plugin scanning and per-instance settings files.
- `crates/vesper-vst-host`: the `vesper-vst-host` binary (VST3 hosting through the `vst3` bindings). Plugins only ever load in this process.
- `crates/vesper-app`: the `vesper` binary and the Slint UI.

## Target machine

KDE Plasma on Wayland, PipeWire 1.6, AMD RX 9070 (RDNA4, Mesa radeonsi) on `/dev/dri/renderD128`. `renderD129` is the iGPU: never pick it by default.

## Rules

- Keep frames on the GPU. No CPU readback in the hot path.
- No dependency for a few lines of code.
- Before every commit: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- Branches: `feat/<topic>`, `fix/<topic>`, `chore/<topic>`. One topic per branch, merged through a PR into `master`.
- Commits: Conventional Commits, in English.
