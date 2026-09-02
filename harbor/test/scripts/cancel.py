#!/usr/bin/env python3
"""cancel.py — stopping a statement that has already started.

Runs its own server, because most of what is below needs a query that takes
seconds and a pool small enough to saturate on purpose.

  test/scripts/cancel.py [--db PATH] [--keep]

A statement inside DuckDB does not come back until it is done, so a runaway
query is not a slow request — it is a connection permanently out of service.
Three things can stop one: the client by name, a deadline, and the reaper
taking back a lease that has outlived its TTL. What is worth testing is less
"does it stop" than everything around it: that the connection is usable
afterwards, that a cancel aimed at one statement cannot land on another, and
that the accounting still balances when clients cancel things at random.
"""

import argparse
import json
import os
import random
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request


# Every berth a test starts registers under $HARBOR_HOME. Run through the
# suite, check.sh sets it; run directly — which the usage line above invites —
# nothing did, so sockets, tokens and lock files landed in the operator's real
# runtime directory and each run left a dead name behind. `setdefault` keeps
# the harness in charge when there is one.
#
# Short, and under /tmp deliberately: a macOS unix socket path must fit in
# SUN_LEN (104 bytes), and the per-user $TMPDIR alone is most of that.
def _isolate_fleet():
    import tempfile
    if not os.environ.get("HARBOR_HOME"):
        os.environ["HARBOR_HOME"] = tempfile.mkdtemp(prefix="hb-", dir="/tmp")


_isolate_fleet()


HERE = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
TOKEN = "cancel-suite-token"

# Long enough to cancel by hand, short enough that a test which fails to cancel
# finishes rather than hanging the suite. Measured at roughly five seconds on an
# M-series laptop; the assertions below check elapsed time against a fraction of
# it rather than against an absolute, so a slower machine does not fail them.
LONG = "SELECT count(DISTINCT i) FROM range(300000000) t(i)"

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


def yes(name, cond, detail=""):
    if cond:
        ok(name, detail)
    else:
        bad(name, detail or "expected true")


def until(fn, want, tries=50, delay=0.1):
    """Poll fn() until it equals want, up to tries*delay seconds. A connection
    cancelled by its deadline comes back on the reaper's next tick, so recovery
    is quick but not instantaneous — a slow runner needs a beat before the next
    query lands on a reset worker. Returns the last value either way, so a
    genuine failure to recover still fails the assertion."""
    last = fn()
    for _ in range(tries):
        if last == want:
            return last
        time.sleep(delay)
        last = fn()
    return last


def section(title):
    print(f"\n\033[1m{title}\033[0m")


class Harbor:
    def __init__(self, base, token=TOKEN):
        self.base = base
        self.token = token

    def call(self, method, path, body=None, timeout=120):
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

    def sql(self, statement, session=None, query=None, timeout_ms=None, timeout=120):
        body = {"sql": statement}
        if session:
            body["sessionId"] = session
        if query:
            body["queryId"] = query
        if timeout_ms is not None:
            body["timeoutMs"] = timeout_ms
        return self.call("POST", "/sql", body, timeout=timeout)

    def value(self, statement, session=None):
        st, doc, _ = self.sql(statement, session)
        if st != 200 or not doc.get("data"):
            return None
        return doc["data"][0][0]

    def open(self, ttl_ms=None):
        body = {} if ttl_ms is None else {"ttlMs": ttl_ms}
        return self.call("POST", "/sql/sessions", body)

    def release(self, sid):
        return self.call("DELETE", "/sql/sessions/" + sid)

    def cancel(self, qid):
        return self.call("DELETE", "/sql/queries/" + qid)

    def connections(self):
        return self.call("GET", "/sql/sessions")[1]["connections"]


