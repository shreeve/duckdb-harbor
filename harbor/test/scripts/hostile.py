#!/usr/bin/env python3
"""hostile.py — the HTTP surface under adversarial input.

Runs its own server, because every case here is designed to hurt it: the point
is to wedge, flood, desync and leak on purpose, and a shared berth could not
survive that or report cleanly afterwards.

  test/scripts/hostile.py [--cases N] [--seed N] [--db PATH] [--keep]

WHAT THIS IS FOR, AND WHY IT IS NOT A PARSER FUZZER

Every network-facing bug found in this server so far parsed *fine*. The header
flood, the unbounded body drain, the dripping body that took every worker, the
connection held open forever by one anonymous /ready, the 51-minute eager read:
in all of them the request was well-formed and the parser was right. What was
wrong was everything around it — what got allocated, what got held, and for how
long. A `cargo-fuzz` target asserting "no panic" would have caught none of them.

So the oracle here is not "did it parse". It is what the process DID: is it
still answering, did the threads and descriptors come back, did memory stay
put, and did one request produce exactly one response. Those are side effects,
not opinions — the same discipline as the canary table in fuzz.py, where a
dropped table is proof a second statement ran and no amount of reasoning about
the lexer is.

WHAT IT DOES NOT COVER

Request smuggling proper. Smuggling is a *disagreement* between two parsers —
harbor and whatever proxy sits in front of it — and disagreement cannot be
detected by asking one of them. `one_response_per_request` below catches harbor
desyncing against itself, which is the half that is observable from here. The
other half needs a differential harness against a reference parser.
"""

import argparse
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
TOKEN = "hostile-suite-token"
WORKERS = 6

passed = 0
failed = []


def ok(name, detail=""):
    global passed
    passed += 1
    print(f"  \033[32m✓\033[0m {name}" + (f" \033[2m{detail}\033[0m" if detail else ""))


def bad(name, detail):
    failed.append(name)
    print(f"  \033[31m✗\033[0m {name}\n     \033[2m{detail}\033[0m")


def section(title):
    print(f"\n\033[1m{title}\033[0m")


# ---------------------------------------------------------------------------
# The berth under test
# ---------------------------------------------------------------------------


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def start_server(db, port):
    log = open(db + ".log", "w")
    proc = subprocess.Popen(
        [
            *os.environ.get(
                "HARBOR_LAUNCHER",
                os.path.join(HERE, "target", "release", "harbor"),
            ).split(),
            db, "serve", "--create",
            "--port", str(port), "--token", TOKEN, "--workers", str(WORKERS),
        ],
        stdout=log, stderr=log, stdin=subprocess.DEVNULL,
    )
    for _ in range(120):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/ready", timeout=1).read()
            return proc, log
        except Exception:
            if proc.poll() is not None:
                break
            time.sleep(0.25)
    log.flush()
    print(open(db + ".log").read(), file=sys.stderr)
    raise SystemExit("hostile: the server never came up")


# ---------------------------------------------------------------------------
# The oracles — all four read the process from outside, never its opinion
# ---------------------------------------------------------------------------


def rss_kb(pid):
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)],
                         capture_output=True, text=True).stdout.strip()
    return int(out) if out else 0


def threads(pid):
    out = subprocess.run(["ps", "-M", str(pid)], capture_output=True, text=True).stdout
    return max(0, len(out.strip().splitlines()) - 1)


def fds(pid):
    out = subprocess.run(["lsof", "-p", str(pid)], capture_output=True, text=True).stdout
    return max(0, len(out.strip().splitlines()) - 1)


def responsive(port, timeout=8):
    """Oracle 1 — liveness. The berth must still answer while under attack.

    This is the one that would have caught every denial of service found so
    far: each of them left /ready timing out while the process looked healthy
    to anything watching the pid.
    """
    began = time.time()
    try:
        r = urllib.request.urlopen(f"http://127.0.0.1:{port}/ready", timeout=timeout)
        return r.status == 200, time.time() - began
    except Exception:
        return False, time.time() - began


def settled(pid, base_threads, base_fds, slack=8, wait=45):
    """Oracle 2 — no leak. Threads and descriptors must come back.

    Held connections are the quiet failure: nothing errors, nothing logs, the
    berth answers normally, and the process accumulates a thread and two
    descriptors per abandoned socket until it hits a limit. Polls rather than
    sampling once, because reclamation is on a timeout, not immediate.
    """
    deadline = time.time() + wait
    t = f = None
    while time.time() < deadline:
        t, f = threads(pid), fds(pid)
        if t <= base_threads + slack and f <= base_fds + slack:
            return True, t, f
        time.sleep(1)
    return False, t, f


