#!/usr/bin/env python3
"""
braces.py — check brace expansion against the engine's own parser.

    test/scripts/braces.py                      # the suite's run, a few seconds
    test/scripts/braces.py --count 100000       # a long run
    test/scripts/braces.py --seed 7             # reproduce a failure

Each case is built twice by one grammar, with no lexer: once with brace
groups, as a client writes it, and once as the expansion it stands for,
spelled out. Harbor expands the first (`unbrace::tests::expand_a_file`, the
lib's unit-test binary run as a filter), and the engine parses both
(`json_serialize_sql`). The two parse trees must be equal, positions aside.
The unit tests' fuzz checks the expansion against harbor's own lexer; this
checks it against the engine, which no reading of harbor's can stand in for.

The grammar: select lists of paths whose groups nest three deep, items
separated by commas, spaces, comments and the Unicode spaces the engine
strips, suffixes (`::VARCHAR`, `::DECIMAL(10,2)`, `->>'$.k'`, `[1]`), and
beside them struct literals, `MAP {…}`, lambdas, and strings and names that
hold braces. It runs in two modes, since the engine's Unicode-space pre-pass
misreads E-strings, backticks, dollar quotes and quotes in comments, and an
expansion that repeats one of those near a Unicode space is refused, rightly:

    A  Unicode spaces, and none of what the pre-pass misreads
    B  what the pre-pass misreads, and ASCII spaces only

and a third set of cases whose expansion the engine would read another way
than written (an `E` that comes to begin the word before a string, a dollar
quote that opens only after a brace): those must come back as they went in.

Every failure prints the seed. Exit 77 when there is no duckdb CLI to parse
with: the suite could not run, which is not a pass.
"""

import argparse
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parents[2]
SKIPPED = 77

NBSP, IDEO = " ", "　"
UNICODE_SPACES = [NBSP, IDEO, " ", " ", "﻿", "​"]


