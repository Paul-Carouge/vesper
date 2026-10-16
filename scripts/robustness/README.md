# Robustness lab

How the lab works, and the round 1 scenarios: see the header of `run.sh`.
Every scenario runs inside `run.sh`'s private namespaces; `lib.sh` refuses to
run (exit 1) when `WAYLAND_DISPLAY`/`DISPLAY` is set, `/run/user` is visible,
there is no user namespace or `HOME` is not the lab's. Never run a scenario,
`$VESPER` or a test binary from a normal shell.

## Round 2: performance and robustness

The r2 scenarios run an optimized build with debug assertions (the dev entry
points `--record-game`, `--stream-game` and `VESPER_DEMO` need them):

    scripts/robustness/r2_build.sh                       # cargo build into target-da/release
    scripts/robustness/r2 <lab> <scenario>.sh [VAR=value ...]

`r2` is `go` with `BUILD=target-da/release` and a malloc counting shim
(`r2_allocshim.c`, built on first use into the lab data dir and passed as
`SHIM`; `r2_perf.sh` and `r2_ui.sh` preload it). Raise `TMO` (seconds, default 110) for long runs.

| Scenario | What it measures |
| --- | --- |
| `r2_perf.sh` | CPU, wakeups, mallocs, GPU per thread group while recording or streaming a game. `CASE`: rec, rec3, rec3-vst, stream-own, stream-shared |
| `r2_ui.sh` | Idle cost of the real window under headless gamescope with a fake tray (`r2_fake_tray.py`): shown, hidden, replay on, device list open, audio churn |
| `r2_latency.sh` / `r2_calib.sh` | Stream arrival jitter at a local RTMP receiver and burstiness on the wire (`r2_wire.py`); `r2_calib.sh` measures the receiver's own jitter with ffmpeg as the source |
| `r2_fault.sh` | Faults while recording and streaming. `FAULT`: serverdead, tls-untrusted, tls-plain, tls-die (`r2_tlsproxy.py`), pwrestart, pwfreeze, diskfull, gamekill, vstkill, vststorm, vstfreeze, term |
| `r2_faults_all.sh` | The `r2_fault.sh` matrix, one lab per fault (runs `r2` itself) |
| `r2_term_ui.sh` | The UI recording and streaming gets `SIG` (TERM, INT, GAMEKILL): exit time, file finalized, no leftover children |
| `r2_corrupt.sh` | Hostile VST3 settings files and filter lines: recording succeeds, no hang, no memory blow-up |
| `r2_flap.sh` | A track on a device WirePlumber keeps refusing: CPU of the reconnect loop |
| `r2_mem.sh` | The UI with a 2 min replay buffer and a live stream for `MEM_SECS` (1200): RSS, threads, fds |
| `r2_long.sh` | Record and stream for `LONG_SECS` (3600): resources over time, track drift, the received stream |
| `r2_flakes.sh` | `cargo test --workspace` `N` times (default 10) inside the lab, failures by test |

The VST scenarios need `VST=<a .vst3 bundle>` (a gain plugin) and its class id
in `VST_CID` (default: the one used in round 2). Scenarios measure with
`r2_sample.py <pid> <seconds> [label]`.

Examples:

    scripts/robustness/r2 perf r2_perf.sh CASE=stream-own
    TMO=4000 scripts/robustness/r2 long r2_long.sh LONG_SECS=3600
    scripts/robustness/r2_faults_all.sh serverdead pwrestart