# ---------------------------------------------------------------------------
# Hostile request generation
#
# Grammar-aware rather than random bytes: the interesting inputs here are the
# ones that are *almost* well-formed, because those are what reach the code
# past the first reject. Random noise mostly exercises the 400 path.
# ---------------------------------------------------------------------------

# PRI and `*` are here for one shape in particular: `PRI * HTTP/2.0`, the
# prior-knowledge HTTP/2 connection preface. A version this server cannot
# speak used to strand the connection thread permanently, and the realistic
# way to send one is not a hand-typed `GET / HTTP/2.0` — it is a client
# quietly attempting h2c. Generating the version alone found that bug; the
# preface is how it would actually have reached a berth.
METHODS = ["GET", "POST", "DELETE", "PUT", "HEAD", "OPTIONS", "PATCH", "TRACE",
           "PRI", "\x00BAD"]
PATHS = ["/ready", "/sql", "/info", "/sessions", "/catalog", "/keepalive",
         "/sql/sessions/new", "/sql/sessions/x", "/sql/queries/x", "/", "/nope",
         "/sql/../sql", "/sql%00", "*", "/" + "a" * 900]
VERSIONS = ["HTTP/1.1", "HTTP/1.0", "HTTP/0.9", "HTTP/2.0", "HTTP/3.0", "HTTP/9.9",
            "HTTP/1.1x", ""]

# Framing headers are their own list: ambiguous framing is the one class where
# being wrong is a smuggling primitive rather than a bad request.
FRAMING = [
    "Content-Length: 0",
    "Content-Length: 5",
    "Content-Length: -1",
    "Content-Length: 99999999999999999999",
    "Content-Length: 0x10",
    "Content-Length: 5\r\nContent-Length: 6",       # conflicting duplicates
    "Content-Length: 5\r\nContent-Length: 5",       # agreeing duplicates
    "Content-Length: 5\r\nTransfer-Encoding: chunked",
    "Transfer-Encoding: chunked",
    "Transfer-Encoding: chunked, identity",
    "Transfer-Encoding: cHuNkEd",
    "Transfer-Encoding: bogus",
    "TE: identity",
    "TE: chunked;q=0.5, identity;q=1.0",
]

ODD = [
    "Connection: close", "Connection: keep-alive", "Connection: upgrade",
    "Expect: 100-continue", "Expect: something-else",
    "Authorization: Bearer " + TOKEN, "Authorization: Bearer wrong",
    "Authorization: bearer " + TOKEN, "Authorization: Basic x",
    "Authorization: Bearer " + TOKEN + "\r\nAuthorization: Bearer " + TOKEN,
    "Accept: application/json", "Accept: application/x-ndjson", "Accept: */*",
    "Host: x", "Host:", "X-Long: " + "v" * 4000, "X-Empty:",
    ": novalue", "NoColon", "X-Bad\x00Name: v", "X-Space : v",
]


# The exact bytes a prior-knowledge HTTP/2 client opens with. Kept verbatim
# rather than assembled from the lists above, because the trailing `SM\r\n\r\n`
# is part of the preface and is what a naive server reads as a second request.
H2_PREFACE = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"


def hostile_head(rng):
    method = rng.choice(METHODS)
    path = rng.choice(PATHS)
    version = rng.choice(VERSIONS)
    lines = [f"{method} {path} {version}".strip()]
    if rng.random() < 0.85:
        lines.append(rng.choice(FRAMING))
    for _ in range(rng.randint(0, 6)):
        lines.append(rng.choice(ODD))
    if rng.random() < 0.05:                      # header flood
        lines += [f"X-{i}: v" for i in range(rng.randint(120, 400))]
    eol = rng.choice(["\r\n"] * 12 + ["\n", "\r"])
    return (eol.join(lines) + eol + eol).encode("utf-8", "surrogateescape")


def one_response_per_request(port, rng):
    """Oracle 4 — no desync.

    One request must draw exactly one response. Two means the connection was
    left at an offset the two sides disagreed about and harbor answered
    something the client never sent, which is smuggling with the proxy played
    by the bug itself. The bait is a second request line dribbled into a body
    the server was told to expect.
    """
    s = socket.create_connection(("127.0.0.1", port), timeout=15)
    try:
        s.sendall(b"POST /sql HTTP/1.1\r\nHost: x\r\nContent-Length: 4000000\r\n\r\n")
        for _ in range(rng.randint(3, 10)):
            try:
                # DELETE is the legacy shutdown verb — using it here also
                # proves the alias stays served beside canonical POST.
                s.sendall(b"DELETE /shutdown HTTP/1.1\r\nHost: x\r\n\r\n")
            except OSError:
                break
            time.sleep(0.2)
        s.settimeout(6)
        seen = b""
        while True:
            try:
                chunk = s.recv(8192)
            except (socket.timeout, OSError):
                break
            if not chunk:
                break
            seen += chunk
        return seen.count(b"HTTP/1.1")
    finally:
        s.close()


