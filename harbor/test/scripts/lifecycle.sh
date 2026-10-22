#!/usr/bin/env bash
#
# lifecycle.sh — the two lifetimes, end to end, through the real binary.
#
#   test/scripts/lifecycle.sh
#
# The law under test is the product's one breath: bare — the server is
# everyone's, it lives while anyone is connected; start — the server is
# yours, it lives until you stop it. Everything here is behavior no unit test
# can see: spawn-on-use across processes, two clients landing on one server,
# the repl's own anchor as a mooring, the last departure sweeping the socket,
# and a start-lifetime server ignoring the refcount entirely.

set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
harbor=${LIFECYCLE_HARBOR:-$here/target/release/harbor}
[[ -x $harbor ]] || { echo "lifecycle: build first (make harbor)" >&2; exit 77; }

# macOS's TMPDIR ends in a slash; a doubled one would make every path this
# suite compares against the server's differ from the server's spelling.
tmp=${TMPDIR:-/tmp}
work=$(mktemp -d "${tmp%/}/harbor-lifecycle.XXXXXX")
# Servers hold the canonical spelling (macOS resolves /var to /private/var),
# which is too long for a unix socket to live under, so $work stays as typed
# and cleanup matches either form when it inspects a command line.
work_real=$(cd "$work" && pwd -P)
export HARBOR_HOME="$work"
# Short windows so departure is waitable. Test-only overrides, not API: the
# shipped constants are 30s grace / 3s linger.
export HARBOR_STARTUP_GRACE_MS=5000
export HARBOR_LINGER_MS=1500
cleanup() {
  local sock pid tracked args
  # Bash 3.2 treats an empty array expansion as unbound under `set -u`, even
  # after declaration. The skipped sentinel keeps all expansions portable.
  local -a pids=("")

  # Keep direct children even if they were interrupted before binding their
  # socket. Clear these variables after an intentional wait below so a PID
  # that has already exited can never be mistaken for something new.
  for tracked in "${srv:-}" "${csrv:-}" "${tsrv:-}"; do
    [[ $tracked =~ ^[0-9]+$ ]] && pids+=("$tracked")
  done

  # Spawn-on-use servers are reparented and are not shell children. Discover
  # them through this test's private runtime registry instead of matching a
  # command line: the database precedes `start`, paths contain regex syntax,
  # and either detail makes pkill an unreliable cleanup mechanism.
  for sock in "$work"/runtime/*.sock; do
    [[ -S $sock ]] || continue
    pid=$(curl -s --max-time 1 --unix-socket "$sock" http://harbor/info \
      | python3 -c 'import json,sys; print(json.load(sys.stdin).get("pid",""))' 2>/dev/null)
    [[ $pid =~ ^[0-9]+$ ]] && pids+=("$pid")
  done

  for pid in "${pids[@]}"; do
    [[ -n $pid ]] && kill -TERM "$pid" 2>/dev/null
  done
  sleep 0.5
  for pid in "${pids[@]}"; do
    [[ -n $pid ]] || continue
    args=$(ps -p "$pid" -o command= 2>/dev/null)
    [[ $args == *"$work"* || $args == *"$work_real"* ]] && kill -KILL "$pid" 2>/dev/null
  done
  for pid in "${pids[@]}"; do
    [[ -n $pid ]] && wait "$pid" 2>/dev/null
  done
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

fails=0
ok() { printf '  ✓ %s\n' "$1"; }
bad() {
  printf '  ✗ %s\n' "$1"
  fails=$((fails + 1))
}
check() { # check <description> <expected-exit> <required-substring> <cmd...>
  local desc=$1 want=$2 pat=$3 out status=0
  shift 3
  out=$("$@" 2>&1) || status=$?
  if [[ $status -ne $want ]]; then
    bad "$desc (exit $status, wanted $want): $out"
    return
  fi
  if [[ -n $pat && $out != *"$pat"* ]]; then
    bad "$desc (missing \"$pat\"): $out"
    return
  fi
  ok "$desc"
}
# The one socket the database file derives to, live right now or "".
live_sock() { ls "$work"/runtime/*.sock 2>/dev/null | head -1; }
server_pid() { # via /info over the socket — the registry IS the server
  local sock; sock=$(live_sock)
  [[ -n $sock ]] || { echo ""; return; }
  curl -s --max-time 2 --unix-socket "$sock" http://harbor/info \
    | python3 -c 'import json,sys; print(json.load(sys.stdin).get("pid",""))' 2>/dev/null
}
wait_gone() { # poll until no socket answers (the departure), bounded
  for _ in $(seq 1 60); do
    [[ -z $(live_sock) ]] && return 0
    sleep 0.2
  done
  return 1
}

echo "— spawn on use"
check "a file target spawns a server and answers" 0 "42" \
  "$harbor" "$work/x.duckdb" --mode csv -c "SELECT 42 AS answer"
pid1=$(server_pid)
[[ -n $pid1 ]] && ok "the server registered itself (its socket answers /info)" \
               || bad "no server answering after spawn"
check "a second client joins, not spawns" 0 "42" \
  "$harbor" "$work/x.duckdb" --mode csv -c "SELECT 42 AS answer"
pid2=$(server_pid)
[[ -n $pid1 && $pid1 == "$pid2" ]] && ok "same server both times (pid $pid1)" \
                                   || bad "the second client raised a second server ($pid1 vs $pid2)"
check "the list shows the database" 0 "x.duckdb" "$harbor"
# A transaction is one connection's, and the client sends a statement a
# request: it holds a session from BEGIN to COMMIT or ROLLBACK, so the work
# between them is undone or kept together, never committed a statement at a
# time behind a BEGIN that held nothing.
kept="SELECT 'kept=' || count(*) AS kept FROM txn"
check "a rollback undoes everything since its begin" 0 "left=0" \
  "$harbor" "$work/x.duckdb" --mode csv -c \
  "CREATE TABLE txn(a INT); BEGIN; INSERT INTO txn VALUES (1); CREATE TABLE txn_inner(a INT); ROLLBACK; SELECT 'left=' || ((SELECT count(*) FROM txn) + (SELECT count(*) FROM duckdb_tables() WHERE table_name = 'txn_inner')) AS left_behind"
check "a commit keeps it" 0 "kept=2" \
  "$harbor" "$work/x.duckdb" --mode csv -c "BEGIN; INSERT INTO txn VALUES (1), (2); COMMIT; $kept"
check "a script that fails inside a transaction says it was rolled back" 1 "has been rolled back" \
  "$harbor" "$work/x.duckdb" --mode csv -c \
  "BEGIN; INSERT INTO txn VALUES (3); INSERT INTO no_such_table VALUES (1); COMMIT"
check "and nothing of it stays" 0 "kept=2" "$harbor" "$work/x.duckdb" --mode csv -c "$kept"
check "a script that ends with its transaction open is rolled back too" 0 "has been rolled back" \
  "$harbor" "$work/x.duckdb" --mode csv -c "BEGIN; INSERT INTO txn VALUES (4)"
check "leaving the count where it was" 0 "kept=2" "$harbor" "$work/x.duckdb" --mode csv -c "$kept"
# A COMMIT the parser refuses never reached the engine: the transaction is
# still open, and the script that stops there has it rolled back, out loud.
check "a commit that does not parse leaves the transaction to be rolled back" 1 "has been rolled back" \
  "$harbor" "$work/x.duckdb" --mode csv -c "BEGIN; INSERT INTO txn VALUES (5); COMMIT nonsense"
check "and keeps nothing" 0 "kept=2" "$harbor" "$work/x.duckdb" --mode csv -c "$kept"
# The same words behind a byte order mark, which an editor may put first.
check "a transaction behind a byte order mark is a transaction" 0 "kept=2" \
  bash -c "printf '\\xef\\xbb\\xbfBEGIN; INSERT INTO txn VALUES (6); ROLLBACK; %s' \"$kept\" | '$harbor' '$work/x.duckdb' --mode csv"
# An analyzed EXPLAIN runs what it explains, so it ends the transaction too.
check "an analyzed commit commits, and the session goes back" 0 "kept=3" \
  "$harbor" "$work/x.duckdb" --mode csv -c "BEGIN; INSERT INTO txn VALUES (7); EXPLAIN ANALYZE COMMIT; $kept"
# The release can reach the server a moment before the request that preceded
# it has let the session go; the reaper takes it on its next pass.
held=1
for _ in $(seq 1 20); do
  curl -s --unix-socket "$(live_sock)" http://harbor/sql/sessions | grep -q '"live":0' && { held=0; break; }
  sleep 0.1
done
[[ $held -eq 0 ]] && ok "and no session is left held on the server" \
                  || bad "a session is still held: $(curl -s --unix-socket "$(live_sock)" http://harbor/sql/sessions)"
# EXPLAIN's answer is a drawing: the boxed mode prints it as the engine drew
# it, never as a cell with `\n` in it cut off at the column edge.
plan=$("$harbor" "$work/x.duckdb" -c "EXPLAIN SELECT 42 AS answer" 2>/dev/null)
if [[ $plan == *"Projection"* && $plan != *'\n'* && $plan != *"explain_value"* ]]; then
  ok "explain prints the plan itself, not a table of it"
else
  bad "explain came out boxed or escaped: $plan"
fi
check "a bare word is refused, never served" 1 "names nothing running" \
  "$harbor" nosuchname -c "SELECT 1"
if [[ -f $work/nosuchname ]]; then bad "a bare word conjured a file"; else ok "no file conjured for a bare word"; fi

echo "— --block-size shapes the file a summon creates"
# Block size is fixed when a database is CREATED, and a summon is one of the
# ways a database gets created — so the flag has to reach the server this
# client spawns, not just an explicit `start`.
check "a summoned database takes the block size it was asked for" 0 "16384" \
  "$harbor" "$work/sized.duckdb" --block-size 16k --mode csv \
  -c "CREATE TABLE t AS SELECT 1 AS i; CHECKPOINT; SELECT block_size FROM pragma_database_size()"
# ...and says so when it reached nothing, because the file already existed and
# a server is already holding it. Silence here would read as success.
check "the size is refused quietly when there is nothing to shape" 0 "was not used" \
  "$harbor" "$work/sized.duckdb" --block-size 64k -c "SELECT 1"
check "a size DuckDB will not take is refused by the client" 1 "power of two" \
  "$harbor" "$work/other.duckdb" --block-size 48k -c "SELECT 1"
if [[ -f $work/other.duckdb ]]; then bad "a refused size still conjured a file"; else ok "a refused size conjures no file"; fi
"$harbor" "$work/sized.duckdb" stop >/dev/null 2>&1

echo "— the client reads a script as the duckdb shell does, and writes for programs"
# csv is the program format: NULL, the word NULL and the empty string are
# three values there, the way DuckDB's COPY writes them.
check "csv writes NULL as an empty field and an empty string quoted" 0 ',NULL,"",1' \
  "$harbor" "$work/x.duckdb" --mode csv -c "SELECT NULL::VARCHAR AS a, 'NULL' AS b, '' AS c, 1 AS d"
check "markdown escapes a bar inside a cell, and --mode=markdown is --mode markdown" 0 '| a\|b |' \
  "$harbor" "$work/x.duckdb" --mode=markdown -c "SELECT 'a|b' AS v"
out=$("$harbor" "$work/x.duckdb" -c "CREATE OR REPLACE TABLE ddl(i INT)" 2>&1)
[[ $out != *Count* && $out != *"0 rows"* ]] && ok "a statement that changes only the schema draws nothing" \
                                            || bad "a schema change drew a frame: $out"
# A reader that leaves early is the Unix goodbye. The plan goes out in one
# write, and nothing after a closed pipe runs: its reader is gone.
plan_sql="EXPLAIN SELECT 1 $(printf 'UNION ALL SELECT %d ' $(seq 400))"
rc=$("$harbor" "$work/x.duckdb" -c "$plan_sql" 2>"$work/epipe.err" | head -1 >/dev/null; echo "${PIPESTATUS[0]}")
[[ $rc == 0 && $(cat "$work/epipe.err") != *panicked* ]] && ok "a plan into a closed pipe ends quietly" \
                                                        || bad "a plan into a closed pipe: exit $rc, $(cat "$work/epipe.err")"
"$harbor" "$work/x.duckdb" --mode csv \
  -c "SELECT * FROM range(200000); CREATE TABLE after_pipe AS SELECT 1 AS x" 2>/dev/null | head -1 >/dev/null
check "a closed output stops the script, so nothing after it runs" 0 "0" \
  "$harbor" "$work/x.duckdb" --mode csv -c "SELECT count(*) FROM duckdb_tables() WHERE table_name = 'after_pipe'"
check "a mistyped mode is refused before anything opens" 1 'unknown mode "bogus"' \
  "$harbor" "$work/typo.duckdb" --mode bogus -c "SELECT 1"
[[ -e $work/typo.duckdb ]] && bad "a mistyped mode conjured a file" || ok "and conjures no file"
check "dot commands run in a piped script" 0 "one
1
(none)" \
  bash -c "printf '.mode csv\nSELECT 1 AS one;\n.nullvalue (none)\n.mode list\nSELECT NULL;\n' | '$harbor' '$work/x.duckdb'"
printf '.mode csv\nSELECT 2 AS two;\n' >"$work/a script.sql"
check ".read runs a file's dot commands too, from a path with a space" 0 "two
2" \
  bash -c "printf '.read %s\n' '$work/a script.sql' | '$harbor' '$work/x.duckdb'"
# A dot command that fails ends a script, as a statement that fails does.
printf 'SELECT * FROM no_such_table;\n' >"$work/bad.sql"
check "a .read whose statement fails fails the script" 1 "no_such_table" \
  bash -c "printf '.read %s\nCREATE TABLE after_read(x INT);\n' '$work/bad.sql' | '$harbor' '$work/x.duckdb'"
check "and nothing after it runs" 0 "0" \
  "$harbor" "$work/x.duckdb" --mode csv -c "SELECT count(*) FROM duckdb_tables() WHERE table_name = 'after_read'"
for dot in ".read $work/no-such-file.sql" ".mdoe csv" ".mode cvs"; do
  check "$dot fails a script" 1 "harbor:" bash -c "printf '%s\nSELECT 1;\n' '$dot' | '$harbor' '$work/x.duckdb'"
done
printf '.read %s\n' "$work/self.sql" >"$work/self.sql"
check "a file that reads itself ends, and says why" 1 "more than 32 deep" \
  bash -c "printf '.read %s\n' '$work/self.sql' | '$harbor' '$work/x.duckdb'"
check "an empty -c is refused" 1 "no SQL given" "$harbor" "$work/x.duckdb" -c ""
check "as is one that is only a comment" 1 "no SQL given" "$harbor" "$work/x.duckdb" -c "-- nothing"
check "a second -c is refused, not dropped" 1 "-c is given once" \
  "$harbor" "$work/x.duckdb" -c "SELECT 1" -c "SELECT 2"
check "stdin that is not UTF-8 says so" 1 "not UTF-8" \
  bash -c "printf 'SELECT \\xff;' | '$harbor' '$work/x.duckdb'"
check "a scheme in capitals is a URL, not a file" 1 "cannot reach harbor" \
  "$harbor" "HTTP://127.0.0.1:1" -c "SELECT 1"
# The REPL's history holds whatever was typed, so it is made where only its
# user can read it. A URL with nothing behind it is enough to reach the prompt.
mkdir -m 755 "$work/home"
history=0
HARBOR_HOME="$work/home" python3 - "$harbor" "$work/home" <<'PY' >"$work/history.log" 2>&1 || history=$?
import os, pty, select, sys, time
harbor, home = sys.argv[1], sys.argv[2]
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm"
    os.execv(harbor, [harbor, "http://127.0.0.1:1"])
    os._exit(1)
buf = b""
def drain(seconds):
    global buf
    end = time.time() + seconds
    while time.time() < end:
        if not select.select([fd], [], [], 0.2)[0]:
            continue
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            return
        if not chunk:
            return
        buf += chunk
        if b"\x1b]11;?" in chunk: os.write(fd, b"\x1b]11;rgb:1c1c/1c1c/1c1c\x07")
        if b"\x1b[6n" in chunk:   os.write(fd, b"\x1b[1;1R")
        if b"\x1b[c" in chunk:    os.write(fd, b"\x1b[?62;22c")
drain(3)
os.write(fd, b".mode csv\r"); drain(1)
os.write(fd, b".mode\r");     drain(1)
os.write(fd, b".quit\r");     drain(2)
for close in (lambda: os.close(fd), lambda: os.waitpid(pid, 0)):
    try: close()
    except Exception: pass
out = buf.decode("utf8", "replace")
print(out[-2000:])
mode = os.stat(home).st_mode & 0o777
print(f"home mode {mode:o}")
sys.exit(0 if "mode: csv" in out and mode == 0o700 else 1)
PY
(( history == 0 )) && ok "the prompt's history lives in a directory only its user can read" \
                   || bad "the history directory stayed open, or the prompt misbehaved (see $work/history.log)"
# A terminal over a slow line answers the background-color query slowly. Its
# answer must not reach the prompt as typed text: once a terminal has begun
# to answer, the REPL waits for the answer to DA1, which it sends last.
late=0
HARBOR_HOME="$work/home" python3 - "$harbor" <<'PY' >"$work/late.log" 2>&1 || late=$?
import os, pty, select, sys, time
harbor = sys.argv[1]
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm"
    os.execv(harbor, [harbor, "http://127.0.0.1:1"])
    os._exit(1)
buf, due = b"", []
def drain(seconds):
    global buf, due
    end = time.time() + seconds
    while time.time() < end:
        if due and time.time() >= due[0][0]:
            os.write(fd, due.pop(0)[1])
        if not select.select([fd], [], [], 0.02)[0]:
            continue
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            return
        if not chunk:
            return
        buf += chunk
        if b"\x1b]11;?" in chunk:
            now = time.time()
            due = [(now + 0.05, b"\x1b]11;rgb:1c1c/1c1c/1c1c"), (now + 0.4, b"\x07\x1b[?62;22c")]
        if b"\x1b[6n" in chunk:   os.write(fd, b"\x1b[1;1R")
drain(3)
os.write(fd, b".mode\r"); drain(1.5)
os.write(fd, b".quit\r"); drain(1.5)
for close in (lambda: os.close(fd), lambda: os.waitpid(pid, 0)):
    try: close()
    except Exception: pass
out = buf.decode("utf8", "replace")
print(out[-2000:])
sys.exit(0 if "mode: duckbox" in out and "rgb:" not in out.split("\x1b]11;?")[-1] else 1)
PY
(( late == 0 )) && ok "a slow answer to the background query never reaches the prompt" \
                || bad "a slow terminal answer was typed into the prompt (see $work/late.log)"
# A terminal that answers neither query (Emacs's shell, a serial console)
# costs the short wait for a first byte at every start, not the long one.
silent=0
HARBOR_HOME="$work/home" python3 - "$harbor" <<'PY' >"$work/silent.log" 2>&1 || silent=$?
import os, pty, select, sys, time
harbor = sys.argv[1]
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm"
    os.execv(harbor, [harbor, "http://127.0.0.1:1"])
    os._exit(1)
asked = waited = None
end = time.time() + 5
while time.time() < end and waited is None:
    if not select.select([fd], [], [], 0.01)[0]:
        continue
    try:
        chunk = os.read(fd, 65536)
    except OSError:
        break
    now = time.time()
    if asked is not None and chunk:
        waited = now - asked
    if b"\x1b]11;?" in chunk:
        asked = now
    if b"\x1b[6n" in chunk:
        os.write(fd, b"\x1b[1;1R")
os.write(fd, b".quit\r")
time.sleep(0.5)
for close in (lambda: os.close(fd), lambda: os.waitpid(pid, 0)):
    try: close()
    except Exception: pass
print(f"waited {waited}")
sys.exit(0 if waited is not None and waited < 0.5 else 1)
PY
(( silent == 0 )) && ok "a terminal that answers nothing costs a start no more than a moment" \
                  || bad "a silent terminal held the start (see $work/silent.log)"

echo "— the server is everyone's: it lives while anyone is connected"
sock=$(live_sock)
# The mooring belongs to the CLIENT, so only the shipped client can prove it
# holds — a hand-rolled socket here would test justhttp and let harbor's own
# anchor rot. The repl wants a terminal, so it gets one. A statement, a pause
# longer than the first-request timeout (60s, the clock that reclaims a
# connection which has never spoken), then a second statement. That second
# one is where a berth which departed while its human was thinking says
# "cannot reach harbor". Nothing shorter than the real clock can see it.
repl=0
python3 - "$harbor" "$work/x.duckdb" <<'PY' >"$work/repl.log" 2>&1 || repl=$?
import os, pty, select, sys, time
harbor, db = sys.argv[1], sys.argv[2]
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm"
    os.execv(harbor, [harbor, db])
    os._exit(1)
buf = b""
def drain(seconds):          # read, and answer the queries a terminal owes
    global buf
    end = time.time() + seconds
    while time.time() < end:
        if not select.select([fd], [], [], 0.2)[0]:
            continue
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            return
        if not chunk:
            return
        buf += chunk
        if b"\x1b]11;?" in chunk: os.write(fd, b"\x1b]11;rgb:1c1c/1c1c/1c1c\x07")
        if b"\x1b[6n" in chunk:   os.write(fd, b"\x1b[1;1R")
        if b"\x1b[c" in chunk:    os.write(fd, b"\x1b[?62;22c")
drain(5)
os.write(fd, b"SELECT 1 AS early;\r"); drain(3)
# A transaction left open across the pause: the prompt keeps its session past
# the server's thirty-second idle limit, which would otherwise roll it back.
os.write(fd, b"CREATE TABLE held(a INT);\r");     drain(2)
os.write(fd, b"BEGIN;\r");                        drain(2)
os.write(fd, b"INSERT INTO held VALUES (1);\r");  drain(2)
drain(65)
os.write(fd, b"SELECT 2 AS late;\r");  drain(5)
os.write(fd, b"SELECT 'inside=' || count(*) AS seen FROM held;\r"); drain(3)
os.write(fd, b"ROLLBACK;\r");                                      drain(2)
os.write(fd, b"SELECT 'after=' || count(*) AS seen FROM held;\r");  drain(3)
os.write(fd, b".quit\r");              drain(3)
for close in (lambda: os.close(fd), lambda: os.waitpid(pid, 0)):
    try: close()
    except Exception: pass
out = buf.decode("utf8", "replace")
print(out[-3000:])
if "cannot reach harbor" in out or "late" not in out:
    sys.exit(1)
sys.exit(0 if "inside=1" in out and "after=0" in out and "transaction is over" not in out else 2)
PY
if (( repl != 1 )); then ok "the repl's own anchor moors the berth across a 65s pause"
else bad "the berth departed while the repl still held it (see $work/repl.log)"; fi
if (( repl == 0 )); then ok "and its open transaction outlives the server's idle limit, then rolls back when told"
else bad "the transaction did not survive the pause, or its rollback kept the row (see $work/repl.log)"; fi
if wait_gone; then ok "the last departure ends the server"; else bad "the server outlived its last client"; fi
if [[ ! -e $sock ]]; then ok "it swept its socket on the way out"; else bad "departure left the socket behind"; fi
if [[ -f $work/x.duckdb && ! -e $work/x.duckdb.wal ]]; then
  ok "the database stays ashore, checkpointed (no wal)"
else
  bad "departure left the database missing or with a wal"
fi

echo "— start: the server is yours, it lives until you stop it"
"$harbor" "$work/x.duckdb" start >"$work/start.log" 2>&1 &
srv=$!
up=0
for _ in $(seq 1 50); do [[ -n $(live_sock) ]] && { up=1; break; }; sleep 0.1; done
(( up )) && ok "start came up" || bad "start never came up: $(cat "$work/start.log")"
check "a client joins the started database" 0 "7" \
  "$harbor" "$work/x.duckdb" --mode csv -c "SELECT 7 AS seven"
# Well past the linger an ephemeral server would have left on. A start
# lifetime has no clock at all — only SIGTERM or `stop` ends it.
sleep 2
kill -0 "$srv" 2>/dev/null && ok "no refcount: it survives its clients leaving" \
                           || bad "the start-lifetime server left on the refcount"
check "a second start on the same file is refused" 1 "already being served" \
  "$harbor" "$work/x.duckdb" start
kill -TERM "$srv"
wait "$srv" 2>/dev/null
srv=""
if [[ -z $(live_sock) && ! -e $work/x.duckdb.wal ]]; then
  ok "SIGTERM departs clean: socket swept, database checkpointed"
else
  bad "SIGTERM left residue (socket or wal)"
fi

echo "— the grammar is database-first"
check "verb-first is redirected, not parsed" 1 "database comes first" \
  "$harbor" start "$work/x.duckdb"

echo "— the list is the truth, and sweeps what is not"
python3 - "$work/runtime/dead-cafe0000.sock" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])  # bound then abandoned: the kill -9 leftover shape
s.close()
PY
check "nothing running says so" 0 "Nothing running" "$harbor"
if [[ ! -e $work/runtime/dead-cafe0000.sock ]]; then
  ok "a stale socket is unlinked by the list"
else
  bad "the stale socket survived the list"
fi

echo "— config supplies a berth's standing settings"
# A bare start reads this file's entry and applies it — no flags. The marker
# table proves the init SQL ran; the memory-limit proves a typed field lands
# (1234MB is 1.1 GiB, distinct from the 2GB / 1.9 GiB default).
cat > "$work/config.toml" <<TOML
[connection.cfg]
path = "$work/cfg.duckdb"
memory-limit = "1234MB"
init = ["CREATE TABLE cfg_marker(x INTEGER)", "INSERT INTO cfg_marker VALUES (7)"]

[connection.cfg.settings]
preserve_insertion_order = false

[connection.tcp]
path = "$work/tcp.duckdb"
port = 9531
TOML
chmod 600 "$work/config.toml"
"$harbor" "$work/cfg.duckdb" start >"$work/cfg.log" 2>&1 &
csrv=$!
cup=0
for _ in $(seq 1 50); do
  "$harbor" "$work/cfg.duckdb" -c "SELECT 1" >/dev/null 2>&1 && { cup=1; break; }
  sleep 0.1
done
(( cup )) || bad "config-started server never came up: $(cat "$work/cfg.log")"
check "a bare start runs the config's init SQL" 0 "7" \
  "$harbor" "$work/cfg.duckdb" --mode csv -c "SELECT x FROM cfg_marker"
check "a bare start applies the config's memory-limit" 0 "1.1 GiB" \
  "$harbor" "$work/cfg.duckdb" --mode csv -c "SELECT current_setting('memory_limit')"
check "a [settings] key becomes a SET (default is true)" 0 "false" \
  "$harbor" "$work/cfg.duckdb" --mode csv -c "SELECT current_setting('preserve_insertion_order')"
kill -TERM "$csrv" 2>/dev/null
wait "$csrv" 2>/dev/null
csrv=""

# Exposure from config: an explicit start honors the entry's port, while a
# summon ignores it and stays on the unix socket, so opening the file never
# lands on TCP.
check "a summon ignores the config port (unix socket answers)" 0 "5" \
  "$harbor" "$work/tcp.duckdb" --mode csv -c "SELECT 5 AS five"
wait_gone
"$harbor" "$work/tcp.duckdb" start >"$work/tcp.log" 2>&1 &
tsrv=$!
tup=0
for _ in $(seq 1 50); do
  curl -sf http://127.0.0.1:9531/ready >/dev/null 2>&1 && { tup=1; break; }
  sleep 0.1
done
(( tup )) && ok "an explicit start binds the config port" \
          || bad "config port never answered: $(cat "$work/tcp.log")"
check "and serves on it" 0 "9" \
  "$harbor" http://127.0.0.1:9531 --mode csv -c "SELECT 9 AS nine"
kill -TERM "$tsrv" 2>/dev/null
wait "$tsrv" 2>/dev/null
tsrv=""

echo "— a config that will not load stops a start, and says why"
# It may hold `sealed`, a statement ceiling or the boot SQL; a server that
# came up without them would look configured while being open.
mv "$work/config.toml" "$work/config.good"
printf '[connection.typo]\npath = "%s/typo.duckdb"\npth = 1\n' "$work" > "$work/config.toml"
chmod 600 "$work/config.toml"
check "a start refuses, naming the file and the key" 1 "config.toml is not valid" \
  "$harbor" "$work/typo.duckdb" start
check "and so does the server a client summons, saying where on one line" 1 "config.toml is not valid: line 3, column 1: unknown field \`pth\`" \
  "$harbor" "$work/typo.duckdb" -c "SELECT 1"
printf '[defaults]\nmode = "csv"\n' > "$work/config.toml"
check "a [defaults] section is refused with the fix" 1 "delete that section" \
  "$harbor" "$work/typo.duckdb" start
chmod 664 "$work/config.toml"
check "a config others can write is refused with the fix" 1 "chmod go-w" \
  "$harbor" "$work/typo.duckdb" start
[[ -e $work/typo.duckdb ]] && bad "a refused start made the database" || ok "and nothing was made"
mv "$work/config.good" "$work/config.toml"

echo "— a restart brings a server back as it was"
wait_gone
mkdir -p "$work/from" "$work/elsewhere"
check "a restart with options brings one up in the background" 0 "serving" \
  "$harbor" "$work/again.duckdb" restart --port 9532 --sealed --init "SELECT 'hunter2'"
check "a bare restart" 0 "stopped" "$harbor" "$work/again.duckdb" restart
check "keeps the port it was started with" 0 "9" \
  "$harbor" http://127.0.0.1:9532 --mode csv -c "SELECT 9 AS nine"
check "and stays sealed" 0 "false" \
  "$harbor" "$work/again.duckdb" --mode csv -c "SELECT current_setting('enable_external_access')"
( cd "$work/from" && "$harbor" "$work/cwd.duckdb" restart --init "ATTACH 'beside.duckdb' AS beside" >/dev/null 2>&1 )
( cd "$work/elsewhere" && "$harbor" "$work/cwd.duckdb" restart >/dev/null 2>&1 )
check "a restart reads a relative path from where the server was started" 0 "$work_real/from/beside.duckdb" \
  "$harbor" "$work/cwd.duckdb" --mode csv -c "SELECT path FROM duckdb_databases() WHERE database_name = 'beside'"
"$harbor" "$work/cwd.duckdb" stop >/dev/null 2>&1
# The options can hold a secret, and /info answers whoever reaches the port.
info=$(curl -s --max-time 2 http://127.0.0.1:9532/info)
[[ $info == *'"pid"'* && $info != *hunter2* && $info != *'"args"'* ]] \
  && ok "/info over TCP publishes no start options" || bad "/info shows the start options: $info"
args_file=$(ls "$work"/runtime/*.args 2>/dev/null | head -1)
mode=$(python3 -c 'import os,sys; print(oct(os.stat(sys.argv[1]).st_mode & 0o777))' "$args_file" 2>/dev/null)
[[ $mode == 0o600 && $(cat "$args_file") == *hunter2* ]] && ok "they are kept beside the socket, for its user alone" \
                                                        || bad "the options are not kept privately: ${args_file:-none} $mode"
# A restart that could not start again stops nothing.
cp "$work/config.toml" "$work/config.good"
printf '[connection.typo]\npath = "%s/typo.duckdb"\npth = 1\n' "$work" > "$work/config.toml"
check "a restart under a config that will not load refuses, naming the line" 1 "line 3, column 1" \
  "$harbor" "$work/again.duckdb" restart
check "and stops nothing" 0 "9" "$harbor" http://127.0.0.1:9532 --mode csv -c "SELECT 9 AS nine"
mv "$work/config.good" "$work/config.toml"
check "nor does one whose typed option will not parse" 1 "was not stopped" \
  "$harbor" "$work/again.duckdb" restart --port nine
# A server that kept no record of its options, as one started by an older
# harbor, is not restarted on a guess: it would come back open, or without
# its port.
mv "$args_file" "$work/args.kept"
check "a server with no record of its options is refused, its port named" 1 "--port 9532" \
  "$harbor" "$work/again.duckdb" restart
check "and left running" 0 "9" "$harbor" http://127.0.0.1:9532 --mode csv -c "SELECT 9 AS nine"
mv "$work/args.kept" "$args_file"
check "a server is stopped by its URL, read as the client reads one" 0 "stopped" "$harbor" HTTP://127.0.0.1:9532/ stop
[[ -z $(live_sock) && -z $(ls "$work"/runtime/*.args 2>/dev/null) ]] && ok "drained, with its socket and its options gone" \
                                                                   || bad "the URL stop left a server: $(ls "$work"/runtime)"
"$harbor" "$work/again.duckdb" detach restart >/dev/null 2>&1
"$harbor" "$work/again.duckdb" restart >/dev/null 2>&1
eph=$(curl -s --max-time 2 --unix-socket "$(live_sock)" http://harbor/info \
  | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ephemeral"))' 2>/dev/null)
[[ $eph == True ]] && ok "a server that leaves with its clients comes back that way" \
                   || bad "the restart changed the lifetime (ephemeral: $eph)"
wait_gone

echo "— a name belongs to a file"
mkdir -p "$work/a" "$work/b"
"$harbor" "$work/a/data.duckdb" attach >/dev/null 2>&1
check "detaching another file with the same stem" 0 "was not attached" \
  "$harbor" "$work/b/data.duckdb" detach
grep -q "a/data.duckdb" "$work/config.toml" && ok "leaves the attached one's entry" \
                                          || bad "it removed a/data.duckdb's entry"
check "detaching the file itself" 0 "detached data" "$harbor" "$work/a/data.duckdb" detach
# A login item filed under a name another file's item holds is refused
# before the attach `autostart` implies, which would otherwise stay. The
# item is a stand-in in a scratch home, read and never loaded.
mkdir -p "$work/fakehome/Library/LaunchAgents" "$work/xdg/systemd/user"
printf '<plist><dict><key>ProgramArguments</key><array><string>/x/harbor</string><string>%s/elsewhere/kx.duckdb</string><string>start</string></array></dict></plist>\n' \
  "$work" >"$work/fakehome/Library/LaunchAgents/harbor.kx.plist"
printf '[Service]\nExecStart=/x/harbor %s/elsewhere/kx.duckdb start\n' "$work" >"$work/xdg/systemd/user/harbor-kx.service"
check "autostart under another file's login item is refused" 1 "frees the name" \
  env HOME="$work/fakehome" XDG_CONFIG_HOME="$work/xdg" "$harbor" "$work/kx.duckdb" autostart
grep -q '^\[connection\.kx\]' "$work/config.toml" && bad "and its attach stayed" || ok "and attaches nothing"
check "attach refuses a name the CLI reads as a verb" 1 "reads as a verb" \
  "$harbor" "$work/start.duckdb" attach
check "a stem past the name law still opens" 0 "42" \
  "$harbor" "$work/$(printf 'y%.0s' $(seq 1 100)).duckdb" --mode csv -c "SELECT 42"
check "a relative HARBOR_HOME is refused, not ignored" 1 "absolute path" env HARBOR_HOME=rel "$harbor"
check "a database's own -h is the whole help" 0 "start options" "$harbor" "$work/x.duckdb" -h
wait_gone

# — backup and restore ---------------------------------------------------
#
# The round trip is the whole claim, and only a real EXPORT/IMPORT pair can
# make it: the three-value dialect (bare NULL, quoted "NULL", empty field),
# a sequence's current value, and a block size that only exists because the
# restore created the file.
echo
echo "— backup and restore"
wait_gone
"$harbor" "$work/bk.duckdb" -c "
  CREATE TABLE t(id INTEGER, s VARCHAR);
  INSERT INTO t VALUES (1, 'NULL'), (2, ''), (3, NULL);
  CREATE SEQUENCE bkseq START 42;" >/dev/null 2>&1
wait_gone
check "backup writes a directory of tab-separated files" 0 "backed up 1 table" \
  "$harbor" "$work/bk.duckdb" backup "$work/bk.out"
[[ -f $work/bk.out/schema.sql && -f $work/bk.out/load.sql && -f $work/bk.out/t.csv ]] \
  && ok "schema.sql, load.sql and one .csv per table" \
  || bad "the backup directory is missing a file: $(ls "$work/bk.out" 2>&1)"
check "and refuses to write into a directory that is there" 1 "already exists" \
  "$harbor" "$work/bk.duckdb" backup "$work/bk.out"
check "a directory named with a trailing slash is that directory" 0 "backed up 1 table" \
  "$harbor" "$work/bk.duckdb" backup "$work/slash.out/"
[[ -f $work/slash.out/load.sql ]] && ok "and it holds the backup" || bad "a trailing slash: $(ls -a "$work/slash.out" 2>&1)"
# Each COPY names its file and nothing more. An absolute path would nail the
# directory to the machine that wrote it, which is the opposite of the point.
grep -q "FROM 't.csv'" "$work/bk.out/load.sql" \
  && ok "load.sql names its files relatively, so the directory can move" \
  || bad "load.sql carries a path: $(grep FROM "$work/bk.out/load.sql")"
wait_gone

check "restore builds a new database at a chosen block size" 0 "restored 1 table" \
  "$harbor" "$work/rs.duckdb" restore "$work/bk.out" --block-size 16k
check "the block size is the restore's, not the original's" 0 "16384" \
  "$harbor" "$work/rs.duckdb" --mode csv -c "SELECT block_size FROM pragma_database_size()"
# Bare NULL is a null and quoted "NULL" is the string: the failure this
# guards is duckdb#25501, where load.sql reads both back as NULL.
check "a bare NULL restores as a null" 0 "1" \
  "$harbor" "$work/rs.duckdb" --mode csv -c "SELECT count(*) FROM t WHERE s IS NULL"
check 'a quoted "NULL" restores as the string' 0 "1" \
  "$harbor" "$work/rs.duckdb" --mode csv -c "SELECT count(*) FROM t WHERE s = 'NULL'"
check "an empty string restores as an empty string" 0 "1" \
  "$harbor" "$work/rs.duckdb" --mode csv -c "SELECT count(*) FROM t WHERE s = ''"
# The ordinary file pays nothing for the quoting rule: an empty string beside
# another column is an empty FIELD, which is unambiguous once NULL has a name.
grep -q '^2	$' "$work/bk.out/t.csv" \
  && ok "an empty string stays bare where a bare field is safe" \
  || bad "an empty string was quoted needlessly: $(cat -e "$work/bk.out/t.csv")"
check "a sequence restores at its current value" 0 "42" \
  "$harbor" "$work/rs.duckdb" --mode csv -c "SELECT nextval('bkseq')"
wait_gone

# The reader takes all four spellings, which is what makes the file editable by
# hand: a person fixing one row writes `""` for an empty string without having
# to know that the writer would have left the field bare.
cp -R "$work/bk.out" "$work/bk.hand"
printf 'id\ts\n1\tNULL\n2\t"NULL"\n3\t""\n4\t\n' > "$work/bk.hand/t.csv"
check "hand-written NULL, \"NULL\", \"\" and a bare field all read as documented" 0 "1,true,~
2,false,NULL
3,false,\"\"
4,false,\"\"" \
  bash -c '"$1" "$2" restore "$3" >/dev/null 2>&1
           "$1" "$2" --mode csv -c "SELECT id, s IS NULL, coalesce(s, '"'"'~'"'"') FROM t ORDER BY id" | tail -n +2' \
  _ "$harbor" "$work/hand.duckdb" "$work/bk.hand"
wait_gone

# The row that is only a row if the empty string kept its quotes.
wait_gone
"$harbor" "$work/solo.duckdb" -c "CREATE TABLE s(v VARCHAR);
  INSERT INTO s VALUES ('x'), (''), (NULL);" >/dev/null 2>&1
wait_gone
"$harbor" "$work/solo.duckdb" backup "$work/solo.out" >/dev/null 2>&1
wait_gone
grep -q '^""$' "$work/solo.out/s.csv" \
  && ok "and is quoted in the one place it would be an empty line" \
  || bad "a one-column empty string was left bare: $(cat -e "$work/solo.out/s.csv")"
check "an empty string in a ONE-column table survives the trip" 0 "3 rows, 1 empty" \
  bash -c '"$1" "$2" restore "$3" >/dev/null 2>&1
           "$1" "$2" --mode csv -c "
             SELECT count(*) || '"'"' rows, '"'"' || count(*) FILTER (WHERE length(v) = 0) || '"'"' empty'"'"' FROM s" | tail -1' \
  _ "$harbor" "$work/solo.rs.duckdb" "$work/solo.out"
wait_gone

# The proof of a relative load.sql: move the whole directory and restore again.
mv "$work/bk.out" "$work/bk.moved"
check "a moved backup directory still restores" 0 "restored 1 table" \
  "$harbor" "$work/mv.duckdb" restore "$work/bk.moved"
check "with its rows" 0 "3" \
  "$harbor" "$work/mv.duckdb" --mode csv -c "SELECT count(*) FROM t"
wait_gone
mv "$work/bk.moved" "$work/bk.out"

check "restore refuses a database that exists" 1 "only ever makes a NEW database" \
  "$harbor" "$work/rs.duckdb" restore "$work/bk.out"
check "and refuses a directory that is not a backup" 1 "no load.sql" \
  "$harbor" "$work/no.duckdb" restore "$work"
[[ -e $work/no.duckdb ]] && bad "a refused restore left a file behind" \
                         || ok "a refused restore leaves no file behind"
# A tab-separated file has to carry a tab. RFC-4180 quoting, not backslash
# escapes: the field is wrapped and the tab, newline or quote sits inside it
# literally — so a backslash in the data is just a backslash, and the two
# characters `\t` stay distinct from a real tab.
wait_gone
"$harbor" "$work/hard.duckdb" -c "
  CREATE TABLE h(id INTEGER, s VARCHAR);
  INSERT INTO h VALUES
    (1, e'hey\tyou\tguys!'),
    (2, e'line one\nline two'),
    (3, 'she said \"hi\"'),
    (4, 'back\\slash'),
    (5, 'two chars: \\t');" >/dev/null 2>&1
wait_gone
"$harbor" "$work/hard.duckdb" backup "$work/hard.out" >/dev/null 2>&1
wait_gone
grep -q '"hey	you	guys!"' "$work/hard.out/h.csv" \
  && ok "a value with tabs is quoted, with the tabs literal inside it" \
  || bad "a tabbed value was not quoted: $(grep -c . "$work/hard.out/h.csv") lines"
check "tabs, newlines, quotes and backslashes all round-trip" 0 "5 of 5 survived" \
  bash -c '"$1" "$2" restore "$3" >/dev/null 2>&1
           "$1" "$2" --mode csv -c "
             SELECT count(*) || '"'"' of 5 survived'"'"' FROM h WHERE (id, s) IN (
               (1, e'"'"'hey\tyou\tguys!'"'"'),
               (2, e'"'"'line one\nline two'"'"'),
               (3, '"'"'she said \"hi\"'"'"'),
               (4, '"'"'back\\slash'"'"'),
               (5, '"'"'two chars: \\t'"'"'))" | tail -1' \
  _ "$harbor" "$work/hard.rs.duckdb" "$work/hard.out"
wait_gone

# A backup is written under a name of its own and renamed into place last,
# and a restore the same way, so neither path ever holds a fragment that
# would be taken for the real thing.
mode=$(python3 -c 'import os,sys; print(oct(os.stat(sys.argv[1]).st_mode & 0o777))' "$work/bk.out")
[[ $mode == 0o700 ]] && ok "a backup directory is its owner's alone" || bad "a backup directory is $mode"
"$harbor" "$work/un.duckdb" -c "CREATE TABLE u(x UNION(n INTEGER, s VARCHAR)); INSERT INTO u VALUES (1)" >/dev/null 2>&1
wait_gone
check "a backup that cannot finish fails" 1 "cannot be written as text" \
  "$harbor" "$work/un.duckdb" backup "$work/un.out" --strict
if [[ ! -e $work/un.out ]] && ! ls -d "$work"/un.out.partial-* >/dev/null 2>&1; then
  ok "and leaves no directory, whole or partial"
else
  bad "a failed backup left $(ls -d "$work"/un.out* 2>&1)"
fi
wait_gone
"$harbor" "$work/bk.duckdb" -c "EXPORT DATABASE '$work/raw.out' (FORMAT csv)" >/dev/null 2>&1
wait_gone
check "restore refuses EXPORT DATABASE's own directory" 1 "not a harbor backup" \
  "$harbor" "$work/raw.duckdb" restore "$work/raw.out"
cp -R "$work/bk.out" "$work/bk.bad"
printf 'id\ts\nnot-a-number\tx\n' > "$work/bk.bad/t.csv"
check "a restore whose load fails" 1 "" "$harbor" "$work/bad.duckdb" restore "$work/bk.bad"
if [[ ! -e $work/bad.duckdb ]] && ! ls "$work"/bad.duckdb.restoring-* >/dev/null 2>&1; then
  ok "leaves no database, whole or partial"
else
  bad "a failed restore left $(ls "$work"/bad.duckdb* 2>&1)"
fi
wait_gone

check "neither verb combines with a lifetime verb" 1 "combines with nothing" \
  "$harbor" "$work/bk.duckdb" start backup
check "nor with each other" 1 "runs alone" \
  "$harbor" "$work/bk.duckdb" backup restore
wait_gone

echo "— a data file opens in the workspace"
# A name with a quote, a space, a bracket and a star, beside a decoy its
# glob would match: the view reads the one file named, and no other.
mkdir -p "$work/files"
data="$work/files/it's [q1]*.csv"
printf 'id,name\n1,ann\n2,bob\n' > "$data"
printf 'id,name\n9,decoy\n' > "$work/files/it's [q1]x.csv"
check "a CSV opens as a view named for it, and says so" 0 "it_s_q1: a view over" \
  "$harbor" "$data" --mode csv -c "SELECT 1"
check "the view reads that file and no other" 0 "ann,bob" \
  "$harbor" "$data" --mode csv -c "SELECT string_agg(name, ',' ORDER BY id) AS names FROM it_s_q1"
check "a second opening finds the view the first made" 0 "already in the workspace" \
  "$harbor" "$data" --mode csv -c "SELECT 1"
check "in the workspace database, under HARBOR_HOME" 0 "1" \
  "$harbor" "$work/workspace.duckdb" --mode csv -c "SELECT count(*) FROM duckdb_views() WHERE view_name = 'it_s_q1'"
printf 'id,name\n1,x\n' > "$work/files/order.csv"
check "a reserved word is passed over for the next free name" 0 "order_2: a view over" \
  "$harbor" "$work/files/order.csv" --mode csv -c "SELECT * FROM order_2"
check "a missing data file is refused, not made a database" 1 "no such file" \
  "$harbor" "$work/files/nope.csv" -c "SELECT 1"
[[ ! -e $work/files/nope.csv ]] && ok "and nothing is made there" || bad "a database was made at nope.csv"
printf 'hello' > "$work/files/notes.bin"
check "a file of no kind harbor opens is refused with what was looked at" 1 'begins "hello"' \
  "$harbor" "$work/files/notes.bin" -c "SELECT 1"
printf 'id\n1\n' > "$work/files/fake.parquet"
check "and so is one its bytes contradict" 1 "not Parquet" \
  "$harbor" "$work/files/fake.parquet" -c "SELECT 1"
check "a verb refuses a data file" 1 "opens it in the workspace" \
  "$harbor" "$work/files/order.csv" start
wait_gone

echo "— the workspace is whole at every start"
# An ATTACH lasts as long as the server that ran it, and ST_Read reads only
# where spatial is loaded: a server that opens the workspace attaches what it
# records and loads what its views call into, before it serves anyone.
mkdir -p "$work/files/b"
"$harbor" "$work/mk.duckdb" -c "ATTACH '$work/files/app.sqlite' AS app (TYPE sqlite); CREATE TABLE app.people AS SELECT 1 AS id;
  ATTACH '$work/files/b/app.sqlite' AS b (TYPE sqlite); CREATE TABLE b.t AS SELECT 1 AS id" >/dev/null 2>&1
wait_gone
check "a SQLite file attaches in the workspace" 0 "app: the SQLite database" \
  "$harbor" "$work/files/app.sqlite" --mode csv -c "SELECT count(*) FROM app.people"
printf '{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"elm"},"geometry":{"type":"Point","coordinates":[1,2]}}]}' \
  > "$work/files/parks.geojson"
spatial=1
out=$("$harbor" "$work/files/parks.geojson" --mode csv -c "SELECT name FROM parks" 2>&1) || spatial=0
if (( spatial )); then
  [[ $out == *elm* ]] && ok "a GeoJSON file opens as a view" || bad "the GeoJSON view did not read: $out"
else
  echo "  - spatial did not install here, so the GeoJSON view is not checked: $out"
fi
wait_gone
# The views and the attachment a fresh server shows, one line each, and any
# relation listed in the record's schema.
catalog_lines() {
  curl -s --max-time 10 --unix-socket "$(live_sock)" http://harbor/catalog | python3 -c '
import json, sys
d = json.load(sys.stdin)
for v in d["views"]: print("view", v["name"], "reads" if v["columns"] and not v["error"] else v["error"])
for a in d["attached"]: print("attached", a["name"], [t["name"] for t in a["tables"]], a["error"])
for t in d["tables"]: print("table", t["schema"], t["name"])'
}
workspace_start() {
  "$harbor" "$work/workspace.duckdb" start "$@" >"$work/ws.log" 2>&1 &
  srv=$!
  for _ in $(seq 1 100); do [[ -n $(live_sock) ]] && return; sleep 0.1; done
  bad "the workspace never came up: $(cat "$work/ws.log")"
}
workspace_stop() {
  "$harbor" "$work/workspace.duckdb" stop >/dev/null 2>&1
  wait "$srv" 2>/dev/null
  srv=""
}
workspace_start
lines=$(catalog_lines)
[[ $lines == *"view it_s_q1 reads"* && $lines == *"view order_2 reads"* ]] \
  && ok "after a restart, the CSV views read" || bad "the CSV views after a restart: $lines"
if (( spatial )); then
  [[ $lines == *"view parks reads"* ]] && ok "the GeoJSON view reads, spatial loaded at the start" \
                                       || bad "the GeoJSON view after a restart: $lines"
fi
[[ $lines == *"attached app ['people'] None"* ]] && ok "the SQLite file is attached again" \
                                                  || bad "the SQLite file after a restart: $lines"
[[ $lines != *"table "* ]] && ok "and the record of it is not listed" || bad "the catalog lists a table: $lines"
workspace_stop
mv "$work/files/app.sqlite" "$work/files/app.away"
workspace_start
lines=$(catalog_lines)
[[ $lines == *"attached app [] $work/files/app.sqlite: no such file"* ]] \
  && ok "a recorded file that is gone is said in the catalog, and the server serves" \
  || bad "a gone file after a restart: $lines"
grep -q "app is not attached" "$work/ws.log" && ok "and in its log" || bad "the log does not say so: $(cat "$work/ws.log")"
check "another file of its stem takes the next name, not the gone one's" 0 "app_2: the SQLite database" \
  "$harbor" "$work/files/b/app.sqlite" -c "DETACH app_2"
workspace_stop
mv "$work/files/app.away" "$work/files/app.sqlite"
workspace_start
[[ $(catalog_lines) == *"attached app ['people'] None"* ]] && ok "and it is attached again once it is back" \
                                                          || bad "the file back: $(catalog_lines)"
check "a plain DETACH" 0 "" "$harbor" "$work/workspace.duckdb" -c "DETACH app"
workspace_stop
workspace_start
[[ $(catalog_lines) != *"attached app"* ]] && ok "lasts past the stop" || bad "detached, then: $(catalog_lines)"
workspace_stop

echo "— an attached SQLite file whose catalog does not read"
# Attaching a SQLite file reads nothing of it, and the engine reads every
# attached database's catalog for any catalog read: one that does not read
# would fail them all.
printf 'a,b\n1,2\n' > "$work/files/ab.csv"
workspace_start --init "INSERT INTO _harbor.attached VALUES ('notsq', '$work/files/ab.csv', 'sqlite', false)"
lines=$(catalog_lines)
[[ $lines == *"attached notsq [] "*"file is not a database"* ]] \
  && ok "a recorded file that is not SQLite is detached at the start, and the catalog says why" \
  || bad "a recorded non-SQLite file: $lines"
[[ $lines == *"view it_s_q1 reads"* ]] && ok "and the rest of the catalog reads" || bad "the catalog beside it: $lines"
grep -q "notsq is not attached" "$work/ws.log" && ok "and the log says so" || bad "the log: $(cat "$work/ws.log")"
workspace_stop
check "its row stays in the record" 0 "1" \
  "$harbor" "$work/workspace.duckdb" --mode csv -c "SELECT count(*) FROM _harbor.attached WHERE name = 'notsq'"
wait_gone
# A writer that holds a SQLite file locked, any connection or another
# program: the file stays attached, and a catalog read answers as the
# server last read it, the file saying why, in about the second harbor's
# attachment waits, not the engine's five; a later read asks the file alone.
lock_sqlite() { # lock_sqlite <file>: hold it locked until killed; returns once held
  python3 -c '
import sqlite3, sys, time
c = sqlite3.connect(sys.argv[1], isolation_level=None)
c.execute("BEGIN EXCLUSIVE")
print("locked", flush=True)
time.sleep(60)' "$1" > "$work/lock.out" 2>&1 &
  locker=$!
  for _ in $(seq 1 50); do grep -q locked "$work/lock.out" 2>/dev/null && return; sleep 0.1; done
}
# A file replaced by one that is not SQLite, and put back.
replace_sqlite() {
  mv "$work/files/app.sqlite" "$work/files/app.keep"
  printf 'not a SQLite file, and long enough to be read as one: %.0s' 1 2 3 4 > "$work/files/app.sqlite"
}
restore_sqlite() { mv "$work/files/app.keep" "$work/files/app.sqlite"; }
workspace_start
check "a SQLite file attaches in the workspace again" 0 "app: the SQLite database" \
  "$harbor" "$work/files/app.sqlite" --mode csv -c "SELECT count(*) FROM app.people"
catalog_timed() { # the status and seconds of a catalog read, its lines in $work/cat.json
  curl -s -o "$work/cat.json" -w '%{http_code} %{time_total}\n' --max-time 20 \
    --unix-socket "$(live_sock)" http://harbor/catalog
}
# Attached by the start, from the record, as harbor attaches a SQLite file.
workspace_stop
workspace_start
[[ $(catalog_lines) == *"attached app ['people'] None"* ]] && ok "the start attaches it, and the catalog reads it" \
  || bad "before the lock: $(catalog_lines)"
lock_sqlite "$work/files/app.sqlite"
read -r status secs < <(catalog_timed)
lines=$(catalog_lines)
[[ $status == 200 && $lines == *"attached app ['people'] "*"database is locked"*"may be stale"* ]] \
  && ok "a locked one stays attached, and the catalog lists it as last read, with why" \
  || bad "the catalog of a locked file: $status $lines"
[[ $lines == *"view it_s_q1 reads"* ]] && ok "and the rest of the catalog reads" || bad "the catalog beside it: $lines"
python3 -c "import sys; sys.exit(0 if float(sys.argv[1]) < 4 else 1)" "$secs" \
  && ok "in about a second (${secs}s)" || bad "the catalog of a locked file took ${secs}s"
read -r status secs < <(catalog_timed)
python3 -c "import sys; sys.exit(0 if float(sys.argv[1]) < 1 else 1)" "$secs" \
  && ok "and a read while it stays locked waits on it no longer (${secs}s)" || bad "the next read took ${secs}s"
out=$("$harbor" "$work/workspace.duckdb" --mode csv -c "SELECT count(*) FROM app.people" 2>&1)
[[ $out == *"locked"* && $out != *"does not exist"* ]] && ok "and a statement that reads it waits on its lock" \
  || bad "a statement on a locked file: $out"
kill "$locker" 2>/dev/null; wait "$locker" 2>/dev/null
[[ $(catalog_lines) == *"attached app ['people'] None"* ]] && ok "and it is read anew once the lock is gone" \
  || bad "after the lock: $(catalog_lines)"
# A client's ATTACH of its name that fails, or changes nothing, leaves it
# the server's, attached again below once it reads.
out=$("$harbor" "$work/workspace.duckdb" -c "ATTACH ':memory:' AS app" 2>&1)
[[ $out == *"already exists"* ]] && ok "a client's ATTACH of its name fails" || bad "a client's ATTACH of its name: $out"
check "and one IF NOT EXISTS changes nothing" 0 "" \
  "$harbor" "$work/workspace.duckdb" -c "ATTACH IF NOT EXISTS ':memory:' AS app"
# A recorded file that stops being SQLite: detached, listed with why, and,
# since this server attached it from the record, attached again once it
# reads.
replace_sqlite
lines=$(catalog_lines)
[[ $lines == *"attached app [] "*"file is not a database"*"detached by the server"*"attached again"* ]] \
  && ok "a recorded file replaced by one that is not SQLite is detached, and the catalog says why" \
  || bad "the catalog of a replaced file: $lines"
[[ $lines == *"view it_s_q1 reads"* ]] && ok "and the rest of the catalog reads" || bad "the catalog beside it: $lines"
restore_sqlite
# A catalog read that attaches it again, and a client's ATTACH pipelined
# behind it on one connection: each is answered, neither waiting on the
# other.
out=$(python3 -c '
import json, socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.settimeout(15)
body = json.dumps({"sql": "ATTACH '"'"':memory:'"'"' AS piped"}).encode()
s.sendall(b"GET /catalog HTTP/1.1\r\nHost: harbor\r\n\r\n"
          + b"POST /sql HTTP/1.1\r\nHost: harbor\r\nContent-Length: %d\r\n\r\n" % len(body) + body)
got = b""
try:
    while not (got.count(b"HTTP/1.1 200") == 2 and b"\"type\":\"end\"" in got and got.endswith(b"0\r\n\r\n")):
        more = s.recv(65536)
        if not more: break
        got += more
except socket.timeout:
    pass
print("answered", got.count(b"HTTP/1.1 200"))' "$(live_sock)" 2>&1)
[[ $out == "answered 2" ]] && ok "a catalog read that attaches it again, and an ATTACH pipelined behind it, are both answered" \
  || bad "a pipelined catalog read and ATTACH: $out"
check "and the ATTACH ran" 0 "" "$harbor" "$work/workspace.duckdb" -c "DETACH piped"
[[ $(catalog_lines) == *"attached app ['people'] None"* ]] && ok "and it is attached again once it reads" \
  || bad "after it is back: $(catalog_lines)"
replace_sqlite
catalog_timed >/dev/null
check "a file set aside whose row is deleted" 0 "" \
  "$harbor" "$work/workspace.duckdb" -c "DELETE FROM _harbor.attached WHERE name = 'app'"
[[ $(catalog_lines) != *"attached app"* ]] && ok "is forgotten" || bad "after its row: $(catalog_lines)"
restore_sqlite
[[ $(catalog_lines) != *"attached app"* ]] && ok "and not attached again once it reads" || bad "after it is back: $(catalog_lines)"
check "a SQLite file attaches in the workspace again" 0 "app: the SQLite database" \
  "$harbor" "$work/files/app.sqlite" --mode csv -c "SELECT count(*) FROM app.people"
replace_sqlite
catalog_timed >/dev/null
workspace_stop
restore_sqlite
check "a file set aside as the server stops keeps its row" 0 "1" \
  "$harbor" "$work/workspace.duckdb" --mode csv -c "SELECT count(*) FROM _harbor.attached WHERE name = 'app'"
wait_gone
lock_sqlite "$work/files/app.sqlite"
started=$(python3 -c 'import time; print(time.time())')
workspace_start
took=$(python3 -c "import time, sys; print(round(time.time() - float(sys.argv[1]), 2))" "$started")
lines=$(catalog_lines)
[[ $lines == *"attached app [] "*"database is locked"* ]] \
  && ok "a recorded file locked at the start is detached again, and the catalog says why" \
  || bad "a file locked at the start: $lines"
python3 -c "import sys; sys.exit(0 if float(sys.argv[1]) < 5 else 1)" "$took" \
  && ok "and the start waits about a second on it (${took}s)" || bad "the start took ${took}s"
kill "$locker" 2>/dev/null; wait "$locker" 2>/dev/null
check "and it opens again once it reads" 0 "app: the SQLite database" \
  "$harbor" "$work/files/app.sqlite" --mode csv -c "DETACH app"
workspace_stop

echo "— a database attached as the record's schema"
# The record is _harbor.attached, which a catalog named _harbor makes
# ambiguous; the server names it in the workspace's own catalog.
workspace_start
check "a database attaches as _harbor" 0 "" \
  "$harbor" "$work/workspace.duckdb" -c "ATTACH '$work/hx.duckdb' AS _harbor"
workspace_stop
grep -q "could not be written" "$work/ws.log" && bad "the record at the stop: $(cat "$work/ws.log")" \
  || ok "the record is written at the stop beside it"
workspace_start
[[ $(catalog_lines) == *"attached _harbor [] None"* ]] && ok "and the database is attached again at the start" \
  || bad "a database named _harbor after a restart: $(catalog_lines)"
check "and detaches" 0 "" "$harbor" "$work/workspace.duckdb" -c "DETACH _harbor"
workspace_stop

echo "— a database --init attaches is the operator's"
workspace_start --init "ATTACH '$work/i2.duckdb' AS initdb"
[[ $(catalog_lines) == *"attached initdb [] None"* ]] && ok "it is attached" || bad "--init's database: $(catalog_lines)"
workspace_stop
check "and not recorded at the stop" 0 "0" \
  "$harbor" "$work/workspace.duckdb" --mode csv -c "SELECT count(*) FROM _harbor.attached WHERE name = 'initdb'"
wait_gone
workspace_start --init "ATTACH '$work/i3.duckdb' AS notsq"
grep -q "notsq is attached by --init" "$work/ws.log" && ok "a recorded name --init takes is said in the log" \
  || bad "the log: $(cat "$work/ws.log")"
workspace_stop
check "and its row stays as it was" 0 "$work/files/ab.csv" \
  "$harbor" "$work/workspace.duckdb" --mode csv -c "SELECT path FROM _harbor.attached WHERE name = 'notsq'"
wait_gone

if (( spatial )); then
  echo "— an extension the workspace needs and does not have"
  # An install is a download, and never stands between the database and the
  # socket: a repository that does not answer holds the start for as long as
  # a connection takes to time out. The SQLite file the workspace records
  # attaches through an extension the engine would install on demand, from
  # the same repository, and is attached in the background too.
  mkdir -p "$work/noext"
  started=$(python3 -c 'import time; print(time.time())')
  workspace_start --init "SET extension_directory='$work/noext'; SET custom_extension_repository='http://10.255.255.1'"
  took=$(python3 -c "import time, sys; print(round(time.time() - float(sys.argv[1]), 2))" "$started")
  python3 -c "import sys; sys.exit(0 if float(sys.argv[1]) < 5 else 1)" "$took" \
    && ok "a start whose extension repository does not answer serves at once (${took}s)" \
    || bad "the start took ${took}s"
  lines=$(catalog_lines)
  [[ $lines == *"view parks "*"spatial"* ]] && ok "and the view that needs it says why it does not read" \
    || bad "the spatial view without spatial: $lines"
  [[ $lines == *"attached notsq [] attached once sqlite"*"in the background"* ]] \
    && ok "and the SQLite file the workspace records says it waits on sqlite" || bad "the SQLite file without sqlite: $lines"
  workspace_stop
  check "and its row stays in the record" 0 "1" \
    "$harbor" "$work/workspace.duckdb" --mode csv -c "SELECT count(*) FROM _harbor.attached WHERE name = 'notsq'"
  wait_gone
fi

echo
if ((fails)); then
  echo "lifecycle: $fails failing"
  exit 1
fi
echo "lifecycle: all green"
