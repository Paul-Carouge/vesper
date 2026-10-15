//! A track's plugin against fake hosts: the wet signal and its latency, the
//! deadline, a host that stalls, crashes or is killed (the audio goes on
//! dry, the host comes back), and saved state.

mod common;

use std::time::{Duration, Instant};

use common::*;
use vesper_dsp::{Filter, FilterSpec};
use vesper_vst::{DEADLINE, Status, sidecar};

const BLOCK: usize = 960;

/// Runs `blocks` blocks of the ramp from frame `from`, every `pace`.
/// Returns (input, output, longest block).
fn run(
    f: &mut impl Filter,
    from: usize,
    blocks: usize,
    pace: Duration,
) -> (Vec<f32>, Vec<f32>, Duration) {
    let (mut input, mut output, mut longest) = (Vec::new(), Vec::new(), Duration::ZERO);
    for b in 0..blocks {
        let mut buf = ramp(from + b * BLOCK, BLOCK);
        input.extend_from_slice(&buf);
        let t = Instant::now();
        f.process(&mut buf);
        longest = longest.max(t.elapsed());
        output.extend_from_slice(&buf);
        std::thread::sleep(pace);
    }
    (input, output, longest)
}

/// Whether `out` is `input` delayed by `latency` frames times `gain`
/// (after the first `latency` frames).
fn is_wet(input: &[f32], out: &[f32], latency: usize, gain: f32) -> bool {
    let d = latency * 2;
    out[d..]
        .iter()
        .zip(input)
        .all(|(o, i)| (o - i * gain).abs() < 1e-6)
}

/// Margin over the deadline for a loaded test machine.
const SLACK: Duration = Duration::from_millis(15);

