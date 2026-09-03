#!/usr/bin/env bash
#
# lifecycle.sh — the two lifetimes, end to end, through the real binary.
#
#   test/scripts/lifecycle.sh
#
# The law under test is the product's one breath: bare — the server is
# everyone's, it lives while anyone is connected; start — the server is
# yours, it lives until you leave. Everything here is behavior no unit test
# can see: spawn-on-use across processes, two clients landing on one server,
# an idle connection as a mooring, the last departure sweeping the socket,
# and a start-lifetime server ignoring the refcount entirely.

set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
harbor=${LIFECYCLE_HARBOR:-$here/target/release/harbor}
[[ -x $harbor ]] || { echo "lifecycle: build first (make harbor)" >&2; exit 77; }

work=$(mktemp -d "${TMPDIR:-/tmp}/harbor-lifecycle.XXXXXX")
export HARBOR_HOME="$work"
# Short windows so departure is waitable. Test-only overrides, not API: the
# shipped constants are 30s grace / 3s linger.
export HARBOR_STARTUP_GRACE_MS=5000
export HARBOR_LINGER_MS=1500
cleanup() {
  pkill -f "start.*$work" 2>/dev/null
  rm -rf "$work"
}
trap cleanup EXIT

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
check "a bare word is refused, never served" 1 "names nothing running" \
  "$harbor" nosuchname -c "SELECT 1"
if [[ -f $work/nosuchname ]]; then bad "a bare word conjured a file"; else ok "no file conjured for a bare word"; fi

echo "— the server is everyone's: it lives while anyone is connected"
sock=$(live_sock)
# A silent open connection, well past the linger AND past justhttp's 5s read
# timeout — presence is the mooring, no heartbeat, no traffic.
python3 - "$sock" <<'PY' &
import socket, sys, time
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
time.sleep(7)
s.close()
PY
holder=$!
sleep 6
if [[ -n $(live_sock) ]]; then ok "an idle connection holds the server (6s > 1s linger)"; else bad "the server left while a client was still connected"; fi
wait "$holder"
if wait_gone; then ok "the last departure ends the server"; else bad "the server outlived its last client"; fi
if [[ ! -e $sock ]]; then ok "it swept its socket on the way out"; else bad "departure left the socket behind"; fi
if [[ -f $work/x.duckdb && ! -e $work/x.duckdb.wal ]]; then
  ok "the database stays ashore, checkpointed (no wal)"
else
  bad "departure left the database missing or with a wal"
fi

echo "— start: the server is yours, it lives until you leave"
"$harbor" "$work/x.duckdb" start >"$work/start.log" 2>&1 &
srv=$!
up=0
for _ in $(seq 1 50); do [[ -n $(live_sock) ]] && { up=1; break; }; sleep 0.1; done
(( up )) && ok "start came up" || bad "start never came up: $(cat "$work/start.log")"
check "a client joins the started database" 0 "7" \
  "$harbor" "$work/x.duckdb" --mode csv -c "SELECT 7 AS seven"
# Well past the linger an ephemeral server would have left on. A start
# lifetime has no clock at all — only SIGTERM (or .quit at the helm) ends it.
sleep 2
kill -0 "$srv" 2>/dev/null && ok "no refcount: it survives its clients leaving" \
                           || bad "the start-lifetime server left on the refcount"
check "a second start on the same file is refused" 1 "already being served" \
  "$harbor" "$work/x.duckdb" start
kill -TERM "$srv"
wait "$srv" 2>/dev/null
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

echo
if ((fails)); then
  echo "lifecycle: $fails failing"
  exit 1
fi
echo "lifecycle: all green"
