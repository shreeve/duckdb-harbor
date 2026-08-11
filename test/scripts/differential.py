#!/usr/bin/env python3
"""
differential.py — run the corpus against the v1 harbor and this one and
compare the answers.

    test/scripts/differential.py --old-port 9401 --new-port 9402 --token T

The v1 harbor has been in production and is the reference for what clients
already expect. That does not make it the specification. Where harbor
answers differently *and better*, the difference is the point, so this runner
classifies rather than merely diffs:

    same        the two agree once irrelevant fields are set aside
    improved    they differ, and the difference is a deliberate, recorded
                improvement — every one is listed in DELIBERATE below with
                the reason it exists
    DIFFERENT   they differ and nobody decided that in advance

Only the third kind is a failure. A run is green when it is empty, which is a
much more useful bar than byte equality: it means every divergence between the
old implementation and the new one is one a person chose.

Fields that could never match are excluded up front rather than allowlisted
per query — see IGNORED_KEYS.
"""

import argparse
import http.client
import json
import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import corpus  # noqa: E402


# Timing is not a result. sessionId is assigned per connection.
IGNORED_KEYS = {"timeMs", "sessionId"}


# Divergences that are intended. The key is (group, name) or (group, "*") for a
# whole group; the value is why the new behaviour is the one we want.
# Cases where the two are allowed to disagree on the HTTP status, not just the
# body. Kept as an explicit set rather than inferred from the reason text, so
# adding a note can never quietly widen what is tolerated.
STATUS_MAY_DIFFER = {
    ("errors", "multi-statement"),
    ("errors", "multi-like-escape"),
    ("errors", "multi-ilike-escape"),
    ("errors", "multi-escape-keyword"),
    ("errors", "multi-date-literal"),
    ("errors", "multi-dollar-param"),
    ("types", "varchar-unicode"),
    ("params", "param-unicode"),
    ("types", "time-ns"),
    ("types", "variant"),
}

DELIBERATE = {
    ("types", "varchar-unicode"):
        "The v1 harbor answers 500 to a non-ASCII string literal; harbor "
        "returns it. Found by this runner.",
    ("params", "param-unicode"):
        "The v1 harbor answers 400 to a non-ASCII bound parameter; harbor "
        "binds it. Same underlying defect as types/varchar-unicode.",
    ("types", "timetz-utc"):
        "TIME WITH TIME ZONE: the v1 harbor keeps the UTC offset, harbor "
        "cannot — duckdb-rs decodes the value to a local time and discards the "
        "offset before harbor sees it. harbor marks the column "
        "lossless:false rather than emit a time that silently means something "
        "else. A real limitation, recorded here so it stays visible.",
    ("types", "timetz-offset"):
        "See types/timetz-utc.",
    # Listed case by case rather than as ("types", "*"): a wildcard here would
    # excuse every future type divergence, which is exactly what this ledger
    # exists to prevent.
    **{("types", name): (
        "BIGNUM: the v1 harbor casts the value to VARCHAR and marks the column "
        "lossless:false, encoding:'varchar-cast'. harbor decodes DuckDB's "
        "storage directly — three-byte header, big-endian magnitude, one's-"
        "complemented when negative — and reports lossless:true. Both emit the "
        "same digits; the difference is that harbor's metadata is true. "
        "Small values also go out as bare JSON numbers under the same rule as "
        "every other integer, rather than always as text."
    ) for name in [
        "bignum-zero", "bignum-one", "bignum-neg-one", "bignum-small",
        "bignum-neg-small", "bignum-json-safe", "bignum-past-safe",
        "bignum-huge", "bignum-neg-huge", "bignum-past-hugeint",
    ]},
    ("types", "time-ns"):
        "TIME_NS: the v1 harbor encodes it; harbor refuses with 400. "
        "duckdb-rs has no decoder for nanosecond TIME and reaches an "
        "unreachable! rather than returning an error, so attempting it "
        "panicked the executor thread — the client got 200 with an empty body "
        "and the connection never returned to the pool. harbor now rejects "
        "the query before any header is sent and names the cast that works. A "
        "real limitation, refused loudly instead of silently mis-answered.",
    ("types", "variant"):
        "VARIANT: DuckDB's own Arrow layer refuses to decode it ('decoding "
        "Variant columns is not supported'), so harbor surfaces that as a "
        "400. The v1 harbor reads DuckDB's vectors directly and never crosses "
        "Arrow, so it can encode the value. Inherited from the Arrow path, not "
        "a harbor defect, and it fails cleanly.",
    # Enumerated, not wildcarded. The two servers disagree about the error
    # envelope, and that is the whole of the disagreement: v1 answers
    # {"ok":false,"error":...,"errorCode":"TABLE_NOT_FOUND"}, harbor answers
    # {"type":"error","code":"sql_error","message":...}. Both answer 400, and
    # both reproduce DuckDB's message verbatim. The codes are different
    # vocabularies — DuckDB's own error names against the class a client
    # branches on — so they are reported as an envelope difference rather than
    # equated.
    **{("errors", name): (
        "Error envelope: v1 answers {ok:false, error, errorCode:<DuckDB's own "
        "name>}, harbor answers {type:'error', code:<class>, message}. Same "
        "status, same message text; the code vocabularies differ by design."
    ) for name in [
        "unknown-table", "unknown-column", "syntax-error", "cast-failure",
        "out-of-range-cast", "unknown-function",
    ]},

    # v1 accepts multi-statement text and runs it. duckdb-rs executes every
    # statement but the last during prepare, so harbor must refuse it, and
    # these are the shapes that once got past its scanner: a keyword ending in
    # `e` butted against a literal reads as an E'...' escape string, the
    # backslash then hides the closing quote, and the terminator after it goes
    # unnoticed. Recorded here because it is a divergence, but the direction
    # matters: each of these dropped a table on the v1 side.
    **{("errors", name): (
        "harbor refuses more than one statement per request with a 400; v1 "
        "accepts the text and executes it. Found by this runner."
    ) for name in [
        "multi-statement", "multi-like-escape", "multi-ilike-escape",
        "multi-escape-keyword", "multi-date-literal", "multi-dollar-param",
    ]},

    # No ("errors", "*") entry, deliberately. The reason it used to carry —
    # "both return the same status and the same error code, and the prose is
    # not a contract" — describes something this runner cannot even see: parse()
    # keeps only {"code": ...} from an error envelope, so the prose is discarded
    # before anything is compared. The only difference the wildcard could
    # actually excuse was a difference in the error *code*, which is precisely
    # the thing clients branch on and the thing the reason claims is identical.
    # harbor answering "internal" where v1 answers "sql_error" would have
    # printed as a deliberate improvement.
}


