"""A timestamping TCP proxy: what the sender puts on the wire, and when.

usage: r2_wire.py <listen_port> <target_port> <seconds>

Forwards sender -> receiver and back on loopback. Writes of the sender that
arrive less than 2 ms apart count as one burst (a frame); prints the burst
rate, the intervals between bursts (ms: p50, p99, max) and the bytes per
burst. Independent of the receiver's own buffering.
"""
import select
import socket
import sys
import time

lp, tp, secs = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", lp))
srv.listen(1)
a, _ = srv.accept()
t_end = None
b = socket.create_connection(("127.0.0.1", tp))
events = []
start = time.monotonic()
open_ = True
while open_:
    r, _, _ = select.select([a, b], [], [], 1.0)
    for s in r:
        data = s.recv(65536)
        if not data:
            open_ = False
            break
        (b if s is a else a).sendall(data)
        if s is a:
            events.append((time.monotonic() - start, len(data)))
bursts = []
for t, n in events:
    if bursts and t - bursts[-1][1] < 0.002:
        bursts[-1][1] = t
        bursts[-1][2] += n
    else:
        bursts.append([t, t, n])
bursts = [x for x in bursts if x[0] > 8]  # warm-up
if len(bursts) < 10:
    print("wire: too few bursts", len(bursts))
    sys.exit(0)
iv = sorted((y[0] - x[0]) * 1000 for x, y in zip(bursts, bursts[1:]))
sz = sorted(x[2] for x in bursts)
span = bursts[-1][0] - bursts[0][0]
p = lambda v, q: v[min(len(v) - 1, int(len(v) * q))]
print(f"wire: {len(bursts)} bursts in {span:.0f}s ({len(bursts) / span:.0f}/s), interval ms p50 {p(iv, .5):.1f} p99 {p(iv, .99):.1f} max {iv[-1]:.1f}; bytes p50 {p(sz, .5)} max {sz[-1]}")
