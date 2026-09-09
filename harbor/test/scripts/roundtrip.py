#!/usr/bin/env python3
"""
roundtrip.py — back a database up, restore it, and prove nothing changed.

  test/scripts/roundtrip.py [--seed N] [--tables N] [--rows N] [--keep]

The claim under test is the only one a backup has to be able to make: what
comes back is what went in. Not "the row count matches" — every value, every
declared type, every constraint, every sequence's position, compared by the
database itself.

Four bodies of input, hardest last:

  types     one table per entry in the shared corpus, so a type exercised
            anywhere in this repo is exercised here too
  schema    the parts that are not values — NOT NULL, DEFAULT, CHECK, PRIMARY
            KEY, UNIQUE, indexes, views, ENUMs, a sequence stopped mid-run,
            empty tables, and identifiers that have to be quoted
  dialect   the strings that attack the format itself: the null marker spelled
            out, every line ending, tabs, quotes, backslashes, and values that
            bait a CSV sniffer into re-inferring a column's type
  fuzz      random tables of random types, seeded, so a failure replays

The comparison never reads the exported text. It opens the RESTORED database,
attaches the ORIGINAL beside it read-only, and asks DuckDB: `EXCEPT ALL` in
both directions on every table, plus a set difference over duckdb_columns,
duckdb_constraints, duckdb_indexes and duckdb_sequences. Comparing the export
to itself would prove only that the writer agrees with the writer.

Every check emits rows ONLY when something is wrong, so silence is the pass.
"""

import argparse
import json
import random
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(HERE))
import corpus  # noqa: E402  (the path has to be set first)

HARBOR = ROOT / "target" / "release" / "harbor"

GREEN, RED, DIM, OFF = "\033[32m", "\033[31m", "\033[2m", "\033[0m"

fails = 0


def ok(msg):
    print(f"  {GREEN}✓{OFF} {msg}")


def bad(msg, detail=""):
    global fails
    fails += 1
    print(f"  {RED}✗{OFF} {msg}")
    for line in str(detail).strip().splitlines()[:12]:
        print(f"      {DIM}{line}{OFF}")


# ---------------------------------------------------------------------------
# Driving the binary
#
# Through the CLI rather than the HTTP client, because the CLI is what a person
# types and the verbs under test are CLI verbs. Every call is its own process,
# which means its own server: harbor summons one for a file nothing serves and
# it departs when the call ends.
# ---------------------------------------------------------------------------


def run(*args, expect=0):
    done = subprocess.run([str(HARBOR), *[str(a) for a in args]],
                          capture_output=True, text=True)
    if expect is not None and done.returncode != expect:
        raise RuntimeError(
            f"harbor {' '.join(str(a) for a in args)[:120]} exited "
            f"{done.returncode}\n{done.stdout}\n{done.stderr}")
    return done


def sql(db, text, mode="jsonlines"):
    """Run statements against `db` and return the result rows as dicts."""
    out = run(db, "--mode", mode, "-c", text).stdout
    return [json.loads(line) for line in out.splitlines() if line.strip()]


def quiet(db, text):
    """A statement whose result is not wanted — DDL, loads, the big builds."""
    run(db, "--mode", "trash", "-c", text)


# ---------------------------------------------------------------------------
# The round trip
# ---------------------------------------------------------------------------


def ident(name):
    return '"' + name.replace('"', '""') + '"'


def literal(value):
    """A VARCHAR as a DuckDB string literal. Nothing is backslash-escaped in
    a standard SQL string, so a tab, a newline or a backslash goes in as
    itself and only the quote has to double."""
    return "'" + value.replace("'", "''") + "'"


def roundtrip(name, build, work, block=None, fmt=None):
    """Build a database, back it up, restore it, and diff the two.

    Returns the list of mismatches — empty is the pass. `build` is a list of
    statements; the tables it leaves behind are discovered rather than
    declared, so a test cannot forget to check one.
    """
    src = work / f"{name}.duckdb"
    dst = work / f"{name}_restored.duckdb"
    backup = work / f"{name}.backup"

    for statement in build:
        quiet(src, statement)
    tables = [r["table_name"] for r in
              sql(src, "SELECT table_name FROM duckdb_tables() "
                       "WHERE database_name = current_database() ORDER BY 1")]
    # Both files have to be quiescent before the comparison attaches them:
    # a database another process holds open for writing cannot be attached,
    # even read-only.
    run(src, "stop")
    run(src, "backup", backup, *(["--format", fmt] if fmt else []))
    run(src, "stop")
    run(dst, "restore", backup, *(["--block-size", block] if block else []))
    return diff(src, dst, tables), tables