class Server:
    def __init__(self, name, host, port, token):
        self.name, self.host, self.port, self.token = name, host, port, token
        self.conn = None

    def _connect(self):
        self.conn = http.client.HTTPConnection(self.host, self.port, timeout=180)

    def close(self):
        if self.conn:
            self.conn.close()
            self.conn = None

    def sql(self, sql, params=None):
        body = json.dumps({"sql": sql, **({"params": params} if params is not None else {})})
        headers = {"Authorization": "Bearer " + self.token, "Content-Type": "application/json"}
        for attempt in (0, 1):
            try:
                if self.conn is None:
                    self._connect()
                self.conn.request("POST", "/sql", body, headers)
                resp = self.conn.getresponse()
                payload = resp.read().decode("utf-8", "replace")
                status = resp.status
                # The v1 harbor closes the connection on some paths; reopen
                # rather than let that look like a failure.
                if resp.getheader("Connection", "").lower() == "close":
                    self.close()
                return status, parse(payload)
            except Exception as e:
                self.close()
                if attempt == 1:
                    return 0, {"transport-error": str(e)}
        return 0, {}


def parse(payload):
    """NDJSON, or a single JSON object, reduced to what is worth comparing."""
    lines = []
    # NDJSON is delimited by \n alone. splitlines() also breaks on U+2028,
    # U+2029 and other Unicode line separators, which are legal inside a JSON
    # string — using it here would make the harness fail on payloads a real
    # client reads fine.
    for line in payload.split("\n"):
        line = line.strip()
        if not line:
            continue
        try:
            lines.append(json.loads(line))
        except json.JSONDecodeError:
            return {"unparseable": line[:200]}

    out = {"columns": None, "rows": [], "rowCount": None, "error": None}
    for l in lines:
        kind = l.get("type")
        if kind == "schema":
            out["columns"] = [strip_ignored(c) for c in l.get("columns", [])]
        elif kind == "row":
            out["rows"].append(l.get("values"))
        elif kind == "end":
            out["rowCount"] = l.get("rowCount")
        elif kind == "error":
            out["error"] = {"code": l.get("code"), "vocab": "ng"}
        elif "ok" in l:
            # The v1 harbor's one-shot envelope, which harbor does not use.
            # A failure comes back through the same object with ok:false, so
            # without this branch every v1 error parsed as an empty success —
            # which is why every errors/* case reported "one errored and the
            # other did not" and was then waved through by a wildcard. Nothing
            # in that group had ever actually been compared.
            if l.get("ok") is False or "errorCode" in l:
                out["error"] = {"code": l.get("errorCode"), "vocab": "v1"}
            else:
                out["columns"] = [strip_ignored(c) for c in l.get("columns", [])]
                out["rows"] = l.get("rows", [])
                out["rowCount"] = len(out["rows"])
    return out


def strip_ignored(obj):
    if isinstance(obj, dict):
        return {k: strip_ignored(v) for k, v in obj.items() if k not in IGNORED_KEYS}
    if isinstance(obj, list):
        return [strip_ignored(x) for x in obj]
    return obj


def deliberate_reason(group, name):
    return DELIBERATE.get((group, name)) or DELIBERATE.get((group, "*"))


