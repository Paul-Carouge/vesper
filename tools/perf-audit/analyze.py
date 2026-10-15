#!/usr/bin/env python3
"""Summarizes a run dir written by run_game.sh / run_ui.sh."""
import csv, glob, os, re, sys
from collections import defaultdict

HZ = os.sysconf("SC_CLK_TCK")


def rate_window(rows, skip=3):
    """First and last sample after `skip` s of warm-up."""
    rows = sorted(rows)
    if len(rows) < skip + 2:
        return rows[0], rows[-1]
    return rows[skip], rows[-1]


def threads(d):
    p = os.path.join(d, "threads.txt")
    if not os.path.exists(p):
        return
    by = defaultdict(list)  # tid -> [(ts, name, ticks, vcs, nvcs)]
    for line in open(p):
        f = line.split()
        if len(f) < 8:
            continue
        _, ts, tid, name, ut, st, vcs, nvcs = f[:8]
        by[tid].append((float(ts), name, int(ut) + int(st), int(vcs), int(nvcs)))
    t_all = sorted({r[0] for v in by.values() for r in v})
    if len(t_all) < 5:
        return
    t0, t1 = t_all[3], t_all[-1]
    agg = defaultdict(lambda: [0.0, 0.0, 0.0])
    for tid, rows in by.items():
        rows = [r for r in rows if t0 - 0.5 <= r[0] <= t1 + 0.5]
        if len(rows) < 2:
            continue
        a, b = rows[0], rows[-1]
        dt = b[0] - a[0]
        if dt <= 0:
            continue
        name = re.sub(r"\d+$", "N", b[1])
        agg[name][0] += (b[2] - a[2]) / 1e9 / dt * 100
        agg[name][1] += (b[3] - a[3]) / dt
        agg[name][2] += (b[4] - a[4]) / dt
    total = sum(v[0] for v in agg.values())
    print(f"  vesper threads over {t1 - t0:.0f}s: total CPU {total:.2f}% of one core")
    for name, (cpu, vcs, nvcs) in sorted(agg.items(), key=lambda kv: -kv[1][0]):
        if cpu >= 0.05 or vcs >= 5:
            print(f"    {name:<22} cpu {cpu:6.2f}%  wakeups(vcsw)/s {vcs:7.1f}  preempt(nvcsw)/s {nvcs:6.1f}")


def engines_line(line):
    return {k: int(v) for k, v in re.findall(r"(drm-engine-[a-z_0-9]+)=(\d+)", line)}


def engine_rates(rows):
    """Median per-interval busy % per engine (robust to counter glitches)."""
    per = defaultdict(list)
    for a, b in zip(rows[3:], rows[4:]):
        ea, eb = engines_line(" ".join(a)), engines_line(" ".join(b))
        dt = float(b[0]) - float(a[0])
        for k in eb:
            if k in ea and dt > 0:
                per[k].append((eb[k] - ea[k]) / 1e9 / dt * 100)
    out = []
    for k, v in sorted(per.items()):
        v.sort()
        out.append(f"{k[11:]} {v[len(v)//2]:.1f}%")
    return "  ".join(out)


def proc(d):
    p = os.path.join(d, "vproc.txt")
    if not os.path.exists(p):
        return
    rows = [l.split() for l in open(p) if l.strip()]
    if len(rows) < 5:
        return
    ts = [float(r[0]) for r in rows]
    rss = [int(r[1].split("=")[1]) for r in rows]
    fds = [int(r[2].split("=")[1]) for r in rows]
    thr = [int(r[3].split("=")[1]) for r in rows]
    print(f"  RSS start {rss[2]/1024:.1f} MiB  end {rss[-1]/1024:.1f} MiB  max {max(rss)/1024:.1f} MiB  fds {fds[2]}..{max(fds)}  threads {thr[2]}..{max(thr)}")
    print("  recorder GPU engine busy (median/s): " + engine_rates(rows))


def game(d):
    p = os.path.join(d, "game.txt")
    if not os.path.exists(p):
        return
    rows = [l.split() for l in open(p) if l.strip()]
    if len(rows) < 5:
        return
    a, b = rows[3], rows[-1]
    dt = float(b[0]) - float(a[0])
    cpu = (int(b[1]) - int(a[1])) / 1e9 / dt * 100
    eng = engine_rates(rows)
    print(f"  game CPU {cpu:.1f}%  game GPU engines: {eng}")


def gpu(d):
    p = os.path.join(d, "gpu.txt")
    if not os.path.exists(p):
        return
    v = [int(l.split("=")[1]) for l in open(p) if "=" in l][3:]
    if v:
        print(f"  card1 gpu_busy mean {sum(v)/len(v):.1f}%  max {max(v)}")


def mango(d):
    for p in glob.glob(os.path.join(d, "*.csv")):
        if p.endswith("_summary.csv"):
            continue
        lines = open(p).read().splitlines()
        rows = list(csv.DictReader(lines[2:]))
        ft = sorted(float(r["frametime"]) for r in rows if r.get("frametime"))
        if not ft:
            continue
        n = len(ft)
        mean = sum(ft) / n
        pct = lambda q: ft[min(n - 1, int(q * (n - 1)))]
        print(f"  game frames {n}  mean frametime {mean:.3f} ms ({1000/mean:.1f} fps)  p50 {pct(.5):.3f}  p99 {pct(.99):.3f}  p99.9 {pct(.999):.3f}  max {ft[-1]:.3f}")


def main():
    for d in sys.argv[1:]:
        print(f"== {os.path.basename(d.rstrip('/'))}")
        for f in ("probe.txt",):
            p = os.path.join(d, f)
            if os.path.exists(p):
                print("  " + open(p).read().strip().replace("\n", "  "))
        vl = os.path.join(d, "vesper.log")
        if os.path.exists(vl):
            for l in open(vl):
                if l.startswith("[audit]") or "dropped" in l and "Stats" not in l:
                    print("  " + l.rstrip())
        threads(d)
        proc(d)
        game(d)
        gpu(d)
        mango(d)


main()
