"""CPU and GPU engine use of one process over a window.

usage: cost.py <pid> <seconds>

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


pid, secs = int(sys.argv[1]), float(sys.argv[2])
c0, e0, t0 = cpu(pid), engines(pid), time.monotonic()
time.sleep(secs)
c1, e1, t1 = cpu(pid), engines(pid), time.monotonic()
span = t1 - t0
parts = [f"cpu {100 * (c1 - c0) / span:.1f}% (of one core)"]
for k in sorted(e1):
    busy = (e1[k] - e0.get(k, 0)) / 1e9 / span
    if busy > 0:
        parts.append(f"{k} {100 * busy:.1f}%")
print(", ".join(parts))