def diff(src, dst, tables):
    """Every difference between two database FILES, asked of DuckDB.

    One process, so one server, so one ATTACH: it is instance-wide and every
    statement after it sees it, whichever pooled connection they land on.
    """
    # Two spellings of each database's name, and they are not
    # interchangeable: `FROM db.table` needs the identifier, and
    # `WHERE database_name = 'db'` needs the bare text.
    orig, restored = "orig", dst.stem
    orig_id, restored_id = ident(orig), ident(restored)

    def both_ways(select, key):
        a = select.format(db=orig)
        b = select.format(db=restored)
        return (f"SELECT '{key}' AS check, x AS detail FROM "
                f"(({a} EXCEPT ALL {b}) UNION ALL ({b} EXCEPT ALL {a}))")

    checks = [
        both_ways(
            "SELECT table_name AS x FROM duckdb_tables() "
            "WHERE database_name = '{db}'", "table"),
        both_ways(
            "SELECT table_name || '.' || column_name || ' ' || data_type || "
            "' null=' || is_nullable || ' default=' || "
            "coalesce(column_default, '~') AS x FROM duckdb_columns() "
            "WHERE database_name = '{db}'", "column"),
        both_ways(
            "SELECT coalesce(table_name, '') || ' ' || constraint_text AS x "
            "FROM duckdb_constraints() WHERE database_name = '{db}'",
            "constraint"),
        both_ways(
            "SELECT index_name || ' ' || coalesce(sql, '') AS x "
            "FROM duckdb_indexes() WHERE database_name = '{db}'", "index"),
        # A sequence's POSITION is the subtle one: a load that supplies its own
        # ids does not advance the counter, so a restore that reset it would
        # hand out ids that already exist.
        both_ways(
            "SELECT sequence_name || ' next=' || "
            "coalesce(last_value + increment_by, start_value) || "
            "' inc=' || increment_by || ' cycle=' || cycle AS x "
            "FROM duckdb_sequences() WHERE database_name = '{db}'", "sequence"),
        both_ways(
            "SELECT view_name || ' ' || sql AS x FROM duckdb_views() "
            "WHERE database_name = '{db}' AND internal = false", "view"),
    ]
    # The data itself, table by table, typed: EXCEPT ALL compares values as
    # values rather than as text, so a DECIMAL that came back as a DOUBLE is
    # a difference and not a rounding this suite would print identically.
    for table in tables:
        t = ident(table)
        checks.append(
            f"SELECT 'data' AS check, {literal(table)} || ' — ' || "
            f"count(*) || ' rows differ' AS detail FROM "
            f"((SELECT * FROM {orig_id}.{t} EXCEPT ALL SELECT * FROM {restored_id}.{t}) "
            f"UNION ALL "
            f"(SELECT * FROM {restored_id}.{t} EXCEPT ALL SELECT * FROM {orig_id}.{t})) "
            f"HAVING count(*) > 0")

    text = f"ATTACH {literal(str(src))} AS {orig} (READ_ONLY);\n"
    text += ";\n".join(checks)
    rows = sql(dst, text)
    run(dst, "stop")
    return [f"{r['check']}: {r['detail']}" for r in rows]


# ---------------------------------------------------------------------------
# The four bodies of input
# ---------------------------------------------------------------------------


# Parquet has a hole of its own, and it is not the one text has: it cannot
# write a NEGATIVE interval at all ("Parquet files do not support negative
# intervals"). Text takes it without complaint. So no format carries every
# type, which is why a format can fail — asserted below.
NEGATIVE_INTERVAL = {"interval-negative"}


def types_build(without=frozenset()):
    """One table per corpus entry. The corpus is the shared body every other
    suite runs, so a type that is covered anywhere is covered here — and its
    boundary values were chosen to break the conversions, which is exactly
    what a round trip through text needs pointed at it."""
    return [f"CREATE TABLE {ident('ty_' + name)} AS {select}"
            for name, select in corpus.TYPES if name not in without]


