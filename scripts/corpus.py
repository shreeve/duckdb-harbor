"""
corpus.py — the shared body of queries every suite runs.

One list, used three ways: the differential runner sends it to both the C++
harbor and harbor-ng and compares the answers, the type checker asserts the
spelled-out SPEC §5.4 encoding for the cases where harbor-ng formats values
itself, and the fuzzer uses the generators at the bottom to produce far more
of the same shapes than anyone would write by hand.

Keeping it in one place is the point. A type that is exercised by only one of
the three is a type where a bug can hide.

Every entry is a single SELECT — harbor takes one statement per request — and
every entry is deterministic, so two runs against two servers are comparable.
"""

# ---------------------------------------------------------------------------
# Types
#
# The hand-formatted cases matter most. In the C++ harbor these conversions
# were DuckDB's own (Timestamp::ToString, UUID::ToString, Bit::ToString,
# DecimalToString); harbor-ng reimplements them in Rust — civil_from_days,
# fmt_time, fmt_timestamp per TimeUnit, the interval micros conversion, the
# UUID high-bit flip, base64 — so these are original code paths with original
# bugs available. The boundary values below are chosen to break them.
# ---------------------------------------------------------------------------

TYPES = [
    # -- booleans
    ("bool-true",            "SELECT true::BOOLEAN AS v"),
    ("bool-false",           "SELECT false::BOOLEAN AS v"),
    ("bool-null",            "SELECT NULL::BOOLEAN AS v"),

    # -- signed integers, at their limits
    ("tinyint-min",          "SELECT (-128)::TINYINT AS v"),
    ("tinyint-max",          "SELECT 127::TINYINT AS v"),
    ("smallint-min",         "SELECT (-32768)::SMALLINT AS v"),
    ("smallint-max",         "SELECT 32767::SMALLINT AS v"),
    ("integer-min",          "SELECT (-2147483648)::INTEGER AS v"),
    ("integer-max",          "SELECT 2147483647::INTEGER AS v"),
    ("bigint-min",           "SELECT (-9223372036854775808)::BIGINT AS v"),
    ("bigint-max",           "SELECT 9223372036854775807::BIGINT AS v"),

    # The IEEE-754 safe-integer boundary, from both sides. 2^53-1 must stay a
    # JSON number; 2^53+1 must become a string, or a JavaScript client reads
    # 9007199254740992 and never finds out.
    ("bigint-safe",          "SELECT 9007199254740991::BIGINT AS v"),
    ("bigint-safe-plus-one", "SELECT 9007199254740992::BIGINT AS v"),
    ("bigint-unsafe",        "SELECT 9007199254740993::BIGINT AS v"),
    ("bigint-safe-neg",      "SELECT (-9007199254740991)::BIGINT AS v"),
    ("bigint-unsafe-neg",    "SELECT (-9007199254740993)::BIGINT AS v"),
    ("bigint-null",          "SELECT NULL::BIGINT AS v"),

    ("hugeint-max",          "SELECT 170141183460469231731687303715884105727::HUGEINT AS v"),
    ("hugeint-min",          "SELECT (-170141183460469231731687303715884105726)::HUGEINT AS v"),
    ("hugeint-small",        "SELECT 42::HUGEINT AS v"),
    ("hugeint-neg-small",    "SELECT (-1)::HUGEINT AS v"),
    ("hugeint-null",         "SELECT NULL::HUGEINT AS v"),

    # -- unsigned integers
    ("utinyint-max",         "SELECT 255::UTINYINT AS v"),
    ("utinyint-zero",        "SELECT 0::UTINYINT AS v"),
    ("usmallint-max",        "SELECT 65535::USMALLINT AS v"),
    ("uinteger-max",         "SELECT 4294967295::UINTEGER AS v"),
    ("ubigint-max",          "SELECT 18446744073709551615::UBIGINT AS v"),
    ("ubigint-safe",         "SELECT 9007199254740991::UBIGINT AS v"),
    ("ubigint-unsafe",       "SELECT 9007199254740993::UBIGINT AS v"),
    ("uhugeint-max",         "SELECT 340282366920938463463374607431768211455::UHUGEINT AS v"),
    ("uhugeint-small",       "SELECT 7::UHUGEINT AS v"),

    # -- floating point, including the values JSON cannot represent
    ("float-simple",         "SELECT 1.5::FLOAT AS v"),
    ("float-neg",            "SELECT (-2.25)::FLOAT AS v"),
    ("float-inf",            "SELECT 'inf'::FLOAT AS v"),
    ("float-neg-inf",        "SELECT '-inf'::FLOAT AS v"),
    ("float-nan",            "SELECT 'nan'::FLOAT AS v"),
    ("double-simple",        "SELECT 1.5::DOUBLE AS v"),
    ("double-tiny",          "SELECT 5e-324::DOUBLE AS v"),
    ("double-huge",          "SELECT 1.7976931348623157e308::DOUBLE AS v"),
    ("double-inf",           "SELECT 'inf'::DOUBLE AS v"),
    ("double-nan",           "SELECT 'nan'::DOUBLE AS v"),

    # BIGNUM (VARINT before v1.5). Arbitrary precision, and stored as a
    # three-byte header plus a big-endian magnitude that is one's-complemented
    # when negative — so the sign, the zero case and the boundary where the
    # value stops fitting a double all exercise different code.
    ("bignum-zero",          "SELECT 0::VARINT AS v"),
    ("bignum-one",           "SELECT 1::VARINT AS v"),
    ("bignum-neg-one",       "SELECT (-1)::VARINT AS v"),
    ("bignum-small",         "SELECT 42::VARINT AS v"),
    ("bignum-neg-small",     "SELECT (-255)::VARINT AS v"),
    ("bignum-json-safe",     "SELECT 9007199254740991::VARINT AS v"),
    ("bignum-past-safe",     "SELECT 9007199254740993::VARINT AS v"),
    ("bignum-huge",          "SELECT 123456789012345678901234567890::VARINT AS v"),
    ("bignum-neg-huge",      "SELECT (-123456789012345678901234567890)::VARINT AS v"),
    ("bignum-past-hugeint",  "SELECT 170141183460469231731687303715884105728::VARINT AS v"),
    ("double-null",          "SELECT NULL::DOUBLE AS v"),

    # -- decimals: width and scale have to survive, and so do trailing zeros
    ("decimal-4-2",          "SELECT 12.34::DECIMAL(4,2) AS v"),
    ("decimal-trailing",     "SELECT 4.5::DECIMAL(10,2) AS v"),
    ("decimal-neg",          "SELECT (-0.01)::DECIMAL(4,2) AS v"),
    ("decimal-zero",         "SELECT 0::DECIMAL(4,2) AS v"),
    ("decimal-scale-zero",   "SELECT 1234::DECIMAL(9,0) AS v"),
    ("decimal-18-4",         "SELECT 12345678901234.5678::DECIMAL(18,4) AS v"),
    ("decimal-38-12",        "SELECT 12345678901234567890123456.123456789012::DECIMAL(38,12) AS v"),
    ("decimal-38-0",         "SELECT 12345678901234567890123456789012345678::DECIMAL(38,0) AS v"),
    ("decimal-null",         "SELECT NULL::DECIMAL(10,2) AS v"),

    # -- text, including everything that has to be escaped
    ("varchar-empty",        "SELECT ''::VARCHAR AS v"),
    ("varchar-ascii",        "SELECT 'hello'::VARCHAR AS v"),
    ("varchar-unicode",      "SELECT 'héllo — 世界 🦆'::VARCHAR AS v"),
    ("varchar-quote",        "SELECT 'she said \"hi\"'::VARCHAR AS v"),
    ("varchar-backslash",    "SELECT 'a\\b'::VARCHAR AS v"),
    ("varchar-tab-newline",  "SELECT ('a' || chr(9) || 'b' || chr(10) || 'c')::VARCHAR AS v"),
    ("varchar-cr",           "SELECT ('a' || chr(13) || 'b')::VARCHAR AS v"),
    ("varchar-nul-adjacent", "SELECT ('a' || chr(1) || 'b')::VARCHAR AS v"),
    ("varchar-json-ish",     "SELECT '{\"not\": \"json\"}'::VARCHAR AS v"),
    # U+2028/U+2029 are legal inside a JSON string but are line terminators to
    # a Unicode-aware splitter, which is a hazard in a line-delimited format.
    ("varchar-line-sep",     "SELECT ('a' || chr(8232) || 'b')::VARCHAR AS v"),
    ("varchar-para-sep",     "SELECT ('a' || chr(8233) || 'b')::VARCHAR AS v"),
    ("varchar-long",         "SELECT repeat('x', 10000)::VARCHAR AS v"),
    ("varchar-null",         "SELECT NULL::VARCHAR AS v"),

    # -- blobs: base64 padding is wrong for exactly one input length at a time
    ("blob-empty",           "SELECT ''::BLOB AS v"),
    ("blob-1",               "SELECT 'a'::BLOB AS v"),
    ("blob-2",               "SELECT 'ab'::BLOB AS v"),
    ("blob-3",               "SELECT 'abc'::BLOB AS v"),
    ("blob-4",               "SELECT 'abcd'::BLOB AS v"),
    ("blob-high-bytes",      "SELECT '\\xFF\\xFE\\x00\\x01'::BLOB AS v"),
    ("blob-null",            "SELECT NULL::BLOB AS v"),

    ("bit-simple",           "SELECT '101010'::BIT AS v"),
    ("bit-one",              "SELECT '1'::BIT AS v"),
    ("bit-null",             "SELECT NULL::BIT AS v"),

    # -- UUID: DuckDB stores it as a HUGEINT with the high bit flipped, so the
    # all-zero and all-one values are the ones that catch a sign mistake.
    ("uuid-known",           "SELECT '6ba7b810-9dad-11d1-80b4-00c04fd430c8'::UUID AS v"),
    ("uuid-nil",             "SELECT '00000000-0000-0000-0000-000000000000'::UUID AS v"),
    ("uuid-max",             "SELECT 'ffffffff-ffff-ffff-ffff-ffffffffffff'::UUID AS v"),
    ("uuid-high-bit",        "SELECT '80000000-0000-0000-0000-000000000000'::UUID AS v"),
    ("uuid-null",            "SELECT NULL::UUID AS v"),

    # -- dates: the civil-from-days conversion has to hold before 1970, across
    # leap days, and at the century rules.
    ("date-epoch",           "SELECT '1970-01-01'::DATE AS v"),
    ("date-day-before",      "SELECT '1969-12-31'::DATE AS v"),
    ("date-pre-1970",        "SELECT '1900-01-01'::DATE AS v"),
    ("date-leap-2000",       "SELECT '2000-02-29'::DATE AS v"),
    ("date-leap-2024",       "SELECT '2024-02-29'::DATE AS v"),
    ("date-non-leap-1900",   "SELECT '1900-03-01'::DATE AS v"),
    ("date-year-1",          "SELECT '0001-01-01'::DATE AS v"),
    ("date-year-9999",       "SELECT '9999-12-31'::DATE AS v"),
    ("date-modern",          "SELECT '2026-08-11'::DATE AS v"),
    ("date-null",            "SELECT NULL::DATE AS v"),

    # -- times
    ("time-midnight",        "SELECT '00:00:00'::TIME AS v"),
    ("time-one-micro",       "SELECT '00:00:00.000001'::TIME AS v"),
    ("time-almost-midnight", "SELECT '23:59:59.999999'::TIME AS v"),
    ("time-noon",            "SELECT '12:00:00'::TIME AS v"),
    ("time-null",            "SELECT NULL::TIME AS v"),
    ("timetz-utc",           "SELECT '12:00:00+00'::TIMETZ AS v"),
    ("timetz-offset",        "SELECT '12:00:00+02:30'::TIMETZ AS v"),

    # -- timestamps, one per storage unit
    ("timestamp-epoch",      "SELECT '1970-01-01 00:00:00'::TIMESTAMP AS v"),
    ("timestamp-pre-epoch",  "SELECT '1969-07-20 20:17:40'::TIMESTAMP AS v"),
    ("timestamp-micros",     "SELECT '2026-08-11 07:24:05.123456'::TIMESTAMP AS v"),
    ("timestamp-millis",     "SELECT '2026-08-11 07:24:05.123'::TIMESTAMP AS v"),
    ("timestamp-null",       "SELECT NULL::TIMESTAMP AS v"),
    ("timestamp-s",          "SELECT '2026-08-11 07:24:05'::TIMESTAMP_S AS v"),
    ("timestamp-s-pre",      "SELECT '1900-01-01 00:00:00'::TIMESTAMP_S AS v"),
    ("timestamp-ms",         "SELECT '2026-08-11 07:24:05.123'::TIMESTAMP_MS AS v"),
    ("timestamp-ms-pre",     "SELECT '1900-01-01 00:00:00.001'::TIMESTAMP_MS AS v"),
    ("timestamp-ns",         "SELECT '2026-08-11 07:24:05.123456789'::TIMESTAMP_NS AS v"),
    ("timestamp-ns-round",   "SELECT '2026-08-11 07:24:05.000000001'::TIMESTAMP_NS AS v"),
    ("timestamptz-utc",      "SELECT '2026-08-11 07:24:05+00'::TIMESTAMPTZ AS v"),
    ("timestamptz-offset",   "SELECT '2026-08-11 07:24:05+02'::TIMESTAMPTZ AS v"),

    # -- intervals: months, days and micros are stored separately and none of
    # them normalises into the others
    ("interval-months",      "SELECT INTERVAL 14 MONTHS AS v"),
    ("interval-days",        "SELECT INTERVAL 45 DAYS AS v"),
    ("interval-micros",      "SELECT INTERVAL 90 SECONDS AS v"),
    ("interval-mixed",       "SELECT (INTERVAL 1 MONTH + INTERVAL 2 DAYS + INTERVAL 3 HOURS) AS v"),
    ("interval-negative",    "SELECT (-INTERVAL 5 DAYS) AS v"),
    ("interval-sub-second",  "SELECT INTERVAL 1 MILLISECOND AS v"),
    ("interval-null",        "SELECT NULL::INTERVAL AS v"),

    # -- containers
    ("list-empty",           "SELECT []::INTEGER[] AS v"),
    ("list-ints",            "SELECT [1, 2, 3]::INTEGER[] AS v"),
    ("list-strings",         "SELECT ['a', 'b'] AS v"),
    ("list-with-null",       "SELECT [1, NULL, 3]::INTEGER[] AS v"),
    ("list-null",            "SELECT NULL::INTEGER[] AS v"),
    ("list-nested",          "SELECT [[1, 2], [3]]::INTEGER[][] AS v"),
    ("list-of-struct",       "SELECT [{'a': 1}, {'a': 2}] AS v"),
    ("list-of-decimal",      "SELECT [1.5, 2.25]::DECIMAL(5,2)[] AS v"),
    ("list-of-date",         "SELECT ['2026-01-01', '1969-12-31']::DATE[] AS v"),

    ("struct-simple",        "SELECT {'a': 1, 'b': 'x'} AS v"),
    ("struct-with-null",     "SELECT {'a': NULL, 'b': 'x'} AS v"),
    ("struct-nested",        "SELECT {'a': {'b': {'c': 1}}} AS v"),
    ("struct-of-list",       "SELECT {'a': [1, 2]} AS v"),
    ("struct-null",          "SELECT NULL::STRUCT(a INTEGER) AS v"),
    ("struct-odd-names",     "SELECT {'a b': 1, 'c\"d': 2} AS v"),

    ("array-fixed",          "SELECT [1, 2, 3]::INTEGER[3] AS v"),
    ("array-of-varchar",     "SELECT ['a', 'b']::VARCHAR[2] AS v"),

    ("map-simple",           "SELECT MAP {'a': 1, 'b': 2} AS v"),
    ("map-empty",            "SELECT MAP {}::MAP(VARCHAR, INTEGER) AS v"),
    ("map-int-keys",         "SELECT MAP {1: 'x', 2: 'y'} AS v"),

    ("union-int",            "SELECT union_value(num := 2) AS v"),
    ("union-str",            "SELECT union_value(str := 'x') AS v"),

    ("enum-value",           "SELECT 'happy'::ENUM('sad', 'ok', 'happy') AS v"),
    ("enum-null",            "SELECT NULL::ENUM('sad', 'happy') AS v"),

    ("json-object",          "SELECT '{\"a\": 1}'::JSON AS v"),
    ("json-array",           "SELECT '[1, 2]'::JSON AS v"),
    ("json-null-literal",    "SELECT 'null'::JSON AS v"),

    ("sqlnull",              "SELECT NULL AS v"),
]

