#!/usr/bin/env python3
"""
swarm.py — how does harbor behave when many HTTP clients hit it at once?

    scripts/swarm.py --port 9499 --token T --levels 1,2,4,8,16,32,64
    scripts/swarm.py --port 9499 --token T --write-pct 25 --seconds 5

Each level runs N client threads for a fixed wall-clock duration, each thread
issuing requests back to back, and reports throughput and the latency
distribution. Latency percentiles matter more than the mean here: harbor
executes a bounded number of statements at once, so past that bound requests
queue, and queueing shows up in the tail long before it shows up in the
average.

Two things this is careful about:

Connections are reused, one per client thread, because that is what a real
client does and what harbor now supports. `--no-keepalive` opens a fresh
connection per request instead; the contrast is stark, and not because the
server got slower. Without reuse each request burns a client ephemeral port
for the TIME_WAIT interval, so a few thousand requests per second exhausts
the ~16k-port range in seconds and the client — not the server — starts
failing with "Can\'t assign requested address".

The reported throughput is measured against wall-clock across the whole
level, not summed from per-request timings, so thread scheduling noise cannot
inflate it.
"""

import argparse
import http.client
import json
import random
import statistics
import sys
import threading
import time


class Level:
    """Results for one concurrency level."""

    def __init__(self, clients):
        self.clients = clients
        self.latencies = []
        self.errors = 0
        self.wrong = 0
        self.status = {}
        self.lock = threading.Lock()

    def record(self, seconds, code, ok):
        with self.lock:
            self.latencies.append(seconds)
            self.status[code] = self.status.get(code, 0) + 1
            if code != 200:
                self.errors += 1
            elif not ok:
                self.wrong += 1


class Client:
    """A single HTTP connection, reopened if the server or network drops it."""

    def __init__(self, host, port, token, keepalive=True, timeout=60):
        self.host, self.port, self.token = host, port, token
        self.keepalive, self.timeout = keepalive, timeout
        self.conn = None
        self.reconnects = 0

    def _connect(self):
        self.conn = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)

    def close(self):
        if self.conn is not None:
            self.conn.close()
            self.conn = None

    def request(self, sql, params=None):
        """One POST /sql. Returns (status, seconds, parsed-envelope)."""
        body = json.dumps({"sql": sql, **({"params": params} if params else {})})
        headers = {"Authorization": "Bearer " + self.token,
                   "Content-Type": "application/json"}
        started = time.perf_counter()
        for attempt in (0, 1):
            try:
                if self.conn is None:
                    self._connect()
                self.conn.request("POST", "/sql", body, headers)
                resp = self.conn.getresponse()
                # Decoded before splitting. NDJSON is delimited by \n alone
                # and splitlines() would also break on U+2028, which is legal
                # inside a JSON string — but split("\n") on the raw bytes
                # needs a bytes separator, so the decode is not optional.
                payload = resp.read().decode("utf-8", "replace")
                if not self.keepalive:
                    self.close()
                elapsed = time.perf_counter() - started
                lines = [json.loads(l) for l in payload.split("\n") if l.strip()]
                return resp.status, elapsed, lines
            except Exception as e:
                self.close()
                # One retry covers a connection the server closed between
                # requests. A second failure is a real failure.
                if attempt == 1:
                    return 0, time.perf_counter() - started, [{"type": "error", "message": str(e)}]
                self.reconnects += 1
        return 0, time.perf_counter() - started, []


def request(host, port, token, sql, params=None, timeout=60):
    """One-off request, for setup and probing."""
    c = Client(host, port, token, keepalive=False, timeout=timeout)
    try:
        return c.request(sql, params)
    finally:
        c.close()


def rows_of(lines):
    return [l["values"] for l in lines if l.get("type") == "row"]


