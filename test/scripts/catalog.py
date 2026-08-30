#!/usr/bin/env python3
"""catalog.py — GET /catalog: the whole schema in one call, in one contract.

Runs its own server, because it needs fixture databases the shared one does
not have: one with the shapes a migration differ cares about — a foreign key,
a compound of schemas, a sequence-backed primary key, a unique index and a
plain one, uniqueness declared inline and table-level — and one with nothing
in it at all.

  test/scripts/catalog.py [--keep]

The endpoint exists so that clients stop parsing constraint prose and stop
caring which DuckDB is linked, so the assertions here are about the contract:
the foreign key arrives structurally, the ordering never moves, and two calls
answer byte-identical documents.
"""

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
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
TOKEN = "catalog-suite-token"

passed = 0
failed = []


def ok(name, detail=""):
    global passed
    passed += 1
    print(f"  \033[32m✓\033[0m {name}" + (f" \033[2m{detail}\033[0m" if detail else ""))


def bad(name, detail):
    failed.append(name)
    print(f"  \033[31m✗\033[0m {name}\n     \033[2m{detail}\033[0m")


def eq(name, expected, actual):
    if expected == actual:
        ok(name)
    else:
        bad(name, f"expected [{expected}], got [{actual}]")


def section(title):
    print(f"\n\033[1m{title}\033[0m")


# ---------------------------------------------------------------------------
# HTTP
# ---------------------------------------------------------------------------


def fetch(base, path, token=TOKEN):
    """Status, raw bytes, headers. Raw rather than parsed, because one of the
    assertions below is about the bytes themselves."""
    headers = {}
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(base + path, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read(), dict(e.headers)


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
            *os.environ.get("HARBOR_LAUNCHER", os.path.join(HERE, "target", "release", "harbor") + " serve").split(), db,
            "--port", str(port), "--token", TOKEN, "--workers", "2",
        ],
        stdout=log, stderr=log, stdin=subprocess.DEVNULL,
    )
    base = f"http://127.0.0.1:{port}"
    for _ in range(120):
        try:
            urllib.request.urlopen(base + "/ready", timeout=1).read()
            return proc, base, log
        except Exception:
            if proc.poll() is not None:
                break
            time.sleep(0.25)
    log.flush()
    print(open(db + ".log").read(), file=sys.stderr)
    raise SystemExit("catalog: the server never came up")


def stop_server(proc):
    if proc.poll() is None:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            proc.kill()


# ---------------------------------------------------------------------------


