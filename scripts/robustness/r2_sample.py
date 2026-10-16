"""Per-thread-group cost of a process over a window (audit round 2).

usage: r2_sample.py <pid> <seconds> [label]

CPU % of one core (schedstat, ns precision), voluntary wakeups/s (context
switches), malloc calls/s (needs the LD_PRELOAD r2_allocshim.so and SHIM_OUT
in the target's environment, and the same SHIM_OUT here), grouped by thread
name; then RSS, fds, threads, GPU engines (DRM fdinfo).
"""
import os
import signal
import sys
import time

pid, secs = int(sys.argv[1]), float(sys.argv[2])
label = sys.argv[3] if len(sys.argv) > 3 else ""
shim = os.environ.get("SHIM_OUT")
# SIGUSR2 kills a process without the shim's handler: only signal when it is loaded.
try:
    if shim and "r2_allocshim" not in open(f"/proc/{pid}/maps").read():
        shim = None
except OSError:
    shim = None


def tasks():
    out = {}
    for tid in os.listdir(f"/proc/{pid}/task"):
        try:
            comm = open(f"/proc/{pid}/task/{tid}/comm").read().strip()
            run = int(open(f"/proc/{pid}/task/{tid}/schedstat").read().split()[0])
            vol = 0
            for line in open(f"/proc/{pid}/task/{tid}/status"):
                if line.startswith("voluntary_ctxt_switches"):
                    vol = int(line.split()[1])
        except (OSError, IndexError):
            continue
        out[int(tid)] = (comm, run, vol)
    return out


def allocs():
    if not shim:
        return {}
    try:
        old = os.stat(shim).st_mtime_ns
    except OSError:
        old = 0
    os.kill(pid, signal.SIGUSR2)
    for _ in range(100):
        try:
            if os.stat(shim).st_mtime_ns != old:
                break
        except OSError:
            pass
        time.sleep(0.02)
    out = {}
    try:
        for line in open(shim):
            t, n, b, f = line.split("\t")
            out[int(t)] = (int(n), int(b), int(f))
    except OSError:
        pass
    return out


def engines():
    clients = {}
    for fd in os.listdir(f"/proc/{pid}/fdinfo"):
        try:
            text = open(f"/proc/{pid}/fdinfo/{fd}").read()
        except OSError:
            continue
        info = dict(l.split(":", 1) for l in text.splitlines() if ":" in l)
        c = info.get("drm-client-id")
        if c is not None:
            clients[c.strip()] = {
                k[11:]: int(v.split()[0]) for k, v in info.items() if k.startswith("drm-engine-")
            }
    tot = {}
    for c in clients.values():
        for k, v in c.items():
            tot[k] = tot.get(k, 0) + v
    return tot


t0, a0, e0, w0 = tasks(), allocs(), engines(), time.monotonic()
time.sleep(secs)
t1, a1, e1, w1 = tasks(), allocs(), engines(), time.monotonic()
span = w1 - w0
groups = {}
for tid, (comm, run, vol) in t1.items():
    r0, v0 = t0.get(tid, (comm, 0, 0))[1:]
    n0, b0, _ = a0.get(tid, (0, 0, 0))
    n1, b1, _ = a1.get(tid, (0, 0, 0))
    g = groups.setdefault(comm, [0, 0, 0, 0, 0])
    g[0] += run - r0
    g[1] += vol - v0
    g[2] += n1 - n0
    g[3] += b1 - b0
    g[4] += 1
tot = [sum(g[i] for g in groups.values()) for i in range(4)]
print(f"== {label} window {span:.1f}s")
print(
    f"TOTAL cpu {100 * tot[0] / 1e9 / span:.2f}% wake/s {tot[1] / span:.0f} "
    f"malloc/s {tot[2] / span:.0f} alloc-MB/s {tot[3] / span / 1e6:.2f}"
)
for comm, g in sorted(groups.items(), key=lambda kv: -kv[1][0]):
    cpu = 100 * g[0] / 1e9 / span
    if cpu < 0.02 and g[1] / span < 2 and g[2] / span < 20:
        continue
    print(f"  {comm:<16} x{g[4]:<2} cpu {cpu:6.2f}%  wake/s {g[1] / span:7.0f}  malloc/s {g[2] / span:8.0f}")
st = dict(l.split(":", 1) for l in open(f"/proc/{pid}/status"))
print(f"RSS {int(st['VmRSS'].split()[0]) / 1024:.0f} MB, threads {st['Threads'].strip()}, fds {len(os.listdir(f'/proc/{pid}/fd'))}")
print("GPU " + ", ".join(f"{k} {100 * (e1[k] - e0.get(k, 0)) / 1e9 / span:.1f}%" for k in sorted(e1) if e1[k] - e0.get(k, 0) > 0))