# ---------------------------------------------------------------------------
# Shapes
#
# Not about any one type: about the envelope holding together for results that
# are empty, wide, long, or ragged.
# ---------------------------------------------------------------------------

SHAPES = [
    ("empty-result",         "SELECT 1 AS v WHERE false"),
    ("single-row",           "SELECT 1 AS v"),
    ("multi-column",         "SELECT 1 AS a, 'x' AS b, 1.5 AS c, NULL AS d"),
    ("duplicate-names",      "SELECT 1 AS a, 2 AS a"),
    ("unnamed-column",       "SELECT 1 + 1"),
    ("many-rows",            "SELECT i FROM range(5000) t(i)"),
    ("chunk-boundary",       "SELECT i FROM range(2048) t(i)"),
    ("chunk-boundary-plus",  "SELECT i FROM range(2049) t(i)"),
    ("wide-64",              "SELECT " + ", ".join("%d AS c%d" % (i, i) for i in range(64))),
    ("mixed-nulls",          "SELECT CASE WHEN i % 2 = 0 THEN NULL ELSE i END AS v FROM range(10) t(i)"),
    ("all-null-column",      "SELECT NULL::INTEGER AS v FROM range(5)"),
    ("long-value-rows",      "SELECT repeat('y', 1000) AS v FROM range(50)"),
]

