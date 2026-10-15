//! The plugins of a chain share one deadline per block: three stalled
//! hosts cost one deadline, not three. Its own test binary: it installs
//! the global registry.

mod common;

use std::time::{Duration, Instant};

use common::*;
use vesper_dsp::FilterSpec;
use vesper_vst::DEADLINE;

#[test]
fn stalled_plugins_share_the_block_deadline() {
    let dir = temp_dir("chain-deadline");
    let reg = vesper_vst::install(common::registry(&dir, "slow", 0, 0)).unwrap();
    let ids: Vec<u32> = (0..3).map(|_| reg.add(&fake_plugin()).unwrap()).collect();
    let instances: Vec<_> = ids.iter().map(|&id| reg.instance(id).unwrap()).collect();
    for i in &instances {
        wait_for("the hosts", || running(i));
    }
    let specs: Vec<_> = ids.iter().map(|&id| FilterSpec::Vst3 { id }).collect();
    let (mut chain, _control) = vesper_dsp::live(&specs, 2);
    // The first block warms the chain up (allocations, RNNoise): not timed.
    chain.process(&mut ramp(0, 960));
    let mut longest = Duration::ZERO;
    for _ in 0..5 {
        let mut buf = ramp(0, 960);
        let t = Instant::now();
        chain.process(&mut buf);
        longest = longest.max(t.elapsed());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        longest < DEADLINE + Duration::from_millis(10),
        "{longest:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
