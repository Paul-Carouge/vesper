//! The tick grid on `CLOCK_MONOTONIC`, and local time.

use super::*;

/// Offset of frame `k` from the start: exact multiples of `1 / fps`.
pub(super) fn frame_offset(k: u64, fps: u32) -> Duration {
    Duration::from_nanos(k * 1_000_000_000 / u64::from(fps))
}

/// First tick of the shared grid ([`protocol::tick_ns`]) at or after `t`.
pub(super) fn next_tick(t: Duration, fps: u32) -> Duration {
    let t = t.as_nanos() as u64;
    let n = protocol::tick_at(t, fps);
    let n = if protocol::tick_ns(n, fps) < t {
        n + 1
    } else {
        n
    };
    Duration::from_nanos(protocol::tick_ns(n, fps))
}

pub(super) fn monotonic_now() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Sleeps until the `CLOCK_MONOTONIC` instant `t` (returns at once if past).
pub(super) fn sleep_until(t: Duration) {
    let ts = libc::timespec {
        tv_sec: t.as_secs() as libc::time_t,
        tv_nsec: t.subsec_nanos() as libc::c_long,
    };
    while unsafe {
        libc::clock_nanosleep(
            libc::CLOCK_MONOTONIC,
            libc::TIMER_ABSTIME,
            &ts,
            std::ptr::null_mut(),
        )
    } == libc::EINTR
    {}
}

/// Local time as `2026-10-15 14-03-22` (no `:`, file-name safe everywhere).
pub(super) fn local_timestamp() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}
