"""CPU and GPU engine use of one process over a window.

usage: cost.py <pid> <seconds> [thread name prefix]

CPU from /proc/<pid>/stat (all threads); GPU per engine (gfx, enc, ...)
from the DRM fdinfo of its render node clients, counted once per client.
"""
import os
import sys
import time


def cpu(pid):
    fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def engines(pid):
    clients = {}
    for fd in os.listdir(f"/proc/{pid}/fdinfo"):
        try:
            text = open(f"/proc/{pid}/fdinfo/{fd}").read()
        except OSError:
            continue
        info = dict(line.split(":", 1) for line in text.splitlines() if ":" in line)
        client = info.get("drm-client-id")
        if client is None:
            continue
        clients[client.strip()] = {
            k[len("drm-engine-"):]: int(v.split()[0])
            for k, v in info.items()
            if k.startswith("drm-engine-")
        }
    total = {}
    for c in clients.values():
        for k, v in c.items():
            total[k] = total.get(k, 0) + v
    return total


def threads(pid):
    out = {}
    for tid in os.listdir(f"/proc/{pid}/task"):
        try:
            comm = open(f"/proc/{pid}/task/{tid}/comm").read().strip()
            fields = open(f"/proc/{pid}/task/{tid}/stat").read().rsplit(")", 1)[1].split()
        except OSError:
            continue
        out[(tid, comm)] = (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
    return out


pid, secs = int(sys.argv[1]), float(sys.argv[2])
# Optional: CPU of the threads whose name starts with this, summed per name.
prefix = sys.argv[3] if len(sys.argv) > 3 else None
c0, e0, th0, t0 = cpu(pid), engines(pid), threads(pid), time.monotonic()
time.sleep(secs)
c1, e1, th1, t1 = cpu(pid), engines(pid), threads(pid), time.monotonic()
span = t1 - t0
parts = [f"cpu {100 * (c1 - c0) / span:.1f}% (of one core)"]
for k in sorted(e1):
    busy = (e1[k] - e0.get(k, 0)) / 1e9 / span
    if busy > 0:
        parts.append(f"{k} {100 * busy:.1f}%")
if prefix:
    by_name = {}
    for k, v in th1.items():
        if k[1].startswith(prefix):
            by_name[k[1]] = by_name.get(k[1], 0) + v - th0.get(k, v)
    parts += [f"{n} {100 * c / span:.2f}%" for n, c in sorted(by_name.items())]
print(", ".join(parts))
