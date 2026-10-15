#!/usr/bin/python3 -I
"""Hostile layer clients against the live capture socket.
modes: garbage (random packets, reconnecting), idle (valid hello, then silence), churn (connect/close)."""
import os, random, socket, struct, sys, time

mode, secs = sys.argv[1], float(sys.argv[2])
path = f"/tmp/vesper-{os.getuid()}/capture.sock"
hello = struct.pack("<IIII", 1, 1, os.getpid(), 4) + b"evil"
end = time.time() + secs
socks = []
n = 0
while time.time() < end:
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
        s.connect(path)
    except OSError:
        time.sleep(0.01)
        continue
    n += 1
    if mode == "garbage":
        try:
            s.send(hello)
            for _ in range(200):
                s.send(bytes(random.getrandbits(8) for _ in range(random.randint(0, 100))))
        except OSError:
            pass
        s.close()
    elif mode == "idle":
        s.send(hello)
        s.send(struct.pack("<III", 2, 1280, 720))
        socks.append(s)
        if len(socks) >= 40:
            time.sleep(0.5)
    else:
        s.close()
print(f"{mode}: {n} connections, {len(socks)} held", flush=True)
time.sleep(float(sys.argv[3]) if len(sys.argv) > 3 else 0)
