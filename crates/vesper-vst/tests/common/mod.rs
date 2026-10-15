//! A fake plugin host: this test binary, started again with
//! `VESPER_FAKE_HOST` set, serves a fake plugin before `main` (libtest)
//! runs. Real processes, so deadlines, crashes and restarts are real.
//!
//! Modes: `gain` (output = input delayed by `VESPER_FAKE_LATENCY` frames,
//! times the gain parameter, 0.5 by default), `slow` (gain, then 50 ms per
//! block after `VESPER_FAKE_AFTER` blocks), `crash` (aborts after that
//! many blocks), `nan` (NaN output after that many blocks), `fail`
//! (refuses to load).

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vesper_vst::host::{Loaded, Plugin, Processor};
use vesper_vst::protocol::{Load, Param};
use vesper_vst::{HostCommand, PluginInfo, Registry, Status};

#[used]
#[unsafe(link_section = ".init_array")]
static FAKE_HOST: extern "C" fn() = fake_host;

extern "C" fn fake_host() {
    let Some(mode) = std::env::var_os("VESPER_FAKE_HOST") else {
        return;
    };
    let num = |name| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let plugin = Fake {
        mode: mode.into_string().unwrap(),
        latency: num("VESPER_FAKE_LATENCY"),
        after: num("VESPER_FAKE_AFTER"),
        gain: 0.5,
    };
    let code = i32::from(vesper_vst::host::serve(plugin).is_err());
    std::process::exit(code);
}

struct Fake {
    mode: String,
    latency: usize,
    after: usize,
    gain: f64,
}

impl Plugin for Fake {
    fn load(&mut self, load: &Load) -> Result<Loaded, String> {
        if self.mode == "fail" {
            return Err("no stereo bus".into());
        }
        if let Ok(bytes) = load.component.as_slice().try_into() {
            self.gain = f64::from_le_bytes(bytes);
        }
        let param = Param {
            id: 0,
            title: "Gain".into(),
            units: String::new(),
            steps: 0,
            default: 0.5,
            value: self.gain,
        };
        let samples = self.latency * load.channels as usize;
        let proc = FakeProcessor {
            mode: self.mode.clone(),
            after: self.after,
            blocks: 0,
            gain: self.gain as f32,
            latency: self.latency as u32,
            line: std::iter::repeat_n(0.0, samples).collect(),
        };
        Ok(("Fake".into(), vec![param], Box::new(proc)))
    }

    fn set_param(&mut self, id: u32, value: f64) {
        if id == 0 {
            self.gain = value;
        }
    }

    fn state(&mut self) -> Result<(Vec<u8>, Vec<u8>), String> {
        Ok((self.gain.to_le_bytes().to_vec(), Vec::new()))
    }
}

struct FakeProcessor {
    mode: String,
    after: usize,
    blocks: usize,
    gain: f32,
    latency: u32,
    line: VecDeque<f32>,
}

impl Processor for FakeProcessor {
    fn process(&mut self, input: &[f32], output: &mut [f32], changes: &[(u32, f64)]) {
        for &(id, value) in changes {
            if id == 0 {
                self.gain = value as f32;
            }
        }
        self.blocks += 1;
        if self.blocks > self.after {
            match self.mode.as_str() {
                "slow" => std::thread::sleep(Duration::from_millis(50)),
                "crash" => std::process::abort(),
                "nan" => {
                    output.fill(f32::NAN);
                    return;
                }
                _ => {}
            }
        }
        for (i, o) in input.iter().zip(output) {
            self.line.push_back(*i);
            *o = self.line.pop_front().unwrap() * self.gain;
        }
    }

    fn latency(&self) -> u32 {
        self.latency
    }
}

/// A fresh settings directory for one test.
pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vesper-vst-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A registry in `dir` whose hosts are fakes in `mode`.
pub fn registry(dir: &Path, mode: &str, latency: usize, after: usize) -> Registry {
    let mut host = HostCommand::new(std::env::current_exe().unwrap());
    host.env = vec![
        ("VESPER_FAKE_HOST".into(), mode.into()),
        ("VESPER_FAKE_LATENCY".into(), latency.to_string().into()),
        ("VESPER_FAKE_AFTER".into(), after.to_string().into()),
    ];
    Registry::new(dir, host)
}

pub fn fake_plugin() -> PluginInfo {
    PluginInfo {
        path: "/nowhere/Fake.vst3".into(),
        cid: [7; 16],
        name: "Fake".into(),
        vendor: String::new(),
        category: "Fx".into(),
    }
}

/// Waits up to 10 s for `f`.
pub fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn running(i: &vesper_vst::Instance) -> bool {
    i.view().status == Status::Running
}

/// A stereo ramp, distinct per sample, starting at `from` frames.
pub fn ramp(from: usize, frames: usize) -> Vec<f32> {
    (from..from + frames)
        .flat_map(|n| {
            let v = (n % 1000) as f32 / 1000.0;
            [v, -v]
        })
        .collect()
}