#[test]
fn plugin_processes_with_its_latency() {
    let dir = temp_dir("wet");
    let reg = common::registry(&dir, "gain", 64, 0);
    let id = reg.add(&fake_plugin()).unwrap();
    let inst = reg.instance(id).unwrap();
    wait_for("the host", || running(&inst));
    assert_eq!(inst.view().name, "Fake");
    assert_eq!(inst.view().params[0].title, "Gain");
    let mut f = inst.filter();
    let (input, output, _) = run(&mut f, 0, 10, Duration::ZERO);
    assert_eq!(f.latency(), 64);
    assert!(is_wet(&input, &output, 64, 0.5));
    // Odd sizes and blocks over the shared buffer's size are split.
    let mut big = ramp(0, 3_333);
    let want: Vec<f32> = big.iter().map(|s| s * 0.5).collect();
    let mut f2 = reg.instance(id).unwrap().filter();
    f2.process(&mut big);
    // A fresh filter shares the plugin's delay line: only the tail is known.
    assert!(
        big[2 * 64..]
            .iter()
            .zip(&want)
            .all(|(o, w)| (o - w).abs() < 1e-6)
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn slow_host_goes_dry_within_the_deadline_and_restarts() {
    let dir = temp_dir("slow");
    let reg = common::registry(&dir, "slow", 0, 3);
    let id = reg.add(&fake_plugin()).unwrap();
    let inst = reg.instance(id).unwrap();
    wait_for("the host", || running(&inst));
    let first = inst.pid().unwrap();
    let mut f = inst.filter();
    let (input, output, _) = run(&mut f, 0, 3, Duration::ZERO);
    assert!(is_wet(&input, &output, 0, 0.5));
    // Now 50 ms per block: every block goes out dry, on time.
    let (input, output, longest) = run(&mut f, 3 * BLOCK, 4, Duration::from_millis(20));
    assert!(longest < DEADLINE + SLACK, "{longest:?}");
    assert_eq!(input, output, "dry");
    // Too many late blocks: the host is replaced.
    wait_for("a restart", || {
        run(&mut f, 0, 1, Duration::from_millis(20));
        inst.pid().is_some_and(|p| p != first) && running(&inst)
    });
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn killed_host_leaves_the_audio_dry_and_comes_back_with_its_parameters() {
    let dir = temp_dir("kill");
    let reg = common::registry(&dir, "gain", 0, 0);
    let id = reg.add(&fake_plugin()).unwrap();
    let inst = reg.instance(id).unwrap();
    wait_for("the host", || running(&inst));
    inst.set_param(0, 0.25);
    let mut f = inst.filter();
    wait_for("the parameter", || {
        let (i, o, _) = run(&mut f, 0, 1, Duration::ZERO);
        is_wet(&i, &o, 0, 0.25)
    });
    let pid = inst.pid().unwrap();
    // SAFETY: a process this test started.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    let mut longest = Duration::ZERO;
    let mut saw_dry = false;
    wait_for("the new host", || {
        let (i, o, l) = run(&mut f, 0, 1, Duration::from_millis(20));
        longest = longest.max(l);
        saw_dry |= i == o;
        inst.pid().is_some_and(|p| p != pid) && running(&inst)
    });
    assert!(saw_dry, "the audio went on dry");
    assert!(longest < DEADLINE + SLACK, "{longest:?}");
    // The parameter set before the crash is replayed.
    assert_eq!(inst.view().params[0].value, 0.25);
    wait_for("wet again", || {
        let (i, o, _) = run(&mut f, 0, 1, Duration::ZERO);
        is_wet(&i, &o, 0, 0.25)
    });
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn crashing_host_is_restarted_with_a_growing_delay() {
    let dir = temp_dir("crash");
    let reg = common::registry(&dir, "crash", 0, 2);
    let id = reg.add(&fake_plugin()).unwrap();
    let inst = reg.instance(id).unwrap();
    let mut f = inst.filter();
    let mut pids = Vec::new();
    let mut restarts = Vec::new();
    let start = Instant::now();
    wait_for("three hosts", || {
        let (i, o, l) = run(&mut f, 0, 1, Duration::from_millis(10));
        assert!(l < DEADLINE + SLACK, "{l:?}");
        assert!(is_wet(&i, &o, 0, 0.5) || i == o, "wet or dry, nothing else");
        if let Some(p) = inst.pid()
            && pids.last() != Some(&p)
        {
            pids.push(p);
            restarts.push(start.elapsed());
        }
        pids.len() == 3
    });
    // 0.5 s then 1 s between restarts (plus the time to crash).
    let gap = restarts[2] - restarts[1];
    assert!(gap >= Duration::from_millis(900), "{restarts:?}");
    assert!(matches!(
        inst.view().status,
        Status::Running | Status::Restarting(_)
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn non_finite_output_goes_out_dry() {
    let dir = temp_dir("nan");
    let reg = common::registry(&dir, "nan", 0, 0);
    let id = reg.add(&fake_plugin()).unwrap();
    let inst = reg.instance(id).unwrap();
    wait_for("the host", || running(&inst));
    let mut f = inst.filter();
    let (i, o, _) = run(&mut f, 0, 3, Duration::ZERO);
    assert_eq!(i, o);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn plugin_that_cannot_load_is_marked_failed() {
    let dir = temp_dir("fail");
    let reg = common::registry(&dir, "fail", 0, 0);
    let id = reg.add(&fake_plugin()).unwrap();
    let inst = reg.instance(id).unwrap();
    wait_for("the failure", || {
        inst.view().status == Status::Failed("no stereo bus".into())
    });
    let mut f = inst.filter();
    let (i, o, _) = run(&mut f, 0, 2, Duration::ZERO);
    assert_eq!(i, o);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn state_is_saved_beside_the_settings_and_restored() {
    let dir = temp_dir("state");
    let id = {
        let reg = common::registry(&dir, "gain", 0, 0);
        let id = reg.add(&fake_plugin()).unwrap();
        let inst = reg.instance(id).unwrap();
        wait_for("the host", || running(&inst));
        inst.set_param(0, 0.75);
        reg.persist_all();
        wait_for("the state", || {
            sidecar::Sidecar::load(&dir, id).unwrap().component == 0.75f64.to_le_bytes()
        });
        id
    };
    let text = std::fs::read_to_string(sidecar::file(&dir, id)).unwrap();
    assert!(text.contains(&format!(
        "component={}",
        sidecar::base64(&0.75f64.to_le_bytes())
    )));
    let reg = common::registry(&dir, "gain", 0, 0);
    let inst = reg.instance(id).unwrap();
    wait_for("the host", || running(&inst));
    assert_eq!(inst.view().params[0].value, 0.75);
    // Removing the filter deletes its file; stale files are cleaned up.
    let other = reg.add(&fake_plugin()).unwrap();
    assert_ne!(other, id);
    reg.retain(&[id]);
    assert_eq!(sidecar::ids(&dir), [id]);
    reg.remove(id);
    assert!(sidecar::ids(&dir).is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn chains_build_plugins_and_follow_their_latency() {
    let dir = temp_dir("chain");
    let reg = vesper_vst::install(common::registry(&dir, "gain", 128, 0)).unwrap();
    let id = reg.add(&fake_plugin()).unwrap();
    let spec = FilterSpec::Vst3 { id };
    let (mut chain, _control) = vesper_dsp::live(&[spec, FilterSpec::Gain { db: 0.0 }], 2);
    let inst = reg.instance(id).unwrap();
    // Dry with no latency until the host is up.
    let mut buf = ramp(0, BLOCK);
    chain.process(&mut buf);
    wait_for("the host", || running(&inst));
    wait_for("the plugin's latency", || {
        let mut buf = ramp(0, BLOCK);
        chain.process(&mut buf);
        chain.latency() == 128
    });
    std::fs::remove_dir_all(&dir).unwrap();
}
