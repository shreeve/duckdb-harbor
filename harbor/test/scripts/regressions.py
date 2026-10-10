#!/usr/bin/env python3
"""Review regressions: request isolation, settings policy, body limits and config writes.

Runs an isolated one-worker/two-connection server so connection reuse is deterministic,
and a two-worker one for the race a single worker would serialize.
"""
import concurrent.futures
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
BINARY = ROOT / "target/release/harbor"
LIMIT = 8 << 20


class Harbor:
    """A server of its own for the test class, and requests to it."""
    WORKERS, POOL = 1, 2

    @classmethod
    def setUpClass(cls):
        cls.work = tempfile.TemporaryDirectory(prefix="hb-reg-", dir="/tmp")
        cls.root = Path(cls.work.name)
        cls.env = dict(os.environ, HARBOR_HOME=str(cls.root / "home"), HARBOR_POOL_SIZE=str(cls.POOL))
        cls.db = cls.root / "source.duckdb"
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            cls.port = s.getsockname()[1]
        cls.log = open(cls.root / "server.log", "w")
        cls.server = subprocess.Popen(
            [str(BINARY), str(cls.db), "start", "--port", str(cls.port), "--workers", str(cls.WORKERS)],
            env=cls.env, stdout=cls.log, stderr=cls.log,
        )
        for _ in range(100):
            try:
                if cls.request("GET", "/ready")[0] == 200:
                    return
            except OSError:
                pass
            if cls.server.poll() is not None:
                break
            time.sleep(.1)
        cls.tearDownClass()
        raise RuntimeError("regression server failed to start")

    @classmethod
    def tearDownClass(cls):
        cls.server.terminate()
        try:
            cls.server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            cls.server.kill()
            cls.server.wait()
        cls.log.close()
        cls.work.cleanup()

    @classmethod
    def request(cls, method, path, data=None, chunked=False, extra=None):
        body = json.dumps(data).encode() if isinstance(data, dict) else data
        headers = {"Content-Type": "application/json", "Accept": "application/json", **(extra or {})}
        if chunked:
            headers["Transfer-Encoding"] = "chunked"
            body = [body]
        conn = http.client.HTTPConnection("127.0.0.1", cls.port, timeout=20)
        try:
            conn.request(method, path, body, headers, encode_chunked=chunked)
            response = conn.getresponse()
            return response.status, json.loads(response.read())
        finally:
            conn.close()

    def sql(self, sql, session=None, status=200):
        body = {"sql": sql}
        if session:
            body["sessionId"] = session
        actual, doc = self.request("POST", "/sql", body)
        self.assertEqual(actual, status, (sql, doc))
        return doc

    def session(self):
        # A session released the instant its last answer arrived can still
        # hold its claim, and is then released by the reaper's next tick:
        # every lease taken is the documented 503, retried.
        deadline = time.time() + 3
        while True:
            status, doc = self.request("POST", "/sql/sessions", {})
            if doc.get("code") != "no_lease_available" or time.time() > deadline:
                break
            time.sleep(.05)
        self.assertEqual(status, 200, doc)
        return doc["sessionId"]

    def release(self, sid):
        self.assertEqual(self.request("DELETE", "/sql/sessions/" + sid)[0], 200)


