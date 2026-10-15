# Vesper

Fast, bloat-free screen recorder in Rust. See README.md for goals, architecture and scope.

## Layout

- `crates/vesper-core`: shared types (`VideoFrame`, `EncodedPacket`, `Codec`). Change only with care: every crate depends on it.
- `crates/vesper-capture`: xdg-desktop-portal ScreenCast + PipeWire, DMA-BUF frames.
- `crates/vesper-gamecapture`: game capture server (Unix socket) and its wire protocol, DMA-BUF frames from the layer.
- `crates/vesper-vklayer`: implicit Vulkan layer (cdylib) loaded into games; copies presented images into exported DMA-BUFs. Must never crash or block the game. `install.sh` installs it per user.
- `crates/vesper-encode`: VAAPI hardware encoding.
- `crates/vesper-mux`: Matroska (MKV) muxing of encoded video and Opus audio.
- `crates/vesper-audio`: PipeWire capture (desktop, microphone, devices, applications), mixing, Opus encoding (libopus via ffmpeg-next).
- `crates/vesper-app`: the `vesper` binary and the Slint UI.

## Target machine

KDE Plasma on Wayland, PipeWire 1.6, AMD RX 9070 (RDNA4, Mesa radeonsi) on `/dev/dri/renderD128`. `renderD129` is the iGPU: never pick it by default.

## Rules

- Keep frames on the GPU. No CPU readback in the hot path.
- No dependency for a few lines of code.
- Before every commit: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- Branches: `feat/<topic>`, `fix/<topic>`, `chore/<topic>`. One topic per branch, merged through a PR into `master`.
- Commits: Conventional Commits, in English.