def describe(old, new):
    """Every concrete difference between two parsed results, or None.

    All of them, not the first: a schema difference often sits on top of a
    value difference, and reporting only the outer one hides the bug that
    actually matters.
    """
    out = []
    if old.get("error") or new.get("error"):
        if bool(old.get("error")) != bool(new.get("error")):
            out.append("one errored and the other did not: old=%s new=%s"
                       % (old.get("error"), new.get("error")))
        elif old["error"].get("vocab") != new["error"].get("vocab"):
            # Different envelopes, so different code vocabularies: v1 reports
            # DuckDB's own error names (TABLE_NOT_FOUND), harbor reports the
            # class a client branches on (sql_error). Not comparable value for
            # value, and recorded as such rather than silently equated.
            out.append("error envelope: old=%s(%s) new=%s(%s)"
                       % (old["error"].get("vocab"), old["error"].get("code"),
                          new["error"].get("vocab"), new["error"].get("code")))
        elif old["error"].get("code") != new["error"].get("code"):
            out.append("error code: old=%s new=%s"
                       % (old["error"]["code"], new["error"]["code"]))
        return "\n      ".join(out) if out else None
    if old.get("columns") != new.get("columns"):
        out.append("schema:\n      old=%s\n      new=%s"
                   % (json.dumps(old.get("columns"))[:400], json.dumps(new.get("columns"))[:400]))
    if old.get("rowCount") != new.get("rowCount"):
        out.append("rowCount: old=%s new=%s" % (old.get("rowCount"), new.get("rowCount")))
    if old.get("rows") != new.get("rows"):
        shown = 0
        for i, (a, b) in enumerate(zip(old["rows"], new["rows"])):
            if a != b:
                out.append("row %d:\n      old=%s\n      new=%s"
                           % (i, json.dumps(a)[:300], json.dumps(b)[:300]))
                shown += 1
                if shown == 3:
                    break
        if not shown:
            out.append("row counts differ: %d vs %d" % (len(old["rows"]), len(new["rows"])))
    return "\n      ".join(out) if out else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--old-port", type=int, required=True)
    ap.add_argument("--new-port", type=int, required=True)
    ap.add_argument("--token", required=True)
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()

    old = Server("cpp", args.host, args.old_port, args.token)
    new = Server("ng", args.host, args.new_port, args.token)

    cases = [(g, n, s, None) for g, n, s in corpus.all_queries()]
    cases += [(g, n, s, p) for g, n, s, p in corpus.all_params()]

    same = improved = 0
    different = []
    improvements = {}

    # A precondition, not a case. `describe` returns None when both servers
    # produced the same thing, and two servers that are both broken produce the
    # same thing — point this runner at a database without the fixture tables
    # and all 27 DATA cases fail identically on both, count as "same", and the
    # run exits 0 having compared nothing. Prove first that both servers can
    # actually answer.
    for server in (old, new):
        status, res = server.sql("SELECT count(*) AS n FROM sites")
        rows = res.get("rows") or []
        if status != 200 or not rows or not isinstance(rows[0][0], int) or rows[0][0] <= 0:
            print("differential: %s cannot answer the fixture (status %s, %s) — "
                  "the corpus needs the sites/plans/rules/cohorts tables. Without "
                  "this check both servers failing the same way would have counted "
                  "as agreement." % (server.name, status, res))
            return 2

    for group, name, sql, params in cases:
        old_status, old_res = old.sql(sql, params)
        new_status, new_res = new.sql(sql, params)

        if "transport-error" in old_res or "transport-error" in new_res:
            different.append((group, name, "transport: old=%s new=%s"
                              % (old_res.get("transport-error"), new_res.get("transport-error"))))
            continue

        diff = describe(old_res, new_res)
        status_diff = None
        # Both must agree on success vs failure. The exact 4xx code is allowed
        # to differ only if it is a recorded improvement.
        if (old_status == 200) != (new_status == 200):
            status_diff = "status: old=%d new=%d" % (old_status, new_status)

        if diff is None and status_diff is None:
            same += 1
            if args.verbose:
                print("  same      %s/%s" % (group, name))
            continue

        reason = deliberate_reason(group, name)
        if reason and (status_diff is None or (group, name) in STATUS_MAY_DIFFER):
            improved += 1
            improvements.setdefault(reason, []).append("%s/%s" % (group, name))
            if args.verbose:
                print("  improved  %s/%s" % (group, name))
            continue

        different.append((group, name, status_diff or diff))
        print("  DIFFERENT %s/%s\n      %s" % (group, name, status_diff or diff))

    old.close()
    new.close()

    print()
    print("%d cases: %d same, %d deliberate improvements, %d unexplained differences"
          % (len(cases), same, improved, len(different)))
    if improvements:
        print("\ndeliberate:")
        for reason, names in improvements.items():
            print("  %d cases — %s" % (len(names), reason))
    if different:
        print("\nunexplained:")
        for group, name, why in different:
            print("  %s/%s" % (group, name))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