# ---------------------------------------------------------------------------
# Cases
# ---------------------------------------------------------------------------


def case_random_heads(port, pid, rng, n, base_rss):
    """Malformed and almost-malformed heads, with liveness and memory watched."""
    peak = base_rss
    for _ in range(n):
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=5)
            s.sendall(H2_PREFACE if rng.random() < 0.04 else hostile_head(rng))
            s.settimeout(3)
            try:
                s.recv(4096)
            except (socket.timeout, OSError):
                pass
            s.close()
        except OSError:
            pass
        peak = max(peak, rss_kb(pid))
    alive, _ = responsive(port)
    return alive, peak


def case_slowloris_head(port, rng, n):
    """A head delivered a byte at a time: the shape that grew RSS unbounded."""
    socks = []
    for _ in range(n):
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=5)
            s.sendall(b"GET /ready HTTP/1.1\r\nX-Junk: ")
            socks.append(s)
        except OSError:
            pass
    for _ in range(6):
        for s in socks:
            try:
                s.sendall(b"v" * 64)
            except OSError:
                pass
        time.sleep(0.4)
    alive = responsive(port)
    for s in socks:
        s.close()
    return alive


def case_dripping_body(port, n, authed):
    """The wedge: a declared body delivered slower than anyone will wait."""
    stop = [False]
    socks = []

    def drip():
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=60)
            socks.append(s)
            auth = (b"Authorization: Bearer " + TOKEN.encode() + b"\r\n") if authed else b""
            s.sendall(b"POST /sql HTTP/1.1\r\nHost: x\r\n" + auth +
                      b"Content-Length: 8000000\r\n\r\n")
            while not stop[0]:
                s.sendall(b'{"sql":')
                time.sleep(2)
        except OSError:
            pass

    ts = [threading.Thread(target=drip, daemon=True) for _ in range(n)]
    for t in ts:
        t.start()
    time.sleep(9)
    verdict = responsive(port)
    stop[0] = True
    time.sleep(1)
    for s in socks:
        try:
            s.close()
        except OSError:
            pass
    return verdict


def case_stalled_heads(port, n):
    """A head begun and then abandoned. Reclaimed on the head timeout (~10s),
    which is what makes this the leak check the lane can afford to run every
    time: the same accumulation as a held keep-alive connection, on a clock
    short enough to assert against."""
    socks = []
    for _ in range(n):
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=10)
            s.sendall(b"GET /ready HTTP/1.1\r\nX-Partial: begun")
            socks.append(s)
        except OSError:
            pass
    return socks


def case_held_connections(port, n):
    """One cheap unauthenticated request, then silence forever.

    The quiet accumulation: nothing errors, the berth answers normally, and a
    thread plus two descriptors pile up per socket. Reclaimed only on the
    keep-alive idle cap, which is minutes by design — so this one is behind
    --slow rather than run on every `make test`."""
    socks = []
    for _ in range(n):
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=10)
            s.sendall(b"GET /ready HTTP/1.1\r\nHost: x\r\n\r\n")
            s.recv(4096)
            socks.append(s)
        except OSError:
            pass
    return socks


