"""Arrival jitter of a live stream, from ffprobe's packet list on stdin.

usage: ffprobe -v error -show_entries packet=stream_index,pts_time,size,flags -of csv ... | r2_arrival.py [skip_s]

Each line is stamped on arrival. For every video packet, lateness = (arrival
- pts) minus the smallest such value after `skip_s` of warm-up: the variation of
capture-to-receiver latency (the constant part, pipeline + encoder + loopback,
is not measurable without a clock in the picture). Prints percentiles in ms,
for all video packets and for keyframes, and the largest arrival gap.
"""
import sys
import time

skip = float(sys.argv[1]) if len(sys.argv) > 1 else 8.0
t0 = time.monotonic()
rows = []  # (arrival, pts, key, size)
for line in sys.stdin:
    f = line.strip().split(",")
    # csv: packet,stream_index,pts_time,size,flags
    if len(f) < 5 or f[0] != "packet" or f[1] != "0":
        continue
    try:
        rows.append((time.monotonic() - t0, float(f[2]), "K" in f[4], int(f[3])))
    except ValueError:
        pass
rows = [r for r in rows if r[1] >= skip]
if len(rows) < 10:
    print("arrival: too few packets", len(rows))
    sys.exit(1)
late = [r[0] - r[1] for r in rows]
base = min(late)


def pct(vals, p):
    vals = sorted(vals)
    return vals[min(len(vals) - 1, int(len(vals) * p))]


allv = [(x - base) * 1000 for x in late]
keyv = [(x - base) * 1000 for x, r in zip(late, rows) if r[2]]
gaps = [(b[0] - a[0]) * 1000 for a, b in zip(rows, rows[1:])]
print(f"video packets {len(rows)}, keyframes {len(keyv)}, mean size {sum(r[3] for r in rows) / len(rows) / 1000:.1f} kB")
print("lateness ms  p50 %.1f  p90 %.1f  p99 %.1f  max %.1f" % (pct(allv, .5), pct(allv, .9), pct(allv, .99), max(allv)))
if keyv:
    print("keyframes ms p50 %.1f  max %.1f" % (pct(keyv, .5), max(keyv)))
print("arrival gap ms  p50 %.1f  p99 %.1f  max %.1f" % (pct(gaps, .5), pct(gaps, .99), max(gaps)))
