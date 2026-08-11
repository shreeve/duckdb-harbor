#!/usr/bin/env bash
#
# resilience.sh — the questions you only care about in production.
#
#   scripts/resilience.sh [database.duckdb]
#
# Correctness suites ask whether the right answer comes back. These ask what
# happens when it does not get the chance: the process is killed mid-write,
# clients vanish halfway through a stream, connections are opened and never
# used, the same server is started and stopped all afternoon.
#
# The recurring measurement is file descriptors and resident memory before and
# after. A leak of one descriptor per request is invisible in a test that makes
# ten and fatal in a process that runs for a week.

set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
src_db=${1:-$here/../sample.duckdb}
# A free port from the kernel, not a fixed guess — see the same note in
# check.sh. This suite restarts the server fifteen times, so a port that is
# briefly held by something else is more likely to bite here than anywhere.
port=${PORT:-$(python3 -c 'import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()')}
token=${TOKEN:-res-$$}
base="http://127.0.0.1:$port"

work=$(mktemp -d "${TMPDIR:-/tmp}/harbor-res.XXXXXX")
db="$work/res.duckdb"
cp "$src_db" "$db"

# Read the fixture's own row count rather than hardcoding one, so the suite
# runs against any database with these tables — including the one CI builds.
sites_n=$(duckdb -readonly -csv -noheader "$src_db" -c 'SELECT count(*) FROM sites' 2>/dev/null | tail -1)