def text_gives_way_to_parquet(work):
    """The two types text cannot hold, and the file that holds them anyway.

    A `UNION` written as csv loses its tag and the restore refuses it; a
    `VARIANT` is worse, because the restore SUCCEEDS and the contents come
    back retyped — an INT32 returns as a VARCHAR, printing the same and
    comparing unequal. Both survive parquet, so those tables and only those
    become parquet, named in load.sql beside the csv ones.

    Checked here rather than left to the bulk comparison because the point is
    not only that the values survive: it is that the SWAP is visible in the
    directory and announced when it happens.
    """
    src = work / "mixed.duckdb"
    dst = work / "mixed_restored.duckdb"
    backup = work / "mixed.backup"
    quiet(src, "CREATE TABLE plain(id INTEGER, s VARCHAR)")
    quiet(src, "INSERT INTO plain VALUES (1, 'x'), (2, '')")
    quiet(src, "CREATE TABLE u AS SELECT union_value(num := 2) AS v")
    # A quoted name, because DuckDB writes `a space` to `a_space.csv` and the
    # swap has to follow the FILE while matching on the TABLE.
    quiet(src, 'CREATE TABLE "odd name"(v VARIANT)')
    quiet(src, 'INSERT INTO "odd name" VALUES (42::VARIANT)')
    run(src, "stop")
    said = run(src, "backup", backup).stderr
    run(src, "stop")

    for table in ('u', '"odd name"'):
        if f"{table} is parquet, not text" in said:
            ok(f"the backup says {table} is parquet, and why")
        else:
            bad(f"the backup said nothing about {table}", said)
    names = sorted(f.name for f in backup.iterdir())
    want = ["load.sql", "odd_name.parquet", "plain.csv", "schema.sql", "u.parquet"]
    if names == want:
        ok("parquet only where text cannot reach — the rest stays greppable")
    else:
        bad("the backup directory is not the mixed shape", f"{names}\nwanted {want}")

    run(dst, "restore", backup)
    got = sql(dst, "SELECT variant_typeof((SELECT v FROM \"odd name\")) AS held, "
                   "(SELECT v FROM u)::VARCHAR AS tagged, "
                   "(SELECT count(*) FROM plain WHERE length(s) = 0) AS empty")[0]
    run(dst, "stop")
    if got == {"held": "INT32", "tagged": "2", "empty": 1}:
        ok("a VARIANT keeps its inner type and a UNION its tag, through parquet")
    else:
        bad("the mixed restore did not come back whole", got)

    # --strict: the same database, and the answer is a refusal instead.
    strict_dir = work / "strict.backup"
    refused = run(src, "backup", strict_dir, "--strict", expect=None)
    run(src, "stop")
    if refused.returncode == 0:
        bad("--strict wrote a backup it cannot restore as text")
    elif "cannot be written as text" not in refused.stderr:
        bad("--strict refused without saying why", refused.stderr)
    elif strict_dir.exists():
        bad("a refused backup left its half-written directory behind")
    else:
        ok("--strict refuses instead, names the table, and leaves nothing")

    # The mirror image, and the reason the swap is a rule rather than a
    # special case: parquet normalises a TIMETZ to UTC — 12:00:00+02:30 comes
    # back 09:30:00+00, the same instant, a different value, and nothing said.
    # Under --format parquet that table goes to TEXT.
    zone = work / "zoned.duckdb"
    zone_bk = work / "zoned.backup"
    zone_rs = work / "zoned_restored.duckdb"
    quiet(zone, "CREATE TABLE zoned AS SELECT '12:00:00+02:30'::TIMETZ AS v")
    quiet(zone, "CREATE TABLE plain(x INTEGER)")
    run(zone, "stop")
    said = run(zone, "backup", zone_bk, "--format", "parquet").stderr
    run(zone, "stop")
    files = sorted(f.name for f in zone_bk.iterdir())
    run(zone_rs, "restore", zone_bk)
    kept = sql(zone_rs, "SELECT v::VARCHAR AS v FROM zoned")[0]["v"]
    run(zone_rs, "stop")
    if "zoned is text, not parquet" not in said:
        bad("--format parquet said nothing about its TIMETZ column", said)
    elif files != ["load.sql", "plain.parquet", "schema.sql", "zoned.csv"]:
        bad("the swap did not go the other way", files)
    elif kept != "12:00:00+02:30":
        bad(f"the TIMETZ offset was lost anyway ({kept})")
    else:
        ok("under --format parquet a TIMETZ table goes to text — the swap "
           "runs both ways")

    # Parquet's own hole, and the shape of every format failure: loud, named
    # by DuckDB, and leaving nothing that could be mistaken for a backup.
    neg = work / "negative.duckdb"
    quiet(neg, "CREATE TABLE i AS SELECT (-INTERVAL 5 DAYS) AS v")
    run(neg, "stop")
    neg_dir = work / "negative.backup"
    refused = run(neg, "backup", neg_dir, "--format", "parquet", expect=None)
    run(neg, "stop")
    if refused.returncode == 0:
        bad("parquet wrote a negative interval — it used to refuse, so this "
            "check is out of date")
    elif neg_dir.exists():
        bad("a failed parquet backup left its directory behind")
    else:
        ok("a negative interval fails --format parquet loudly, and text "
           "takes it — no format carries everything")

    bogus = run(src, "backup", work / "never", "--format", "avro", expect=None)
    run(src, "stop")
    if bogus.returncode != 0 and "unknown format" in bogus.stderr:
        ok("an unknown format is refused, not guessed at")
    else:
        bad("an unknown format was accepted", bogus.stderr)