# ---------------------------------------------------------------------------
# Errors
#
# Failures have to look the same on both implementations too: same status,
# same envelope. A client that special-cases an error string is relying on it.
# ---------------------------------------------------------------------------

ERRORS = [
    ("unknown-table",        "SELECT * FROM definitely_not_a_table"),
    ("unknown-column",       "SELECT no_such_column FROM sites"),
    ("syntax-error",         "SELECT FROM WHERE"),
    ("division-by-zero",     "SELECT 1 // 0 AS v"),
    ("cast-failure",         "SELECT 'abc'::INTEGER AS v"),
    ("out-of-range-cast",    "SELECT 99999::TINYINT AS v"),
    ("unknown-function",     "SELECT no_such_function(1)"),
]

# ---------------------------------------------------------------------------
# Real data
#
# The loaded tables, queried the way anything real would query them.
# ---------------------------------------------------------------------------

DATA = [
    ("count-sites",          "SELECT count(*) AS n FROM sites"),
    ("count-cohorts",        "SELECT count(*) AS n FROM cohorts"),
    ("count-plans",          "SELECT count(*) AS n FROM plans"),
    ("count-rules",          "SELECT count(*) AS n FROM rules"),
    ("select-star-sites",    "SELECT * FROM sites ORDER BY account"),
    ("select-star-rules",    "SELECT * FROM rules ORDER BY client, name"),
    ("select-star-cohorts",  "SELECT * FROM cohorts ORDER BY ef_id, client"),
    ("select-star-plans",    "SELECT * FROM plans ORDER BY ef_id, client, plan"),
    ("join-two",             "SELECT s.client, count(*) AS n FROM sites s JOIN plans p USING (client) GROUP BY 1 ORDER BY 1"),
    ("join-four",            "SELECT count(*) AS n FROM sites s JOIN rules r USING (client) "
                             "JOIN plans p USING (client) JOIN cohorts c USING (client)"),
    ("left-join",            "SELECT count(*) AS n FROM plans p LEFT JOIN rules r USING (client)"),
    ("group-having",         "SELECT client, count(*) AS n FROM plans GROUP BY 1 HAVING count(*) > 5 ORDER BY 1"),
    ("window",               "SELECT account, row_number() OVER (ORDER BY account) AS rn FROM sites ORDER BY rn LIMIT 20"),
    ("cte",                  "WITH c AS (SELECT DISTINCT client FROM sites) SELECT count(*) AS n FROM c"),
    ("string-agg",           "SELECT string_agg(DISTINCT state, ',' ORDER BY state) AS v FROM sites WHERE state IS NOT NULL"),
    ("md5-digest",           "SELECT md5(string_agg(account, '|' ORDER BY account)) AS v FROM sites"),
    ("union-all",            "SELECT count(*) AS n FROM (SELECT client FROM sites UNION ALL SELECT client FROM rules)"),
    ("correlated",           "SELECT count(*) AS n FROM sites s WHERE EXISTS (SELECT 1 FROM rules r WHERE r.client = s.client)"),
    ("filtered-agg",         "SELECT count(*) FILTER (WHERE status = 'Active') AS a, count(*) AS n FROM rules"),
    ("cross-join",           "SELECT count(*) AS n FROM plans a CROSS JOIN rules b"),
    ("order-limit",          "SELECT client, name FROM rules ORDER BY client, name LIMIT 25"),
    ("distinct-on",          "SELECT DISTINCT ON (client) client, name FROM rules ORDER BY client, name"),
    ("regexp",               "SELECT count(*) AS n FROM sites WHERE regexp_matches(coalesce(name, ''), '^[A-Z]')"),
    ("nested-agg",           "SELECT client, list(name ORDER BY name) AS names FROM rules GROUP BY 1 ORDER BY 1 LIMIT 10"),
    ("struct-from-data",     "SELECT {'client': client, 'name': name} AS v FROM rules ORDER BY client, name LIMIT 10"),
    ("unnest",               "SELECT unnest([1, 2, 3]) AS v"),
    ("pivot-ish",            "SELECT status, count(*) AS n FROM rules GROUP BY 1 ORDER BY 1"),
]

