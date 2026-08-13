#!/usr/bin/env python3
"""sessions.py — transaction leases: lifecycle, isolation, exhaustion, reaping,
and the two shutdown properties that make them safe to hand out at all.

Runs its own server, because it needs a pool size the other suites do not use
and it kills the server mid-transaction, which the shared one cannot survive.

  test/scripts/sessions.py [--db PATH] [--keep]

A lease is a connection pinned across requests. The interesting failures are
not "does BEGIN work" — they are the ones where a connection goes out and does
not come back, so most of what is below is accounting.
"""

import argparse
import json
import os
import random
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
TOKEN = "sessions-suite-token"

passed = 0
failed = []


def ok(name, detail=""):
    global passed
    passed += 1
    print(f"  \033[32m✓\033[0m {name}" + (f" \033[2m{detail}\033[0m" if detail else ""))


def bad(name, detail):
    failed.append(name)
    print(f"  \033[31m✗\033[0m {name}\n     \033[2m{detail}\033[0m")


def eq(name, expected, actual):
    if expected == actual:
        ok(name)
    else:
        bad(name, f"expected [{expected}], got [{actual}]")


def section(title):
    print(f"\n\033[1m{title}\033[0m")


# ---------------------------------------------------------------------------
# HTTP
# ---------------------------------------------------------------------------


class Harbor:
    def __init__(self, base, token=TOKEN):
        self.base = base
        self.token = token

    def call(self, method, path, body=None, timeout=30):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(
            self.base + path,
            data=data,
            method=method,
            headers={
                "Authorization": f"Bearer {self.token}",
                "Accept": "application/json",
                "Content-Type": "application/json",
            },
        )
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                raw = r.read()
                return r.status, (json.loads(raw) if raw else {}), dict(r.headers)
        except urllib.error.HTTPError as e:
            raw = e.read()
            return e.code, (json.loads(raw) if raw else {}), dict(e.headers)

    def sql(self, statement, session=None, params=None):
        body = {"sql": statement}
        if session:
            body["sessionId"] = session
        if params:
            body["params"] = params
        return self.call("POST", "/sql", body)

    def value(self, statement, session=None):
        st, doc, _ = self.sql(statement, session)
        if st != 200 or not doc.get("data"):
            return None
        return doc["data"][0][0]

    def open(self, ttl_ms=None):
        body = {} if ttl_ms is None else {"ttlMs": ttl_ms}
        return self.call("POST", "/sql/sessions/new", body)

    def release(self, sid):
        return self.call("DELETE", "/sql/sessions/" + sid)

    def sessions(self):
        return self.call("GET", "/sessions")[1]

    def connections(self):
        return self.sessions()["connections"]


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def start_server(db, pool_size, workers, port):
    env = dict(os.environ, HARBOR_POOL_SIZE=str(pool_size))
    log = open(db + ".log", "w")
    proc = subprocess.Popen(
        [
            os.path.join(HERE, "bin", "duckdb-harbor"), db,
            "--port", str(port), "--token", TOKEN, "--workers", str(workers),
        ],
        stdout=log, stderr=log, stdin=subprocess.DEVNULL, env=env,
    )
    base = f"http://127.0.0.1:{port}"
    for _ in range(120):
        try:
            urllib.request.urlopen(base + "/ready", timeout=1).read()
            return proc, Harbor(base), log
        except Exception:
            if proc.poll() is not None:
                break
            time.sleep(0.25)
    log.flush()
    print(open(db + ".log").read(), file=sys.stderr)
    raise SystemExit("sessions: the server never came up")


# ---------------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    work = tempfile.mkdtemp(prefix="harbor-sessions-")
    db = os.path.join(work, "sessions.duckdb")
    subprocess.run(
        ["duckdb", "-no-init", db, "-c",
         "CREATE TABLE t (id INTEGER, n INTEGER); INSERT INTO t VALUES (1,0),(2,0),(3,0); CHECKPOINT;"],
        check=True, stdout=subprocess.DEVNULL,
    )

    # Four workers and four lease connections: small enough that exhaustion is
    # reachable in a test, split so neither can starve the other.
    workers, leases = 4, 4
    port = free_port()
    proc, h, log = start_server(db, workers + leases, workers, port)

    try:
        run_all(h, leases, proc, db, port)
    finally:
        if proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                proc.kill()
        log.close()
        if args.keep:
            print(f"\nkept: {work}")
        else:
            shutil.rmtree(work, ignore_errors=True)

    print()
    if failed:
        print(f"\033[31m{passed} passed, {len(failed)} failed\033[0m")
        for f in failed:
            print(f"  - {f}")
        return 1
    print(f"\033[32m{passed} passed, 0 failed\033[0m")
    return 0