SCHEMA_BUILD = [
    # Constraints and defaults, which live in schema.sql rather than the data.
    """CREATE TABLE constrained (
         id      INTEGER PRIMARY KEY,
         code    VARCHAR NOT NULL UNIQUE,
         score   INTEGER DEFAULT 7 CHECK (score >= 0),
         note    VARCHAR DEFAULT 'n/a',
         made_on DATE DEFAULT '2026-01-01'
       )""",
    "INSERT INTO constrained (id, code, score) VALUES (1, 'a', 0), (2, 'b', 99)",
    "INSERT INTO constrained (id, code) VALUES (3, 'c')",

    # A composite key and an index that is not the key.
    "CREATE TABLE composite (a INTEGER, b VARCHAR, v INTEGER, PRIMARY KEY (a, b))",
    "INSERT INTO composite VALUES (1, 'x', 10), (1, 'y', 20), (2, 'x', 30)",
    "CREATE INDEX composite_v ON composite (v)",

    # An empty table is a file with a header and nothing under it, which is
    # the case a line-counting reader gets wrong.
    "CREATE TABLE empty_table (a INTEGER, b VARCHAR)",

    # One column, and many columns.
    "CREATE TABLE one_column (only_one VARCHAR)",
    "INSERT INTO one_column VALUES ('solo'), (NULL)",
    "CREATE TABLE wide (" + ", ".join(f"c{i} INTEGER" for i in range(64)) + ")",
    "INSERT INTO wide VALUES (" + ", ".join(str(i) for i in range(64)) + ")",

    # A sequence stopped part way. EXPORT writes its CURRENT value as the new
    # START, and a restore that lost that would hand out ids that already
    # exist the first time it was used.
    "CREATE SEQUENCE counter START 1",
    "CREATE TABLE seq_user (id INTEGER DEFAULT nextval('counter'), s VARCHAR)",
    "INSERT INTO seq_user (s) VALUES ('one'), ('two'), ('three')",

    # A named ENUM is a type in the catalog, not a column property.
    "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')",
    "CREATE TABLE feelings (who VARCHAR, how mood)",
    "INSERT INTO feelings VALUES ('a', 'happy'), ('b', NULL), ('c', 'sad')",

    "CREATE VIEW happy_people AS SELECT who FROM feelings WHERE how = 'happy'",

    # Identifiers that need quoting, including a table called `schema` — which
    # medlabs really has, next to the schema.sql the export writes.
    'CREATE TABLE "schema" (version INTEGER)',
    'INSERT INTO "schema" VALUES (7)',
    'CREATE TABLE "load" (x INTEGER)',
    'INSERT INTO "load" VALUES (1)',
    'CREATE TABLE "a space" ("a column" INTEGER, "quo""ted" VARCHAR)',
    'INSERT INTO "a space" VALUES (1, \'x\')',
    'CREATE TABLE "sÉlect 世界" (v INTEGER)',
    'INSERT INTO "sÉlect 世界" VALUES (1)',
]

