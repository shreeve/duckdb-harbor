#!/usr/bin/env python3
"""
fuzz.py — throw a lot of random values at the encoder and check every one
against an oracle that is not harbor.

    test/scripts/fuzz.py --port 9499 --token T --cases 20000
    test/scripts/fuzz.py --port 9499 --token T --seed 7    # reproduce a failure

The hand-written suites cover the boundaries someone thought of. This covers
the ones nobody did. It is aimed squarely at the conversions harbor
implements itself — civil_from_days, the per-unit timestamp formatting, base64,
the JSON-safe integer rule — because a mistake there is harbor's, not DuckDB's,
and an off-by-one shows up in one date out of a thousand rather than in the
ones a person would pick.

The oracle is Python's own `datetime` and `base64`, computed from the same
inputs before the query runs. Nothing here compares harbor to harbor.

Values are batched into one statement per few hundred cases: the point is
coverage of the encoder, not of the HTTP path, and thousands of round trips
would make the suite too slow to run often.

Every failure prints the seed, so a red run is reproducible.
"""

import argparse
import base64
import datetime
import http.client
import json
import os
import random
import sys
import tempfile


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


BATCH = 250

# Two to the 53rd minus one: the largest integer a double holds exactly, and so
# the point past which harbor quotes rather than emits a bare number.
JSON_SAFE = 9007199254740991


def request(host, port, token, sql):
    conn = http.client.HTTPConnection(host, port, timeout=120)
    try:
        conn.request("POST", "/sql", json.dumps({"sql": sql}),
                     {"Authorization": "Bearer " + token, "Content-Type": "application/json"})
        resp = conn.getresponse()
        payload = resp.read().decode("utf-8", "replace")
        rows, err = [], None
        # NDJSON is delimited by \n alone. splitlines() also breaks on U+2028,
        # U+2029 and other Unicode line separators, which are legal inside a
        # JSON string — using it here would make the harness fail on payloads
        # a real client reads fine.
        for line in payload.split("\n"):
            if not line.strip():
                continue
            try:
                parsed = json.loads(line)
            except json.JSONDecodeError as e:
                # A line that is not JSON is a bug in the envelope, not in the
                # test. Keep the whole response so the offending bytes can be
                # looked at, rather than dying with a traceback that says
                # nothing about which value caused it.
                # A unique path per failure: a fixed one is overwritten by the
                # next bad line and clobbered by a concurrent run, so the file
                # named in the message is often not the one that caused it.
                fd, dump = tempfile.mkstemp(prefix="harbor-fuzz-bad-envelope.", suffix=".txt")
                with os.fdopen(fd, "w") as fh:
                    fh.write(payload)
                return resp.status, rows, {
                    "message": "malformed envelope line (%s) at offset %d; full response in %s; line: %r"
                               % (e, payload.find(line), dump, line[:200])}
            if parsed.get("type") == "row":
                rows.append(parsed["values"])
            elif parsed.get("type") == "error":
                err = parsed
        return resp.status, rows, err
    finally:
        conn.close()


def run_batch(args, literals, expected, failures, label):
    """Send one SELECT per batch and compare each row to its oracle value."""
    for start in range(0, len(literals), BATCH):
        chunk = literals[start:start + BATCH]
        want = expected[start:start + BATCH]
        sql = "SELECT * FROM (VALUES " + ", ".join("(%s)" % lit for lit in chunk) + ") t(v)"
        status, rows, err = request(args.host, args.port, args.token, sql)
        if status != 200 or len(rows) != len(chunk):
            failures.append((label, "batch failed: status=%s rows=%d want=%d err=%s"
                             % (status, len(rows), len(chunk), (err or {}).get("message", "")[:400])))
            return
        for literal, w, row in zip(chunk, want, rows):
            if row[0] != w:
                failures.append((label, "%s -> got %r, oracle says %r" % (literal, row[0], w)))
                if len(failures) > 40:
                    return


def fuzz_dates(args, rng, n, failures):
    """Random dates across the whole DATE range, against Python's calendar."""
    literals, expected = [], []
    for _ in range(n):
        day = rng.randint(datetime.date.min.toordinal(), datetime.date.max.toordinal())
        d = datetime.date.fromordinal(day)
        literals.append("DATE '%04d-%02d-%02d'" % (d.year, d.month, d.day))
        expected.append(d.isoformat())
    run_batch(args, literals, expected, failures, "date")


