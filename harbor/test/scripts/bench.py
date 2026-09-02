#!/usr/bin/env python3
"""
bench.py — is this harbor faster than that harbor, and can we trust the answer?

    test/scripts/bench.py                              # bench target/release/harbor
    test/scripts/bench.py ./old-harbor ./new-harbor    # A/B two binaries
    test/scripts/bench.py --rounds 9 --shapes ints,heavy

Every binary serves its own fresh database on its own port, all servers up for
the whole run, and each round times every (shape, binary) pair with the binary
order rotated per round. The rotation is the point: measurements taken
sequentially — all of binary A, then all of binary B — pick up whatever the
machine was doing during each binary's turn, and a bisect run that way once
fingered an innocent commit. Interleaving spreads background noise across all
binaries evenly, so it cancels out of the comparison instead of settling into
one column.

Two clocks per request, because they disagree in useful ways: wall time is
what a client feels, and the server's own timeMs (from the end object) is
immune to client-side and transport noise. When the two diverge, the problem
is not in the encoder.

The shape matrix exists because query shapes fail differently. A fetch-path
change once made streaming 6x faster and compute-heavy queries 17x slower,
and the streaming-only benchmarks of the day called it a pure win. Every
shape here earned its place by catching something:

    point    server round-trip floor, no data to speak of
    ints     cheap production, encoder-bound: the integer hot path
    strings  the JSON-escape scan path
    mixed    several columns per row, the row-framing overhead
    temporal the date/timestamp formatting path
    heavy    compute-bound: one row out, the worker pool does everything

The spread column (max/min within a shape's samples) is the honesty check: a
spread beyond ~2x means the machine was noisy — VMs, swap, a build — and the
medians should be re-earned on quiet hardware, not quoted.
"""

import argparse
import http.client
import json
import os
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time

SHAPES = [
    ("point", "SELECT 42"),
    ("ints", "SELECT i FROM range(5000000) t(i)"),
    ("strings", "SELECT 'x' || i::VARCHAR AS s FROM range(1000000) t(i)"),
    ("mixed", "SELECT i, i*2, i::VARCHAR FROM range(2000000) t(i)"),
    ("temporal", "SELECT TIMESTAMP '2020-01-01' + INTERVAL (i) SECOND AS ts FROM range(1000000) t(i)"),
    ("heavy", "SELECT count(DISTINCT i) FROM range(100000000) t(i)"),
]

BASE_PORT = 18800
TOKEN = "bench"


class Server:
    """One harbor binary serving one scratch database."""

    def __init__(self, binary, port, workdir):
        self.binary = binary
        self.port = port
        self.name = os.path.basename(binary)
        self.db = os.path.join(workdir, f"bench-{port}.duckdb")
        self.log = open(os.path.join(workdir, f"bench-{port}.log"), "wb")
        self.proc = subprocess.Popen(
            [binary, self.db, "start", "--port", str(port), "--token", TOKEN],
            stdout=self.log, stderr=self.log,
        )
        self.conn = None

    def ready(self, seconds=20):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                return False
            try:
                c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=1)
                c.request("GET", "/ready")
                ok = c.getresponse().status == 200
                c.close()
                if ok:
                    return True
            except OSError:
                pass
            time.sleep(0.2)
        return False

    def sql(self, query, timeout=300):
        """Run one query; returns (wall_seconds, server_time_ms)."""
        # One persistent connection per server, like a real client; rebuilt
        # on error so a single hiccup does not poison the rest of the run.
        if self.conn is None:
            self.conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)
        body = json.dumps({"sql": query})
        start = time.monotonic()
        try:
            self.conn.request("POST", "/sql", body, {"Authorization": f"Bearer {TOKEN}"})
            resp = self.conn.getresponse()
            data = resp.read()
        except OSError:
            self.conn.close()
            self.conn = None
            raise
        wall = time.monotonic() - start
        if resp.status != 200:
            raise RuntimeError(f"{self.name}: HTTP {resp.status}: {data[:200]!r}")
        end = json.loads(data.splitlines()[-1])
        if end.get("type") != "end":
            raise RuntimeError(f"{self.name}: no end object: {data[-200:]!r}")
        return wall, end["timeMs"]

    def stop(self):
        if self.conn:
            self.conn.close()
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.log.close()


