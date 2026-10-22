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
# nothing did, so sockets and logs landed in the operator's real
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
LAUNCHER = os.environ.get("HARBOR_LAUNCHER", os.path.join(HERE, "target", "release", "harbor")).split()
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


def fetch(base, path):
    """Status, raw bytes, headers. Raw rather than parsed, because one of the
    assertions below is about the bytes themselves."""
    req = urllib.request.Request(base + path)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read(), dict(e.headers)


def sql(base, text):
    """Run one statement; None when it succeeded, else the error message."""
    body = json.dumps({"sql": text}).encode()
    req = urllib.request.Request(base + "/sql", data=body, method="POST",
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            for line in r.read().decode().splitlines():
                event = json.loads(line)
                if event.get("type") == "error":
                    return event.get("message", "")
            return None
    except urllib.error.HTTPError as e:
        return json.loads(e.read() or b"{}").get("message", f"HTTP {e.code}")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def start_server(db, port, workers=2):
    log = open(db + ".log", "w")
    proc = subprocess.Popen(
        [*LAUNCHER, db, "start", "--port", str(port), "--workers", str(workers)],
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
  name VARCHAR,
  slug VARCHAR GENERATED ALWAYS AS (lower(name))
);
CREATE UNIQUE INDEX idx_users_email ON users(email);
CREATE TABLE posts (
  id INTEGER PRIMARY KEY,
  user_id INTEGER REFERENCES users(id),
  title VARCHAR
);
CREATE INDEX idx_posts_title ON posts(title);
INSERT INTO posts VALUES
  (1, NULL, 'ada'), (2, NULL, 'bo'), (3, NULL, 'cy'), (4, NULL, 'deleted');
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
CREATE SCHEMA "odd schema";
CREATE TABLE "odd schema"."say ""hi"" now" (i INTEGER);
INSERT INTO "odd schema"."say ""hi"" now" VALUES (1), (2);
CHECKPOINT;
DELETE FROM posts WHERE id = 4;
"""


# Views over files, one of whose files is removed before the server starts,
# and a view over nothing but SQL in a schema of its own.
VIEWS_FIXTURE = """
CREATE TABLE t (i INTEGER);
CREATE VIEW sales AS SELECT * FROM read_csv('{work}/sales.csv');
CREATE VIEW gone AS SELECT * FROM read_csv('{work}/gone.csv');
CREATE SCHEMA s2;
CREATE VIEW s2.calc AS SELECT 1 AS a, 2.5::DECIMAL(4,1) AS b;
CHECKPOINT;
"""

# A DuckDB file attached read-only beside it.
OTHER_FIXTURE = """
CREATE TABLE items (id INTEGER PRIMARY KEY, label VARCHAR);
INSERT INTO items VALUES (1, 'a'), (2, 'b'), (3, 'c');
CREATE VIEW labels AS SELECT label FROM items;
CHECKPOINT;
"""


def write_csv(path, text):
    with open(path, "w") as f:
        f.write(text)


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

    views = os.path.join(work, "views.duckdb")
    write_csv(os.path.join(work, "sales.csv"), "id,name\n1,ann\n2,bob\n")
    write_csv(os.path.join(work, "gone.csv"), "id\n1\n")
    subprocess.run(["duckdb", "-no-init", views, "-c", VIEWS_FIXTURE.format(work=work)],
                   check=True, stdout=subprocess.DEVNULL)
    subprocess.run(["duckdb", "-no-init", os.path.join(work, "other.duckdb"), "-c", OTHER_FIXTURE],
                   check=True, stdout=subprocess.DEVNULL)
    os.remove(os.path.join(work, "gone.csv"))
    proc, base, log = start_server(views, free_port())
    try:
        sqlite_ok = run_views(base, work)
        run_race(base, work, sqlite_ok)
    finally:
        stop_server(proc)
        log.close()
    busy = os.path.join(work, "busy.duckdb")
    # Four workers: a long statement, an ATTACH that waits on it, and one more.
    proc, base, log = start_server(busy, free_port(), workers=4)
    try:
        if sqlite_ok:
            run_busy(base, work)
            run_unlisted(base, work)
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
    generated = next((c for c in users.get("columns", []) if c["name"] == "slug"), {})
    eq("generated columns are identified structurally", True, generated.get("generated"))
    expression = generated.get("generationExpression") or ""
    eq("their engine-rendered expression is carried separately", True,
       "lower" in expression and "name" in expression)
    eq("every column declares whether DuckDB generates it", [],
       [c["name"] for t in doc["tables"] for c in t.get("columns", [])
        if not isinstance(c.get("generated"), bool)])
    for column in users.get("columns", []):
        column.pop("default", None)
    for table in doc["tables"]:
        for column in table.get("columns", []):
            column.pop("generated", None)
            column.pop("generationExpression", None)

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
        {"name": "post_tags", "schema": "app", "rowCount": 0,
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
        {"name": "tags", "schema": "app", "rowCount": 0,
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
        {"name": "memberships", "schema": "main", "rowCount": 0,
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
        {"name": "posts", "schema": "main", "rowCount": 3,
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
        {"name": "users", "schema": "main", "rowCount": 0,
         "columns": [
             {"name": "id", "type": "INTEGER", "notNull": True, "primary": True},
             {"name": "email", "type": "VARCHAR", "notNull": True, "primary": False},
             {"name": "name", "type": "VARCHAR", "notNull": False, "primary": False},
             {"name": "slug", "type": "VARCHAR", "notNull": False, "primary": False},
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
        ("main", "posts"), ("main", "users"), ("odd schema", 'say "hi" now')],
       [(t["schema"], t["name"]) for t in doc["tables"]])
    eq("every full table has one exact rowCount", [],
       [t["name"] for t in doc["tables"] if not isinstance(t.get("rowCount"), int)])
    eq("physical deletes do not inflate rowCount", 3,
       next(t for t in doc["tables"] if t["name"] == "posts")["rowCount"])
    eq("quoted relations are counted exactly", 2,
       next(t for t in doc["tables"] if t["name"] == 'say "hi" now')["rowCount"])
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
    # UNIQUE constraints are not indexes to duckdb_indexes(), so without this
    # field a hand-written `CREATE TABLE (... UNIQUE)` schema reads as having
    # no uniqueness at all. Both spellings arrive structurally: inline on a
    # column and table-level across two.
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
    # double quotes, so taken as DuckDB gives it `"a b"` arrives quoted and
    # matches nothing — three of four names on this table. Every entry joins,
    # and a computed index is reported separately instead of posing as a
    # column with an extraordinary name.
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
    section("The lite style: what exists, not how big or how it is built")
    # -----------------------------------------------------------------------
    # Same document family at lower fidelity: the versions, the sizes, and
    # each table as name/schema — nothing else, so a client drawing a
    # database list pays neither the count/shape queries nor the DDL bytes. A
    # field a style omits is absent, never differently shaped.
    st, lbody, _ = fetch(base, "/catalog?style=lite")
    eq("lite is a 200", 200, st)
    lite = json.loads(lbody)
    eq("its keys are the inventory alone",
       ["harborVersion", "duckdbVersion", "databaseSizeBytes", "walSizeBytes", "tables", "views", "attached"],
       list(lite.keys()))
    eq("a lite table is name and schema — nothing else",
       [], [t for t in lite["tables"] if sorted(t.keys()) != ["name", "schema"]])
    eq("the same tables in the same order as the full document",
       [(t["schema"], t["name"]) for t in doc["tables"]],
       [(t["schema"], t["name"]) for t in lite["tables"]])
    eq("and the same sizes", (doc["databaseSizeBytes"], doc["walSizeBytes"]),
       (lite["databaseSizeBytes"], lite["walSizeBytes"]))
    eq("lite too answers byte-identical documents", lbody, fetch(base, "/catalog?style=lite")[1])
    eq("style=full is the default document, byte for byte", body,
       fetch(base, "/catalog?style=full")[1])
    st, ebody, _ = fetch(base, "/catalog?style=fancy")
    eq("an unknown style is refused loudly", 400, st)
    eq("with the usual envelope", "bad_request", json.loads(ebody).get("code"))
    eq("an unknown parameter passes — that tolerance is version grace", 200,
       fetch(base, "/catalog?future=1")[0])

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
    eq("no views", [], doc.get("views"))
    eq("nothing attached", [], doc.get("attached"))
    eq("but its file still has a size", True,
       isinstance(doc.get("databaseSizeBytes"), int) and doc["databaseSizeBytes"] > 0)
    eq("but the versions still say who answered", True,
       bool(doc.get("harborVersion")) and bool(doc.get("duckdbVersion")))
    eq("and it too answers byte-identical documents", body, fetch(base, "/catalog")[1])


def run_views(base, work):
    # -----------------------------------------------------------------------
    section("Views: their columns as they read, no count, and why one does not")
    # -----------------------------------------------------------------------
    st, body, _ = fetch(base, "/catalog")
    eq("a catalog with views is a 200", 200, st)
    doc = json.loads(body)
    eq("tables are still tables alone", [("main", "t")], [(t["schema"], t["name"]) for t in doc["tables"]])
    eq("views are ordered by (schema, name)",
       [("main", "gone"), ("main", "sales"), ("s2", "calc")],
       [(v["schema"], v["name"]) for v in doc["views"]])
    views = {v["name"]: v for v in doc["views"]}
    eq("a view's keys", ["name", "schema", "columns", "error", "ddl"], list(views["sales"].keys()))
    cols = lambda v: [(c["name"], c["type"]) for c in v["columns"]]
    eq("a view over a CSV has the CSV's columns", [("id", "BIGINT"), ("name", "VARCHAR")], cols(views["sales"]))
    eq("and no error", None, views["sales"]["error"])
    eq("and the engine's DDL", True, (views["sales"]["ddl"] or "").startswith("CREATE VIEW"))
    eq("a view over SQL has its types", [("a", "INTEGER"), ("b", "DECIMAL(4,1)")], cols(views["calc"]))
    eq("a view over a missing file has no columns", [], views["gone"]["columns"])
    error = views["gone"]["error"] or ""
    eq("and says why, naming the file", True, "gone.csv" in error)
    eq("in the engine's words, without a LINE excerpt", False, "LINE" in error)
    eq("a stable database answers byte-identical documents", body, fetch(base, "/catalog")[1])

    write_csv(os.path.join(work, "sales.csv"), "id,name,amount\n1,ann,2.5\n")
    write_csv(os.path.join(work, "gone.csv"), "id\n1\n")
    views = {v["name"]: v for v in json.loads(fetch(base, "/catalog")[1])["views"]}
    eq("a file that changed shape shows its shape",
       [("id", "BIGINT"), ("name", "VARCHAR"), ("amount", "DOUBLE")], cols(views["sales"]))
    eq("a file that came back reads again", (None, [("id", "BIGINT")]),
       (views["gone"]["error"], cols(views["gone"])))

    if hasattr(os, "mkfifo"):
        # A read of a pipe waits on its writer, out of the engine's reach:
        # the catalog does not read one, and waits on any view's bind only so
        # long, so no catalog read holds a worker on a file.
        import threading
        pipe = os.path.join(work, "pipe.csv")
        write_csv(pipe, "id\n1\n")
        sql(base, f"CREATE VIEW pipe AS SELECT * FROM read_csv('{pipe}')")
        sql(base, f"CREATE VIEW pipe_where AS SELECT * FROM read_csv('{pipe}') WHERE id > 0")
        os.remove(pipe)
        os.mkfifo(pipe)
        began = time.time()
        st, body, _ = fetch(base, "/catalog")
        took = time.time() - began
        views = {v["name"]: v for v in json.loads(body).get("views", [])} if st == 200 else {}
        eq("a catalog with a view over a pipe answers", 200, st)
        eq("the view over the pipe alone is not bound, and says why",
           (True, []), ("a pipe, a socket or a device" in (views.get("pipe", {}).get("error") or ""),
                        views.get("pipe", {}).get("columns")))
        eq("one of another shape is bound within its budget, and says so", True,
           "took more than 2 s" in (views.get("pipe_where", {}).get("error") or ""))
        eq("so the read takes seconds, not for ever", True, took < 8)
        eq("and the workers still answer", None, sql(base, "SELECT 42"))
        def release():
            with open(pipe, "w") as w:
                w.write("id\n1\n")
        threading.Thread(target=release, daemon=True).start()
        sql(base, "DROP VIEW pipe")
        sql(base, "DROP VIEW pipe_where")

    # -----------------------------------------------------------------------
    section("Attached databases: each its own catalog")
    # -----------------------------------------------------------------------
    other = os.path.join(work, "other.duckdb")
    eq("a DuckDB file attaches", None, sql(base, f"ATTACH '{other}' AS other (READ_ONLY)"))
    app = os.path.join(work, "app.sqlite")
    import sqlite3
    con = sqlite3.connect(app)
    con.executescript("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT);"
                      "INSERT INTO people (name) VALUES ('ann'), ('bob');"
                      "CREATE VIEW names AS SELECT name FROM people;")
    con.commit()
    con.close()
    # The sqlite extension installs on first use, from DuckDB's repository.
    # Offline, that is the one failure taken as a skip, and said so.
    refused = sql(base, f"ATTACH '{app}' AS app (TYPE sqlite)")
    sqlite_ok = refused is None
    if refused and any(w in refused for w in ("download", "HTTP", "Failed to", "extension")):
        print(f"  \033[33m-\033[0m SQLite skipped: the sqlite extension did not install \033[2m{refused.splitlines()[0]}\033[0m")
    elif refused:
        bad("a SQLite file attaches", refused)

    st, body, _ = fetch(base, "/catalog")
    eq("still a 200", 200, st)
    doc = json.loads(body)
    attached = {a["name"]: a for a in doc["attached"]}
    eq("attached databases by name, never system or temp",
       ["app", "other"] if sqlite_ok else ["other"], [a["name"] for a in doc["attached"]])
    eq("the served database's own lists hold only its own",
       ([("main", "t")], 3), ([(t["schema"], t["name"]) for t in doc["tables"]], len(doc["views"])))
    o = attached.get("other", {})
    eq("an attached database's keys",
       ["name", "path", "type", "readOnly", "error", "tables", "views"], list(o.keys()))
    # The engine reports a DuckDB file's path resolved (macOS's /var is
    # /private/var), so the two are compared as the same file.
    real = lambda p: os.path.realpath(p) if p else p
    eq("its path, type and read-only flag", (real(other), "duckdb", True),
       (real(o.get("path")), o.get("type"), o.get("readOnly")))
    items = (o.get("tables") or [{}])[0]
    eq("its tables in the full shape, counted exactly",
       ("items", 3, ["id"], [("id", "INTEGER"), ("label", "VARCHAR")]),
       (items.get("name"), items.get("rowCount"), items.get("primaryKey"),
        [(c["name"], c["type"]) for c in items.get("columns", [])]))
    eq("and its views", [("labels", [("label", "VARCHAR")], None)],
       [(v["name"], cols(v), v["error"]) for v in o.get("views", [])])
    if sqlite_ok:
        a = attached["app"]
        eq("a SQLite file is type sqlite, read-write", (app, "sqlite", False), (a["path"], a["type"], a["readOnly"]))
        people = (a["tables"] or [{}])[0]
        eq("its tables carry columns and keys, and no count",
           ("people", None, ["id"], ["id", "name"]),
           (people.get("name"), people.get("rowCount"), people.get("primaryKey"),
            [c["name"] for c in people.get("columns", [])]))
        eq("and its views", ["names"], [v["name"] for v in a["views"]])
    eq("two calls, one answer, attachments and all", body, fetch(base, "/catalog")[1])

    lite = json.loads(fetch(base, "/catalog?style=lite")[1])
    eq("lite names the views", [(v["schema"], v["name"]) for v in doc["views"]],
       [(v["schema"], v["name"]) for v in lite["views"]])
    eq("and each attached database with what it holds, by name",
       [(a["name"], a["type"], a["readOnly"], [t["name"] for t in a["tables"]], [v["name"] for v in a["views"]])
        for a in doc["attached"]],
       [(a["name"], a["type"], a["readOnly"], [t["name"] for t in a["tables"]], [v["name"] for v in a["views"]])
        for a in lite["attached"]])

    eq("detached, it is gone from the catalog", None, sql(base, "DETACH other"))
    eq("and the catalog says so", False, "other" in [a["name"] for a in json.loads(fetch(base, "/catalog")[1])["attached"]])
    if sqlite_ok:
        sql(base, "DETACH app")
    return sqlite_ok


def run_busy(base, work):
    # -----------------------------------------------------------------------
    section("An attached SQLite file that is busy, or gone for good")
    # -----------------------------------------------------------------------
    # The engine reads every attached database's catalog together, so one
    # SQLite file locked, or not SQLite any more, fails every listing. A
    # busy one stays attached, since the lock may be any connection's, the
    # server's own long write among them, and the catalog is answered as
    # the server last read it, the busy one saying why; with nothing read
    # yet, the read is refused, naming it. One gone for good is detached
    # and listed with why, and a client's is left for the client to attach.
    import sqlite3
    app = os.path.join(work, "busy.sqlite")
    con = sqlite3.connect(app)
    con.executescript("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO people (name) VALUES ('ann');")
    con.commit()
    con.close()
    eq("a table of its own", None, sql(base, "CREATE TABLE own (i INTEGER)"))
    eq("a SQLite file attaches, waiting two seconds on a lock", None,
       sql(base, f"ATTACH '{app}' AS app (TYPE sqlite, BUSY_TIMEOUT 2000)"))
    locker = sqlite3.connect(app, isolation_level=None)

    def lock():
        locker.execute("BEGIN EXCLUSIVE")

    def release():
        locker.execute("ROLLBACK")

    def attached_now():
        """None when app is attached, else why not."""
        return sql(base, "SELECT CASE WHEN count(*) = 0 THEN error('app is not attached') END "
                         "FROM duckdb_databases() WHERE database_name = 'app'")

    lock()
    st, body, _ = fetch(base, "/catalog")
    message = json.loads(body or b"{}").get("message", "")
    eq("locked before any catalog is read: a 400 that names it",
       (400, True, True), (st, "app (" in message, "database is locked" in message))
    release()
    st, body, _ = fetch(base, "/catalog")
    doc = json.loads(body)
    eq("read once the lock is gone",
       (200, [("app", ["people"], None)]), (st, [(a["name"], [t["name"] for t in a["tables"]], a["error"]) for a in doc["attached"]]))
    last = doc

    lock()
    for style in ("", "?style=lite"):
        started = time.time()
        st, body, _ = fetch(base, "/catalog" + style)
        took = time.time() - started
        doc = json.loads(body)
        a = (doc.get("attached") or [{}])[0]
        error = a.get("error") or ""
        eq(f"locked again{style}: a 200, as last read, the busy one saying why",
           (200, [t["name"] for t in doc.get("tables", [])], "app", ["people"], True, True),
           (st, ["own"], a.get("name"), [t["name"] for t in a.get("tables", [])],
            "database is locked" in error, "may be stale" in error))
        stale = doc.get("stale") or ""
        eq(f"and the document says it may be stale{style}, naming the busy one, last",
           (True, True, "stale"), ("may be stale" in stale, f"app ({app})" in stale, list(doc)[-1]))
        if not style:
            eq("and its columns as last read", last["attached"][0]["tables"], a.get("tables"))
    started = time.time()
    fetch(base, "/catalog")
    took = time.time() - started
    if took < 1:
        ok("a read while it stays busy waits on it no longer", f"{took:.2f}s")
    else:
        bad("a read while it stays busy waits on it no longer", f"{took:.2f}s")
    eq("and it stays attached for every connection", None, attached_now())
    refused = sql(base, "SELECT count(*) FROM app.people") or ""
    eq("where reading it waits on its lock", (True, False), ("locked" in refused, "does not exist" in refused))
    release()
    st, body, _ = fetch(base, "/catalog")
    eq("the lock gone, it is read anew", (200, None, False),
       (st, json.loads(body)["attached"][0]["error"], "stale" in json.loads(body)))
    eq("and reads", None, sql(base, "SELECT count(*) FROM app.people"))
    locker.close()

    os.rename(app, app + ".away")
    write_csv(app, "not a SQLite file, and long enough to be read as one: " * 4)
    st, body, _ = fetch(base, "/catalog")
    a = json.loads(body)["attached"][0]
    eq("replaced by a file that is not SQLite: a 200, it detached and listed with why",
       (200, "app", [], True, True),
       (st, a["name"], a["tables"], "file is not a database" in (a["error"] or ""), "attach it again" in (a["error"] or "")))
    eq("and gone from every connection", True, "app is not attached" in (attached_now() or ""))
    os.replace(app + ".away", app)
    a = json.loads(fetch(base, "/catalog")[1])["attached"][0]
    eq("a client's attachment is not attached again by the server once it reads", (False, []),
       (a["error"] is None, a["tables"]))
    eq("a client's DETACH of it", None, sql(base, "DETACH IF EXISTS app"))
    eq("forgets it", [], json.loads(fetch(base, "/catalog")[1])["attached"])
    eq("and it attaches again", None, sql(base, f"ATTACH '{app}' AS app (TYPE sqlite, READ_ONLY)"))
    eq("and lists", [["people"]], [[t["name"] for t in a["tables"]] for a in json.loads(fetch(base, "/catalog")[1])["attached"]])
    os.rename(app, app + ".away")
    a = json.loads(fetch(base, "/catalog")[1])["attached"][0]
    eq("a read-only file deleted is detached and listed with why", (True, []),
       ("unable to open" in (a["error"] or ""), a["tables"]))
    os.replace(app + ".away", app)
    sql(base, "DETACH IF EXISTS app")


def run_unlisted(base, work, seconds=5):
    # -----------------------------------------------------------------------
    section("A view attached while a SQLite file is locked")
    # -----------------------------------------------------------------------
    # The server learns which views read the catalog from the engine's
    # listing of them, which a locked SQLite file fails after its wait.
    # Until a listing reads, every statement is kept apart from an ATTACH
    # or DETACH, the ATTACH does not hold the others for the lock's wait,
    # and the listing is read again at a later ATTACH or DETACH, beside the
    # statements an ATTACH or DETACH waits for: a view
    # attached meanwhile that reads the catalog, raced against a SQLite
    # ATTACH/DETACH loop, does not bring the server down.
    import sqlite3
    import threading
    app = os.path.join(work, "busy.sqlite")
    other = os.path.join(work, "other.sqlite")
    sqlite3.connect(other).close()
    dv = os.path.join(work, "busy_views.duckdb")
    subprocess.run(["duckdb", "-no-init", dv, "-c", "CREATE VIEW busy_catview AS SELECT count(*) n FROM duckdb_tables()"],
                   check=True, stdout=subprocess.DEVNULL)
    eq("a SQLite file attaches, waiting on a lock as the engine does", None,
       sql(base, f"ATTACH '{app}' AS app (TYPE sqlite)"))
    locker = sqlite3.connect(app, isolation_level=None)
    locker.execute("BEGIN EXCLUSIVE")
    eq("while it is locked, a DuckDB file with a view over the catalog attaches", None,
       sql(base, f"ATTACH '{dv}' AS dv (READ_ONLY)"))
    started = time.time()
    sql(base, "SELECT count(*) FROM pragma_database_list")
    took = time.time() - started
    if took < 1:
        ok("and a catalog read after it does not wait out the lock", f"{took:.2f}s")
    else:
        bad("and a catalog read after it does not wait out the lock", f"{took:.2f}s")
    # An ATTACH and a long answer pipelined on one connection, while another
    # statement holds the gate: the answer runs in its turn, after the
    # ATTACH's is written, never beside it holding the gate the ATTACH waits
    # for while its own answer waits behind the ATTACH's.
    held = threading.Thread(target=sql, args=(base, "SELECT sum(hash(i)) FROM range(300000000) t(i)"))
    held.start()
    time.sleep(0.3)
    piped = socket.create_connection(("127.0.0.1", int(base.rsplit(":", 1)[1])), timeout=30)

    def post(statement, last=False):
        body = json.dumps({"sql": statement}).encode()
        close = b"Connection: close\r\n" if last else b""
        return b"POST /sql HTTP/1.1\r\nHost: 127.0.0.1\r\n%sContent-Length: %d\r\n\r\n%s" % (close, len(body), body)

    piped.sendall(post("ATTACH ':memory:' AS piped_mem")
                  + post("SELECT i FROM range(200000) r(i)", last=True))
    got = b""
    try:
        while chunk := piped.recv(65536):
            got += chunk
    except OSError:
        pass
    piped.close()
    held.join(timeout=60)
    eq("an ATTACH and a long answer pipelined behind it are both answered, whole",
       (2, True), (got.count(b"HTTP/1.1 200"), b'"rowCount":200000' in got))
    eq("and an ATTACH after them is answered", None, sql(base, "DETACH piped_mem"))

    # Every statement that names anything shares the gate while the lock
    # keeps the names unknown, and a long one holds an ATTACH for as long as
    # it runs. The ATTACH gives up at its deadline, and when its client
    # leaves, and the statements after it wait for it half a second at most.
    def answer(statement, client_timeout=60, **extra):
        """(HTTP status, error code or None, seconds) of one statement."""
        body = json.dumps({"sql": statement, **extra}).encode()
        req = urllib.request.Request(base + "/sql", data=body, method="POST",
                                     headers={"Content-Type": "application/json"})
        began = time.time()
        try:
            with urllib.request.urlopen(req, timeout=client_timeout) as r:
                events = [json.loads(line) for line in r.read().decode().splitlines()]
                code = next((e.get("code") for e in events if e.get("type") == "error"), None)
                return r.status, code, time.time() - began
        except urllib.error.HTTPError as e:
            return e.code, json.loads(e.read() or b"{}").get("code"), time.time() - began
        except OSError:
            return None, "left", time.time() - began

    def long_one():
        thread = threading.Thread(target=sql, args=(base, "SELECT sum(hash(i)) FROM range(1000000000) t(i)"))
        thread.start()
        time.sleep(0.3)
        return thread

    held = long_one()
    status, code, took = answer("ATTACH ':memory:' AS late_mem", timeoutMs=1500)
    if (status, code) == (499, "cancelled") and took < 4:
        ok("an ATTACH that waits for a long statement while the names are unknown gives up at its deadline",
           f"{took:.2f}s")
    else:
        bad("an ATTACH that waits for a long statement while the names are unknown gives up at its deadline",
            f"{status} {code} {took:.2f}s")
    waits = {}
    attach = threading.Thread(target=lambda: waits.update(attach=answer("ATTACH ':memory:' AS late_mem")))
    attach.start()
    time.sleep(1.5)
    status, code, took = answer("SELECT count(*) AS n FROM range(3)")
    if status == 200 and code is None and took < 1.5:
        ok("a statement after an ATTACH that keeps others out waits for it half a second at most", f"{took:.2f}s")
    else:
        bad("a statement after an ATTACH that keeps others out waits for it half a second at most",
            f"{status} {code} {took:.2f}s")
    held.join(timeout=60)
    attach.join(timeout=60)
    eq("and the ATTACH runs once the long one is over", (200, None), waits.get("attach", (None, "none"))[:2])
    sql(base, "DETACH IF EXISTS late_mem")
    held = long_one()
    eq("an ATTACH whose client leaves while it waits", (None, "left"),
       answer("ATTACH ':memory:' AS left_mem", client_timeout=1)[:2])
    held.join(timeout=60)
    time.sleep(0.3)
    eq("is not run once the long one is over", None,
       sql(base, "SELECT CASE WHEN count(*) = 0 THEN 1 ELSE error('attached') END "
                 "FROM duckdb_databases() WHERE database_name = 'left_mem'"))
    locker.execute("ROLLBACK")
    locker.close()
    # Not yet read again, every statement shares the gate, and a long one
    # holds an ATTACH. The statements after the ATTACH do not wait for it:
    # it asks for the views beside the long one, and they pass the gate by,
    # and once they are read, the long one reads nothing and holds no
    # ATTACH.
    answers = {}

    def timed(key, statement):
        began = time.time()
        answers[key] = (sql(base, statement), time.time() - began)

    long = threading.Thread(target=timed, args=("long", "SELECT sum(hash(i)) FROM range(1000000000) t(i)"))
    long.start()
    time.sleep(0.5)
    attach = threading.Thread(target=timed, args=("attach", "ATTACH ':memory:' AS busy_mem"))
    attach.start()
    time.sleep(0.3)
    timed("after", "SELECT 42")
    long.join(timeout=60)
    attach.join(timeout=60)
    refused, took = answers["after"]
    if refused is None and took < 1:
        ok("a statement after an ATTACH that waits on a long one does not wait", f"{took:.2f}s")
    else:
        bad("a statement after an ATTACH that waits on a long one does not wait", f"{took:.2f}s {refused}")
    eq("and the ATTACH runs", (None, None),
       (answers.get("attach", ("not answered",))[0], answers.get("long", ("not answered",))[0]))
    took = answers.get("attach", (None, 60))[1]
    if took < 2:
        ok("without waiting for the long one, which reads nothing once the names are known", f"{took:.2f}s")
    else:
        bad("without waiting for the long one, which reads nothing once the names are known", f"{took:.2f}s")
    sql(base, "DETACH IF EXISTS busy_mem")

    # The names are read again soon after a locked file reads again, with no
    # ATTACH, DETACH or /catalog to ask.
    log = os.path.join(work, "busy.duckdb.log")
    relock = os.path.join(work, "relock.sqlite")
    sqlite3.connect(relock).execute("CREATE TABLE r(a)")
    locker = sqlite3.connect(relock, isolation_level=None)
    locker.execute("BEGIN EXCLUSIVE")
    time.sleep(1.2)
    eq("another SQLite file attaches while it is locked", None, sql(base, f"ATTACH '{relock}' AS relock (TYPE sqlite)"))
    read_again = open(log).read().count("are read from it")
    unread = open(log).read().count("are not read while")
    eq("and an in-memory database after it, whose views are not read", None, sql(base, "ATTACH ':memory:' AS relock_mem"))
    began = time.time()
    while time.time() - began < 3 and open(log).read().count("are not read while") == unread:
        time.sleep(0.05)
    eq("and the server says so", unread + 1, open(log).read().count("are not read while"))
    locker.execute("ROLLBACK")
    locker.close()
    began = time.time()
    while time.time() - began < 5 and open(log).read().count("are read from it") == read_again:
        time.sleep(0.05)
    took = time.time() - began
    if took < 3:
        ok("the views are read again soon after the lock is gone, nothing asking", f"{took:.2f}s")
    else:
        bad("the views are read again soon after the lock is gone, nothing asking", f"{took:.2f}s")
    sql(base, "DETACH IF EXISTS relock_mem")
    sql(base, "DETACH IF EXISTS relock")
    stop = threading.Event()
    counts, failures = [0, 0], []

    def reader():
        while not stop.is_set():
            try:
                refused = sql(base, "SELECT n FROM dv.busy_catview")
            except (urllib.error.URLError, ConnectionError, OSError) as e:
                failures.append(str(e))
                return
            if refused is not None:
                failures.append(refused)
            counts[0] += 1

    def churner():
        while not stop.is_set():
            try:
                for statement in (f"ATTACH '{other}' AS lite_other (TYPE sqlite)", "DETACH lite_other"):
                    refused = sql(base, statement)
                    if refused is not None:
                        failures.append(refused)
            except (urllib.error.URLError, ConnectionError, OSError) as e:
                failures.append(str(e))
                return
            counts[1] += 1

    threads = [threading.Thread(target=reader, daemon=True), threading.Thread(target=reader, daemon=True),
               threading.Thread(target=churner, daemon=True)]
    for thread in threads:
        thread.start()
    time.sleep(seconds)
    stop.set()
    for thread in threads:
        thread.join(timeout=30)
    eq(f"the lock gone, {counts[0]} reads of the view raced against {counts[1]} SQLite ATTACH/DETACH: failures",
       0, len(failures))
    if failures:
        print(f"     \033[2mfirst: {failures[0][:300]}\033[0m")
    try:
        alive = sql(base, "SELECT 42")
    except (urllib.error.URLError, ConnectionError, OSError) as e:
        alive = str(e)
    eq("and the server answers after", None, alive)
    if alive is None:
        sql(base, "DETACH IF EXISTS dv")
        sql(base, "DETACH IF EXISTS app")


def run_race(base, work, sqlite_ok, rounds=300):
    # -----------------------------------------------------------------------
    section("A relation dropped while the catalog is read is left out")
    # -----------------------------------------------------------------------
    # The catalog lists, then counts and binds: a table, a view or an
    # attachment another client takes away in between must cost only itself.
    # One client churns all three while this one reads the catalog; every
    # read must answer.
    #
    # A second client attaches and detaches a SQLite file, which the engine
    # cannot take while it walks its catalog: it segfaults, or invalidates
    # the database for every statement after. The server keeps the two
    # apart, so the catalog read, the inventory, a catalog function and the
    # engine's completer all answer through it, and the server outlives it.
    import threading
    churn = os.path.join(work, "churn.duckdb")
    subprocess.run(["duckdb", "-no-init", churn, "-c", "CREATE TABLE c (i INTEGER); CHECKPOINT;"],
                   check=True, stdout=subprocess.DEVNULL)
    app = os.path.join(work, "app.sqlite")
    # Readers whose statement names no catalog function and reads the
    # catalog all the same: through query() and query_table(), through a
    # view an IMPORT DATABASE makes, and through a view of a DuckDB file
    # attached under the name sqlite.
    export, own = os.path.join(work, "export"), os.path.join(work, "own.duckdb")
    subprocess.run(["duckdb", "-no-init", os.path.join(work, "exported.duckdb"), "-c",
                    "CREATE VIEW gate_iv AS SELECT * FROM duckdb_tables(); "
                    f"EXPORT DATABASE '{export}'"], check=True, stdout=subprocess.DEVNULL)
    subprocess.run(["duckdb", "-no-init", own, "-c", "CREATE VIEW gate_ov AS SELECT * FROM duckdb_tables()"],
                   check=True, stdout=subprocess.DEVNULL)
    for statement in ("CREATE VIEW gate_v AS SELECT * FROM duckdb_tables()",
                      "CREATE VIEW gate_q AS SELECT * FROM query('SELECT * FROM duckdb_tables()')",
                      f"IMPORT DATABASE '{export}'",
                      f"ATTACH '{own}' AS sqlite (READ_ONLY)"):
        eq(f"set up: {statement[:40]}", None, sql(base, statement))
    stop = threading.Event()
    churned = [0, 0]

    def churner():
        while not stop.is_set():
            for statement in ("CREATE TABLE race (i INTEGER)", "CREATE VIEW race_v AS SELECT 1 AS x",
                              f"ATTACH '{churn}' AS churn (READ_ONLY)",
                              "DROP VIEW race_v", "DROP TABLE race", "DETACH churn"):
                sql(base, statement)
            churned[0] += 1

    def sqlite_churner():
        while not stop.is_set():
            sql(base, f"ATTACH '{app}' AS lite_churn (TYPE sqlite)")
            sql(base, "DETACH lite_churn")
            churned[1] += 1

    threads = [threading.Thread(target=churner, daemon=True)]
    if sqlite_ok:
        threads.append(threading.Thread(target=sqlite_churner, daemon=True))
    else:
        print("  \033[33m-\033[0m SQLite churn skipped: the sqlite extension did not install")
    for thread in threads:
        thread.start()
    reads = ("/catalog", "/catalog?style=lite",
             "SELECT database_name, table_name, column_name FROM duckdb_columns()",
             "SELECT * FROM sql_auto_complete('SELECT * FROM lite_')",
             "SELECT count(*) FROM query('SELECT * FROM duckdb_tables()')",
             "SELECT count(*) FROM gate_q",
             "SELECT count(*) FROM query_table('gate_v')",
             "SELECT count(*) FROM gate_iv",
             "SELECT count(*) FROM sqlite.gate_ov")
    failures = []
    for i in range(rounds):
        read = reads[i % len(reads)]
        try:
            if read.startswith("/"):
                st, body, _ = fetch(base, read)
                if st != 200:
                    failures.append(json.loads(body or b"{}").get("message", f"HTTP {st}"))
            else:
                refused = sql(base, read)
                if refused is not None:
                    failures.append(refused)
        except (urllib.error.URLError, ConnectionError, OSError) as e:
            failures.append(f"{read}: {e}")
            break
    stop.set()
    for thread in threads:
        thread.join(timeout=30)
    what = f"{churned[0]} rounds of DROP and DETACH" + (f" and {churned[1]} of a SQLite DETACH" if sqlite_ok else "")
    eq(f"reads that failed, of {rounds} raced against {what}", 0, len(failures))
    if failures:
        print(f"     \033[2mfirst: {failures[0][:300]}\033[0m")
    try:
        alive = sql(base, "SELECT 42")
    except (urllib.error.URLError, ConnectionError, OSError) as e:
        alive = str(e)
    eq("and the server answers after", None, alive)


def harbor_version():
    """The version the binary under test reports, not the one Cargo.toml
    holds: a bump made after the build would fail a correct server."""
    out = subprocess.run([*LAUNCHER, "--version"], capture_output=True, text=True).stdout
    return out.split()[-1] if out.split() else ""


if __name__ == "__main__":
    sys.exit(main())