class Grammar:
    """Pairs of (braced, expanded) statements, built side by side."""

    def __init__(self, rng, mode):
        a, b = mode == "A", mode == "B"
        self.r = rng
        self.spaces = [" ", "  ", "\t", "\n", "\x0b", "\x0c", "\r\n"] + (UNICODE_SPACES if a else [])
        self.comments = ["/* c */", "/* {a,b} */", "/* /* nest */ */", "-- {x,y} \n", "-- c\r"]
        self.names = ["a", "b", "c_1", "é", "naïve", "x1", '"q{x}"', '"it\'\'s"', '"a b"', '"e"',
                      "a::DECIMAL(9,3)", "b::STRUCT(k INT, v VARCHAR(4))"]
        self.suffixes = ["", "", "", "::VARCHAR", "->>'$.k'", "->>'{a,b}'", "[1]", "::INT[]",
                         "::DECIMAL(10,2)", "::VARCHAR(20)", "::STRUCT(k INT, v DECIMAL(4,1))", "::DECIMAL(4,1)[]"]
        self.joins = [",", ", ", " ,", " ", "\n  ", "\x0b", " /* , */ ", " -- }\n", ",\x0c"]
        self.leads = ["", "", "\n  ", " "]
        self.plain = ["'{a,b}'", "{'k': 1, 'j': {'m': 'n'}}", '"weird{n}"', "1", "{'a': [1,2]}", "'don''t'",
                      "MAP {'k': 'v'}", "{'k': 'v'}.k", "[1,2][1]", "list_transform([1], lambda x: x)",
                      "1.5::DECIMAL(10,2)", "CAST(x AS DECIMAL(10,2))"]
        if a:
            self.suffixes += ["->'x" + NBSP + "y'"]
            self.joins += ["," + NBSP, "," + IDEO, IDEO]
            self.leads += [NBSP]
            self.plain += ["'x" + NBSP + "y'"]
        if b:
            self.comments += ["/* don't */", "-- ' {a}\n", "/*'*/"]
            self.names += ["`bt`", "`it's`"]
            self.suffixes += ["->>E'\\'{a}'", "->>e'\\t'", "->>$$x$$"]
            self.joins += [" /*'*/ ", " -- '\n"]
            self.leads += ["\t"]
            self.plain += ["E'\\'{c,d}'", "$$ {a,b} $$", "$t$ x.{a} $t$", "`a{b}`", "e'a\\tb'", "/*'*/ 1", "E'\\\\'"]

    def sep(self):
        s = self.r.choice(self.spaces)
        if self.r.random() < 0.3:
            s += self.r.choice(self.comments) + self.r.choice(self.spaces[:5])
        return s

    def group(self, depth=0):
        r = self.r
        braced, expanded = [], []
        for _ in range(r.randint(1, 4)):
            if depth < 2 and r.random() < 0.25:
                prefix = r.choice(self.names[:10]) + "."
                gb, ge = self.group(depth + 1)
                braced.append(prefix + gb)
                expanded.extend(prefix + x for x in ge)
            else:
                name = r.choice(self.names)
                braced.append(name)
                expanded.append(name)
        body = braced[0] + "".join(r.choice(self.joins) + item for item in braced[1:])
        if r.random() < 0.2:
            body += ","
        return "{" + r.choice(self.leads) + body + r.choice(["", "", "\n", " "]) + "}", expanded

    def term(self):
        r = self.r
        roll = r.random()
        if roll < 0.55:
            base = r.choice(["r", '"my col"', "t.r", "r.p"]) + "."
            gb, ge = self.group()
            suffix = r.choice(self.suffixes)
            return base + gb + suffix, ", ".join(base + x + suffix for x in ge)
        if roll < 0.6:
            gb, ge = self.group()
            return "coalesce(r." + gb + ")", "coalesce(" + ", ".join("r." + x for x in ge) + ")"
        if roll < 0.63:
            return "{a,b}_x", "a_x, b_x"
        if roll < 0.66:
            return "x_{1,2}", "x_1, x_2"
        p = r.choice(self.plain)
        return p, p

    def statement(self, extra=None):
        """A statement, and the expansion it stands for. An `extra` term is
        set among the others as written."""
        r = self.r
        terms = [self.term() for _ in range(r.randint(1, 5))]
        if extra:
            terms.insert(r.randint(0, len(terms)), (extra, extra))
        braced = expanded = "SELECT" + self.sep()
        for k, (tb, te) in enumerate(terms):
            if k:
                s = "," + self.sep()
                braced, expanded = braced + s, expanded + s
            braced, expanded = braced + tb, expanded + te
        tail = self.sep() + "FROM t" + r.choice(["", " WHERE s = '{x,y}'", " -- end {a}",
                                                 self.sep() + "WHERE s = 'a b'"])
        return braced + tail, expanded + tail


# Statements the engine would read another way once expanded: each goes out
# exactly as it came, whatever surrounds it.
MISREAD = [
    "{E}'a\\tb'", "{e}'a\\tb'", "t.{d,e}'a\\tb'", "t.{d,E}'x'", "{x,y}e'a\\tb'", "{x}E'\\'''{a,b}'",
    "r.{a,b}$$x.{c,d}$$", "{'a' x}'b'", "{a,b}.E'\\'', 'x" + NBSP + "y'",
]


def misread(rng):
    return Grammar(rng, "A").statement(rng.choice(MISREAD))[0]


def duckdb_cli():
    lib = os.environ.get("HARBOR_LIBDUCKDB")
    for candidate in ([Path(lib).parent / "duckdb"] if lib else []) + [Path.home() / ".duckdb/cli/2.0.0/duckdb"]:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    return shutil.which("duckdb")


def unit_test_binary():
    """The harbor lib's unit-test binary, built with check.sh's unit flags so
    that after its unit suite nothing is rebuilt."""
    out = subprocess.run(
        ["cargo", "test", "--release", "--workspace", "--all-features", "--no-run", "--message-format=json"],
        cwd=HERE, capture_output=True, text=True,
    )
    for line in out.stdout.splitlines():
        try:
            m = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (m.get("reason") == "compiler-artifact" and m["target"]["name"] == "harbor"
                and m["target"]["kind"] == ["lib"] and m["profile"]["test"] and m.get("executable")):
            return m["executable"]
    sys.stderr.write(out.stderr[-4000:])
    raise SystemExit("braces: no unit-test binary for the harbor lib")