# ---------------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", type=int, default=400)
    ap.add_argument("--seed", type=int, default=None)
    ap.add_argument("--db")
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--slow", action="store_true",
                    help="also assert the keep-alive idle cap (minutes)")
    args = ap.parse_args()

    seed = args.seed if args.seed is not None else random.randrange(1 << 30)
    rng = random.Random(seed)
    print(f"hostile: seed {seed} (rerun with --seed {seed})")

    tmp = None
    db = args.db
    if not db:
        tmp = tempfile.mkdtemp(prefix="harbor-hostile-")
        db = os.path.join(tmp, "hostile.duckdb")
    port = free_port()
    proc, log = start_server(db, port)
    pid = proc.pid

    try:
        time.sleep(1)
        base_rss, base_threads, base_fds = rss_kb(pid), threads(pid), fds(pid)
        print(f"  \033[2mbaseline: rss={base_rss}KB threads={base_threads} fds={base_fds}\033[0m")

        section("Malformed heads")
        alive, peak = case_random_heads(port, pid, rng, args.cases, base_rss)
        if alive:
            ok(f"{args.cases} hostile heads, berth still answering")
        else:
            bad("hostile heads", "the berth stopped answering /ready")
        # Generous: the ceiling is against unbounded growth, not against churn.
        if peak < base_rss + 200_000:
            ok("memory stayed bounded", f"peak {peak}KB vs {base_rss}KB baseline")
        else:
            bad("memory", f"peak {peak}KB against a {base_rss}KB baseline")

        section("Slowloris head")
        alive, secs = case_slowloris_head(port, rng, 40)
        if alive:
            ok("40 byte-at-a-time heads, berth still answering", f"{secs:.2f}s")
        else:
            bad("slowloris head", f"/ready failed after {secs:.2f}s")

        section("Dripping body — the wedge")
        for authed in (False, True):
            label = "authenticated" if authed else "anonymous"
            alive, secs = case_dripping_body(port, WORKERS + 2, authed)
            if alive:
                ok(f"{WORKERS + 2} {label} dripping bodies, berth still answering",
                   f"{secs:.2f}s")
            else:
                bad(f"dripping body ({label})",
                    f"/ready failed after {secs:.2f}s — every worker is held")

        section("No desync")
        worst = 0
        for _ in range(6):
            worst = max(worst, one_response_per_request(port, rng))
        if worst <= 1:
            ok("one request drew one response", f"max {worst} seen")
        else:
            bad("desync", f"one request drew {worst} responses — leftover bytes were parsed")

        section("No leak")
        # Every case above abandoned connections on purpose, and each has its
        # own reclamation clock. Wait for those to run out before measuring
        # what THIS case costs — and assert the wait rather than just sleeping
        # it, because a berth that cannot get back to baseline after the cases
        # above is itself the finding.
        clean, t, f = settled(pid, base_threads, base_fds, wait=90)
        if clean:
            ok("earlier cases left nothing behind",
               f"threads {t} vs {base_threads}, fds {f} vs {base_fds}")
        else:
            bad("leak (residual)",
                f"after the cases above: threads {t} (base {base_threads}), "
                f"fds {f} (base {base_fds})")
        ref_t, ref_f = threads(pid), fds(pid)

        socks = case_stalled_heads(port, 80)
        held_t, held_f = threads(pid), fds(pid)
        print(f"  \033[2m80 stalled heads: threads={held_t} fds={held_f}\033[0m")
        clean, t, f = settled(pid, ref_t, ref_f, wait=45)
        for s in socks:
            try:
                s.close()
            except OSError:
                pass
        if clean:
            ok("abandoned heads reclaimed to baseline",
               f"threads {t} vs {ref_t}, fds {f} vs {ref_f}")
        else:
            bad("leak", f"threads {t} (base {base_threads}), fds {f} (base {base_fds}) "
                        f"still held well past the head timeout")

        if args.slow:
            # The keep-alive idle cap is minutes by design (a pooled client
            # between queries is doing nothing wrong), so asserting it costs
            # more wall clock than a per-commit lane should spend.
            socks = case_held_connections(port, 80)
            held_t, held_f = threads(pid), fds(pid)
            print(f"  \033[2m80 idle keep-alives: threads={held_t} fds={held_f}\033[0m")
            clean, t, f = settled(pid, ref_t, ref_f, wait=420)
            for s in socks:
                try:
                    s.close()
                except OSError:
                    pass
            if clean:
                ok("idle keep-alive connections reclaimed to baseline",
                   f"threads {t} vs {base_threads}, fds {f} vs {base_fds}")
            else:
                bad("leak (keep-alive)",
                    f"threads {t} (base {base_threads}), fds {f} (base {base_fds}) "
                    f"still held past the keep-alive idle cap")
        else:
            print("  \033[2m(keep-alive idle reclamation: --slow)\033[0m")

        section("Still alive")
        if proc.poll() is None:
            ok("the process survived every case")
        else:
            bad("process", f"harbor exited with {proc.returncode}")
        alive, _ = responsive(port)
        if alive:
            ok("and still serves /ready")
        else:
            bad("liveness", "the berth no longer answers")

    finally:
        try:
            proc.terminate()
            proc.wait(timeout=20)
        except Exception:
            proc.kill()
        log.flush()
        panics = [ln for ln in open(db + ".log", errors="replace").read().splitlines()
                  if "panicked at" in ln]
        if panics:
            bad("panic", panics[0])
        if tmp and not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)
        elif args.keep:
            print(f"  \033[2mkept: {db}\033[0m")

    print()
    if failed:
        print(f"\033[31m{passed} passed, {len(failed)} failed\033[0m: {', '.join(failed)}")
        print(f"\033[31mreproduce with --seed {seed}\033[0m")
        sys.exit(1)
    print(f"\033[32m{passed} passed, 0 failed\033[0m")


if __name__ == "__main__":
    main()
