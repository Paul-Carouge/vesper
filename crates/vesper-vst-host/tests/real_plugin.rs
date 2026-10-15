//! The real host with a real VST3 plugin: `VESPER_TEST_VST3=<bundle.vst3>`
//! (skipped without it, as on CI). With a plugin named "Vesper Test Gain"
//! (gain 0.5, 64 frames of latency: vst3-rs's example gain plugin with a
//! delay line) the output is checked sample for sample; any other plugin
//! is only checked for running, finite output and recovery.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vesper_dsp::Filter;
use vesper_vst::{DEADLINE, HostCommand, Instance, Registry, Status, scan, sidecar};

const BLOCK: usize = 960;

fn plugin() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("VESPER_TEST_VST3")?);
    assert!(path.exists(), "VESPER_TEST_VST3={path:?} does not exist");
    Some(path)
}

fn host() -> HostCommand {
    HostCommand::new(env!("CARGO_BIN_EXE_vesper-vst-host"))
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vesper-vst-real-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A 1 kHz tone at -12 dBFS, frames `from..from + BLOCK`.
fn tone(from: usize) -> Vec<f32> {
    (from..from + BLOCK)
        .flat_map(|n| {
            let s = 0.25 * (std::f32::consts::TAU * 1000.0 * n as f32 / 48_000.0).sin();
            [s, s]
        })
        .collect()
}

/// Runs `blocks` blocks of tone from `*at`; returns (input, output, longest).
fn run(f: &mut impl Filter, at: &mut usize, blocks: usize) -> (Vec<f32>, Vec<f32>, Duration) {
    let (mut input, mut output, mut longest) = (Vec::new(), Vec::new(), Duration::ZERO);
    for _ in 0..blocks {
        let mut buf = tone(*at);
        *at += BLOCK;
        input.extend_from_slice(&buf);
        let t = Instant::now();
        f.process(&mut buf);
        longest = longest.max(t.elapsed());
        output.extend_from_slice(&buf);
        std::thread::sleep(Duration::from_millis(5));
    }
    (input, output, longest)
}

/// `out` is `input` delayed `latency` frames, times `gain` (from the delay on).
fn matches(input: &[f32], out: &[f32], latency: usize, gain: f32) -> bool {
    out[latency * 2..]
        .iter()
        .zip(input)
        .all(|(o, i)| (o - i * gain).abs() < 1e-5)
}

fn start(reg: &Registry, id: u32) -> Instance {
    let i = reg.instance(id).unwrap();
    wait_for("the plugin", || i.view().status == Status::Running);
    i
}

#[test]
fn real_plugin_processes_survives_a_kill_and_keeps_its_state() {
    let Some(path) = plugin() else {
        eprintln!("skipped: set VESPER_TEST_VST3 to a .vst3 bundle");
        return;
    };
    let dir = temp_dir("plugin");
    // Scanned in its own process, then from the cache.
    let found = scan::scan(
        &host(),
        &[path.parent().unwrap().to_owned()],
        &dir.join("cache"),
    );
    let info = found
        .into_iter()
        .find(|p| p.path == path)
        .expect("the plugin is listed");
    let cache = std::fs::read_to_string(dir.join("cache")).unwrap();
    assert!(cache.contains(&info.line()));
    let reg = Registry::new(&dir, host());
    let id = reg.add(&info).unwrap();
    let inst = start(&reg, id);
    let gain_plugin = inst.view().name == "Vesper Test Gain";
    let mut f = inst.filter();
    let mut at = 0;
    let (input, output, _) = run(&mut f, &mut at, 25);
    assert!(output.iter().all(|s| s.is_finite()));
    let latency = f.latency();
    eprintln!(
        "{:?}: latency {latency}, {} parameters",
        info.name,
        inst.view().params.len()
    );
    if gain_plugin {
        assert_eq!(latency, 64);
        assert!(matches(&input, &output, 64, 0.5));
    }

    // kill -9 mid-stream: the audio goes on dry (with the same delay), on
    // time, and the host comes back.
    let pid = inst.pid().unwrap();
    // SAFETY: a process this test started.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    let mut longest = Duration::ZERO;
    let mut dry = false;
    wait_for("a new host", || {
        let (i, o, l) = run(&mut f, &mut at, 1);
        longest = longest.max(l);
        dry |= matches(&i, &o, latency, 1.0);
        inst.pid().is_some_and(|p| p != pid) && inst.view().status == Status::Running
    });
    assert!(
        dry,
        "the audio went on dry, delayed by the plugin's latency"
    );
    assert!(
        longest < DEADLINE + Duration::from_millis(15),
        "{longest:?}"
    );
    let (input, output, _) = run(&mut f, &mut at, 10);
    assert!(output.iter().all(|s| s.is_finite()));
    if gain_plugin {
        assert!(matches(&input, &output, 64, 0.5), "wet again");
    }

    // A parameter change is saved next to the settings and restored.
    if !inst.view().params.is_empty() {
        inst.set_param(0, 0.25);
        reg.persist_all();
        wait_for("the state", || {
            sidecar::Sidecar::load(&dir, id).is_ok_and(|s| {
                !s.component.is_empty() && (!gain_plugin || s.component == 0.25f64.to_le_bytes())
            })
        });
        drop((f, inst, reg));
        let reg = Registry::new(&dir, host());
        let inst = start(&reg, id);
        assert!((inst.view().params[0].value - 0.25).abs() < 1e-6);
        if gain_plugin {
            let mut f = inst.filter();
            let (input, output, _) = run(&mut f, &mut at, 10);
            assert!(matches(&input, &output, 64, 0.25));
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