FIXTURE = """
CREATE SEQUENCE users_seq START 1;
CREATE TABLE users (
  id INTEGER PRIMARY KEY DEFAULT nextval('users_seq'),
  email VARCHAR NOT NULL,
  name VARCHAR
);
CREATE UNIQUE INDEX idx_users_email ON users(email);
CREATE TABLE posts (
  id INTEGER PRIMARY KEY,
  user_id INTEGER REFERENCES users(id),
  title VARCHAR
);
CREATE INDEX idx_posts_title ON posts(title);
INSERT INTO posts VALUES (1, NULL, 'ada'), (2, NULL, 'bo'), (3, NULL, 'cy');
CREATE TABLE awkward (
  "a b" INTEGER,
  "c'd" INTEGER,
  "é" INTEGER,
  plain INTEGER
);
CREATE INDEX idx_awk_space ON awkward("a b");
CREATE INDEX idx_awk_quote ON awkward("c'd");
CREATE INDEX idx_awk_unicode ON awkward("é");
CREATE INDEX idx_awk_expr ON awkward(lower(CAST(plain AS VARCHAR)));
CREATE TABLE memberships (
  org_id INTEGER,
  user_id INTEGER,
  via VARCHAR UNIQUE,
  UNIQUE (user_id, org_id)
);
CREATE SCHEMA app;
CREATE TABLE app.tags (id INTEGER PRIMARY KEY, label VARCHAR UNIQUE);
CREATE TABLE app.post_tags (
  tag_id INTEGER REFERENCES app.tags(id),
  note VARCHAR
);
CHECKPOINT;
"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    work = tempfile.mkdtemp(prefix="harbor-catalog-")
    db = os.path.join(work, "catalog.duckdb")
    empty = os.path.join(work, "empty.duckdb")
    subprocess.run(["duckdb", "-no-init", db, "-c", FIXTURE],
                   check=True, stdout=subprocess.DEVNULL)
    subprocess.run(["duckdb", "-no-init", empty, "-c", "CHECKPOINT;"],
                   check=True, stdout=subprocess.DEVNULL)

    proc, base, log = start_server(db, free_port())
    try:
        run_fixture(base)
    finally:
        stop_server(proc)
        log.close()

    proc, base, log = start_server(empty, free_port())
    try:
        run_empty(base)
    finally:
        stop_server(proc)
        log.close()
        if args.keep:
            print(f"\nkept: {work}")
        else:
            shutil.rmtree(work, ignore_errors=True)

    print()
    if failed:
        print(f"\033[31m{passed} passed, {len(failed)} failed\033[0m")
        for f in failed:
            print(f"  - {f}")
        return 1
    print(f"\033[32m{passed} passed, 0 failed\033[0m")
    return 0


def run_fixture(base):
    # -----------------------------------------------------------------------
    section("The door is locked")
    # -----------------------------------------------------------------------
    st, body, _ = fetch(base, "/catalog", token=None)
    eq("no token is a 401", 401, st)
    eq("with the usual envelope", "unauthorized", json.loads(body).get("code"))
    st, _, _ = fetch(base, "/catalog", token="not-the-token")
    eq("a wrong token is a 401 too", 401, st)

    # -----------------------------------------------------------------------
    section("One call, the whole schema")
    # -----------------------------------------------------------------------
    st, body, headers = fetch(base, "/catalog")
    eq("the catalog is a 200", 200, st)
    eq("as application/json", "application/json", headers.get("Content-Type"))
    doc = json.loads(body)
    eq("harborVersion is harbor's own", harbor_version(), doc.get("harborVersion"))
    duckdb = doc.get("duckdbVersion", "")
    ok("duckdbVersion comes from the engine", duckdb) if duckdb.startswith("v") else \
        bad("duckdbVersion comes from the engine", f"got [{duckdb}]")

    # The default expression is engine-reported text, so its exact spelling
    # belongs to the engine; that it names the sequence does not.
    users = next((t for t in doc["tables"] if t["name"] == "users"), {})
    default = (users.get("columns") or [{}])[0].get("default") or ""
    eq("the sequence-backed pk keeps its default", True, default.startswith("nextval("))
    for column in users.get("columns", []):
        column.pop("default", None)

    # The DDL is likewise the engine's own rendering — asserted for presence
    # and shape, then set aside so the structural expectations below stay
    # exact without transcribing engine prose.
    ddl = users.get("ddl") or ""
    eq("every table carries its own DDL", [],
       [t["name"] for t in doc["tables"] if not (t.get("ddl") or "").startswith("CREATE TABLE")])
    eq("and the DDL names its table", True, "users" in ddl)
    for table in doc["tables"]:
        table.pop("ddl", None)

    expected = [
        {"name": "post_tags", "schema": "app", "estimatedRows": 0,
         "columns": [
             {"name": "tag_id", "type": "INTEGER", "notNull": False, "default": None, "primary": False},
             {"name": "note", "type": "VARCHAR", "notNull": False, "default": None, "primary": False},
         ],
         "primaryKey": [],
         "uniqueConstraints": [],
         "indexes": [],
         "foreignKeys": [
             {"columns": ["tag_id"], "refTable": "tags", "refSchema": "app", "refColumns": ["id"]},
         ]},
        {"name": "tags", "schema": "app", "estimatedRows": 0,
         "columns": [
             {"name": "id", "type": "INTEGER", "notNull": True, "default": None, "primary": True},
             {"name": "label", "type": "VARCHAR", "notNull": False, "default": None, "primary": False},
         ],
         "primaryKey": ["id"],
         "uniqueConstraints": [
             {"columns": ["label"]},
         ],
         "indexes": [],
         "foreignKeys": []},
        {"name": "memberships", "schema": "main", "estimatedRows": 0,
         "columns": [
             {"name": "org_id", "type": "INTEGER", "notNull": False, "default": None, "primary": False},
             {"name": "user_id", "type": "INTEGER", "notNull": False, "default": None, "primary": False},
             {"name": "via", "type": "VARCHAR", "notNull": False, "default": None, "primary": False},
         ],
         "primaryKey": [],
         "uniqueConstraints": [
             {"columns": ["user_id", "org_id"]},
             {"columns": ["via"]},
         ],
         "indexes": [],
         "foreignKeys": []},
        {"name": "posts", "schema": "main", "estimatedRows": 3,
         "columns": [
             {"name": "id", "type": "INTEGER", "notNull": True, "default": None, "primary": True},
             {"name": "user_id", "type": "INTEGER", "notNull": False, "default": None, "primary": False},
             {"name": "title", "type": "VARCHAR", "notNull": False, "default": None, "primary": False},
         ],
         "primaryKey": ["id"],
         "uniqueConstraints": [],
         "indexes": [
             {"name": "idx_posts_title", "columns": ["title"], "expressions": [], "unique": False},
         ],
         "foreignKeys": [
             {"columns": ["user_id"], "refTable": "users", "refSchema": "main", "refColumns": ["id"]},
         ]},
        {"name": "users", "schema": "main", "estimatedRows": 0,
         "columns": [
             {"name": "id", "type": "INTEGER", "notNull": True, "primary": True},
             {"name": "email", "type": "VARCHAR", "notNull": True, "primary": False},
             {"name": "name", "type": "VARCHAR", "notNull": False, "primary": False},
         ],
         "primaryKey": ["id"],
         "uniqueConstraints": [],
         "indexes": [
             {"name": "idx_users_email", "columns": ["email"], "expressions": [], "unique": True},
         ],
         "foreignKeys": []},
    ]
    for want in expected:
        got = next((t for t in doc["tables"] if (t["schema"], t["name"]) == (want["schema"], want["name"])), None)
        eq(f"{want['schema']}.{want['name']} answers its whole shape", want, got)
    eq("tables are ordered by (schema, name)",
       [("app", "post_tags"), ("app", "tags"), ("main", "awkward"), ("main", "memberships"),
        ("main", "posts"), ("main", "users")],
       [(t["schema"], t["name"]) for t in doc["tables"]])
    eq("the sequence is there, with its start", [{"name": "users_seq", "start": 1}], doc["sequences"])

    # The point of the endpoint: the foreign key arrived from structured
    # catalog fields — table, schema and both column lists as JSON, with no
    # constraint prose anywhere in the document to parse.
    fk = next(t for t in doc["tables"] if t["name"] == "posts")["foreignKeys"][0]
    eq("the fk names its referenced table structurally", ("users", "main", ["id"]),
       (fk["refTable"], fk["refSchema"], fk["refColumns"]))

    # -----------------------------------------------------------------------
    section("Uniqueness declared in the table, not just indexed on")
    # -----------------------------------------------------------------------
    # UNIQUE constraints are not indexes to duckdb_indexes(), so before this
    # field existed a hand-written `CREATE TABLE (... UNIQUE)` schema read as
    # having no uniqueness at all. Both spellings arrive structurally: inline
    # on a column and table-level across two.
    tables = {t["name"]: t for t in doc["tables"]}
    eq("an inline UNIQUE arrives as a constraint",
       [{"columns": ["label"]}], tables["tags"]["uniqueConstraints"])
    # The composite keeps its declaration order — (user_id, org_id), which no
    # alphabetizer would produce — while the list is sorted by column lists:
    # the catalog stored the inline `via` constraint first, and it emits last.
    eq("a composite UNIQUE keeps declaration order, in a sorted list",
       [{"columns": ["user_id", "org_id"]}, {"columns": ["via"]}],
       tables["memberships"]["uniqueConstraints"])
    eq("a table whose uniqueness is an index keeps an empty list",
       [], tables["users"]["uniqueConstraints"])
    eq("primary keys never masquerade as unique constraints", [],
       [u for t in doc["tables"] for u in t["uniqueConstraints"]
        if set(u["columns"]) & set(t["primaryKey"])])

    # -----------------------------------------------------------------------
    section("Index columns are column names, and expressions are not columns")
    # -----------------------------------------------------------------------
    # The point of this field is to be joined against columns[].name. DuckDB
    # renders anything beyond a plain identifier single-quoted and keeps the
    # double quotes, so `"a b"` used to arrive quoted and matched nothing —
    # three of four names on this table. Every entry now joins, and a computed
    # index is reported separately instead of posing as a column with an
    # extraordinary name.
    awk = tables["awkward"]
    names = {c["name"] for c in awk["columns"]}
    by_name = {i["name"]: i for i in awk["indexes"]}
    eq("a quoted identifier joins to its column", ["a b"], by_name["idx_awk_space"]["columns"])
    eq("an embedded quote survives the round trip", ["c'd"], by_name["idx_awk_quote"]["columns"])
    eq("a non-ASCII identifier joins too", ["é"], by_name["idx_awk_unicode"]["columns"])
    eq("every index column is a real column", [],
       [c for i in awk["indexes"] for c in i["columns"] if c not in names])
    eq("a computed index claims no columns", [], by_name["idx_awk_expr"]["columns"])
    eq("and reports its expression instead", 1, len(by_name["idx_awk_expr"]["expressions"]))
    eq("a plain index has no expressions", [], by_name["idx_awk_space"]["expressions"])

    # -----------------------------------------------------------------------
    section("Sizes are exact bytes, from the server's own stat")
    # -----------------------------------------------------------------------
    # The engine pretty-prints its sizes ("1.2 MiB"); the contract never
    # relays that. The server stats the file it serves, so clients render
    # their own units from real bytes — and a checkpointed database has a
    # WAL of exactly 0 bytes, not an unknown.
    eq("databaseSizeBytes is a positive integer", True,
       isinstance(doc.get("databaseSizeBytes"), int) and doc["databaseSizeBytes"] > 0)
    eq("walSizeBytes is 0 after CHECKPOINT", 0, doc.get("walSizeBytes"))

    # -----------------------------------------------------------------------
    section("Two calls, one answer")
    # -----------------------------------------------------------------------
    again = fetch(base, "/catalog")[1]
    eq("a stable database answers byte-identical documents", body, again)


def run_empty(base):
    # -----------------------------------------------------------------------
    section("An empty database is empty, not an error")
    # -----------------------------------------------------------------------
    st, body, _ = fetch(base, "/catalog")
    eq("still a 200", 200, st)
    doc = json.loads(body)
    eq("no tables", [], doc.get("tables"))
    eq("no sequences", [], doc.get("sequences"))
    eq("but its file still has a size", True,
       isinstance(doc.get("databaseSizeBytes"), int) and doc["databaseSizeBytes"] > 0)
    eq("but the versions still say who answered", True,
       bool(doc.get("harborVersion")) and bool(doc.get("duckdbVersion")))
    eq("and it too answers byte-identical documents", body, fetch(base, "/catalog")[1])


def harbor_version():
    """The crate version, read from the one place it is written: the harbor crate."""
    crate = os.path.join(HERE, "crates", "harbor")
    with open(os.path.join(crate, "Cargo.toml")) as f:
        for line in f:
            if line.startswith("version"):
                return line.split('"')[1]
    return ""


if __name__ == "__main__":
    sys.exit(main())
