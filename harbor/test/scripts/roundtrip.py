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


def run(*args, expect=0, stdin=None):
    done = subprocess.run([str(HARBOR), *[str(a) for a in args]],
                          input=stdin, capture_output=True, text=True)
    if expect is not None and done.returncode != expect:
        raise RuntimeError(
            f"harbor {' '.join(str(a) for a in args)[:120]} exited "
            f"{done.returncode}\n{done.stdout}\n{done.stderr}")
    return done


def sql(db, text, mode="jsonlines"):
    """Run statements against `db` and return the result rows as dicts."""
    # Fixtures can exceed the OS per-argument limit; stdin carries the same
    # SQL through the CLI without truncating or shrinking the test data.
    out = run(db, "--mode", mode, stdin=text).stdout
    return [json.loads(line) for line in out.splitlines() if line.strip()]


def quiet(db, text):
    """A statement whose result is not wanted — DDL, loads, the big builds."""
    run(db, "--mode", "trash", stdin=text)


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
    """The type text cannot hold, and the file that holds it anyway.

    A `UNION` written as csv loses its tag and the restore refuses it. It
    survives parquet, so those tables and only those become parquet, named
    in load.sql beside the csv ones. (A plain VARIANT column is not in this
    list: it travels as JSON, see variant_as_json.)

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
    quiet(src, 'CREATE TABLE "odd name" AS SELECT union_value(str := \'x\') AS v')
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
    got = sql(dst, "SELECT union_tag((SELECT v FROM \"odd name\"))::VARCHAR AS held, "
                   "(SELECT v FROM u)::VARCHAR AS tagged, "
                   "(SELECT count(*) FROM plain WHERE length(s) = 0) AS empty")[0]
    run(dst, "stop")
    if got == {"held": "str", "tagged": "2", "empty": 1}:
        ok("a UNION keeps its tag through parquet")
    else:
        bad("the mixed restore did not come back whole", got)

    # A VARIANT nested inside another type is beyond both formats: text
    # retypes it and parquet has no writer for a variant below the root. So
    # the answer is a refusal either way. Under text it is harbor's, by
    # name, before anything is written; under parquet the engine's own
    # EXPORT fails first, with its own words, and the directory goes too.
    nested = work / "nested.duckdb"
    quiet(nested, "CREATE TABLE n(s STRUCT(v VARIANT))")
    quiet(nested, "INSERT INTO n VALUES ({'v': 42::VARIANT})")
    run(nested, "stop")
    for fmt, words in (("tsv", "n cannot round-trip in either backup format"),
                       ("parquet", "not a root column")):
        refused = run(nested, "backup", work / f"nested.{fmt}", "--format", fmt, expect=None)
        run(nested, "stop")
        if refused.returncode != 0 and words in refused.stderr and not (work / f"nested.{fmt}").exists():
            ok(f"a nested VARIANT is refused under --format {fmt}, leaving nothing")
        else:
            bad(f"a nested VARIANT was not refused cleanly under --format {fmt}", refused.stderr)

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


def variant_as_json(work):
    """A plain VARIANT column travels as JSON text, and the trip is exact for
    what entered as JSON.

    The display rendering cannot come back — 42 and "42" both print as `42`,
    and the reader hands every cell back as a string — but JSON tells them
    apart, so the column is cast to JSON on the way out and decoded on the
    way in, through after.sql, since IMPORT DATABASE takes nothing but COPY.
    What JSON has no word for (a DATE put inside a variant from SQL) comes
    back as JSON's nearest type, and the backup says so; --strict refuses.
    """
    src = work / "vj.duckdb"
    dst = work / "vj_restored.duckdb"
    backup = work / "vj.backup"
    # Two VARIANT columns, one with a name that has to be quoted, and a
    # third column to prove the REPLACE leaves the others alone. Every value
    # entered as JSON: the number 42 and the string "42", nested containers
    # with nulls, an empty string, the null marker as a string, the null
    # word as a string, a real JSON null, and the characters that attack the
    # dialect.
    quiet(src, 'CREATE TABLE "odd name"(id INTEGER, v VARIANT, "quoted col" VARIANT, note VARCHAR)')
    docs = ['42', '"42"', '-1.5', 'true', 'null', '""', '"NULL"', '"null"',
            '{"n":42,"s":"42","l":[1,"2",null,{"b":false}]}', '[]', '{}',
            '"tab\\there \\"quoted\\" back\\\\slash \\u00e9"']
    rows = ", ".join(f"({i}, {literal(d)}::JSON::VARIANT, {literal(docs[-1 - i])}::JSON::VARIANT, 'n{i}')"
                     for i, d in enumerate(docs))
    quiet(src, f'INSERT INTO "odd name" VALUES {rows}')
    quiet(src, "CREATE TABLE one(v VARIANT)")
    quiet(src, "INSERT INTO one VALUES ('\"\"'::JSON::VARIANT)")
    run(src, "stop")
    said = run(src, "backup", backup).stderr
    run(src, "stop")

    names = sorted(f.name for f in backup.iterdir())
    want = ["after.sql", "load.sql", "odd_name.csv", "one.csv", "schema.sql"]
    if names == want:
        ok("a VARIANT column stays text, with after.sql beside load.sql")
    else:
        bad("the backup directory is not the JSON shape", f"{names}\nwanted {want}")
    if "holds" in said:
        bad("the backup complained about values that entered as JSON", said)
    text = (backup / "odd_name.csv").read_text()
    if "\t42\t" in text and '\t"""42"""\t' in text:
        ok("the number 42 is written as 42 and the string as \"42\"")
    else:
        bad("the JSON text does not tell 42 from \"42\"", text)
    # Rendered, an empty string is an empty LINE and the one-column table
    # would need every value quoted; as JSON it is `""`, a record like any
    # other. The check has to have looked at the JSON file to know that.
    if "one is quoted throughout" in said:
        bad("the blank-record check looked at EXPORT's file, not the JSON one", said)
    else:
        ok("the blank-record check looked at the JSON file, where \"\" is not blank")

    run(dst, "restore", backup)
    same = diff(src, dst, ["odd name", "one"])
    typed = sql(dst, 'SELECT string_agg(variant_typeof(v), \',\' ORDER BY id) AS t FROM "odd name"')[0]["t"]
    run(dst, "stop")
    if same:
        bad("the JSON round trip changed a value", same)
    elif typed != ("UINT64,VARCHAR,DOUBLE,BOOL_TRUE,VARIANT_NULL,VARCHAR,VARCHAR,VARCHAR,"
                   "OBJECT(n, s, l),ARRAY(0),OBJECT(),VARCHAR"):
        bad("the inner types did not come back", typed)
    else:
        ok("every value that entered as JSON returns with its inner type")

    # Stock DuckDB gets the same directory: load.sql is still pure COPY, so
    # IMPORT DATABASE takes it, and after.sql is one more file to run.
    plain = work / "vj_plain.duckdb"
    quiet(plain, f"IMPORT DATABASE {literal(str(backup))}")
    before = sql(plain, 'SELECT variant_typeof(v) AS t FROM "odd name" WHERE id = 0')[0]["t"]
    quiet(plain, (backup / "after.sql").read_text())
    after = sql(plain, 'SELECT variant_typeof(v) AS t FROM "odd name" WHERE id = 0')[0]["t"]
    run(plain, "stop")
    if (before, after) == ("VARCHAR", "UINT64"):
        ok("IMPORT DATABASE alone gives the JSON text; after.sql decodes it")
    else:
        bad("the directory does not import by hand the way it says", (before, after))

    # A CHECK on a VARIANT column. A plain COPY lands each cell as a VARIANT
    # string for after.sql to decode, and `variant_typeof(v) LIKE 'OBJECT%'`
    # refuses every one of those; harbor's restore loads the documents as
    # documents, so the table never holds the string. One CHECK is the
    # table's and one a column's, under a name that has to be quoted; a NOT
    # NULL VARIANT, a SQL NULL and a child table that references its parent
    # ride along, beside a table with no VARIANT at all.
    checked = work / "checked.duckdb"
    checked_dst = work / "checked_restored.duckdb"
    checked_backup = work / "checked.backup"
    quiet(checked, "CREATE TABLE g(id INTEGER PRIMARY KEY, v VARIANT, note VARCHAR, "
                   "CHECK (v IS NULL OR variant_typeof(v) LIKE 'OBJECT%'))")
    quiet(checked, "INSERT INTO g VALUES "
                   "(1, '{\"a\":{\"b\":[1,2,{\"c\":null}]},\"t\":\"tab\\there\",\"n\":null}'::JSON, 'first'), "
                   "(2, NULL, NULL), "
                   "(3, '{\"q\":\"say \\\"hi\\\"\",\"big\":18446744073709551615}'::JSON, 'third')")
    quiet(checked, 'CREATE TABLE "odd child"(id INTEGER, gid INTEGER REFERENCES g(id), '
                   '"the doc" VARIANT CHECK (variant_typeof("the doc") LIKE \'OBJECT%\'), extra VARIANT NOT NULL)')
    quiet(checked, 'INSERT INTO "odd child" VALUES (10, 1, \'{"k":[]}\'::JSON, \'[1,"two"]\'::JSON), '
                   '(11, 3, \'{}\'::JSON, \'"a string"\'::JSON)')
    quiet(checked, "CREATE TABLE ordinary(id INTEGER, name VARCHAR)")
    quiet(checked, "INSERT INTO ordinary VALUES (1, 'one'), (2, NULL), (3, '')")
    shape = ("SELECT (SELECT string_agg(coalesce(variant_typeof(v), '-') || ':' || (v IS NULL), ',' ORDER BY id) FROM g) "
             "|| ' / ' || (SELECT string_agg(variant_typeof(\"the doc\") || ':' || variant_typeof(extra), ',' ORDER BY id) "
             "FROM \"odd child\") AS t")
    before = sql(checked, shape)[0]["t"]
    run(checked, "stop")
    said = run(checked, "backup", checked_backup).stderr
    run(checked, "stop")
    names = sorted(f.name for f in checked_backup.iterdir())
    if "parquet" in said or names != ["after.sql", "g.csv", "load.sql", "odd_child.csv", "ordinary.csv", "schema.sql"]:
        bad("a CHECK on a VARIANT column changed how the table is written", f"{names}\n{said}")
    else:
        ok("a table with a CHECK on its VARIANT column is written as text like any other")
    restored = run(checked_dst, "restore", checked_backup, expect=None)
    if restored.returncode != 0:
        bad("a table with a CHECK on its VARIANT column did not restore", restored.stderr)
    else:
        same = diff(checked, checked_dst, ["g", "odd child", "ordinary"])
        after = sql(checked_dst, shape)[0]["t"]
        left = sql(checked_dst, "SELECT count(*) AS n FROM duckdb_tables() WHERE database_name = current_database()")[0]["n"]
        refused = run(checked_dst, "--mode", "trash", stdin="INSERT INTO g VALUES (4, '[1]'::JSON, 'an array')", expect=None)
        run(checked_dst, "stop")
        if same:
            bad("the restore through a CHECK changed a value", same)
        elif after != before or "VARIANT_NULL:true" not in after:
            bad("the documents under a CHECK did not come back as they were", f"{before}\n{after}")
        elif left != 3:
            bad("the restore left a table of its own behind", left)
        elif refused.returncode == 0 or "CHECK constraint failed" not in refused.stderr:
            bad("the restored CHECK does not check", refused.stderr)
        else:
            ok("it restores: every document a document, the SQL NULL a NULL, the CHECK in force")
    # The engine writes a SQL NULL VARIANT as the JSON text `null`
    # (duckdb#25873). A file that holds the null marker there instead, as one
    # written once that is fixed would, restores to the same NULL.
    marked = work / "checked.marked"
    shutil.copytree(checked_backup, marked)
    text = (marked / "g.csv").read_text()
    if "2\tnull\tNULL\n" not in text:
        bad("the SQL NULL VARIANT is not written the way this test assumes", text)
    (marked / "g.csv").write_text(text.replace("2\tnull\tNULL\n", "2\tNULL\tNULL\n"))
    marked_dst = work / "checked_marked.duckdb"
    remarked = run(marked_dst, "restore", marked, expect=None)
    after = sql(marked_dst, shape)[0]["t"] if remarked.returncode == 0 else remarked.stderr
    run(marked_dst, "stop")
    if after == before:
        ok("a SQL NULL VARIANT written as the null marker restores to the same NULL")
    else:
        bad("the null marker in a VARIANT column did not restore to NULL", f"{before}\n{after}")
    # Stock DuckDB cannot do this: IMPORT DATABASE lands the strings, and the
    # CHECK refuses them before after.sql has its turn.
    plain = work / "checked_plain.duckdb"
    stock = run(plain, "--mode", "trash", stdin=f"IMPORT DATABASE {literal(str(checked_backup))}", expect=None)
    run(plain, "stop")
    if stock.returncode != 0 and "CHECK constraint failed" in stock.stderr:
        ok("IMPORT DATABASE by hand is refused by that CHECK, as the README says")
    else:
        bad("IMPORT DATABASE by hand was not refused by the CHECK — the README says it is", stock.stderr)

    # What JSON has no word for: said out loud, written anyway, refused
    # under --strict, and kept by parquet.
    dated = work / "dated.duckdb"
    quiet(dated, "CREATE TABLE d(v VARIANT)")
    quiet(dated, "INSERT INTO d VALUES ('42'::JSON::VARIANT), (DATE '2020-01-01'::VARIANT), "
                 "({'when': DATE '2020-01-01', 'n': 1}::VARIANT)")
    run(dated, "stop")
    said = run(dated, "backup", work / "dated.backup").stderr
    run(dated, "stop")
    if 'd."v" holds DATE, OBJECT — written as JSON' in said and "--format parquet keeps it" in said:
        ok("a DATE inside a VARIANT is named, once, with the way to keep it")
    else:
        bad("a DATE inside a VARIANT went out as JSON without a word", said)
    refused = run(dated, "backup", work / "dated.strict", "--strict", expect=None)
    run(dated, "stop")
    if refused.returncode != 0 and "holds DATE" in refused.stderr and not (work / "dated.strict").exists():
        ok("--strict refuses it, names the column, and leaves nothing")
    else:
        bad("--strict did not refuse a VARIANT JSON cannot carry", refused.stderr)
    run(dated, "backup", work / "dated.pq", "--format", "parquet")
    run(dated, "stop")
    kept = work / "dated_pq.duckdb"
    run(kept, "restore", work / "dated.pq")
    # Parquet has relabelings of its own (an integer's width, an object's
    # key order), so the claim is the values and the DATE, not a type string.
    same = diff(dated, kept, ["d"])
    held = sql(kept, "SELECT string_agg(variant_typeof(v), ',') AS t FROM d")[0]["t"]
    run(kept, "stop")
    if not same and "DATE" in held:
        ok("--format parquet keeps the DATE, as promised")
    else:
        bad("parquet did not keep what the note promised", (held, same))


# A GENERATED column is the table's to compute: no COPY takes one back, so no
# file in a backup holds one, whichever statement wrote the file. One table
# for each way a file gets written: the JSON route a VARIANT column takes
# (the generated column between two stored ones, then three of them reading
# the document under names that have to be quoted), EXPORT's own COPY, the
# swap to parquet a UNION forces, and the requote a blank record forces. A
# VARIANT under a CHECK with no generated column rides along.
GENERATED_BUILD = [
    "CREATE TABLE o(id INTEGER PRIMARY KEY, doc VARIANT, qty INTEGER, "
    "doubled INTEGER GENERATED ALWAYS AS (qty * 2) VIRTUAL, note VARCHAR)",
    "INSERT INTO o VALUES (1, '{\"a\":\"x\",\"n\":1}'::JSON, 3, 'first'), (2, NULL, NULL, NULL), "
    "(3, '[1,\"two\",null]'::JSON, 7, ''), (4, '\"42\"'::JSON, 0, 'NULL'), (5, '42'::JSON, -2, 'fifth')",
    'CREATE TABLE "odd gen"(id INTEGER PRIMARY KEY, "the a" VARCHAR GENERATED ALWAYS AS ("the doc"[\'a\']::VARCHAR), '
    '"the doc" VARIANT, "quo""ted" INTEGER GENERATED ALWAYS AS (id + 100), '
    'same VARIANT GENERATED ALWAYS AS ("the doc"::VARIANT), kind VARCHAR GENERATED ALWAYS AS (variant_typeof("the doc")), '
    'tail VARCHAR)',
    'INSERT INTO "odd gen" VALUES (1, \'{"a":"x","n":1}\'::JSON, \'t1\'), (2, NULL, NULL), '
    '(3, \'{"a":5}\'::JSON, \'t3\'), (4, \'{"b":true}\'::JSON, \'\')',
    "CREATE TABLE p(id INTEGER PRIMARY KEY, price DECIMAL(10,2), qty INTEGER, "
    "total DECIMAL(12,2) GENERATED ALWAYS AS (price * qty) VIRTUAL)",
    "INSERT INTO p VALUES (1, 9.99, 3), (2, NULL, 4), (3, 0.50, NULL)",
    "CREATE TABLE tagged(id INTEGER, x UNION(num INTEGER, str VARCHAR), twice INTEGER GENERATED ALWAYS AS (id * 2))",
    "INSERT INTO tagged VALUES (1, 5), (2, 'five'), (3, NULL)",
    "CREATE TABLE blank(s VARCHAR, len BIGINT GENERATED ALWAYS AS (length(s)))",
    "INSERT INTO blank VALUES (''), ('x'), (NULL)",
    "CREATE TABLE c(id INTEGER PRIMARY KEY, v VARIANT CHECK (v IS NULL OR variant_typeof(v) LIKE 'OBJECT%'), n INTEGER)",
    "INSERT INTO c VALUES (1, '{\"k\":[1,2]}'::JSON, 1), (2, NULL, NULL), (3, '{}'::JSON, 3)",
]


def generated_columns(work):
    """A generated column is computed by the restored table, not loaded.

    `EXPORT DATABASE` leaves one out of the file it writes, and so does every
    file harbor writes in that one's place: a `SELECT *` or a `COPY <table>
    TO` would write it, and then nothing reads the file back. The values are
    compared all the same, generated ones included, since the comparison is
    `SELECT *` on both sides.
    """
    src = work / "gen.duckdb"
    dst = work / "gen_restored.duckdb"
    backup = work / "gen.backup"
    tables = ["blank", "c", "o", "odd gen", "p", "tagged"]
    for statement in GENERATED_BUILD:
        quiet(src, statement)
    shape = ("SELECT (SELECT string_agg(variant_typeof(doc) || ':' || (doc IS NULL) || ':' || coalesce(doubled::VARCHAR, '-'), ',' ORDER BY id) FROM o) "
             "|| ' / ' || (SELECT string_agg(concat_ws(':', variant_typeof(\"the doc\"), coalesce(\"the a\", '-'), \"quo\"\"ted\", "
             "variant_typeof(same), kind), ',' ORDER BY id) FROM \"odd gen\") AS t")
    computed = ("SELECT string_agg(table_name || '.' || column_name || '=' || generation_expression, '; ' "
                "ORDER BY table_name, column_index) AS t FROM duckdb_columns() "
                "WHERE database_name = current_database() AND is_generated")
    before = sql(src, shape)[0]["t"], sql(src, computed)[0]["t"]
    run(src, "stop")
    said = run(src, "backup", backup).stderr
    run(src, "stop")

    names = sorted(f.name for f in backup.iterdir())
    heads = {f.name: f.read_text().splitlines()[0] for f in backup.glob("*.csv")}
    want = {"blank.csv": '"s"', "c.csv": "id\tv\tn", "o.csv": "id\tdoc\tqty\tnote",
            "odd_gen.csv": "id\tthe doc\ttail", "p.csv": "id\tprice\tqty"}
    if names != sorted(["after.sql", "load.sql", "schema.sql", "tagged.parquet", *want]):
        bad("the backup directory is not the shape these tables call for", f"{names}\n{said}")
    elif heads != want:
        bad("a file holds a column no COPY will take back", heads)
    else:
        ok("no file holds a generated column, whichever statement wrote it")

    restored = run(dst, "restore", backup, expect=None)
    if restored.returncode != 0:
        bad("tables with generated columns did not restore", restored.stderr)
        return
    same = diff(src, dst, tables)
    after = sql(dst, shape)[0]["t"], sql(dst, computed)[0]["t"]
    left = sql(dst, "SELECT count(*) AS n FROM duckdb_tables() WHERE database_name = current_database()")[0]["n"]
    run(dst, "stop")
    if same:
        bad("a generated column changed what came back", same)
    elif after != before or "VARIANT_NULL:true:-" not in after[0] or "OBJECT(a):5:103:OBJECT(a):OBJECT(a)" not in after[0]:
        bad("the restored tables do not compute what the originals do", f"{before}\n{after}")
    elif left != len(tables):
        bad("the restore left a table of its own behind", left)
    else:
        ok("it restores: every generated value computed again, from the document too")

    # A text file that does hold the generated columns, as a `SELECT *` of the
    # table writes it, says so in its header. The restore stages it whole and
    # leaves those fields behind, with a VARIANT beside them or without.
    whole = work / "gen.whole"
    shutil.copytree(backup, whole)
    for table, file, select in (("o", "o.csv", 'SELECT * REPLACE (doc::JSON AS doc) FROM o'),
                                ("odd gen", "odd_gen.csv", 'SELECT * REPLACE ("the doc"::JSON AS "the doc") FROM "odd gen"'),
                                ("p", "p.csv", "SELECT * FROM p")):
        quiet(src, f"COPY ({select}) TO {literal(str(whole / file))} (FORMAT csv, DELIMITER '\t', NULLSTR 'NULL')")
    run(src, "stop")
    if (whole / "o.csv").read_text().splitlines()[0] != "id\tdoc\tqty\tdoubled\tnote":
        bad("the file written whole is not the one this test means to restore", (whole / "o.csv").read_text())
    whole_dst = work / "gen_whole.duckdb"
    restored = run(whole_dst, "restore", whole, expect=None)
    same = diff(src, whole_dst, tables) if restored.returncode == 0 else restored.stderr
    if not same:
        ok("a file that holds the generated columns restores too, without them")
    else:
        bad("a file that holds the generated columns did not restore", same)


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
        variant_as_json(work)
        generated_columns(work)
        check("generated columns again, through --format parquet", "gen_pq",
              GENERATED_BUILD, work, fmt="parquet")
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
