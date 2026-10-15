//! VST3 effects for Vesper's tracks, each hosted in its own process
//! (`vesper-vst-host`) so a plugin that crashes or stalls cannot take a
//! recording down.
//!
//! - Audio goes through a shared memory region ([`shm`]): the track's encode
//!   thread writes a block, wakes the host with a futex and waits for the
//!   answer at most [`DEADLINE`]. Late, starting or dead hosts leave the
//!   block dry (delayed by the plugin's latency, so the timeline holds).
//! - Control (load, parameters, state) goes over a Unix socket
//!   ([`protocol`]), handled by one supervisor thread per instance, which
//!   also restarts a crashed or stuck host with a growing delay.
//! - A [`vesper_dsp::FilterSpec::Vst3`] names an instance; its plugin and
//!   state live in a settings file of their own ([`sidecar`]). [`install`]
//!   makes the chains build these filters.
//! - Installed plugins are listed by [`Registry::scan`], each bundle opened
//!   by a host process too, with a cache.

mod client;
pub mod host;
pub mod protocol;
pub mod scan;
pub mod shm;
pub mod sidecar;

pub use client::{
    DEADLINE, HOST_BIN, HostCommand, Instance, MAX_LATENCY, Registry, Status, View, Vst3Filter,
    global, install,
};
pub use scan::PluginInfo;

/// Channels of every instance (Vesper's tracks are stereo).
pub const CHANNELS: usize = 2;
/// Sample rate of every instance.
pub const SAMPLE_RATE: u32 = 48_000;
/// Most frames sent to a host at once; longer blocks are split.
pub const MAX_FRAMES: usize = 1024;
