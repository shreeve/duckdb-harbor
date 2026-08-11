#!/usr/bin/env python3
"""
types.py — fail when a DuckDB type is not exercised by any suite.

    test/scripts/types.py

Every other suite checks that the types it knows about are encoded correctly.
None of them notices a type nobody thought to write a case for, and that is not
a hypothetical gap: BIGNUM shipped emitting base64 of DuckDB's private storage,
and TIME_NS shipped panicking the executor thread and returning 200 with an
empty body. Both were in no suite, so nothing failed.

This closes the loop by working from DuckDB's own type list rather than from
ours. It reads the LogicalTypeId enum out of the duckdb-rs source in the cargo
registry — the same list the encoder dispatches on — and requires every variant
to be either covered by a corpus case or listed in EXCUSED with a reason.

The consequence worth having: when a DuckDB upgrade adds a type, this fails on
the next run, before the type reaches anyone's data.
"""

import glob
import re
import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import corpus  # noqa: E402


# Variants that cannot appear as a result column, or cannot be produced here.
# Every entry needs a reason: an excuse list nobody has to justify is just a
# way of turning a failing test green.
EXCUSED = {
    "Invalid": "not a type — the enum's zero value, used for 'unset'",
    "Any": "a binder-internal wildcard for function signatures; no value has it",
    "IntegerLiteral": "an internal type for an un-coerced literal during binding",
    "StringLiteral": "an internal type for an un-coerced literal during binding",
    "Unsupported": "duckdb-rs's own catch-all for ids it does not model",
    "Geometry": "requires the spatial extension, which harbor does not depend on. "
                "Worth adding a case if spatial is ever a dependency.",
}

# How a variant is spelled where a test would produce it. Only needed where the
# Rust name and the SQL name differ, or where the type is written as a suffix.
SQL_SPELLING = {
    "Bignum": ["VARINT", "BIGNUM"],
    "Varchar": ["VARCHAR", "'"],
    "Tinyint": ["TINYINT"],
    "Smallint": ["SMALLINT"],
    "UTinyint": ["UTINYINT"],
    "USmallint": ["USMALLINT"],
    "UInteger": ["UINTEGER"],
    "UBigint": ["UBIGINT"],
    "UHugeint": ["UHUGEINT"],
    "TimestampTZ": ["TIMESTAMPTZ", "TIMESTAMP WITH TIME ZONE"],
    "TimeTZ": ["TIMETZ", "TIME WITH TIME ZONE"],
    "TimestampS": ["TIMESTAMP_S"],
    "TimestampMs": ["TIMESTAMP_MS"],
    "TimestampNs": ["TIMESTAMP_NS"],
    "TimeNs": ["TIME_NS"],
    "SqlNull": ["NULL"],
    # ARRAY is written as a length suffix, never as the word.
    "Array": ["[1]", "[2]", "[3]", "[4]", "[5]"],
    "List": ["[]", "LIST"],
    "Boolean": ["BOOLEAN", "TRUE", "FALSE"],
}


def duckdb_rs_types():
    """The LogicalTypeId variants duckdb-rs models, from its own source."""
    matches = glob.glob(
        "%s/.cargo/registry/src/*/duckdb-*/src/core/logical_type.rs" % __import__("os").path.expanduser("~")
    )
    if not matches:
        return None
    text = open(sorted(matches)[-1], encoding="utf-8").read()
    return sorted(set(re.findall(r"^\s+([A-Za-z]+) = DUCKDB_TYPE", text, re.M)))


def main():
    variants = duckdb_rs_types()
    if variants is None:
        print("type coverage: duckdb-rs not in the cargo registry — skipped")
        return 0

    corpus_sql = " ".join(sql for _, _, sql in corpus.all_queries()).upper()

    missing = []
    for name in variants:
        if name in EXCUSED:
            continue
        needles = SQL_SPELLING.get(name, [name.upper()])
        if not any(n.upper() in corpus_sql for n in needles):
            missing.append(name)

    covered = len(variants) - len(missing) - len([v for v in variants if v in EXCUSED])
    print("type coverage: %d of %d DuckDB types exercised, %d excused"
          % (covered, len(variants), len([v for v in variants if v in EXCUSED])))

    if missing:
        print("\nnot exercised by any corpus case:")
        for name in missing:
            print("  %s" % name)
        print("\nAdd a case to test/scripts/corpus.py, or an entry to EXCUSED here with\n"
              "the reason it cannot be produced.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
