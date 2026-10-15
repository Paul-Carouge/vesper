"""An RTMP server that completes the handshake, then hangs up on every
`connect`/`publish`, like an ingest refusing a stream key.

usage: rtmp_reject.py <port> <connections>
"""
import socket
import sys


def read(conn, n):
    data = b""
    while len(data) < n:
        chunk = conn.recv(n - len(data))
        if not chunk:
            raise ConnectionError("closed")
        data += chunk
    return data


port, count = int(sys.argv[1]), int(sys.argv[2])
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(4)
for _ in range(count):
    conn, _ = srv.accept()
    try:
        c0c1 = read(conn, 1537)
        conn.sendall(b"\x03" + bytes(1536) + c0c1[1:])
        read(conn, 1536)
        conn.recv(4096)  # the connect command
    except ConnectionError:
        pass
    conn.close()
    print("hung up on a client", flush=True)