class Regressions(Harbor, unittest.TestCase):

    def test_backup_lease_policy_and_expiry(self):
        for data in ({"purpose": "unknown"}, {"ttlMs": 0}, {"ttlMs": -1}):
            self.assertEqual(self.request("POST", "/sql/sessions", data)[0], 400)
        status, ordinary = self.request("POST", "/sql/sessions", {"ttlMs": 999999})
        self.assertEqual(status, 200)
        self.assertEqual(ordinary["ttlMs"], 300000)
        self.assertEqual(ordinary["idleTtlMs"], 30000)
        sid = ordinary["sessionId"]
        try:
            # An ordinary session renews its idle clock (sessions.py).
            self.assertEqual(self.request("POST", f"/sql/sessions/{sid}/renew"), (200, {"renewed": True}))
        finally:
            self.release(sid)
        status, backup = self.request("POST", "/sql/sessions", {"purpose": "backup", "ttlMs": 999999})
        self.assertEqual(status, 200)
        self.assertEqual((backup["purpose"], backup["ttlMs"], backup["idleTtlMs"]), ("backup", 60000, 0))
        self.release(backup["sessionId"])
        status, backup = self.request("POST", "/sql/sessions", {"purpose": "backup", "ttlMs": 600})
        self.assertEqual(status, 200)
        sid = backup["sessionId"]
        renew = f"/sql/sessions/{sid}/renew"
        try:
            self.sql("BEGIN", sid)
            for _ in range(5):
                time.sleep(.2)
                self.assertEqual(self.request("POST", renew), (200, {"renewed": True}))
            self.sql("SELECT 1", sid)
            time.sleep(.7)
            self.assertEqual(self.request("POST", renew)[0], 404)
            self.sql("COMMIT", sid, status=404)
        finally:
            self.release(sid)
        self.assertEqual(self.request("POST", renew)[0], 404)

    def test_backup_heartbeats_work_during_busy_query_then_expiry_cancels(self):
        status, backup = self.request("POST", "/sql/sessions", {"purpose": "backup", "ttlMs": 6000})
        self.assertEqual(status, 200)
        sid = backup["sessionId"]
        try:
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
                query = executor.submit(self.request, "POST", "/sql", {
                    "sql": "SELECT sum(sin(i)) FROM range(1000000000000) t(i)", "sessionId": sid,
                })
                # Forwarded lease work activates the probe after the worker's
                # five-second request threshold; allow that initial handoff.
                for _ in range(6):
                    time.sleep(.35)
                    self.assertEqual(self.request("POST", f"/sql/sessions/{sid}/renew")[0], 200)
                    self.assertFalse(query.done(), "query ended despite timely renewals")
                # Stop renewing: expiration must interrupt a busy executor too.
                status, body = query.result(timeout=10)
                self.assertNotEqual(status, 200, body)
            self.assertEqual(self.request("POST", f"/sql/sessions/{sid}/renew")[0], 404)
        finally:
            self.release(sid)
        for _ in range(30):
            status, report = self.request("GET", "/sessions")
            if report["connections"]["free"] == 1:
                break
            time.sleep(.1)
        self.assertTrue(report["connections"]["balanced"])
        self.assertEqual(report["connections"]["free"], 1)
        self.assertEqual(self.sql("SELECT 42")["data"], [[42]])

    def test_wrapped_begin_cannot_capture_another_callers_write(self):
        self.sql("CREATE TABLE acknowledged(x INTEGER)")
        # An analyzed EXPLAIN runs what it explains, so a BEGIN behind one is
        # a BEGIN: refused outside a session, in every spelling the engine
        # takes, and behind every space it skips.
        for wrapped in ("EXPLAIN ANALYZE BEGIN", "EXPLAIN (ANALYZE) BEGIN",
                        "EXPLAIN ANALYSE BEGIN", "\ufeffBEGIN", "\u00a0BEGIN", "\x0bBEGIN"):
            self.sql(wrapped, status=400)
        # A table may be named for the word, and reading one begins nothing.
        self.sql('CREATE TABLE "begin"(x INTEGER)')
        self.sql('"begin"')
        self.sql("INSERT INTO acknowledged VALUES (42)")
        sid = self.session()
        try:
            self.assertEqual(self.sql("SELECT * FROM acknowledged", sid)["data"], [[42]])
        finally:
            self.release(sid)
        self.sql("ROLLBACK", status=400)
        self.assertEqual(self.sql("SELECT * FROM acknowledged")["data"], [[42]])

    def test_commit_on_an_aborted_transaction_is_told_and_rolled_back(self):
        self.sql("CREATE TABLE kept(x INTEGER PRIMARY KEY)")
        sid = self.session()
        try:
            # An error aborts the transaction; the engine would answer the
            # COMMIT with success and roll back. The answer is the rollback,
            # in each spelling the engine runs as a commit.
            for commit in ("COMMIT", "end", "EXPLAIN ANALYZE COMMIT", "EXPLAIN (ANALYZE) COMMIT"):
                self.sql("BEGIN", sid)
                self.sql("INSERT INTO kept VALUES (1)", sid)
                self.sql("SELECT no_such_column FROM kept", sid, status=400)
                doc = self.sql(commit, sid, status=400)
                self.assertEqual(doc["code"], "sql_error", commit)
                self.assertIn("rolled back", doc["message"], commit)
                self.assertIn("nothing since BEGIN", doc["message"], commit)
                # Nothing is left open behind it, and the session still serves.
                self.assertIn("no transaction is active", self.sql("ROLLBACK", sid, status=400)["message"])
                self.assertEqual(self.sql("SELECT count(*) FROM kept", sid)["data"], [[0]])
            # A healthy transaction commits.
            self.sql("BEGIN", sid)
            self.sql("INSERT INTO kept VALUES (2)", sid)
            self.sql("COMMIT", sid)
        finally:
            self.release(sid)
        self.assertEqual(self.sql("SELECT x FROM kept")["data"], [[2]])

    def test_a_statement_that_fails_mid_stream_in_a_session_says_why(self):
        # A VARIANT cell is cast in the statement's transaction, which the
        # statement's own error has already aborted; the answer is that
        # error, not the aborted transaction the next cell then meets.
        sid = self.session()
        try:
            self.sql("BEGIN", sid)
            doc = self.sql("SELECT i::VARIANT v, CASE WHEN i = 250000 THEN error('boom at 250000') END "
                           "FROM range(300000) t(i)", sid, status=400)
            self.assertIn("boom at 250000", doc["message"])
        finally:
            self.release(sid)

    def hang_up(self, body, accept, wait=1.0):
        """Send a statement and close the connection before reading anything."""
        data = json.dumps(body).encode()
        with socket.create_connection(("127.0.0.1", self.port)) as s:
            s.sendall(b"POST /sql HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n"
                      + f"Accept: {accept}\r\nContent-Length: {len(data)}\r\n\r\n".encode() + data)
            time.sleep(wait)

    def test_a_client_that_hangs_up_takes_its_statement_with_it(self):
        # This server has one worker. A statement that sends no row for
        # minutes — a count over a huge range, a cross join, a filter that
        # matches nothing — whose client leaves before any byte comes back
        # is stopped, in either shape, so the worker serves the next request.
        long = ["SELECT count(*) FROM range(2000000000000)",
                "SELECT count(*) FROM range(100000000) a, range(1000000) b",
                "SELECT i FROM range(2000000000000) t(i) WHERE i = -1"]
        for sql in long:
            for accept in ("application/json", "application/x-ndjson"):
                self.hang_up({"sql": sql}, accept)
                # The probe lane sheds with a 503 until the worker is free;
                # it is free within moments, not when the statement would end.
                deadline = time.monotonic() + 5
                while True:
                    status, doc = self.request("POST", "/sql", {"sql": "SELECT 42"})
                    if status != 503 or time.monotonic() > deadline:
                        break
                    time.sleep(.05)
                self.assertEqual((status, doc.get("data")), (200, [[42]]), (sql, accept))
        # In a session the statement stops too, and its transaction is over.
        sid = self.session()
        try:
            self.sql("BEGIN", sid)
            self.hang_up({"sql": long[0], "sessionId": sid}, "application/x-ndjson")
            deadline = time.time() + 5
            while True:
                status, doc = self.request("POST", "/sql", {"sql": "SELECT 1", "sessionId": sid})
                if status != 409 or time.time() > deadline:
                    break
                time.sleep(.05)
            self.assertEqual(status, 400, doc)
            self.assertIn("aborted", doc["message"])
        finally:
            self.release(sid)

    def test_a_statement_cut_short_in_a_transaction_aborts_it_every_time(self):
        self.sql("CREATE TABLE half(x INTEGER)")
        for attempt in range(12):
            sid = self.session()
            try:
                self.sql("BEGIN", sid)
                self.sql(f"INSERT INTO half VALUES ({attempt})", sid)
                # Read the head of a long stream, then walk away from it.
                conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=20)
                conn.request("POST", "/sql", json.dumps({
                    "sql": "SELECT range, repeat('x', 200) FROM range(3000000)", "sessionId": sid,
                }), {"Content-Type": "application/json"})
                response = conn.getresponse()
                self.assertEqual(response.status, 200)
                response.read(1000 * (attempt + 1))
                conn.close()
                # The session is busy until the abandoned statement ends.
                deadline = time.time() + 15
                while True:
                    status, doc = self.request("POST", "/sql", {"sql": "SELECT 1", "sessionId": sid})
                    if status != 409 or time.time() > deadline:
                        break
                    time.sleep(.05)
                self.assertEqual(status, 400, (attempt, doc))
                self.assertIn("aborted", doc["message"])
                self.assertIn("rolled back", self.sql("COMMIT", sid, status=400)["message"])
            finally:
                self.release(sid)
        self.assertEqual(self.sql("SELECT count(*) FROM half")["data"], [[0]])

    def test_worker_discards_connection_local_state(self):
        self.sql("SET VARIABLE secret='previous caller'")
        self.assertEqual(self.sql("SELECT getvariable('secret')")["data"], [[None]])
        self.sql("CREATE TEMP TABLE private_worker AS SELECT 1 x")
        self.sql("SELECT * FROM private_worker", status=400)
        self.sql("PREPARE private_stmt AS SELECT 42")
        self.sql("EXECUTE private_stmt", status=400)

    def test_released_session_discards_all_local_state(self):
        first = self.session()
        try:
            self.sql("CREATE TEMP TABLE private_lease AS SELECT 42 x", first)
            self.sql("SET VARIABLE secret='first lease'", first)
            self.assertEqual(self.sql("SELECT * FROM private_lease", first)["data"], [[42]])
        finally:
            self.release(first)
        second = self.session()
        try:
            self.sql("SELECT * FROM private_lease", second, status=400)
            self.assertEqual(self.sql("SELECT getvariable('secret')", second)["data"], [[None]])
        finally:
            self.release(second)

    def test_settings_cannot_override_operator_policy(self):
        # Process-global settings are locked in the engine, which refuses
        # every way of reaching one and names it.
        settings = "SELECT current_setting('threads'), current_setting('memory_limit')"
        before = self.sql(settings)["data"]
        for sql, name in [("SET threads=1", "threads"), ("SET GLOBAL threads=1", "threads"),
                          ("SET SESSION worker_threads=1", "worker_threads"), ("RESET threads", "threads"),
                          ("PRAGMA threads=1", "threads"), ("SET \"memory_limit\"='1TB'", "memory_limit"),
                          ("/* x */ SET max_memory='1TB'", "max_memory"),
                          ("SET --\r memory_limit='1TB'", "memory_limit"),
                          ("RESET --\r\n external_threads", "external_threads"),
                          ("SET max_temp_directory_size='100TB'", "max_temp_directory_size"),
                          ("PRAGMA temp_directory='/tmp/x'", "temp_directory"),
                          ("EXPLAIN ANALYZE SET threads=2", "threads"),
                          ("EXPLAIN ANALYZE RESET memory_limit", "memory_limit"),
                          ("EXPLAIN ANALYZE SET allowed_configs=['threads']", "allowed_configs"),
                          ("EXPLAIN ANALYZE SET lock_configuration=false", "lock_configuration")]:
            doc = self.sql(sql, status=400)
            self.assertEqual(doc["code"], "sql_error", sql)
            self.assertIn("configuration has been locked", doc["message"], sql)
            self.assertIn(name, doc["message"], sql)
        self.assertEqual(self.sql(settings)["data"], before)
        self.sql("SET default_order='DESC'")
        self.sql("RESET default_order")

    def test_one_statement_per_request(self):
        # The engine's parser counts the statements before anything runs, and
        # more than one is refused; a canary only a second statement could
        # drop is the proof it never ran. Each form here hides its `;` from a
        # reader that disagrees with the engine on one rule: a CR ending a
        # `--` comment, a `$` inside a word, an `e` that ends a keyword, an
        # E string's backslash.
        self.sql("CREATE TABLE canary(x INT)")
        sid = self.session()
        try:
            for sql in ["SELECT 1; DROP TABLE canary", "SELECT ';';DROP TABLE canary",
                        "SELECT 1 --\r; DROP TABLE canary", "SELECT 1 -- note\r\n; DROP TABLE canary",
                        "SELECT 1 a$b$c; DROP TABLE canary", "SELECT 1 a$b$c$$; DROP TABLE canary",
                        "SELECT 1 x$1$; DROP TABLE canary", r"SELECT 1 WHERE 'a' LIKE'\'; DROP TABLE canary",
                        r"SELECT 'a' LIKE 'b' ESCAPE'\'; DROP TABLE canary",
                        r"SELECT date'2020-01-01'; DROP TABLE canary", r"SELECT e'\''; DROP TABLE canary",
                        r"SELECT E'a\\'; DROP TABLE canary", "/* x; */ SELECT 1; DROP TABLE canary"]:
                for session in (None, sid):
                    doc = self.sql(sql, session, status=400)
                    self.assertEqual(doc["code"], "bad_request", sql)
                    self.sql("SELECT count(*) FROM canary")
        finally:
            self.release(sid)
        self.sql("DROP TABLE canary")
        # What follows the one statement may be a comment or nothing at all,
        # and a `;` inside a dollar quote is data, whatever bytes its tag holds.
        self.assertEqual(self.sql("SELECT 1; -- c")["data"], [[1]])
        self.assertEqual(self.sql("SELECT 1;;")["data"], [[1]])
        self.assertEqual(self.sql("SELECT $é$a;b$é$")["data"], [["a;b"]])

    def test_a_web_page_cannot_reach_the_tcp_listener(self):
        # A page's form POST needs no preflight; DNS rebinding reads the answer
        # under a hostname the page controls. Both are refused before routing.
        probe = {"sql": "CREATE TABLE from_a_page AS SELECT 1"}
        for extra in ({"Origin": "https://attacker.example", "Content-Type": "text/plain"},
                      {"Origin": "null"},
                      {"Host": f"attacker.example:{self.port}"}):
            status, doc = self.request("POST", "/sql", probe, extra=extra)
            self.assertEqual((status, doc["code"]), (403, "forbidden"), extra)
        status, _ = self.request("GET", "/info", extra={"Host": f"rebound.example:{self.port}"})
        self.assertEqual(status, 403)
        for host in (f"localhost:{self.port}", f"127.0.0.1:{self.port}"):
            self.assertEqual(self.request("GET", "/info", extra={"Host": host})[0], 200)
        doc = self.sql("SELECT count(*) AS n FROM duckdb_tables() WHERE table_name = 'from_a_page'")
        self.assertEqual(doc["data"], [[0]])

    def test_chunked_limit_applies_to_both_body_endpoints(self):
        for path, data in [("/sql", {"sql": "CREATE TABLE oversized(x INTEGER)"}),
                           ("/sql/sessions", {})]:
            prefix = json.dumps(data).encode()
            body = prefix + b" " * (LIMIT - len(prefix)) + b"not JSON"
            status, doc = self.request("POST", path, body, chunked=True)
            self.assertEqual(status, 413, doc)
        self.sql("SELECT * FROM oversized", status=400)
        sid = self.session()  # the rejected body must not have consumed the only lease
        self.release(sid)
        prefix = b'{"sql":"SELECT 42"}'
        status, doc = self.request("POST", "/sql", prefix + b" " * (LIMIT - len(prefix)), chunked=True)
        self.assertEqual(status, 200, doc)
        self.assertEqual(doc["data"], [[42]])

    def test_params_are_read_once_each(self):
        # A double past 64 bits is told from a whole number by its own text.
        # Read from the whole body once per such param, 50,000 of them held
        # a worker for twenty seconds, and the cost grew with their square.
        body = '{"sql":"SELECT ?","params":[' + ",".join(["1e30"] * 50000) + "]}"
        started = time.monotonic()
        status, doc = self.request("POST", "/sql", body.encode())
        self.assertLess(time.monotonic() - started, 5, doc)
        self.assertEqual(status, 400, doc)
        # Two `params` keys are refused, not read as the last one.
        body = b'{"sql":"SELECT ?::HUGEINT","params":[1],"params":[123456789012345678901234]}'
        status, doc = self.request("POST", "/sql", body)
        self.assertEqual(status, 400, doc)
        self.assertIn("duplicate field", doc["message"])

    def test_concurrent_membership_updates_keep_every_success(self):
        home = self.root / "config-writers"
        env = dict(self.env, HARBOR_HOME=str(home))
        files = [self.root / ("item%d.duckdb" % i) for i in range(32)]
        for path in files:
            path.touch()
        def attach(path):
            return subprocess.run([str(BINARY), str(path), "attach"], env=env,
                                  capture_output=True, text=True, timeout=20)
        with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
            for result in pool.map(attach, files):
                self.assertEqual(result.returncode, 0, result.stderr)
        config = (home / "config.toml").read_text()
        for i in range(len(files)):
            self.assertIn("[connection.item%d]" % i, config)
        self.assertEqual(list(home.glob("*.tmp")), [])
        if os.name == "posix":
            self.assertEqual((home / "config.toml").stat().st_mode & 0o777, 0o600)

    def test_backup_quoted_paths_and_identifiers_restore_after_move(self):
        self.sql('CREATE SCHEMA "odd schema"')
        self.sql('CREATE TABLE "odd schema"."a\'b\nname"(v VARCHAR)')
        self.sql('INSERT INTO "odd schema"."a\'b\nname" VALUES (\'\'), (\'hello\')')
        backup = self.root / "it's a backup"
        result = subprocess.run([str(BINARY), str(self.db), "backup", str(backup)],
                                env=self.env, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        moved = self.root / "moved"
        backup.rename(moved)
        restored = self.root / "restored.duckdb"
        result = subprocess.run([str(BINARY), str(restored), "restore", str(moved)],
                                env=self.env, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        result = subprocess.run([str(BINARY), str(restored), "--mode", "json", "-c",
                                 'SELECT count(*) AS n FROM "odd schema"."a\'b\nname"'],
                                env=self.env, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), [{"n": 2}])

    def test_deep_json_document_does_not_take_the_server_down(self):
        # The engine recurses once per nesting level when it builds a
        # VARIANT from JSON; on the default 2 MiB thread stack a document
        # ~7,700 levels deep overflowed the executor and aborted the whole
        # server. The executor now runs on a 16 MiB stack.
        depth = 20000
        doc = "[" * depth + "1" + "]" * depth
        result = self.sql(f"SELECT variant_typeof('{doc}'::JSON::VARIANT) AS t")
        self.assertIn("ARRAY", json.dumps(result))
        self.assertEqual(self.request("GET", "/ready")[0], 200)
        self.assertIn("1", json.dumps(self.sql("SELECT 1 AS one")))



class CommitRaces(Harbor, unittest.TestCase):
    """Two workers: on one, the cancel would wait behind the COMMIT it races."""
    WORKERS, POOL = 2, 3

    def test_a_cancelled_commit_kept_everything_or_nothing_and_says_which(self):
        # A cancel raced against a healthy COMMIT. Whichever wins, the answer
        # is true: 200 and the row is there, or 499 and it is not, with the
        # transaction left aborted. Never 499 for a commit that landed.
        import threading
        self.sql("CREATE TABLE raced(x INTEGER)")
        landed = cancelled = 0
        for attempt in range(150):
            sid = self.session()
            try:
                self.sql("BEGIN", sid)
                self.sql(f"INSERT INTO raced VALUES ({attempt})", sid)
                name, answer = f"race-{attempt}", {}

                def commit():
                    answer["commit"] = self.request(
                        "POST", "/sql", {"sql": "COMMIT", "sessionId": sid, "queryId": name})

                thread = threading.Thread(target=commit)
                thread.start()
                if attempt % 2:
                    time.sleep((attempt % 8) * 0.0002)
                    self.request("DELETE", "/sql/queries/" + name)
                else:
                    # Asked until it is answered, so a cancel can land before
                    # the COMMIT begins, which on a fast machine a single one
                    # sent after it never does.
                    while thread.is_alive() and not self.request("DELETE", "/sql/queries/" + name)[1]["cancelled"]:
                        pass
                thread.join()
                status, doc = answer["commit"]
                kept = self.sql(f"SELECT count(*) FROM raced WHERE x = {attempt}")["data"][0][0]
                if status == 200:
                    landed += 1
                    self.assertEqual(kept, 1, (attempt, doc))
                else:
                    cancelled += 1
                    self.assertEqual((status, doc.get("code")), (499, "cancelled"), (attempt, doc))
                    self.assertEqual(kept, 0, (attempt, "answered cancelled, and the row is committed"))
                    self.assertIn("aborted", self.sql("SELECT 1", sid, status=400)["message"])
            finally:
                self.release(sid)
        self.assertEqual(landed + cancelled, 150)
        # Both answers were exercised, or the test proved nothing about one.
        self.assertGreater(landed, 0)
        self.assertGreater(cancelled, 0)
        self.assertEqual(self.sql("SELECT count(*) FROM raced")["data"], [[landed]])


if __name__ == "__main__":
    unittest.main(verbosity=2)