def expand(binary, statements, work):
    src, dst = work / "in", work / "out"
    src.write_bytes(b"".join(s.encode() + b"\0" for s in statements))
    env = dict(os.environ, UNBRACE_IN=str(src), UNBRACE_OUT=str(dst))
    subprocess.run([binary, "unbrace::tests::expand_a_file", "--exact", "--ignored", "--quiet"],
                   env=env, check=True, stdout=subprocess.DEVNULL)
    records = dst.read_bytes().split(b"\0")[:-1]
    assert len(records) == len(statements), (len(records), len(statements))
    return [(r[:1].decode(), r[1:].decode()) for r in records]


def parse(cli, pairs, work):
    """The engine's parse of each (got, want), by index."""
    src, dst = work / "pairs.jsonl", work / "trees.jsonl"
    with open(src, "w") as f:
        for i, (got, want) in enumerate(pairs):
            f.write(json.dumps({"i": i, "got": got, "want": want}) + "\n")
    sql = ("COPY (SELECT i, json_serialize_sql(got) AS got, json_serialize_sql(want) AS want "
           "FROM read_json('%s', columns = {'i': 'INTEGER', 'got': 'VARCHAR', 'want': 'VARCHAR'}) ORDER BY i) "
           "TO '%s' (FORMAT json)" % (src, dst))
    # The CLI's notes, such as its agent mode's, go to stderr, and are shown
    # only if it fails.
    done = subprocess.run([cli, "-no-init", "-bail", ":memory:", "-c", sql], capture_output=True, text=True)
    if done.returncode != 0:
        sys.stderr.write(done.stdout + done.stderr)
        raise SystemExit("braces: the duckdb CLI could not parse the cases")
    trees = [json.loads(line) for line in open(dst)]
    return [(t["got"], t["want"]) for t in trees]


def positionless(tree):
    if isinstance(tree, dict):
        return {k: positionless(v) for k, v in tree.items() if k not in ("query_location", "query_location_length")}
    if isinstance(tree, list):
        return [positionless(v) for v in tree]
    return tree


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--count", type=int, default=3000, help="cases per mode, and a tenth as many misread ones")
    ap.add_argument("--seed", type=int, default=None)
    args = ap.parse_args()
    seed = args.seed if args.seed is not None else random.randrange(2**31)
    rng = random.Random(seed)

    cli = duckdb_cli()
    if not cli:
        print("braces: no duckdb CLI beside HARBOR_LIBDUCKDB, in ~/.duckdb/cli/2.0.0 or on PATH")
        return SKIPPED
    binary = unit_test_binary()

    cases = []  # (mode, braced, want)
    for mode in ("A", "B"):
        g = Grammar(rng, mode)
        cases += [(mode,) + g.statement() for _ in range(args.count)]
    cases += [("misread", s, s) for s in (misread(rng) for _ in range(max(1, args.count // 10)))]

    failures, counts = [], {}
    with tempfile.TemporaryDirectory(prefix="hb-braces-", dir="/tmp") as work:
        work = Path(work)
        results = expand(binary, [c[1] for c in cases], work)
        trees = parse(cli, [(out, want) for (_, out), (_, _, want) in zip(results, cases)], work)
        for (mode, braced, want), (tag, out), (got_tree, want_tree) in zip(cases, results, trees):
            counts[(mode, tag)] = counts.get((mode, tag), 0) + 1
            if mode == "misread" or braced == want:
                ok = tag == "S" and out == braced
            elif tag != "X":
                ok = False  # nothing in modes A and B needs refusing
            elif want_tree.get("error"):
                ok = False  # the grammar wrote what the engine cannot parse
            else:
                ok = positionless(got_tree) == positionless(want_tree)
            if not ok:
                failures.append((mode, tag, braced, out, want))

    total = len(cases)
    print("braces: %d cases, seed %d, engine parse via %s" % (total, seed, cli))
    for (mode, tag), n in sorted(counts.items()):
        print("  %-8s %s %6d" % (mode, {"X": "expanded", "S": "as it came", "R": "refused"}[tag], n))
    if failures:
        print("\n%d of %d wrong (rerun with --seed %d), among them:" % (len(failures), total, seed))
        for mode, tag, braced, out, want in failures[:8]:
            print("  [%s %s]\n    sent  %r\n    got   %r\n    want  %r" % (mode, tag, braced, out, want))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