def fuzz_timestamps(args, rng, n, failures):
    """Random microsecond timestamps, against Python's isoformat."""
    literals, expected = [], []
    for _ in range(n):
        day = rng.randint(datetime.date.min.toordinal(), datetime.date.max.toordinal())
        d = datetime.date.fromordinal(day)
        h, m, s = rng.randint(0, 23), rng.randint(0, 59), rng.randint(0, 59)
        # A mixture of whole seconds, milliseconds and microseconds, because
        # the fraction is formatted differently for each.
        micro = rng.choice([0, rng.randint(1, 999) * 1000, rng.randint(1, 999999)])
        ts = datetime.datetime(d.year, d.month, d.day, h, m, s, micro)
        literals.append("TIMESTAMP '%s'" % ts.strftime("%Y-%m-%d %H:%M:%S.%f"))
        # Python pads the fraction to six digits; harbor trims trailing zeros
        # because they claim precision the value does not carry.
        want = ts.isoformat()
        if "." in want:
            want = want.rstrip("0").rstrip(".")
        expected.append(want)
    run_batch(args, literals, expected, failures, "timestamp")


def fuzz_times(args, rng, n, failures):
    literals, expected = [], []
    for _ in range(n):
        h, m, s = rng.randint(0, 23), rng.randint(0, 59), rng.randint(0, 59)
        micro = rng.choice([0, rng.randint(1, 999) * 1000, rng.randint(1, 999999)])
        t = datetime.time(h, m, s, micro)
        literals.append("TIME '%s'" % t.strftime("%H:%M:%S.%f"))
        want = t.isoformat()
        if "." in want:
            want = want.rstrip("0").rstrip(".")
        expected.append(want)
    run_batch(args, literals, expected, failures, "time")


def fuzz_blobs(args, rng, n, failures):
    """Random byte strings of every length mod 3, against Python's base64."""
    literals, expected = [], []
    for _ in range(n):
        raw = bytes(rng.randrange(256) for _ in range(rng.randint(0, 40)))
        literals.append("'%s'::BLOB" % "".join("\\x%02X" % b for b in raw))
        expected.append(base64.b64encode(raw).decode())
    run_batch(args, literals, expected, failures, "blob")


def fuzz_integers(args, rng, n, failures):
    """Random 64-bit integers against the JSON-safe rule."""
    literals, expected = [], []
    for _ in range(n):
        # Deliberately dense around the boundary, where the rule actually bites.
        if rng.random() < 0.5:
            value = rng.randint(JSON_SAFE - 5, JSON_SAFE + 5) * rng.choice([1, -1])
        else:
            value = rng.randint(-(2**63), 2**63 - 1)
        literals.append("%d::BIGINT" % value)
        expected.append(value if abs(value) <= JSON_SAFE else str(value))
    run_batch(args, literals, expected, failures, "bigint")


def fuzz_strings(args, rng, n, failures):
    """Random text, including the code points that have to be escaped."""
    alphabet = ([chr(c) for c in range(32, 127)]
                + ['"', "\\", "\t", "\n", "\r", "\x01", "\x1f"]
                + ["é", "—", "世", "🦆", " ", " "])
    literals, expected = [], []
    for _ in range(n):
        text = "".join(rng.choice(alphabet) for _ in range(rng.randint(0, 30)))
        # Single quotes double inside a SQL literal; everything else is data.
        literals.append("'%s'" % text.replace("'", "''"))
        expected.append(text)
    run_batch(args, literals, expected, failures, "varchar")


def fuzz_decimals(args, rng, n, failures):
    """Random decimals, where the scale is part of the value.

    One width and scale per batch: a VALUES list has a single column type, so
    mixing scales would make DuckDB widen them all to the largest and the
    oracle would be comparing against a type the value was never stored as.
    """
    literals, expected = [], []
    scale = rng.randint(0, 10)
    width = rng.randint(max(scale, 1), 18)
    for i in range(n):
        if i % BATCH == 0:
            scale = rng.randint(0, 10)
            width = rng.randint(max(scale, 1), 18)
        digits = rng.randint(0, 10 ** width - 1)
        sign = rng.choice([1, -1])
        raw = str(digits).rjust(scale + 1, "0")
        text = (raw[:-scale] + "." + raw[-scale:]) if scale else raw
        text = text.lstrip("0") or "0"
        if text.startswith("."):
            text = "0" + text
        if sign < 0 and text != "0" and set(text) != {"0", "."}:
            text = "-" + text
        literals.append("%s::DECIMAL(%d,%d)" % (text, width, scale))
        expected.append(text)
    run_batch(args, literals, expected, failures, "decimal")


