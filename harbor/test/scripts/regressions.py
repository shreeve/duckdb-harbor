#!/usr/bin/env python3
"""Review regressions: request isolation, settings policy, body limits and config writes.

Runs an isolated one-worker/two-connection server so connection reuse is deterministic.
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


class Regressions(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.work = tempfile.TemporaryDirectory(prefix="hb-reg-", dir="/tmp")
        cls.root = Path(cls.work.name)
        cls.env = dict(os.environ, HARBOR_HOME=str(cls.root / "home"), HARBOR_POOL_SIZE="2")
        cls.db = cls.root / "source.duckdb"
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            cls.port = s.getsockname()[1]
        cls.log = open(cls.root / "server.log", "w")
        cls.server = subprocess.Popen(
            [str(BINARY), str(cls.db), "start", "--port", str(cls.port), "--workers", "1"],
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
    def request(cls, method, path, data=None, chunked=False):
        body = json.dumps(data).encode() if isinstance(data, dict) else data
        headers = {"Content-Type": "application/json", "Accept": "application/json"}
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
        status, doc = self.request("POST", "/sql/sessions", {})
        self.assertEqual(status, 200, doc)
        return doc["sessionId"]

    def release(self, sid):
        self.assertEqual(self.request("DELETE", "/sql/sessions/" + sid)[0], 200)

    def test_backup_lease_policy_and_expiry(self):
        for data in ({"purpose": "unknown"}, {"ttlMs": 0}, {"ttlMs": -1}):
            self.assertEqual(self.request("POST", "/sql/sessions", data)[0], 400)
        status, ordinary = self.request("POST", "/sql/sessions", {"ttlMs": 999999})
        self.assertEqual(status, 200)
        self.assertEqual(ordinary["ttlMs"], 300000)
        self.assertEqual(ordinary["idleTtlMs"], 30000)
        sid = ordinary["sessionId"]
        try:
            self.assertEqual(self.request("POST", f"/sql/sessions/{sid}/renew")[0], 400)
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
        self.sql("EXPLAIN ANALYZE BEGIN")
        self.sql("INSERT INTO acknowledged VALUES (42)")
        sid = self.session()
        try:
            self.assertEqual(self.sql("SELECT * FROM acknowledged", sid)["data"], [[42]])
        finally:
            self.release(sid)
        self.sql("ROLLBACK", status=400)
        self.assertEqual(self.sql("SELECT * FROM acknowledged")["data"], [[42]])

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

    def test_wrapped_settings_cannot_override_operator_policy(self):
        before = self.sql("SELECT current_setting('threads')")["data"]
        for sql in ["EXPLAIN ANALYZE SET threads=2", "EXPLAIN ANALYZE SET worker_threads=2",
                    "EXPLAIN ANALYZE SET allowed_configs=['threads']",
                    "EXPLAIN ANALYZE SET lock_configuration=false"]:
            self.sql(sql, status=400)
        self.assertEqual(self.sql("SELECT current_setting('threads')")["data"], before)
        self.sql("SET default_order='DESC'")
        self.sql("RESET default_order")

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


if __name__ == "__main__":
    unittest.main(verbosity=2)