class Background:
    """A statement running on its own thread, so the test can act while it runs."""

    def __init__(self, harbor, **kwargs):
        self.result = None
        self.seconds = None
        started = time.monotonic()

        def run():
            try:
                self.result = harbor.sql(**kwargs)
            except Exception as e:  # a socket error is a result too
                self.result = (0, {"message": str(e)}, {})
            self.seconds = time.monotonic() - started

        self.thread = threading.Thread(target=run, daemon=True)
        self.thread.start()

    def wait(self, timeout=120):
        self.thread.join(timeout)
        return self.result

    @property
    def status(self):
        return self.result[0] if self.result else None

    @property
    def code(self):
        return (self.result[1] or {}).get("code") if self.result else None


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def start_server(db, pool_size, workers, port, env_extra=None):
    env = dict(os.environ, HARBOR_POOL_SIZE=str(pool_size))
    env.update(env_extra or {})
    log = open(db + ".log", "w")
    proc = subprocess.Popen(
        [
            *os.environ.get("HARBOR_LAUNCHER", os.path.join(HERE, "target", "release", "harbor")).split(), db, "serve",
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
    raise SystemExit("cancel: the server never came up")


def stop_server(proc):
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    work = tempfile.mkdtemp(prefix="harbor-cancel-")
    db = os.path.join(work, "cancel.duckdb")
    if args.db and os.path.exists(args.db):
        shutil.copy(args.db, db)

    # Eight connections: four workers, four leases. Small enough that the
    # saturation test can block every worker without running eight slow
    # queries.
    proc, h, log = start_server(db, pool_size=8, workers=4, port=free_port())
    try:
        h.sql("CREATE TABLE IF NOT EXISTS marks(n INTEGER)")
        run_tests(h, db)
    finally:
        stop_server(proc)
        log.close()
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)

    print()
    if failed:
        print(f"\033[31m{passed} passed, {len(failed)} failed\033[0m")
        for name in failed:
            print(f"  - {name}")
        return 1
    print(f"\033[32m{passed} passed, 0 failed\033[0m")
    return 0


def run_tests(h, db):
    # -----------------------------------------------------------------------
    section("Cancelling a query by the name the client gave it")

    # How long the query takes when nobody stops it. Every timing assertion
    # below is relative to this, so the suite does not encode one laptop's
    # speed.
    started = time.monotonic()
    st, _, _ = h.sql(LONG)
    baseline = time.monotonic() - started
    eq("the long query succeeds when left alone", 200, st)
    yes("it is long enough to be worth cancelling", baseline > 1.0, f"{baseline:.1f}s")

    job = Background(h, statement=LONG, query="q1")
    time.sleep(0.5)
    st, doc, _ = h.cancel("q1")
    eq("the cancel is accepted", 200, st)
    eq("and says it cancelled something", True, doc.get("cancelled"))
    job.wait()
    eq("the query fails with 499", 499, job.status)
    eq("and a code that says why", "cancelled", job.code)
    yes(
        "it stopped early rather than running to completion",
        job.seconds < baseline / 2,
        f"{job.seconds:.1f}s of a {baseline:.1f}s query",
    )

    # The failure this whole design exists to prevent. Without a statement id,
    # "cancel that connection" would fire on whatever is running by the time
    # the interrupt lands.
    st, doc, _ = h.cancel("q1")
    eq("cancelling it again cancels nothing", False, doc.get("cancelled"))
    eq("and is not an error", 200, st)
    eq("cancelling a name nobody used is the same", False, h.cancel("never")[1].get("cancelled"))

    eq("the server still answers", 1, h.value("SELECT 1"))
    eq("and can still write", 200, h.sql("INSERT INTO marks VALUES (1)")[0])

    # -----------------------------------------------------------------------
    section("The connection comes back clean")

    # A worker's connection is reused by the next request to land on it. If a
    # cancelled statement left the interrupt flag set or a transaction open,
    # the damage shows up here rather than in the cancelled request.
    for i in range(12):
        st, doc, _ = h.sql(f"SELECT {i} AS n")
        if st != 200 or doc["data"][0][0] != i:
            bad("every worker answers correctly after a cancellation", f"query {i}: {st} {doc}")
            break
    else:
        ok("every worker answers correctly after a cancellation", "12 queries")

    two = h.value("SELECT count(*) AS n FROM marks")
    yes("writes made after the cancellation are all there", two >= 1, f"{two} rows")

    # -----------------------------------------------------------------------
    section("A deadline the client asked for")

    started = time.monotonic()
    st, doc, _ = h.sql(LONG, timeout_ms=800)
    elapsed = time.monotonic() - started
    eq("a statement past its deadline is cancelled", 499, st)
    eq("with the same code as any other cancellation", "cancelled", doc.get("code"))
    yes(
        "close to the deadline, within the reaper's tick",
        elapsed < 800 / 1000 + 1.5,
        f"{elapsed:.2f}s for an 0.8s deadline",
    )

    st, _, _ = h.sql("SELECT 1", timeout_ms=30_000)
    eq("a statement inside its deadline is untouched", 200, st)
    st, _, _ = h.sql("SELECT 1", timeout_ms=0)
    eq("zero means no limit rather than instant death", 200, st)
    eq("a nonsense timeout is a clean 400", 400, h.sql("SELECT 1", timeout_ms=-1)[0])

    # -----------------------------------------------------------------------
    section("A deadline works when nothing else can")

    # The case cancellation exists for and the case an HTTP cancel cannot
    # reach: every worker is blocked inside a query, so there is no thread left
    # to accept a DELETE. The reaper has its own thread and never touches HTTP,
    # which is what makes the deadline the backstop rather than a convenience.
    jobs = [Background(h, statement=LONG, timeout_ms=1500) for _ in range(4)]
    time.sleep(0.5)
    started = time.monotonic()
    for job in jobs:
        job.wait()
    elapsed = time.monotonic() - started
    eq("all four saturating queries were stopped", [499] * 4, [j.status for j in jobs])
    yes("by the deadline, not by finishing", elapsed < baseline, f"{elapsed:.1f}s")
    eq("and the server is serving again", 1, until(lambda: h.value("SELECT 1"), 1))

    # -----------------------------------------------------------------------
    section("Cancelling a statement inside a transaction")

    st, doc, _ = h.open()
    sid = doc["sessionId"]
    eq("BEGIN", 200, h.sql("BEGIN", session=sid)[0])
    eq("a write inside the transaction", 200, h.sql("INSERT INTO marks VALUES (99)", session=sid)[0])

    job = Background(h, statement=LONG, session=sid, query="tx1")
    time.sleep(0.5)
    eq("the transaction's statement can be cancelled", True, h.cancel("tx1")[1].get("cancelled"))
    job.wait()
    eq("it reports cancelled", 499, job.status)

    # DuckDB leaves the transaction in an aborted state, exactly as Postgres
    # does, and harbor deliberately does not paper over it: silently rolling
    # back would let the next statement commit in autocommit under a client
    # that still believed it was in a transaction.
    st, doc, _ = h.sql("SELECT 1", session=sid)
    eq("the transaction is aborted, not silently continued", 400, st)
    yes(
        "and says so",
        "abort" in (doc.get("message") or "").lower(),
        doc.get("message", "")[:80],
    )
    eq("ROLLBACK is accepted", 200, h.sql("ROLLBACK", session=sid)[0])
    eq("and the session works again", 1, h.value("SELECT 1", session=sid))
    eq("the cancelled write did not land", None, h.value("SELECT n FROM marks WHERE n = 99"))
    eq("release", True, h.release(sid)[1].get("released"))

    # -----------------------------------------------------------------------
    section("Releasing a session that is running something")

    before = h.connections()
    st, doc, _ = h.open()
    sid = doc["sessionId"]
    job = Background(h, statement=LONG, session=sid)
    time.sleep(0.5)
    st, doc, _ = h.release(sid)
    eq("the release is accepted", 200, st)
    eq("but the connection is not back yet", False, doc.get("released"))
    eq("because the statement is being stopped", True, doc.get("cancelling"))
    job.wait()
    eq("the statement reports cancelled", 499, job.status)

    for _ in range(40):
        now = h.connections()
        if now["free"] == before["free"] and now["live"] == 0:
            break
        time.sleep(0.25)
    eq("the reaper returns the connection", before["free"], h.connections()["free"])
    eq("and the books balance", True, h.connections()["balanced"])
    eq("releasing it again is a no-op", False, h.release(sid)[1].get("released"))

    # -----------------------------------------------------------------------
    section("A lease that runs past its deadline is taken back")

    # Before cancellation the reaper skipped busy leases, so the one lease that
    # most needed reclaiming — wedged inside a runaway statement — was the one
    # it could never reclaim.
    before = h.connections()
    st, doc, _ = h.open(ttl_ms=1000)
    sid = doc["sessionId"]
    job = Background(h, statement=LONG, session=sid)
    job.wait()
    eq("the statement is cancelled when the lease expires", 499, job.status)
    for _ in range(40):
        if h.connections()["free"] == before["free"]:
            break
        time.sleep(0.25)
    eq("and the connection comes back", before["free"], h.connections()["free"])
    eq("the books balance", True, h.connections()["balanced"])

    # -----------------------------------------------------------------------
    section("Two names cannot mean one statement")

    job = Background(h, statement=LONG, query="dup")
    time.sleep(0.4)
    st, doc, _ = h.sql("SELECT 1", query="dup")
    eq("a queryId already in flight is refused", 409, st)
    eq("with a code that says which", "query_id_in_use", doc.get("code"))
    h.cancel("dup")
    job.wait()
    # And the name is free again the moment the statement ends, so a console
    # that reuses one id per tab is not slowly poisoned.
    eq("the name is reusable once the statement is over", 200, h.sql("SELECT 1", query="dup")[0])
    eq("an over-long queryId is a clean 400", 400, h.sql("SELECT 1", query="x" * 129)[0])

    # -----------------------------------------------------------------------
    section("Chaos: cancel everything, at random, and check the books")

    # The invariant that matters is not that any one cancel worked. It is that
    # after a few hundred of them, no connection has gone out and failed to
    # come back.
    total = h.connections()["total"]
    stop_at = time.monotonic() + 10
    drift = []
    counts = {"cancelled": 0, "finished": 0, "sessions": 0}
    lock = threading.Lock()

    def watcher():
        while time.monotonic() < stop_at:
            c = h.connections()
            if c["free"] + c["live"] + c["inflight"] != c["total"] or c["total"] != total:
                with lock:
                    drift.append(c)
            time.sleep(0.05)

    def chaos(seed):
        rng = random.Random(seed)
        while time.monotonic() < stop_at:
            mode = rng.random()
            if mode < 0.45:
                qid = f"c{seed}-{rng.randrange(1_000_000)}"
                job = Background(h, statement=LONG, query=qid, timeout_ms=3000)
                time.sleep(rng.uniform(0.05, 0.5))
                h.cancel(qid)
                job.wait()
                with lock:
                    counts["cancelled" if job.status == 499 else "finished"] += 1
            elif mode < 0.7:
                # Cancel names that mean nothing, including ones that just
                # finished. Nothing may be disturbed by this.
                h.cancel(f"ghost-{rng.randrange(100)}")
            elif mode < 0.9:
                st, doc, _ = h.open(ttl_ms=rng.choice([500, 1500, 5000]))
                if st != 200:
                    continue
                sid = doc["sessionId"]
                with lock:
                    counts["sessions"] += 1
                h.sql("BEGIN", session=sid)
                job = Background(h, statement=LONG, session=sid)
                time.sleep(rng.uniform(0.05, 0.4))
                if rng.random() < 0.5:
                    h.release(sid)          # release while busy
                job.wait()
                if rng.random() < 0.7:
                    h.release(sid)          # and sometimes again
            else:
                h.sql("SELECT count(*) FROM marks")

    threads = [threading.Thread(target=watcher, daemon=True)]
    threads += [threading.Thread(target=chaos, args=(i,), daemon=True) for i in range(6)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(60)

    eq("connection accounting never drifted", [], drift)
    yes(
        "and cancels were actually landing",
        counts["cancelled"] > 0,
        f"{counts['cancelled']} cancelled, {counts['finished']} finished, "
        f"{counts['sessions']} sessions",
    )

    # Everything must come home. The reaper needs a few ticks to collect the
    # leases the chaos abandoned mid-statement.
    for _ in range(80):
        c = h.connections()
        if c["free"] == c["total"] and c["live"] == 0:
            break
        time.sleep(0.25)
    c = h.connections()
    eq("every connection came back", (c["total"], 0, 0), (c["free"], c["live"], c["inflight"]))
    eq("the server still answers", 1, h.value("SELECT 1"))
    eq("and can still write", 200, h.sql("INSERT INTO marks VALUES (7)")[0])


if __name__ == "__main__":
    sys.exit(main())