def run_level(args, clients, expected_sites, stop_after):
    """Drive `clients` threads for `stop_after` seconds; return a Level."""
    level = Level(clients)
    barrier = threading.Barrier(clients + 1)

    def worker(idx):
        rng = random.Random(idx)
        client = Client(args.host, args.port, args.token, keepalive=not args.no_keepalive)
        # Connect before the clock starts. Otherwise the first request of every
        # thread pays for a simultaneous connect storm: at high client counts
        # the accept backlog overflows, the kernel drops SYNs, and TCP retries
        # at 1s, 2s, 4s. That shows up as a multi-second maximum which says
        # nothing about how fast harbor answers a query.
        client.request("SELECT 1")
        barrier.wait()
        # Each thread times itself from the barrier. A deadline computed before
        # the threads start would be partly spent on warmup, and a slow warmup
        # could consume the whole window.
        deadline = time.time() + stop_after
        while time.time() < deadline:
            if rng.random() * 100 < args.write_pct:
                # Writes contend: same table, and DuckDB is a single-writer
                # engine, so this is where queueing shows up first.
                code, secs, lines = client.request(
                    "INSERT INTO swarm_log(client, n) VALUES (?, ?)",
                    [idx, rng.randint(1, 1_000_000)],
                )
                ok = code == 200
            else:
                choice = rng.random()
                if choice < 0.45:
                    sql, want = "SELECT count(*) AS n FROM sites", str(expected_sites)
                elif choice < 0.75:
                    sql, want = (
                        "SELECT client, count(*) AS n FROM plans GROUP BY 1 "
                        "ORDER BY n DESC, client LIMIT 5",
                        None,
                    )
                else:
                    sql, want = (
                        "SELECT s.client, count(*) AS n FROM sites s "
                        "JOIN plans p USING (client) GROUP BY 1 ORDER BY 1",
                        None,
                    )
                code, secs, lines = client.request(sql)
                rows = rows_of(lines)
                ok = code == 200 and bool(rows) and (want is None or str(rows[0][0]) == want)
            level.record(secs, code, ok)
        client.close()

    threads = [threading.Thread(target=worker, args=(i,), daemon=True) for i in range(clients)]
    for t in threads:
        t.start()
    barrier.wait()
    started = time.perf_counter()
    for t in threads:
        t.join()
    level.wall = time.perf_counter() - started
    return level


def pct(values, p):
    if not values:
        return 0.0
    ordered = sorted(values)
    k = min(len(ordered) - 1, int(round((p / 100) * (len(ordered) - 1))))
    return ordered[k]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=9499)
    ap.add_argument("--token", required=True)
    ap.add_argument("--levels", default="1,2,4,8,16,32,64")
    ap.add_argument("--seconds", type=float, default=4.0)
    ap.add_argument("--write-pct", type=float, default=0.0,
                    help="percentage of requests that are INSERTs")
    ap.add_argument("--no-keepalive", action="store_true",
                    help="open a fresh connection per request")
    args = ap.parse_args()

    code, _, lines = request(args.host, args.port, args.token, "SELECT count(*) AS n FROM sites")
    if code != 200:
        print("swarm: server did not answer (status %s)" % code, file=sys.stderr)
        return 2
    expected_sites = rows_of(lines)[0][0]

    if args.write_pct > 0:
        request(args.host, args.port, args.token,
                "CREATE TABLE IF NOT EXISTS swarm_log(client BIGINT, n BIGINT)")

    print("harbor swarm — %.0fs per level, %.0f%% writes, %s connections, "
          "reads verified against count(sites)=%s"
          % (args.seconds, args.write_pct,
             "fresh" if args.no_keepalive else "reused", expected_sites))
    print()
    print("  clients      req/s     mean      p50      p95      p99      max   non-200   wrong")
    print("  " + "-" * 84)

    regressions = []
    for clients in [int(x) for x in args.levels.split(",")]:
        lv = run_level(args, clients, expected_sites, args.seconds)
        n = len(lv.latencies)
        rps = n / lv.wall if lv.wall else 0
        ms = lambda v: v * 1000
        print("  %7d  %9.0f  %6.1fms %6.1fms %6.1fms %6.1fms %6.1fms  %8d  %6d"
              % (clients, rps, ms(statistics.fmean(lv.latencies)) if n else 0,
                 ms(pct(lv.latencies, 50)), ms(pct(lv.latencies, 95)),
                 ms(pct(lv.latencies, 99)), ms(max(lv.latencies)) if n else 0,
                 lv.errors, lv.wrong))
        if lv.errors or lv.wrong:
            regressions.append((clients, lv.errors, lv.wrong, lv.status))

    print()
    if regressions:
        print("FAILURES")
        for clients, errors, wrong, status in regressions:
            print("  %d clients: %d non-200, %d wrong answers, statuses %s"
                  % (clients, errors, wrong, status))
        return 1
    print("no errors and no wrong answers at any level")
    return 0


if __name__ == "__main__":
    sys.exit(main())
