#!/usr/bin/env python3
"""
typecov.py — fail when a DuckDB type is not exercised by any suite.

    test/scripts/typecov.py --port 9495 --token secret

Every other suite checks that the types it knows about are encoded correctly.
None of them notices a type nobody thought to write a case for, and that is not
a hypothetical gap: BIGNUM shipped emitting base64 of DuckDB's private storage,
and TIME_NS shipped panicking the executor thread and returning 200 with an
empty body. Both were in no suite, so nothing failed.

This closes the loop by working from the type list the encoder dispatches on —
the LogicalTypeId enum in the duckdb-rs source, read out of the cargo registry
at the version Cargo.lock actually pins — and requiring every variant to be
either produced by a corpus case, deliberately refused, or listed in EXCUSED
with a reason.

"Produced" means produced. This suite used to substring-match type names
against the corpus SQL text, which is a much weaker claim than it reads as:
nothing ran, and `"Varchar": ["VARCHAR", "'"]` meant any query containing a
quote counted as covering VARCHAR, `[3]` inside a LIST case covered ARRAY, and
`UBIGINT` covered BIGINT. Deleting every plain TIME, UNION and ARRAY case still
reported full coverage. Now each corpus query is sent to a running server and
the types are read back out of the `duckdbType` fields the server itself
emitted, recursing through `child` and `fields` so a type that only ever
appears nested still counts.

The consequence worth having: when a DuckDB upgrade adds a type, this fails on
the next run, before the type reaches anyone's data.

Exit codes: 0 covered, 1 a gap, 77 could not run (no duckdb-rs source found) —
77 so the runner can report it as skipped rather than as a pass.
"""

import argparse
import glob
import http.client
import json
import os
import re
import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import corpus  # noqa: E402


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



SKIPPED = 77

# Variants that cannot appear as a result column, or cannot be produced here.
# Every entry needs a reason: an excuse list nobody has to justify is just a
# way of turning a failing test green.
EXCUSED = {
    "Invalid": "not a type — the enum's zero value, used for 'unset'",
    "Any": "a binder-internal wildcard for function signatures; no value has it",
    "IntegerLiteral": "an internal type for an un-coerced literal during binding",
    "StringLiteral": "an internal type for an un-coerced literal during binding",
    "Geometry": "requires the spatial extension, which harbor does not depend on. "
                "Worth adding a case if spatial is ever a dependency.",
    "SqlNull": "the type of an untyped NULL during binding; DuckDB coerces it "
               "away before a result column exists. Probed against v1.5.5: "
               "SELECT NULL, NULL::\"NULL\", [NULL] and {a: NULL} all come back "
               "as INTEGER, INTEGER[] and STRUCT(a INTEGER).",
}

# Types harbor refuses rather than encodes. Coverage here means a corpus case
# actually got a 400 naming the type — not that somebody wrote the name down.
# A refusal that silently stopped happening is exactly as bad as an encoding
# that silently started being wrong.
REFUSED = {
    "TimeNs": "TIME_NS",
    "Variant": "VARIANT",
}

# duckdbType strings are SQL spellings; the enum is Rust names. Only the ones
# that differ need an entry — anything else matches case-insensitively.
SQL_TO_VARIANT = {
    "VARINT": "Bignum",
    "BIGNUM": "Bignum",
    "TIMESTAMP WITH TIME ZONE": "TimestampTZ",
    "TIMESTAMPTZ": "TimestampTZ",
    "TIME WITH TIME ZONE": "TimeTZ",
    "TIMETZ": "TimeTZ",
    "TIMESTAMP_S": "TimestampS",
    "TIMESTAMP_MS": "TimestampMs",
    "TIMESTAMP_NS": "TimestampNs",
    "TIME_NS": "TimeNs",
    "NULL": "SqlNull",
    '"NULL"': "SqlNull",
    # DuckDB reports a json column as JSON; it is a VARCHAR underneath and has
    # no LogicalTypeId of its own.
    "JSON": "Varchar",
}


def duckdb_rs_source():
    """The logical_type.rs of the duckdb-rs version this build actually links.

    Picked from Cargo.lock rather than by sorting the registry directory names.
    Sorting them is lexicographic, so with both 1.9.0 and 1.10505.0 unpacked —
    the ordinary state after a version bump — the *older* crate wins, and the
    gate meant to catch "an upgrade added a type" reads the list from before
    the upgrade.
    """
    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    lock = os.path.join(root, "Cargo.lock")
    version = None
    if os.path.exists(lock):
        m = re.search(r'^name = "duckdb"\nversion = "([^"]+)"', open(lock, encoding="utf-8").read(), re.M)
        if m:
            version = m.group(1)
    pattern = "duckdb-%s" % version if version else "duckdb-*"
    matches = glob.glob(os.path.expanduser(
        "~/.cargo/registry/src/*/%s/src/core/logical_type.rs" % pattern))
    return (matches[0], version) if matches else (None, version)