def fmt_ms(ms):
    return f"{ms:8.1f}"


def main():
    ap = argparse.ArgumentParser(description="interleaved harbor benchmark")
    ap.add_argument("binaries", nargs="*", default=None,
                    help="harbor binaries to compare (default: target/release/harbor)")
    ap.add_argument("--rounds", type=int, default=5, help="measured rounds per shape (default 5)")
    ap.add_argument("--shapes", default=None, help="comma-separated subset of shapes")
    args = ap.parse_args()

    here = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    binaries = args.binaries or [os.path.join(here, "target/release/harbor")]
    shapes = SHAPES
    if args.shapes:
        keep = set(args.shapes.split(","))
        unknown = keep - {n for n, _ in SHAPES}
        if unknown:
            sys.exit(f"bench: unknown shapes: {', '.join(sorted(unknown))}")
        shapes = [s for s in SHAPES if s[0] in keep]

    load = os.getloadavg()[0]
    cores = os.cpu_count() or 1
    if load > cores / 2:
        print(f"bench: WARNING load average {load:.1f} on {cores} cores — "
              f"results will be noisy, prefer a quiet machine")

    workdir = tempfile.mkdtemp(prefix="harbor-bench.")
    servers = []
    try:
        for i, b in enumerate(binaries):
            s = Server(b, BASE_PORT + i, workdir)
            servers.append(s)
            if not s.ready():
                sys.exit(f"bench: {b} did not come up — see {s.log.name}")

        # results[shape][server] = list of (wall, timeMs), keyed by the
        # Server object itself so two paths to the same binary (or the same
        # path given twice) keep separate columns.
        results = {name: {s: [] for s in servers} for name, _ in shapes}

        # One unrecorded warmup pass: first-touch costs (page cache, JIT'd
        # anything, the connection handshake) belong to nobody's column.
        for name, query in shapes:
            for s in servers:
                s.sql(query)

        for r in range(args.rounds):
            # Rotate which binary goes first so slow-drift in machine load
            # spreads across all columns instead of biasing the last one.
            order = servers[r % len(servers):] + servers[:r % len(servers)]
            for name, query in shapes:
                for s in order:
                    results[name][s].append(s.sql(query))
            print(f"round {r + 1}/{args.rounds} done", file=sys.stderr)

        width = max(len(os.path.basename(b)) for b in binaries)
        print(f"\n{'shape':10} {'binary':{width}} {'median':>8} {'min':>8} {'wall':>8} "
              f"{'spread':>7}  (server ms, server ms, wall ms, max/min)")
        noisy = False
        for name, _ in shapes:
            base_median = None
            for s in servers:
                samples = results[name][s]
                server_ms = sorted(t for _, t in samples)
                wall_ms = sorted(w * 1000 for w, _ in samples)
                median = statistics.median(server_ms)
                # min matters as much as median: a bimodal engine pathology
                # (the v2 alpha's nap-race makes ~1 in 3 heavy runs ~20x
                # slower) can land the median on either mode, but the min is
                # the machine's honest capability.
                spread = (max(server_ms) / min(server_ms)) if min(server_ms) > 0 else 1.0
                if spread > 2.0:
                    noisy = True
                ratio = ""
                if base_median is None:
                    base_median = median
                elif base_median > 0:
                    ratio = f"  {median / base_median:5.2f}x"
                print(f"{name:10} {os.path.basename(s.binary):{width}} {fmt_ms(median)} "
                      f"{fmt_ms(server_ms[0])} {fmt_ms(statistics.median(wall_ms))} "
                      f"{spread:6.2f}x{ratio}")
        if noisy:
            print("\nbench: spread exceeded 2x on at least one row — treat "
                  "these numbers as suspect and re-run on quiet hardware")
    finally:
        for s in servers:
            s.stop()
        shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    main()
