#!/usr/bin/env python3
"""
spec.py — assert the encoding harbor produces, spelled out.

    test/scripts/spec.py --port 9499 --token T

This suite proves the answer on the wire is the right one, straight from
SPEC. An implementation that misreads SPEC §5.4 agrees with itself
perfectly; only expectations written from the spec catch it, and that is
the whole reason this file exists.

Every expectation below is written out by hand from the SQL literal, not
captured from a run. Capturing output and calling it an expectation only
records what the code did on the day it was written.

The cases are weighted toward the conversions harbor performs itself —
dates, times, timestamps at each storage unit, intervals, UUID, base64, BIT,
the JSON-safe integer boundary — because those are the ones where a bug is
harbor's rather than DuckDB's.
"""

import argparse
import http.client
import json
import sys

# name, sql, expected duckdbType, expected lossless, expected value
CASES = [
    # -- integers and the IEEE-754 boundary -------------------------------
    ("tinyint",        "SELECT (-128)::TINYINT AS v",                 "TINYINT",  True,  -128),
    ("smallint",       "SELECT 32767::SMALLINT AS v",                 "SMALLINT", True,  32767),
    ("integer",        "SELECT (-2147483648)::INTEGER AS v",          "INTEGER",  True,  -2147483648),
    ("bigint-small",   "SELECT 42::BIGINT AS v",                      "BIGINT",   True,  42),
    # 2^53-1 is the largest integer a double holds exactly, so it stays a
    # number; anything past it is quoted or a JavaScript client silently reads
    # a different value than DuckDB stored.
    ("bigint-safe",    "SELECT 9007199254740991::BIGINT AS v",        "BIGINT",   True,  9007199254740991),
    ("bigint-2pow53",  "SELECT 9007199254740992::BIGINT AS v",        "BIGINT",   True,  "9007199254740992"),
    ("bigint-unsafe",  "SELECT 9007199254740993::BIGINT AS v",        "BIGINT",   True,  "9007199254740993"),
    ("bigint-max",     "SELECT 9223372036854775807::BIGINT AS v",     "BIGINT",   True,  "9223372036854775807"),
    ("bigint-min",     "SELECT (-9223372036854775808)::BIGINT AS v",  "BIGINT",   True,  "-9223372036854775808"),
    ("utinyint",       "SELECT 255::UTINYINT AS v",                   "UTINYINT", True,  255),
    ("uinteger",       "SELECT 4294967295::UINTEGER AS v",            "UINTEGER", True,  4294967295),
    ("ubigint-max",    "SELECT 18446744073709551615::UBIGINT AS v",   "UBIGINT",  True,  "18446744073709551615"),
    ("hugeint-max",    "SELECT 170141183460469231731687303715884105727::HUGEINT AS v",
                       "HUGEINT",  True,  "170141183460469231731687303715884105727"),
    ("hugeint-neg",    "SELECT (-1)::HUGEINT AS v",                   "HUGEINT",  True,  -1),
    ("uhugeint-max",   "SELECT 340282366920938463463374607431768211455::UHUGEINT AS v",
                       "UHUGEINT", True,  "340282366920938463463374607431768211455"),

    # -- floating point ----------------------------------------------------
    ("double",         "SELECT 1.5::DOUBLE AS v",                     "DOUBLE",   True,  1.5),
    # JSON has no Infinity or NaN. Emitting null would make an overflow
    # indistinguishable from a SQL NULL, so the names go out as strings.
    ("double-inf",     "SELECT 'inf'::DOUBLE AS v",                   "DOUBLE",   True,  "Infinity"),

    # BIGNUM goes out as its decimal digits, under the same JSON-safe rule as
    # every other integer: a bare number while a double holds it exactly, a
    # quoted string past that. Never base64 — that would be DuckDB's private
    # storage layout leaking onto the wire.
    ("bignum-zero",    "SELECT 0::VARINT AS v",                       "BIGNUM",   True,  0),
    ("bignum-one",     "SELECT 1::VARINT AS v",                       "BIGNUM",   True,  1),
    ("bignum-neg-one", "SELECT (-1)::VARINT AS v",                    "BIGNUM",   True,  -1),
    ("bignum-neg-255", "SELECT (-255)::VARINT AS v",                  "BIGNUM",   True,  -255),
    ("bignum-safe",    "SELECT 9007199254740991::VARINT AS v",        "BIGNUM",   True,  9007199254740991),
    ("bignum-unsafe",  "SELECT 9007199254740993::VARINT AS v",        "BIGNUM",   True,  "9007199254740993"),
    ("bignum-huge",    "SELECT 123456789012345678901234567890::VARINT AS v",
                                                                      "BIGNUM",   True,  "123456789012345678901234567890"),
    ("bignum-neg-huge","SELECT (-123456789012345678901234567890)::VARINT AS v",
                                                                      "BIGNUM",   True,  "-123456789012345678901234567890"),
    ("double-ninf",    "SELECT '-inf'::DOUBLE AS v",                  "DOUBLE",   True,  "-Infinity"),
    ("double-nan",     "SELECT 'nan'::DOUBLE AS v",                   "DOUBLE",   True,  "NaN"),
    ("float-inf",      "SELECT 'inf'::FLOAT AS v",                    "FLOAT",    True,  "Infinity"),
    ("double-max",     "SELECT 1.7976931348623157e308::DOUBLE AS v",  "DOUBLE",   True,  1.7976931348623157e308),

    # -- decimals: the scale is part of the value ---------------------------
    ("decimal-pad",    "SELECT 4.5::DECIMAL(10,2) AS v",              "DECIMAL(10,2)", True, "4.50"),
    ("decimal-neg",    "SELECT (-0.01)::DECIMAL(4,2) AS v",           "DECIMAL(4,2)",  True, "-0.01"),
    ("decimal-zero",   "SELECT 0::DECIMAL(4,2) AS v",                 "DECIMAL(4,2)",  True, "0.00"),
    ("decimal-int",    "SELECT 1234::DECIMAL(9,0) AS v",              "DECIMAL(9,0)",  True, "1234"),
    ("decimal-wide",   "SELECT 12345678901234.5678::DECIMAL(18,4) AS v",
                       "DECIMAL(18,4)", True, "12345678901234.5678"),

    # -- text and bytes ----------------------------------------------------
    ("varchar",        "SELECT 'hello' AS v",                         "VARCHAR", True, "hello"),
    ("varchar-empty",  "SELECT '' AS v",                              "VARCHAR", True, ""),
    ("varchar-uni",    "SELECT 'héllo — 世界 🦆' AS v",                 "VARCHAR", True, "héllo — 世界 🦆"),
    ("varchar-ctrl",   "SELECT ('a' || chr(9) || 'b' || chr(10) || 'c') AS v",
                       "VARCHAR", True, "a\tb\nc"),
    # Escaped on the wire as \u2028/\u2029 so a Unicode-aware line splitter
    # cannot read one row as two; the decoded value is unchanged.
    ("varchar-line-sep", "SELECT ('a' || chr(8232) || 'b') AS v", "VARCHAR", True, "a\u2028b"),
    ("varchar-para-sep", "SELECT ('a' || chr(8233) || 'b') AS v", "VARCHAR", True, "a\u2029b"),
    # base64 pads differently for each input length mod 3; all four are here.
    ("blob-empty",     "SELECT ''::BLOB AS v",                        "BLOB", True, ""),
    ("blob-1",         "SELECT 'a'::BLOB AS v",                       "BLOB", True, "YQ=="),
    ("blob-2",         "SELECT 'ab'::BLOB AS v",                      "BLOB", True, "YWI="),
    ("blob-3",         "SELECT 'abc'::BLOB AS v",                     "BLOB", True, "YWJj"),
    ("blob-4",         "SELECT 'abcd'::BLOB AS v",                    "BLOB", True, "YWJjZA=="),
    ("blob-high",      "SELECT '\\xFF\\xFE\\x00\\x01'::BLOB AS v",    "BLOB", True, "//4AAQ=="),
    # BIT is stored as a pad-count byte then the bits; base64 would be useless.
    ("bit",            "SELECT '101010'::BIT AS v",                   "BIT", True, "101010"),
    ("bit-one",        "SELECT '1'::BIT AS v",                        "BIT", True, "1"),

    # -- UUID: stored as a HUGEINT with the high bit flipped ----------------
    ("uuid",           "SELECT '6ba7b810-9dad-11d1-80b4-00c04fd430c8'::UUID AS v",
                       "UUID", True, "6ba7b810-9dad-11d1-80b4-00c04fd430c8"),
    ("uuid-nil",       "SELECT '00000000-0000-0000-0000-000000000000'::UUID AS v",
                       "UUID", True, "00000000-0000-0000-0000-000000000000"),
    ("uuid-max",       "SELECT 'ffffffff-ffff-ffff-ffff-ffffffffffff'::UUID AS v",
                       "UUID", True, "ffffffff-ffff-ffff-ffff-ffffffffffff"),
    ("uuid-high-bit",  "SELECT '80000000-0000-0000-0000-000000000000'::UUID AS v",
                       "UUID", True, "80000000-0000-0000-0000-000000000000"),

    # -- dates: civil-from-days, on both sides of the epoch and the century
    # rules that trip naive leap-year arithmetic
    ("date-epoch",     "SELECT '1970-01-01'::DATE AS v",              "DATE", True, "1970-01-01"),
    ("date-prev-day",  "SELECT '1969-12-31'::DATE AS v",              "DATE", True, "1969-12-31"),
    ("date-1900",      "SELECT '1900-01-01'::DATE AS v",              "DATE", True, "1900-01-01"),
    # 1900 is not a leap year, 2000 is: the two rules that disagree.
    ("date-1900-mar",  "SELECT '1900-03-01'::DATE AS v",              "DATE", True, "1900-03-01"),
    ("date-leap-2000", "SELECT '2000-02-29'::DATE AS v",              "DATE", True, "2000-02-29"),
    ("date-leap-2024", "SELECT '2024-02-29'::DATE AS v",              "DATE", True, "2024-02-29"),
    ("date-year-1",    "SELECT '0001-01-01'::DATE AS v",              "DATE", True, "0001-01-01"),
    ("date-year-9999", "SELECT '9999-12-31'::DATE AS v",              "DATE", True, "9999-12-31"),

    # -- times: a fraction appears only when there is one -------------------
    ("time-midnight",  "SELECT '00:00:00'::TIME AS v",                "TIME", True, "00:00:00"),
    ("time-micro",     "SELECT '00:00:00.000001'::TIME AS v",         "TIME", True, "00:00:00.000001"),
    ("time-late",      "SELECT '23:59:59.999999'::TIME AS v",         "TIME", True, "23:59:59.999999"),

    # -- timestamps, one per storage unit ----------------------------------
    ("ts-epoch",       "SELECT '1970-01-01 00:00:00'::TIMESTAMP AS v",
                       "TIMESTAMP", True, "1970-01-01T00:00:00"),
    ("ts-pre-epoch",   "SELECT '1969-07-20 20:17:40'::TIMESTAMP AS v",
                       "TIMESTAMP", True, "1969-07-20T20:17:40"),
    ("ts-micros",      "SELECT '2026-08-11 07:24:05.123456'::TIMESTAMP AS v",
                       "TIMESTAMP", True, "2026-08-11T07:24:05.123456"),
    # Trailing zeros would claim precision the value does not carry.
    ("ts-millis",      "SELECT '2026-08-11 07:24:05.123'::TIMESTAMP AS v",
                       "TIMESTAMP", True, "2026-08-11T07:24:05.123"),
    # TIMESTAMP_S has no fractional part by definition.
    ("ts-s",           "SELECT '2026-08-11 07:24:05'::TIMESTAMP_S AS v",
                       "TIMESTAMP_S", True, "2026-08-11T07:24:05"),
    ("ts-ms",          "SELECT '2026-08-11 07:24:05.123'::TIMESTAMP_MS AS v",
                       "TIMESTAMP_MS", True, "2026-08-11T07:24:05.123"),
    ("ts-ns",          "SELECT '2026-08-11 07:24:05.123456789'::TIMESTAMP_NS AS v",
                       "TIMESTAMP_NS", True, "2026-08-11T07:24:05.123456789"),

    # -- intervals: months, days and micros never normalise into each other
    ("interval-mon",   "SELECT INTERVAL 14 MONTHS AS v",  "INTERVAL", True,
                       {"months": 14, "days": 0, "micros": "0"}),
    ("interval-day",   "SELECT INTERVAL 45 DAYS AS v",    "INTERVAL", True,
                       {"months": 0, "days": 45, "micros": "0"}),
    ("interval-sec",   "SELECT INTERVAL 90 SECONDS AS v", "INTERVAL", True,
                       {"months": 0, "days": 0, "micros": "90000000"}),
    ("interval-ms",    "SELECT INTERVAL 1 MILLISECOND AS v", "INTERVAL", True,
                       {"months": 0, "days": 0, "micros": "1000"}),
    ("interval-neg",   "SELECT (-INTERVAL 5 DAYS) AS v",  "INTERVAL", True,
                       {"months": 0, "days": -5, "micros": "0"}),

    # -- containers --------------------------------------------------------
    ("list",           "SELECT [1, 2, 3]::INTEGER[] AS v",  "INTEGER[]", True, [1, 2, 3]),
    ("list-empty",     "SELECT []::INTEGER[] AS v",         "INTEGER[]", True, []),
    ("list-null-item", "SELECT [1, NULL, 3]::INTEGER[] AS v", "INTEGER[]", True, [1, None, 3]),
    ("struct",         "SELECT {'a': 1, 'b': 'x'} AS v",
                       "STRUCT(a INTEGER, b VARCHAR)", True, {"a": 1, "b": "x"}),
    # A field named after a keyword has to be quoted, or the type string is not
    # valid SQL.
    ("struct-keyword", "SELECT {'name': 1} AS v",
                       'STRUCT("name" INTEGER)', True, {"name": 1}),
    ("array",          "SELECT [1, 2, 3]::INTEGER[3] AS v", "INTEGER[3]", True, [1, 2, 3]),
    ("map",            "SELECT MAP {'a': 1} AS v",
                       "MAP(VARCHAR, INTEGER)", True, [["a", 1]]),
    ("enum",           "SELECT 'happy'::ENUM('sad', 'ok', 'happy') AS v",
                       "ENUM('sad', 'ok', 'happy')", True, "happy"),
    ("union",          "SELECT union_value(num := 2) AS v",
                       "UNION(num INTEGER)", True, {"tag": "num", "value": 2}),

    # -- nulls -------------------------------------------------------------
    ("null-varchar",   "SELECT NULL::VARCHAR AS v",  "VARCHAR",  True, None),
    ("null-bigint",    "SELECT NULL::BIGINT AS v",   "BIGINT",   True, None),
    ("null-date",      "SELECT NULL::DATE AS v",     "DATE",     True, None),
    ("null-decimal",   "SELECT NULL::DECIMAL(10,2) AS v", "DECIMAL(10,2)", True, None),

    # -- the lossy column ---------------------------------------------------
    # Every case above asserts lossless=True, which means the flag had never
    # been checked in the one state that carries information. A column that
    # silently started reporting lossless:true would have passed this suite
    # from top to bottom.
    #
    # TIME WITH TIME ZONE is harbor's only lossy type: DuckDB's Arrow exporter
    # drops the offset before harbor sees the value, keeping the local wall
    # clock, so 12:34:56+05 and 12:34:56-08 arrive indistinguishable. The
    # contract is that harbor says so rather than emitting a time that means
    # something else.
    ("timetz-utc",     "SELECT '12:34:56+00'::TIMETZ AS v",
                       "TIME WITH TIME ZONE", False, "12:34:56"),
    ("timetz-offset",  "SELECT '12:34:56+05'::TIMETZ AS v",
                       "TIME WITH TIME ZONE", False, "12:34:56"),
    ("timetz-negative", "SELECT '12:34:56-08'::TIMETZ AS v",
                       "TIME WITH TIME ZONE", False, "12:34:56"),
    # The documented recovery: the offset is still reachable through SQL, and
    # that column is exact.
    ("timetz-offset-recoverable",
                       "SELECT date_part('timezone', '12:34:56+05'::TIMETZ) AS v",
                       "BIGINT", True, 18000),

    # -- HUGEINT minimum ----------------------------------------------------
    # i128::MIN has no positive counterpart, so the "is this JSON-safe" test
    # overflowed on exactly this value and let it out as a bare number — the
    # one thing the envelope exists to prevent, on the type most likely to
    # carry it. hugeint-max above could not catch it.
    ("hugeint-min",    "SELECT (-170141183460469231731687303715884105728)::HUGEINT AS v",
                       "HUGEINT", True, "-170141183460469231731687303715884105728"),
]