# The strings that attack the format. Each is stored twice — alone in a row and
# beside a real NULL — because the failure that matters is the two becoming
# indistinguishable.
DIALECT_STRINGS = [
    "",                       # empty string, which the writer leaves bare
    " ", "  ", "\t", "\t\t",  # whitespace that a stripper would eat
    "  leading", "trailing  ", " both ",
    "NULL", "null", "Null", '"NULL"', "\\N", "NA", "None", "nil",
    "a\tb", "a\t\tb", "\tstart", "end\t",
    "a\nb", "a\rb", "a\r\nb", "\n", "\r", "\r\n",
    '"', '""', '"""', 'she said "hi"', 'a"b"c',
    "\\", "a\\b", "\\t", "\\n", "\\\\",
    ",", ";", "|", "a,b", "a;b",
    "id\ts",                  # a row that looks like the header
    "0", "1", "-1", "007", "1e10", "NaN", "Infinity", "-Infinity",
    "true", "false", "t", "f",
    "2026-01-01", "2026-01-01 00:00:00", "12:00:00",
    "{\"a\": 1}", "[1, 2]",
    "héllo — 世界 🦆", " ", " ", "\x01", "\x7f",
    "x" * 100_000,
]


def dialect_build():
    rows = []
    for i, s in enumerate(DIALECT_STRINGS):
        rows.append(f"({i}, {literal(s)}, {literal(s)})")
    return [
        # `nullable` holds the same strings; `paired` holds them next to a real
        # null in the third column, so any collapse of one into the other shows
        # up as a difference rather than as two rows that happen to match.
        "CREATE TABLE dialect (id INTEGER, nullable VARCHAR, not_null VARCHAR NOT NULL)",
        "INSERT INTO dialect VALUES " + ", ".join(rows),
        f"INSERT INTO dialect VALUES ({len(rows)}, NULL, '')",
        # A single-column table gives every value a line to itself, which is
        # where a trailing separator has nothing after it to hold its place.
        "CREATE TABLE one_per_line (s VARCHAR)",
        "INSERT INTO one_per_line VALUES " + ", ".join(
            f"({literal(s)})" for s in DIALECT_STRINGS[:20]) + ", (NULL)",
    ]


# The fuzz pool. Each entry generates a DuckDB literal of its own type; the
# point is breadth of SHAPE — nested, wide, null-heavy — over volume.
def fuzz_value(rng, kind):
    if rng.random() < 0.15:
        return "NULL"
    if kind == "INTEGER":
        return str(rng.randint(-2**31, 2**31 - 1))
    if kind == "BIGINT":
        return str(rng.randint(-2**63, 2**63 - 1))
    if kind == "DOUBLE":
        return rng.choice([repr(rng.uniform(-1e12, 1e12)), "0.0", "-0.0",
                           "1e308", "1e-308"])
    if kind == "DECIMAL(18,6)":
        return f"{rng.randint(-10**11, 10**11)}.{rng.randint(0, 999999):06d}"
    if kind == "BOOLEAN":
        return rng.choice(["true", "false"])
    if kind == "DATE":
        return f"'{rng.randint(1, 9999):04d}-{rng.randint(1, 12):02d}-01'::DATE"
    if kind == "TIMESTAMP":
        return (f"'{rng.randint(1, 9999):04d}-01-01 "
                f"{rng.randint(0, 23):02d}:{rng.randint(0, 59):02d}:"
                f"{rng.randint(0, 59):02d}.{rng.randint(0, 999999):06d}'::TIMESTAMP")
    if kind == "BLOB":
        return "'" + "".join(f"\\x{rng.randint(0, 255):02X}"
                             for _ in range(rng.randint(0, 8))) + "'::BLOB"
    if kind == "INTEGER[]":
        n = rng.randint(0, 4)
        return ("[" + ", ".join(rng.choice([str(rng.randint(-99, 99)), "NULL"])
                                for _ in range(n)) + "]::INTEGER[]")
    if kind == "STRUCT(a INTEGER, b VARCHAR)":
        return ("{'a': " + rng.choice([str(rng.randint(-99, 99)), "NULL"]) +
                ", 'b': " + rng.choice([literal(rng_string(rng)), "NULL"]) + "}")
    return literal(rng_string(rng))


