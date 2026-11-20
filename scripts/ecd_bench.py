"""ecd GET/PUT round trips and IDS cost (implementation/12-testing.md §Performance checks).

usage: python3 -I scripts/ecd_bench.py <socket> <dir-with-files-under-the-store>
"""
import os
import socket
import sys
import time

sock_path, store = sys.argv[1], sys.argv[2]
N = 4000
VALUE = os.urandom(4096)


class Conn:
    def __init__(self):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(sock_path)
        self.buf = b""

    def line(self):
        while b"\n" not in self.buf:
            chunk = self.s.recv(65536)
            if not chunk:
                raise EOFError
            self.buf += chunk
        line, _, self.buf = self.buf.partition(b"\n")
        return line

    def exact(self, n):
        while len(self.buf) < n:
            self.buf += self.s.recv(65536)
        out, self.buf = self.buf[:n], self.buf[n:]
        return out


def timed(label, n, fn):
    t = time.perf_counter()
    fn()
    dt = time.perf_counter() - t
    print(f"{label:<34} {n / dt:>9.0f} ops/s  {dt / n * 1e6:7.1f} us/op")


c = Conn()


def puts():
    for i in range(N):
        c.s.sendall(b"PUT bench-%d %d\n" % (i, len(VALUE)) + VALUE)
        assert c.line() == b"OK"


def gets():
    for i in range(N):
        c.s.sendall(b"GET bench-%d\n" % i)
        head = c.line()
        assert head == b"OK %d" % len(VALUE), head
        c.exact(len(VALUE))


def gets_pipelined():
    c.s.sendall(b"".join(b"GET bench-%d\n" % i for i in range(N)))
    for _ in range(N):
        head = c.line()
        assert head.startswith(b"OK "), head
        c.exact(int(head[3:]))


paths = sorted(os.path.join(store, f) for f in os.listdir(store))
ids_req = b"IDS %d\n" % len(paths) + b"".join(p.encode() + b"\n" for p in paths)


def ids_once():
    c.s.sendall(ids_req)
    for _ in paths:
        assert c.line().startswith(b"C:")


timed("PUT 4 KiB, one at a time", N, puts)
timed("GET 4 KiB hit, one at a time", N, gets)
timed("GET 4 KiB hit, pipelined", N, gets_pipelined)
timed("IDS 100 paths, first (hashing)", 1, ids_once)
timed("IDS 100 paths, memoised", 200, lambda: [ids_once() for _ in range(200)])