def duckdb_rs_types(path):
    text = open(path, encoding="utf-8").read()
    return sorted(set(re.findall(r"^\s+([A-Za-z]+) = DUCKDB_TYPE", text, re.M)))


def normalise(sql_type):
    """A duckdbType string reduced to the enum variant that produced it."""
    t = sql_type.strip()
    if t.endswith("]"):
        # `INTEGER[]` is a LIST, `INTEGER[3]` an ARRAY. The element type comes
        # back through `child`, so only the outer shape is decided here.
        inner = t[t.rfind("[") + 1:-1]
        return "Array" if inner.isdigit() else "List"
    upper = t.upper()
    if upper in SQL_TO_VARIANT:
        return SQL_TO_VARIANT[upper]
    # STRUCT(a INTEGER), DECIMAL(10,2), MAP(...), UNION(...), ENUM(...)
    base = upper.split("(", 1)[0].strip()
    return SQL_TO_VARIANT.get(base, base)


def types_in(column, found):
    """Every type in a schema column, including the nested ones."""
    if "duckdbType" in column:
        found.add(normalise(column["duckdbType"]))
    for child in ([column["child"]] if isinstance(column.get("child"), dict) else []):
        types_in(child, found)
    for field in (column.get("fields") or []):
        if isinstance(field, dict):
            types_in(field, found)


class Client:
    def __init__(self, host, port, token):
        self.host, self.port, self.token = host, port, token
        self.conn = None

    def sql(self, statement, params=None):
        body = {"sql": statement}
        if params is not None:
            body["params"] = params
        payload = json.dumps(body)
        headers = {"Authorization": "Bearer " + self.token,
                   "Content-Type": "application/json"}
        for attempt in (0, 1):
            try:
                if self.conn is None:
                    self.conn = http.client.HTTPConnection(self.host, self.port, timeout=120)
                self.conn.request("POST", "/sql", payload, headers)
                resp = self.conn.getresponse()
                text = resp.read().decode("utf-8", "replace")
                return resp.status, text
            except Exception:
                if self.conn:
                    self.conn.close()
                self.conn = None
                if attempt == 1:
                    return 0, ""
        return 0, ""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--token", required=True)
    args = ap.parse_args()

    path, version = duckdb_rs_source()
    if path is None:
        print("type coverage: no duckdb-rs source in the cargo registry for %s — cannot run"
              % (version or "any version"))
        return SKIPPED
    variants = duckdb_rs_types(path)
    print("type coverage: checking %d variants from duckdb-rs %s" % (len(variants), version or "?"))

    client = Client(args.host, args.port, args.token)
    produced = set()
    refused = set()

    cases = [(g, n, s, None) for g, n, s in corpus.all_queries()]
    cases += [(g, n, s, p) for g, n, s, p in corpus.all_params()]

    unreachable = 0
    for _group, _name, statement, params in cases:
        status, text = client.sql(statement, params)
        if status == 0:
            unreachable += 1
            continue
        for line in text.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if message.get("type") == "schema":
                for column in message.get("columns") or []:
                    types_in(column, produced)
            elif message.get("type") == "error" and status == 400:
                blurb = (message.get("message") or "").upper()
                for variant, spelling in REFUSED.items():
                    if spelling in blurb:
                        refused.add(variant)

    if unreachable:
        print("type coverage: %d of %d cases never reached the server" % (unreachable, len(cases)))
        return 1

    # The enum spells them Bigint, the wire spells them BIGINT. Fold both to
    # one case rather than adding an entry per type to the table above.
    by_upper = {v.upper(): v for v in variants}
    seen = {by_upper[t.upper()] for t in produced if t.upper() in by_upper}

    missing = []
    for name in variants:
        if name in EXCUSED:
            continue
        if name in REFUSED:
            if name not in refused:
                missing.append((name, "harbor should refuse it with a 400 that names it, and did not"))
            continue
        if name not in seen:
            missing.append((name, "no corpus case produced a column of this type"))

    excused = [v for v in variants if v in EXCUSED]
    print("type coverage: %d produced, %d refused as designed, %d excused, of %d"
          % (len(seen), len(refused), len(excused), len(variants)))

    # A type the server named that the enum does not have means the two lists
    # have drifted — worth saying out loud rather than silently ignoring.
    unknown = sorted(t for t in produced if t.upper() not in by_upper)
    if unknown:
        print("\nreported by the server but not in the LogicalTypeId enum:")
        for name in unknown:
            print("  %s" % name)

    if missing:
        print("\nnot exercised:")
        for name, why in missing:
            print("  %-14s %s" % (name, why))
        print("\nAdd a case to test/scripts/corpus.py, or an entry to EXCUSED here with\n"
              "the reason it cannot be produced.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
