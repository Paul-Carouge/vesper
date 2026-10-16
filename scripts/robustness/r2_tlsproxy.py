"""A TLS terminator for the lab: rtmps://127.0.0.1:<listen> to a plain RTMP server.

usage: r2_tlsproxy.py <listen_port> <target_port> <cert.pem> <key.pem> [die_after_s]

With `die_after_s`, the connection is cut abruptly (RST) that long after it
was accepted: a TLS server that dies mid-stream.
"""
import select
import socket
import ssl
import struct
import sys
import time

lp, tp = int(sys.argv[1]), int(sys.argv[2])
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(sys.argv[3], sys.argv[4])
die = float(sys.argv[5]) if len(sys.argv) > 5 else None
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", lp))
srv.listen(4)
while True:
    raw, _ = srv.accept()
    t0 = time.monotonic()
    try:
        a = ctx.wrap_socket(raw, server_side=True)
    except ssl.SSLError as e:
        print("handshake failed:", e, flush=True)
        continue
    print("tls up", flush=True)
    b = socket.create_connection(("127.0.0.1", tp))
    while True:
        if die is not None and time.monotonic() - t0 > die:
            a.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            a.close()
            b.close()
            print("cut", flush=True)
            break
        r, _, _ = select.select([a, b], [], [], 0.5)
        done = False
        for s in r:
            try:
                data = s.recv(65536)
            except (ssl.SSLError, OSError):
                data = b""
            if not data:
                done = True
                break
            (b if s is a else a).sendall(data)
        if done:
            a.close()
            b.close()
            break