def run_all(h, leases, proc, db, port):
    # -----------------------------------------------------------------------
    section("The pool is split")
    # -----------------------------------------------------------------------
    conns = h.connections()
    eq("lease connections are what the workers did not take", leases, conns["total"])
    eq("all of them start free", leases, conns["free"])
    eq("the accounting balances", True, conns["balanced"])

    # -----------------------------------------------------------------------
    section("A transaction spans requests")
    # -----------------------------------------------------------------------
    h.sql("UPDATE t SET n = 0")
    st, s, _ = h.open()
    eq("opening a lease is a 200", 200, st)
    sid = s["sessionId"]
    ok("it names a session", sid[:12] + "…")
    eq("and declares both clocks", True, "ttlMs" in s and "idleTtlMs" in s)

    h.sql("BEGIN", sid)
    h.sql("UPDATE t SET n = 42 WHERE id = 1", sid)
    # The whole point: the write is real inside the transaction and invisible
    # outside it. Without a pinned connection the second request would land on
    # a different one and see nothing — which is what a lease is for.
    eq("the session sees its own uncommitted write", 42, h.value("SELECT n FROM t WHERE id=1", sid))
    eq("nobody else does", 0, h.value("SELECT n FROM t WHERE id=1"))

    live = h.sessions()["sessions"]
    eq("/sessions shows one lease", 1, len(live))
    eq("and knows it is holding a transaction", True, live[0]["inTransaction"])
    eq("workers keep serving while it is held", 1, h.value("SELECT 1"))

    h.sql("COMMIT", sid)
    eq("the commit is visible to everyone", 42, h.value("SELECT n FROM t WHERE id=1"))
    eq("after COMMIT the lease is not in a transaction", False, h.sessions()["sessions"][0]["inTransaction"])
    eq("releasing reports it released", True, h.release(sid)[1]["released"])
    eq("releasing again is a no-op, not a double free", False, h.release(sid)[1]["released"])
    eq("the connection came back", leases, h.connections()["free"])

    # -----------------------------------------------------------------------
    section("A transaction that rolls back")
    # -----------------------------------------------------------------------
    sid = h.open()[1]["sessionId"]
    h.sql("BEGIN", sid)
    h.sql("UPDATE t SET n = 99 WHERE id = 2", sid)
    h.sql("ROLLBACK", sid)
    eq("the write is gone", 0, h.value("SELECT n FROM t WHERE id=2"))
    h.release(sid)

    # A lease released mid-transaction must roll back, not leak the write into
    # whoever borrows the connection next.
    sid = h.open()[1]["sessionId"]
    h.sql("BEGIN", sid)
    h.sql("UPDATE t SET n = 77 WHERE id = 2", sid)
    h.release(sid)
    eq("releasing mid-transaction rolls back", 0, h.value("SELECT n FROM t WHERE id=2"))
    eq("and the next lease starts clean", 0, h.value("SELECT n FROM t WHERE id=2", h.open()[1]["sessionId"]))
    for s in h.sessions()["sessions"]:
        h.release(s["sessionId"])

    # -----------------------------------------------------------------------
    section("Exhaustion is a refusal, not a hang")
    # -----------------------------------------------------------------------
    held = [h.open()[1]["sessionId"] for _ in range(leases)]
    eq("every lease connection is out", 0, h.connections()["free"])
    st, doc, hdrs = h.open()
    eq("one more is refused", 503, st)
    eq("with a code that says why", "no_lease_available", doc.get("code"))
    eq("and how long to wait", "1", hdrs.get("Retry-After"))
    # The failure mode this design exists to avoid: leases eating the workers.
    eq("ordinary queries are unaffected", 1, h.value("SELECT 1"))
    eq("so is /ready", 200, h.call("GET", "/ready")[0])
    for sid in held:
        h.release(sid)
    eq("all connections return", leases, h.connections()["free"])

    # -----------------------------------------------------------------------
    section("A lease is a sequence, not a pool")
    # -----------------------------------------------------------------------
    sid = h.open()[1]["sessionId"]
    # A long statement, and a second one sent while it runs. Two statements
    # interleaving inside one transaction is something no client could reason
    # about, so the second is refused rather than queued.
    result = {}

    def slow():
        result["slow"] = h.sql("SELECT count(DISTINCT i) FROM range(200000000) t(i)", sid)[0]

    t = threading.Thread(target=slow)
    t.start()
    time.sleep(0.4)
    st, doc, _ = h.sql("SELECT 1", sid)
    t.join()
    eq("the long statement succeeds", 200, result["slow"])
    eq("the concurrent one is refused", 409, st)
    eq("with a code that says why", "session_busy", doc.get("code"))
    # Sequential statements must never see that refusal, however fast they are
    # sent: the claim is released after the response is written, and a client
    # can beat the server to it.
    burst = [h.sql("SELECT 1", sid)[0] for _ in range(60)]
    eq("60 back-to-back statements all land", {200}, set(burst))
    h.release(sid)

    # -----------------------------------------------------------------------
    section("Unknown, released and expired leases are one answer")
    # -----------------------------------------------------------------------
    st, doc, _ = h.sql("SELECT 1", "0" * 36)
    eq("an unknown session is a 404", 404, st)
    eq("with a code that says why", "no_such_session", doc.get("code"))
    sid = h.open()[1]["sessionId"]
    h.release(sid)
    eq("a released one answers the same", "no_such_session", h.sql("SELECT 1", sid)[1].get("code"))

    # -----------------------------------------------------------------------
    section("The reaper reclaims what is abandoned")
    # -----------------------------------------------------------------------
    # A deadline the client asked for, rather than waiting out the idle
    # timeout: the point is that the connection comes back on its own.
    st, s, _ = h.open(ttl_ms=1500)
    sid = s["sessionId"]
    eq("a short lease is granted as asked", 1500, s["ttlMs"])
    h.sql("BEGIN", sid)
    h.sql("UPDATE t SET n = 5 WHERE id = 3", sid)
    eq("it is holding a transaction", 1, len(h.sessions()["sessions"]))
    deadline = time.time() + 10
    while time.time() < deadline and h.sessions()["sessions"]:
        time.sleep(0.25)
    eq("the abandoned lease is reclaimed", 0, len(h.sessions()["sessions"]))
    eq("its connection is back in the pool", leases, h.connections()["free"])
    eq("and its transaction was rolled back", 0, h.value("SELECT n FROM t WHERE id=3"))
    eq("the accounting still balances", True, h.connections()["balanced"])

    # And the row it was holding is writable again. A rollback that only
    # discarded the value but left the row locked would have passed the
    # assertion above and still leaked the thing that matters.
    #
    # (Not a CHECKPOINT assertion, though it is the obvious one to reach for:
    # `harbor_wait` holds a read transaction older than every write for as long
    # as the server runs, so CHECKPOINT over HTTP fails after the first write
    # whether leases exist or not. The shutdown section below is where that
    # property is actually tested, at the one moment it matters.)
    sid = h.open()[1]["sessionId"]
    h.sql("BEGIN", sid)
    eq("the row the abandoned lease held is writable again", 200,
       h.sql("UPDATE t SET n = 6 WHERE id = 3", sid)[0])
    h.sql("COMMIT", sid)
    h.release(sid)
    eq("and that write landed", 6, h.value("SELECT n FROM t WHERE id=3"))

    # -----------------------------------------------------------------------
    section("Connections are conserved under chaos")
    # -----------------------------------------------------------------------
    # A pool has one catastrophic bug: a connection that goes out and never
    # comes back. It shows up in production weeks later as "everything hangs",
    # so it is worth manufacturing here. Every client does something different
    # and some of them misbehave on purpose — abandoning leases, releasing
    # twice, using released ids, committing nothing.
    rng = random.Random(20260813)
    stop_at = time.time() + 12
    drift = []
    errors = []

    def watcher():
        while time.time() < stop_at:
            try:
                c = h.connections()
                if not c["balanced"] or c["free"] + c["live"] + c["inflight"] != c["total"]:
                    drift.append(c)
            except Exception as e:
                errors.append(f"watcher: {e}")
            time.sleep(0.05)

    def client(seed):
        r = random.Random(seed)
        me = Harbor(h.base)
        while time.time() < stop_at:
            try:
                st, s, _ = me.open(ttl_ms=r.choice([800, 1500, 3000]))
                if st != 200:
                    time.sleep(0.05)   # exhausted: the documented answer
                    continue
                sid = s["sessionId"]
                me.sql("BEGIN", sid)
                for _ in range(r.randint(1, 4)):
                    me.sql("UPDATE t SET n = n + 1 WHERE id = ?", sid, params=[r.randint(1, 3)])
                    me.sql("SELECT count(*) FROM t", sid)
                choice = r.random()
                if choice < 0.35:
                    me.sql("COMMIT", sid)
                    me.release(sid)
                elif choice < 0.60:
                    me.sql("ROLLBACK", sid)
                    me.release(sid)
                elif choice < 0.80:
                    me.release(sid)          # release with the transaction open
                    me.release(sid)          # and again, to prove it is idempotent
                    me.sql("SELECT 1", sid)  # and use it after release
                else:
                    pass                     # abandon it and let the reaper work
            except Exception as e:
                errors.append(f"client: {e}")

    threads = [threading.Thread(target=watcher)]
    threads += [threading.Thread(target=client, args=(rng.randint(0, 1 << 30),)) for _ in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    eq("no client saw a transport failure", [], errors[:3])
    eq("the invariant never drifted", [], drift[:3])

    # Everything abandoned has to come back on its own, with no help.
    deadline = time.time() + 20
    while time.time() < deadline and h.connections()["free"] != leases:
        time.sleep(0.25)
    eq("every connection comes back when the storm passes", leases, h.connections()["free"])
    eq("nothing is stuck in flight", 0, h.connections()["inflight"])
    eq("the server still answers", 1, h.value("SELECT 1"))
    eq("and can still write", 200, h.sql("UPDATE t SET n = 0")[0])
    sid = h.open()[1]["sessionId"]
    h.sql("BEGIN", sid)
    eq("and can still run a transaction", 200, h.sql("UPDATE t SET n = 1 WHERE id = 1", sid)[0])
    h.sql("COMMIT", sid)
    h.release(sid)
    eq("which commits", 1, h.value("SELECT n FROM t WHERE id=1"))

    # -----------------------------------------------------------------------
    section("Shutdown with a transaction open")
    # -----------------------------------------------------------------------
    # The property that made the reaper necessary, at the one moment it cannot
    # be retried: SIGTERM folds the WAL, and an open write transaction makes
    # that fail. A lease left open here must not cost the checkpoint.
    sid = h.open(ttl_ms=300000)[1]["sessionId"]
    h.sql("BEGIN", sid)
    h.sql("INSERT INTO t VALUES (9, 900)", sid)
    h.sql("COMMIT", sid)
    h.sql("BEGIN", sid)
    h.sql("UPDATE t SET n = 12345 WHERE id = 9", sid)   # left open, deliberately
    ok("a lease is holding an open write transaction", "committed row 9, then began another")

    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=30)
        ok("SIGTERM stops the server")
    except subprocess.TimeoutExpired:
        proc.kill()
        bad("SIGTERM stops the server", "still running after 30s")

    wal = db + ".wal"
    eq("the WAL was folded in on shutdown", False, os.path.exists(wal))
    out = subprocess.run(
        ["duckdb", "-no-init", "-readonly", "-csv", "-noheader", db, "-c",
         "SELECT n FROM t WHERE id = 9"],
        capture_output=True, text=True,
    ).stdout.strip()
    eq("the committed row survived", "900", out)
    ok("and the open transaction did not", "rolled back before the checkpoint")


if __name__ == "__main__":
    sys.exit(main())