# Extra schema keys a case must carry, beyond duckdbType and lossless.
SCHEMA_EXTRAS = {
    "decimal-pad":    {"decimal": {"width": 10, "scale": 2}},
    "decimal-wide":   {"decimal": {"width": 18, "scale": 4}},
    "array":          {"arrayLength": 3},
    "map":            {"encoding": "pairs"},
    "enum":           {"values": ["sad", "ok", "happy"]},
    # Not just lossless:false — the reason, so a client can tell which of the
    # value's properties it must not trust.
    "timetz-utc":      {"encoding": "time-offset-dropped"},
    "timetz-offset":   {"encoding": "time-offset-dropped"},
    "timetz-negative": {"encoding": "time-offset-dropped"},
}


def request(host, port, token, sql):
    conn = http.client.HTTPConnection(host, port, timeout=60)
    try:
        conn.request("POST", "/sql", json.dumps({"sql": sql}),
                     {"Authorization": "Bearer " + token, "Content-Type": "application/json"})
        resp = conn.getresponse()
        payload = resp.read().decode("utf-8", "replace")
        # split("\n"), not splitlines(): the latter also breaks on U+2028.
        return resp.status, [json.loads(l) for l in payload.split("\n") if l.strip()]
    finally:
        conn.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=9499)
    ap.add_argument("--token", required=True)
    args = ap.parse_args()

    passed = 0
    failures = []

    for name, sql, want_type, want_lossless, want_value in CASES:
        status, lines = request(args.host, args.port, args.token, sql)
        schema = next((l for l in lines if l.get("type") == "schema"), None)
        rows = [l["values"] for l in lines if l.get("type") == "row"]

        if status != 200 or not schema or not rows:
            failures.append((name, "no result: status=%s lines=%s" % (status, json.dumps(lines)[:200])))
            continue

        col = schema["columns"][0]
        problems = []
        if col.get("duckdbType") != want_type:
            problems.append("duckdbType: want %r, got %r" % (want_type, col.get("duckdbType")))
        if col.get("lossless") is not want_lossless:
            problems.append("lossless: want %r, got %r" % (want_lossless, col.get("lossless")))
        for key, want in SCHEMA_EXTRAS.get(name, {}).items():
            if col.get(key) != want:
                problems.append("%s: want %r, got %r" % (key, want, col.get(key)))
        got_value = rows[0][0]
        if got_value != want_value:
            problems.append("value: want %r, got %r" % (want_value, got_value))

        if problems:
            failures.append((name, "; ".join(problems)))
        else:
            passed += 1

    print("spec: %d of %d cases match SPEC §5.4" % (passed, len(CASES)))
    if failures:
        print()
        for name, why in failures:
            print("  FAIL %-16s %s" % (name, why))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