ALPHABET = ("abcXYZ019 \t\n\r\\\"',;|" + "NULL" + "héllo世界🦆" +
            "  \x01\x7f")


def rng_string(rng):
    return "".join(rng.choice(ALPHABET) for _ in range(rng.randint(0, 40)))


FUZZ_KINDS = ["INTEGER", "BIGINT", "DOUBLE", "DECIMAL(18,6)", "VARCHAR",
              "BOOLEAN", "DATE", "TIMESTAMP", "BLOB", "INTEGER[]",
              "STRUCT(a INTEGER, b VARCHAR)"]


def fuzz_build(rng, tables, rows):
    out = []
    for t in range(tables):
        kinds = [rng.choice(FUZZ_KINDS) for _ in range(rng.randint(1, 8))]
        cols = ", ".join(f"c{i} {k}" for i, k in enumerate(kinds))
        out.append(f"CREATE TABLE fz_{t} ({cols})")
        n = rng.randint(0, rows)
        if n:
            values = ", ".join(
                "(" + ", ".join(fuzz_value(rng, k) for k in kinds) + ")"
                for _ in range(n))
            out.append(f"INSERT INTO fz_{t} VALUES {values}")
    return out


# ---------------------------------------------------------------------------


def check(label, name, build, work, block=None, fmt=None):
    try:
        mismatches, tables = roundtrip(name, build, work, block, fmt)
    except RuntimeError as e:
        bad(f"{label} (the round trip itself failed)", e)
        return
    if mismatches:
        bad(f"{label} — {len(mismatches)} difference(s)", "\n".join(mismatches))
    else:
        ok(f"{label} ({len(tables)} tables)")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seed", type=int, default=random.randrange(2**32))
    ap.add_argument("--tables", type=int, default=12)
    ap.add_argument("--rows", type=int, default=200)
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    if not HARBOR.exists():
        print(f"roundtrip: build first (no {HARBOR})", file=sys.stderr)
        return 77

    work = Path(tempfile.mkdtemp(prefix="harbor-roundtrip.", dir="/tmp"))
    # Its own runtime root, so nothing this suite starts lands in the
    # operator's fleet or outlives the run.
    import os
    os.environ["HARBOR_HOME"] = str(work / "home")
    (work / "home").mkdir()

    print("what goes in comes back out")
    print()
    try:
        rng = random.Random(args.seed)
        check("every type in the shared corpus", "types", types_build(), work)
        check("constraints, indexes, views, sequences and odd identifiers",
              "schema", SCHEMA_BUILD, work)
        check("the strings that attack the format", "dialect",
              dialect_build(), work)
        check(f"{args.tables} random tables (seed {args.seed})", "fuzz",
              fuzz_build(rng, args.tables, args.rows), work)
        # The restore is the only moment block size can be chosen, so the
        # smallest and the largest both have to carry the same bytes.
        check("restored at the 16k floor", "floor", dialect_build(), work,
              block="16k")
        check("restored at the 256k default", "ceiling", dialect_build(), work,
              block="256k")
        # --format parquet is the same round trip in one format. Pointed at
        # the whole corpus rather than a sample, because "parquet holds
        # everything" is the claim the fallback below rests on.
        check("every type again, through --format parquet", "pq",
              types_build(without=NEGATIVE_INTERVAL), work, fmt="parquet")
        text_gives_way_to_parquet(work)
    finally:
        if args.keep:
            print(f"\n{DIM}roundtrip: kept {work}{OFF}")
        else:
            shutil.rmtree(work, ignore_errors=True)

    print()
    if fails:
        print(f"roundtrip: {fails} failing (replay with --seed {args.seed})")
        return 1
    print("roundtrip: all green")
    return 0


if __name__ == "__main__":
    sys.exit(main())