pass=0; fail=0
declare -a failures=()
bold=$(tput bold 2>/dev/null || true); red=$(tput setaf 1 2>/dev/null || true)
green=$(tput setaf 2 2>/dev/null || true); dim=$(tput dim 2>/dev/null || true)
off=$(tput sgr0 2>/dev/null || true)
section() { printf '\n%s%s%s\n' "$bold" "$1" "$off"; }
ok()  { pass=$((pass+1)); printf '  %s✓%s %s\n' "$green" "$off" "$1"; }
bad() { fail=$((fail+1)); failures+=("$1"); printf '  %s✗%s %s\n     %s%s%s\n' "$red" "$off" "$1" "$dim" "$2" "$off"; }
eq()  { if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1" "expected [$2], got [$3]"; fi; }

server_pid=""
cleanup() {
  [[ -n "$server_pid" ]] && kill -KILL "$server_pid" 2>/dev/null
  [[ "${KEEP:-0}" == 1 ]] || rm -rf "$work"
}
trap cleanup EXIT

start_server() { # start_server [extra args]
  "$here/bin/duckdb-harbor" "$db" --port "$port" --token "$token" --workers 4 "$@" >>"$work/server.log" 2>&1 &
  server_pid=$!
  for _ in $(seq 1 80); do
    curl -sS -m 1 "$base/health" >/dev/null 2>&1 && return 0
    kill -0 "$server_pid" 2>/dev/null || break
    sleep 0.25
  done
  return 1
}

stop_server() { # stop_server <signal>
  [[ -z "$server_pid" ]] && return 0
  kill "-$1" "$server_pid" 2>/dev/null
  for _ in $(seq 1 80); do kill -0 "$server_pid" 2>/dev/null || break; sleep 0.25; done
  kill -KILL "$server_pid" 2>/dev/null
  server_pid=""
}

sql() { curl -sS -m 60 -H "Authorization: Bearer $token" --data "{\"sql\":$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")}" "$base/sql"; }
scalar() { sql "$1" | python3 -c 'import sys,json
rows=[json.loads(l)["values"] for l in sys.stdin.read().split("\n") if l.strip() and "\"row\"" in l]
print(rows[0][0] if rows else "NO-ROWS")'; }
# Wait until the server is answering normally again. harbor returns 503 when
# every worker is busy — that is the backpressure working, not a failure — so
# checks that need a quiet server wait for one instead of counting a busy
# answer as breakage. Returns non-zero if it never settles.
settle() {
  for _ in $(seq 1 "${1:-60}"); do
    [[ "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $token" \
          -H 'Content-Type: application/json' --data '{"sql":"SELECT 1"}' "$base/sql" 2>/dev/null)" == "200" ]] \
      && return 0
    sleep 1
  done
  return 1
}
fds()  { lsof -p "$server_pid" 2>/dev/null | wc -l | tr -d ' '; }
rss()  { ps -o rss= -p "$server_pid" 2>/dev/null | tr -d ' '; }

# ---------------------------------------------------------------------------
section "Crash recovery (SIGKILL, no chance to checkpoint)"
# ---------------------------------------------------------------------------

start_server || { echo "resilience: server did not start"; cat "$work/server.log"; exit 1; }
sql 'CREATE OR REPLACE TABLE crash(i BIGINT, tag VARCHAR)' > /dev/null
for i in $(seq 1 20); do sql "INSERT INTO crash VALUES ($i, 'before')" > /dev/null; done
eq "20 rows written before the kill" "20" "$(scalar 'SELECT count(*) AS n FROM crash')"

# SIGKILL is the case the checkpoint cannot help with: the WAL is all that is
# left, and DuckDB has to replay it on the next open. If that does not work,
# every committed row since the last checkpoint is gone.
kill -KILL "$server_pid"; wait "$server_pid" 2>/dev/null; server_pid=""
if [[ -f "$db.wal" ]]; then
  ok "a WAL survives the kill, as it must"
else
  bad "a WAL survives the kill, as it must" "no $db.wal — the writes were nowhere"
fi
eq "every committed row replays on reopen" "20" \
   "$(duckdb -readonly -csv -noheader "$db" -c 'SELECT count(*) FROM crash' 2>/dev/null | tail -1)"
eq "the original tables are unharmed by the kill" "$sites_n" \
   "$(duckdb -readonly -csv -noheader "$db" -c 'SELECT count(*) FROM sites' 2>/dev/null | tail -1)"

start_server || { echo "resilience: server did not restart after the kill"; exit 1; }
eq "the server serves the replayed data" "20" "$(scalar 'SELECT count(*) AS n FROM crash')"
eq "and accepts new writes after recovery" "21" \
   "$(sql "INSERT INTO crash VALUES (21, 'after')" >/dev/null; scalar 'SELECT count(*) AS n FROM crash')"

# ---------------------------------------------------------------------------
section "Descriptor and memory hygiene"
# ---------------------------------------------------------------------------

# Warm up first: the first requests allocate buffers and open files that are
# not a leak, and counting them as one would make this test cry wolf.
for _ in $(seq 1 50); do sql 'SELECT 1' > /dev/null; done
fd_before=$(fds); rss_before=$(rss)

for _ in $(seq 1 500); do sql 'SELECT count(*) AS n FROM sites' > /dev/null; done
fd_after=$(fds)
# A per-request descriptor leak would be +500. Allow a small band for the
# keep-alive connections the harness itself leaves in TIME_WAIT.
if (( fd_after - fd_before < 25 )); then
  ok "500 requests leak no descriptors (${fd_before} → ${fd_after})"
else
  bad "500 requests leak no descriptors" "${fd_before} → ${fd_after}"
fi

for _ in $(seq 1 200); do sql 'SELECT * FROM plans' > /dev/null; done
rss_after=$(rss)
# 200 full-table streams; a result-sized leak would show as tens of megabytes.
if (( rss_after - rss_before < 51200 )); then
  ok "200 streamed results leak no memory (${rss_before}K → ${rss_after}K)"
else
  bad "200 streamed results leak no memory" "${rss_before}K → ${rss_after}K RSS"
fi

# ---------------------------------------------------------------------------
section "Clients behaving badly"
# ---------------------------------------------------------------------------

fd_before=$(fds)

# Hang up in the middle of a large stream, many times over. Each abandoned
# response leaves a query running with nobody to read it; harbor has to notice
# and stop rather than finish computing a result into a closed socket.
for _ in $(seq 1 40); do
  curl -sS -m 0.2 -o /dev/null -H "Authorization: Bearer $token" \
       --data '{"sql":"SELECT i, repeat(i::VARCHAR, 50) FROM range(3000000) t(i)"}' \
       "$base/sql" >/dev/null 2>&1 || true
done
# Wait for recovery rather than assuming a fixed pause is enough. Forty
# abandoned multi-million-row scans leave real work in flight, and how long it
# takes to drain depends on the machine — two seconds is plenty on a laptop and
# not enough on a shared CI runner with fewer cores. Reporting how long it
# actually took turns this from a flaky assertion into a measurement; only
# never recovering is a failure.
recovered=""
for attempt in $(seq 1 60); do
  if [[ "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "$base/health" 2>/dev/null)" == "200" ]]; then
    recovered=$attempt
    break
  fi
  sleep 1
done
if [[ -n "$recovered" ]]; then
  ok "the server recovers from 40 mid-stream hangups" "${recovered}s"
else
  bad "the server recovers from 40 mid-stream hangups" "still not answering /health after 60s"
fi
eq "and still correct" "$sites_n" "$(scalar 'SELECT count(*) AS n FROM sites')"
fd_after=$(fds)
if (( fd_after - fd_before < 25 )); then
  ok "abandoned streams release their descriptors (${fd_before} → ${fd_after})"
else
  bad "abandoned streams release their descriptors" "${fd_before} → ${fd_after}"
fi

# A client that opens a transaction and never closes it. Requests are not
# pinned to a connection, so that transaction can never be committed — and if
# the server leaves it open, the connection it landed on is inside a
# transaction for whoever gets it next. When the transaction has already
# failed, every later statement on that connection comes back "Current
# transaction is aborted" until the process restarts. Eight such requests
# would take a pool of eight out of service.
sql 'BEGIN TRANSACTION' > /dev/null
sql 'SELECT * FROM no_such_table_at_all' > /dev/null
sql 'BEGIN TRANSACTION' > /dev/null
# Count only the failure this is about. A 503 means every worker is busy and
# the request was shed on purpose; counting it here made a saturated server
# look like a corrupted one, which is how this first failed in CI.
settle 60 || true
poisoned=0
for _ in $(seq 1 40); do
  sql 'SELECT 1 AS n' | grep -qi 'transaction is aborted\|transaction context' && poisoned=$((poisoned+1))
done
eq "an abandoned transaction does not poison the pool" "0" "$poisoned"

# Connections that open and never send anything. A server that dedicates a
# worker to each would stop answering after four.
python3 - "$port" <<'PY'
import socket, sys
socks = []
for _ in range(64):
    s = socket.socket()
    s.settimeout(2)
    try:
        s.connect(("127.0.0.1", int(sys.argv[1])))
        socks.append(s)
    except OSError:
        pass
print("  %d idle connections held open" % len(socks))
import subprocess
code = subprocess.run(["curl", "-sS", "-m", "10", "-o", "/dev/null",
                       "-w", "%{http_code}", "http://127.0.0.1:%s/health" % sys.argv[1]],
                      capture_output=True, text=True).stdout
print("  health while they are held: %s" % code)
open("/tmp/harbor-idle-result.txt", "w").write(code)
for s in socks:
    s.close()
PY
eq "answers while 64 connections sit idle" "200" "$(cat /tmp/harbor-idle-result.txt 2>/dev/null)"

# A client that sends a request and then reads one byte at a time, slowly. The
# bounded body channel should make the query wait for the reader rather than
# buffer the whole result.
python3 - "$port" "$token" <<'PY'
import json, socket, sys, time
port, token = int(sys.argv[1]), sys.argv[2]
body = json.dumps({"sql": "SELECT i, repeat('x', 200) FROM range(400000) t(i)"})
s = socket.create_connection(("127.0.0.1", port), timeout=30)
s.sendall(("POST /sql HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer %s\r\n"
           "Content-Type: application/json\r\nContent-Length: %d\r\n\r\n%s"
           % (token, len(body), body)).encode())
got = 0
for _ in range(40):
    time.sleep(0.05)
    try:
        chunk = s.recv(256)
    except OSError:
        break
    if not chunk:
        break
    got += len(chunk)
s.close()
open("/tmp/harbor-slow-result.txt", "w").write("streaming" if got > 0 else "nothing")
print("  slow reader received %d bytes in 2s of a ~90 MB result" % got)
PY
eq "a slow reader gets data without stalling the server" "streaming" \
   "$(cat /tmp/harbor-slow-result.txt 2>/dev/null)"
settle 60 || true
eq "other clients are unaffected by the slow one" "$sites_n" "$(scalar 'SELECT count(*) AS n FROM sites')"

# ---------------------------------------------------------------------------
section "Configuration and contention"
# ---------------------------------------------------------------------------

# DuckDB's default checkpoint_threshold is 16MB. At that setting a modest
# writer can run for weeks with every committed row in the WAL and the .duckdb
# file near-empty; one hard kill, or one WAL that fails to replay, and the data
# is gone. bin/duckdb-harbor lowers it deliberately, so assert the launcher actually
# applies it rather than trusting that it still does.
eq "the launcher lowers checkpoint_threshold" "976.5 KiB" \
   "$(scalar "SELECT current_setting('checkpoint_threshold') AS v")"

# DuckDB's file lock is exclusive, even for readers, so a second server on the
# same database cannot work. What matters is that it says so and exits, rather
# than hanging — "clean error or confusing hang" is what someone meets at 2am
# when a supervisor restarts a service whose predecessor has not yet died.
second_log="$work/second.log"
"$here/bin/duckdb-harbor" "$db" --port "$((port + 1))" --token second --workers 2 \
    >"$second_log" 2>&1 &
second_pid=$!
waited=0
while kill -0 "$second_pid" 2>/dev/null && (( waited < 40 )); do
  sleep 0.25
  waited=$((waited + 1))
done
if kill -0 "$second_pid" 2>/dev/null; then
  kill -KILL "$second_pid" 2>/dev/null
  bad "a second server on a locked database exits" "still running after 10s — it hung rather than failing"
else
  ok "a second server on a locked database exits"
  if grep -qi 'lock\|another process\|being used\|conflict' "$second_log"; then
    ok "and says the database is locked" "$(grep -io 'lock[^\"]\{0,40\}' "$second_log" | head -1)"
  else
    bad "and says the database is locked" "exited, but the message does not mention the lock: $(head -c 160 "$second_log")"
  fi
fi
wait "$second_pid" 2>/dev/null

# ---------------------------------------------------------------------------
section "Start/stop churn"
# ---------------------------------------------------------------------------

stop_server TERM
fd_baseline=""
churn_ok=0
for i in $(seq 1 15); do
  if start_server; then
    [[ "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "$base/health")" == "200" ]] && churn_ok=$((churn_ok+1))
    [[ $i == 3 ]] && fd_baseline=$(fds)
    [[ $i == 15 ]] && fd_final=$(fds)
    stop_server TERM
  fi
done
eq "15 serve/stop cycles all come up" "15" "$churn_ok"
if [[ -n "$fd_baseline" ]] && (( ${fd_final:-0} - fd_baseline < 20 )); then
  ok "descriptors do not accumulate across restarts (${fd_baseline} → ${fd_final})"
else
  bad "descriptors do not accumulate across restarts" "${fd_baseline:-?} → ${fd_final:-?}"
fi

# ---------------------------------------------------------------------------
section "Durability after all of that"
# ---------------------------------------------------------------------------

start_server || { echo "resilience: server did not start for the final check"; exit 1; }
sql 'CREATE OR REPLACE TABLE final AS SELECT i FROM range(5000) t(i)' > /dev/null
eq "5000 rows written" "5000" "$(scalar 'SELECT count(*) AS n FROM final')"
stop_server TERM
if [[ -f "$db.wal" ]]; then
  bad "the WAL is folded in on a clean stop" "$db.wal still exists ($(wc -c < "$db.wal") bytes)"
else
  ok "the WAL is folded in on a clean stop"
fi
eq "the data is there on reopen" "5000" \
   "$(duckdb -readonly -csv -noheader "$db" -c 'SELECT count(*) FROM final' 2>/dev/null | tail -1)"
eq "the crash-recovered rows are still there too" "21" \
   "$(duckdb -readonly -csv -noheader "$db" -c 'SELECT count(*) FROM crash' 2>/dev/null | tail -1)"

printf '\n%s%d passed, %d failed%s\n' "$bold" "$pass" "$fail" "$off"
if (( fail > 0 )); then
  printf '\n%sfailures:%s\n' "$red" "$off"
  for f in "${failures[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi
exit 0