# ---------------------------------------------------------------------------
# The single-statement fence, against the only oracle that counts
# ---------------------------------------------------------------------------

# Fragments chosen for what they do to a lexer, not for what they mean: comment
# openers and closers, every quote form, escape strings, dollar-quote tags,
# identifier bytes that `$` can hide inside, and every line terminator either
# side might honor.
FENCE_FRAGMENTS = [
    "--", "-- c", "/*", "*/", "/* c", "c */", "/*/*", "*/*/", "'", "''", "'a",
    "e'", "E'", "\\", "\\'", '"', '""', '"a', "$", "$$", "$t$", "$1$", "$_$",
    "a", "1", "_", "a$b", "$b$", " ", "\t", "\n", "\r", "\r\n", "\x0b", "\x0c",
    ";", "x'41'", "b'1'", "u&'a'", "LIKE", "ESCAPE", "date", "time", "\u00e9", "\u3042",
    "SELECT", "1", "AS", "(", ")", ",",
]


def fuzz_statement_fence(args, rng, n, failures):
    """Does harbor's one-statement verdict match what the engine actually does?

    `ensure_single_statement` is the only thing standing between a client's SQL
    string and multi-statement execution — `duckdb-rs` runs everything but the
    last statement during `prepare`, so a smuggled `DROP` lands before a row is
    fetched. Two holes of exactly this shape have already shipped (a CR ending a
    `--` comment, and a `$` inside an identifier read as a dollar-quote), and
    both were found here rather than by reading.

    The oracle is a side effect, never an opinion: a canary table that only a
    second statement can drop. Harbor answering 200 with the canary gone is a
    bypass. Over-rejection is counted but is not a failure — refusing a
    statement someone could have written differently is the safe direction.
    """
    bypasses = 0
    for _ in range(n):
        body = "".join(rng.choice(FENCE_FRAGMENTS) for _ in range(rng.randint(1, 9)))
        probe = "SELECT 1 %s; DROP TABLE canary" % body
        request(args.host, args.port, args.token, "CREATE TABLE IF NOT EXISTS canary(x INT)")
        status, _, _ = request(args.host, args.port, args.token, probe)
        _, rows, _ = request(args.host, args.port, args.token,
                             "SELECT count(*) FROM duckdb_tables() WHERE table_name = 'canary'")
        alive = bool(rows) and rows[0][0] == 1
        if status == 200 and not alive:
            bypasses += 1
            failures.append(("fence", "accepted, but the engine ran a second statement: %r" % probe))
            if bypasses > 20:
                return


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=9499)
    ap.add_argument("--token", required=True)
    ap.add_argument("--cases", type=int, default=14000, help="total values, split across types")
    ap.add_argument("--fence-cases", type=int, default=400,
                    help="randomized single-statement fence probes (one request each)")
    ap.add_argument("--seed", type=int, default=None)
    args = ap.parse_args()

    seed = args.seed if args.seed is not None else random.randrange(2**31)
    rng = random.Random(seed)

    generators = [fuzz_dates, fuzz_timestamps, fuzz_times,
                  fuzz_blobs, fuzz_integers, fuzz_strings, fuzz_decimals]
    per_type = max(1, args.cases // len(generators))

    failures = []
    for gen in generators:
        gen(args, rng, per_type, failures)

    # The fence gets its own budget: it cannot batch (each probe is a separate
    # request plus a canary check), so it is counted in cases, not values.
    fuzz_statement_fence(args, rng, args.fence_cases, failures)

    total = per_type * len(generators)
    print("fuzz: %d values across %d types + %d fence probes, seed %d"
          % (total, len(generators), args.fence_cases, seed))
    if failures:
        print("\n%d failures (rerun with --seed %d):" % (len(failures), seed))
        for label, why in failures[:40]:
            print("  %-10s %s" % (label, why))
        return 1
    print("no mismatches against the Python oracle, and no statement slipped the fence")
    return 0


if __name__ == "__main__":
    sys.exit(main())