# Parameterised requests: (name, sql, params)
PARAMS = [
    ("param-int",            "SELECT ? + 1 AS v", [41]),
    ("param-string",         "SELECT ? AS v", ["hello"]),
    ("param-null",           "SELECT ?::VARCHAR AS v", [None]),
    ("param-float",          "SELECT ? AS v", [2.5]),
    ("param-bool",           "SELECT ? AS v", [True]),
    ("param-two",            "SELECT ? || ? AS v", ["a", "b"]),
    ("param-unicode",        "SELECT ? AS v", ["héllo 🦆"]),
    ("param-big",            "SELECT ? AS v", [9007199254740993]),
    ("param-negative",       "SELECT ? AS v", [-1]),
    ("param-filter",         "SELECT count(*) AS n FROM plans WHERE client = ?", ["aetna"]),
    ("param-injection",      "SELECT count(*) AS n FROM sites WHERE client = ?", ["' OR 1=1 --"]),
    ("param-quote",          "SELECT ? AS v", ["it's"]),
    ("param-backslash",      "SELECT ? AS v", ["a\\b"]),
    ("param-empty",          "SELECT ? AS v", [""]),
]


def all_queries():
    """Everything with no parameters, as (group, name, sql)."""
    out = []
    for group, entries in (("types", TYPES), ("shapes", SHAPES),
                           ("errors", ERRORS), ("data", DATA)):
        for name, sql in entries:
            out.append((group, name, sql))
    return out


def all_params():
    """The parameterised requests, as (group, name, sql, params)."""
    return [("params", name, sql, params) for name, sql, params in PARAMS]
