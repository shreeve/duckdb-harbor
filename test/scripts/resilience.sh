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

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
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

# Exported so the inline python blocks below write their results here too.
# They used fixed /tmp paths, which survive the run: the assertions that read
# them back passed off a previous run's file without ever contacting a server,
# and two runs at once overwrote each other.
work=$(mktemp -d "${TMPDIR:-/tmp}/harbor-res.XXXXXX")
export WORK="$work"
db="$work/res.duckdb"
cp "$src_db" "$db"

# Read the fixture's own row count rather than hardcoding one, so the suite
# runs against any database with these tables — including the one CI builds.
sites_n=$(duckdb -no-init -readonly -csv -noheader "$src_db" -c 'SELECT count(*) FROM sites' 2>/dev/null | tail -1)

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
# Wait until the server is answering normally again. Requests past the worker
# count wait for a worker rather than being shed, so a burst leaves later
# requests queued behind it; checks that need a quiet server wait for one
# instead of reading that queue as breakage. Non-zero if it never settles.
settle() {
  for _ in $(seq 1 "${1:-60}"); do
    [[ "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $token" \
          -H 'Content-Type: application/json' --data '{"sql":"SELECT 1"}' "$base/sql" 2>/dev/null)" == "200" ]] \
      && return 0
    sleep 1
  done
  return 1
}
# Both of these print nothing when the tool is missing or the process is dead,
# and an empty sample compares as 0. That made every leak check pass in exactly
# the two situations it must not: no lsof (0 - 0 < 25), and a server that had
# crashed during the run (0 - 50 < 25 — a dead server passing a leak test).
# Print a sentinel instead, and let the comparison refuse it.
fds()  { local n; n=$(lsof -p "$server_pid" 2>/dev/null | wc -l | tr -d ' '); if [[ -n "$n" && "$n" != 0 ]]; then echo "$n"; else echo unmeasured; fi; }
rss()  { local n; n=$(ps -o rss= -p "$server_pid" 2>/dev/null | tr -d ' '); if [[ -n "$n" ]]; then echo "$n"; else echo unmeasured; fi; }

# A leak check is only meaningful when both samples are real numbers and the
# count did not fall — a drop means the subject went away, not that it improved.
grew_by() {
  local label=$1 before=$2 after=$3 limit=$4 unit=${5:-}
  if [[ "$before" == unmeasured || "$after" == unmeasured ]]; then
    bad "$label" "could not measure (is lsof installed, and is the server alive?)"
  elif (( after < before )); then
    bad "$label" "sample fell from ${before}${unit} to ${after}${unit} — the process is gone"
  elif (( after - before < limit )); then
    ok "$label (${before}${unit} → ${after}${unit})"
  else
    bad "$label" "${before}${unit} → ${after}${unit}"
  fi
}

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
   "$(duckdb -no-init -readonly -csv -noheader "$db" -c 'SELECT count(*) FROM crash' 2>/dev/null | tail -1)"
eq "the original tables are unharmed by the kill" "$sites_n" \
   "$(duckdb -no-init -readonly -csv -noheader "$db" -c 'SELECT count(*) FROM sites' 2>/dev/null | tail -1)"

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
grew_by "500 requests leak no descriptors" "$fd_before" "$fd_after" 25

for _ in $(seq 1 200); do sql 'SELECT * FROM plans' > /dev/null; done
rss_after=$(rss)
# 200 full-table streams; a result-sized leak would show as tens of megabytes.
grew_by "200 streamed results leak no memory" "$rss_before" "$rss_after" 51200 K

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
grew_by "abandoned streams release their descriptors" "$fd_before" "$fd_after" 25

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
answered=0
for _ in $(seq 1 40); do
  reply=$(sql 'SELECT 1 AS n')
  grep -qi 'transaction is aborted\|transaction context' <<<"$reply" && poisoned=$((poisoned+1))
  grep -q '"values":\[1\]' <<<"$reply" && answered=$((answered+1))
done
# Both halves matter. Counting only poisoned replies passes when the server is
# wedged and answers nothing at all — the worst outcome this section exists to
# catch would have read as a clean run.
eq "an abandoned transaction does not poison the pool" "0" "$poisoned"
eq "and all 40 of those requests were answered" "40" "$answered"

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
import os, subprocess
code = subprocess.run(["curl", "-sS", "-m", "10", "-o", "/dev/null",
                       "-w", "%{http_code}", "http://127.0.0.1:%s/health" % sys.argv[1]],
                      capture_output=True, text=True).stdout
print("  health while they are held: %s" % code)
open(os.environ["WORK"] + "/idle-result.txt", "w").write(code)
for s in socks:
    s.close()
PY
eq "answers while 64 connections sit idle" "200" "$(cat "$work/idle-result.txt" 2>/dev/null)"

# A client that sends a request and then reads one byte at a time, slowly. The
# bounded body channel should make the query wait for the reader rather than
# buffer the whole result.
python3 - "$port" "$token" <<'PY'
import json, os, socket, sys, time
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
open(os.environ["WORK"] + "/slow-result.txt", "w").write("streaming" if got > 0 else "nothing")
print("  slow reader received %d bytes in 2s of a ~90 MB result" % got)
PY
eq "a slow reader gets data without stalling the server" "streaming" \
   "$(cat "$work/slow-result.txt" 2>/dev/null)"
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
   "$(duckdb -no-init -readonly -csv -noheader "$db" -c 'SELECT count(*) FROM final' 2>/dev/null | tail -1)"
eq "the crash-recovered rows are still there too" "21" \
   "$(duckdb -no-init -readonly -csv -noheader "$db" -c 'SELECT count(*) FROM crash' 2>/dev/null | tail -1)"

# ---------------------------------------------------------------------------
section "REPL mode"
# ---------------------------------------------------------------------------

# --repl serves without blocking on harbor_wait, so the same terminal is both a
# server and a SQL prompt. Two things have to hold that do not hold for free.
#
# The prompt has to be live while HTTP is being served — a --repl that hands
# back a prompt but has not actually bound, or has bound and wedged the shell,
# looks fine until someone types into it.
#
# And leaving the prompt has to fold the WAL. It does not do so on its own:
# harbor's workers hold pooled connections, DuckDB will not checkpoint past an
# open connection, and a .quit with the server still up left a WAL behind every
# time it was tried. The launcher covers that from outside the process, which is
# only checkable from outside too — hence reading $repl_db.wal after it exits.
repl_dir="$work/repl"
mkdir -p "$repl_dir"
repl_db="$repl_dir/repl.duckdb"
duckdb -no-init "$repl_db" -c 'CREATE TABLE t AS SELECT i FROM range(3) t(i); CHECKPOINT' >/dev/null 2>&1
repl_port=$((port + 2))
{
  # Sleep first: the prompt is fed from a pipe, so this runs before the shell
  # reads anything, and the request has to arrive after the listener is up.
  python3 - <<PY
import json, time, urllib.request
for _ in range(80):
    try:
        urllib.request.urlopen("http://127.0.0.1:$repl_port/health", timeout=1).read()
        break
    except Exception:
        time.sleep(0.25)
req = urllib.request.Request("http://127.0.0.1:$repl_port/sql",
    data=json.dumps({"sql": "INSERT INTO t VALUES (777)"}).encode(),
    headers={"Authorization": "Bearer $token"})
urllib.request.urlopen(req, timeout=30).read()
PY
  # Written to a file rather than read back off the prompt's own output: the
  # box-drawn table is a display format, and parsing it makes the assertion
  # about DuckDB's rendering rather than about the count.
  printf "COPY (SELECT count(*) AS n FROM t) TO '%s' (FORMAT csv, HEADER false);\n.quit\n" \
         "$repl_dir/count.csv"
} | timeout 120 "$here/bin/duckdb-harbor" "$repl_db" --repl --port "$repl_port" \
      --token "$token" --workers 2 >"$work/repl.log" 2>&1
repl_status=$?

eq "--repl exits cleanly when the prompt is left" "0" "$repl_status"
# 4 = the 3 seeded rows plus the one inserted over HTTP, so this is one
# assertion that the prompt ran SQL and another that the server was serving at
# the same moment.
eq "the prompt sees a row written over HTTP while it was open" "4" \
   "$(cat "$repl_dir/count.csv" 2>/dev/null | tr -d '[:space:]')"
if [[ -f "$repl_db.wal" ]]; then
  bad "leaving the --repl prompt folds the WAL" "$repl_db.wal still exists ($(wc -c < "$repl_db.wal") bytes)"
else
  ok "leaving the --repl prompt folds the WAL"
fi
eq "the HTTP write survived the exit" "4" \
   "$(duckdb -no-init -readonly -csv -noheader "$repl_db" -c 'SELECT count(*) FROM t' 2>/dev/null | tail -1)"

# The documented install unzips the extension next to the downloaded launcher
# and runs it with no --extension. That layout is not the source tree's, and the
# search used to start one directory above the script: every candidate missed,
# and the launcher reported that no extension existed while it sat beside it.
solo="$work/solo"
mkdir -p "$solo"
cp "$here/bin/duckdb-harbor" "$solo/"
solo_ext=${HARBOR_EXTENSION:-$here/build/release/harbor.duckdb_extension}
if [[ ! -f "$solo_ext" ]]; then
  bad "the launcher finds an extension unzipped beside it" "no extension at $solo_ext to copy"
else
  # Named as a release asset is, not as the launcher wants it: that combination
  # -- unusual name, no --extension, not the source layout -- is exactly what a
  # first-time install looks like.
  cp "$solo_ext" "$solo/harbor.duckdb_extension"
  cp "$src_db" "$solo/solo.duckdb"
  solo_port=$((port + 3))
  printf '.quit\n' | timeout 60 env -u HARBOR_EXTENSION "$solo/duckdb-harbor" \
      "$solo/solo.duckdb" --repl --port "$solo_port" --token "$token" \
      >"$work/solo.log" 2>&1
  if grep -q "127.0.0.1:$solo_port" "$work/solo.log"; then
    ok "the launcher finds an extension unzipped beside it"
  else
    bad "the launcher finds an extension unzipped beside it" "$(head -c 200 "$work/solo.log")"
  fi
fi

# A prompt is only a prompt if it answers the keyboard, and every check above
# feeds it through a pipe -- the one input path that cannot catch the way this
# breaks. DuckDB's shell asks the terminal for its background colour (OSC 11)
# and device attributes, and ignores the keyboard until the replies arrive or it
# gives up. Against a terminal that never answers that is 5.02s of dead prompt;
# --dark-mode skips the query and makes it 0.01s.
#
# So the assertion is on the *time* to a working prompt, not on eventually
# getting one: waiting long enough passes either way, which is what made the
# first version of this check unable to fail. 2.5s sits well clear of both.
#
# pty.fork() rather than a pty handed to a subprocess: the child needs the pty
# as its *controlling* terminal (setsid + TIOCSCTTY), and a line editor without
# one behaves nothing like a line editor at a real prompt.
pty_out=$(HARBOR_LAUNCHER="$here/bin/duckdb-harbor" HARBOR_DB="$repl_db" \
          HARBOR_PORT="$((port + 4))" HARBOR_TOKEN_="$token" python3 - <<'PY' 2>&1
import fcntl, os, pty, re, select, struct, sys, termios, time

argv = [os.environ["HARBOR_LAUNCHER"], os.environ["HARBOR_DB"], "--repl",
        "--dark-mode", "--port", os.environ["HARBOR_PORT"],
        "--token", os.environ["HARBOR_TOKEN_"]]

pid, fd = pty.fork()
if pid == 0:
    os.execvp(argv[0], argv)
    os._exit(127)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 110, 0, 0))

STRIP = rb"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07|\x1b[=>]"
out = bytearray()

def read_for(secs):
    end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                b = os.read(fd, 65536)
            except OSError:
                return
            if not b:
                return
            out.extend(b)          # deliberately never answers OSC 11 / DA1

t0 = time.time()
took = None
while time.time() - t0 < 20:
    read_for(0.1)
    if b" D " in re.sub(STRIP, b"", bytes(out)):
        took = time.time() - t0
        break

exited = False
if took is not None:
    os.write(fd, b".quit\r")
    read_for(2.0)
    for _ in range(25):
        if os.waitpid(pid, os.WNOHANG)[0]:
            exited = True
            break
        time.sleep(0.2)
if not exited:
    os.kill(pid, 9)
    os.waitpid(pid, 0)
print("prompt=%s exited=%s" % (int(took is not None and took < 2.5), int(exited)))
PY
)
eq "a real terminal gets a usable prompt without a stall" "prompt=1 exited=1" \
   "$(printf '%s' "$pty_out" | tail -1)"

printf '\n%s%d passed, %d failed%s\n' "$bold" "$pass" "$fail" "$off"
if (( fail > 0 )); then
  printf '\n%sfailures:%s\n' "$red" "$off"
  for f in "${failures[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi
exit 0
