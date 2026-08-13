#!/usr/bin/env bash
#
# differential.sh — stand up the v1 harbor and this one side by side on the
# same data and compare what they answer.
#
#   scripts/differential.sh [database.duckdb]
#   OLD_EXT=/path/to/cpp/harbor.duckdb_extension scripts/differential.sh
#
# The two servers get their own copy of the database, because DuckDB's file
# lock is exclusive — even a read-only opener is refused while another process
# holds it, so they cannot share one file. Copies of the same bytes answer the
# same questions, which is all this needs.
#
# Skips cleanly, rather than failing, when the C++ extension is not built:
# harbor's own suites do not depend on it, and not everyone has a 28 MB
# static DuckDB build lying around.

set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
src_db=${1:-$here/../sample.duckdb}
old_ext=${OLD_EXT:-$here/../duckdb-harbor-v1/build/release/extension/harbor/harbor.duckdb_extension}
new_ext=$here/build/release/harbor.duckdb_extension
old_port=${OLD_PORT:-9401}
new_port=${NEW_PORT:-9402}
token=${TOKEN:-diff-$$}

if [[ ! -f "$old_ext" ]]; then
  echo "differential: no v1 harbor at $old_ext — skipping"
  echo "  build it with 'make release' in duckdb-harbor-v1, or set OLD_EXT"
  exit 0
fi
if [[ ! -f "$new_ext" ]]; then
  echo "differential: harbor not built — run 'make release'" >&2
  exit 2
fi
if [[ ! -f "$src_db" ]]; then
  echo "differential: no database at $src_db" >&2
  exit 2
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/harbor-diff.XXXXXX")
cp "$src_db" "$work/old.duckdb"
cp "$src_db" "$work/new.duckdb"

old_pid=""; new_pid=""
cleanup() {
  for pid in $old_pid $new_pid; do
    kill -TERM "$pid" 2>/dev/null
  done
  for pid in $old_pid $new_pid; do
    for _ in $(seq 1 40); do kill -0 "$pid" 2>/dev/null || break; sleep 0.25; done
    kill -KILL "$pid" 2>/dev/null
  done
  [[ "${KEEP:-0}" == 1 ]] || rm -rf "$work"
}
trap cleanup EXIT

start() { # start <ext> <db> <port> <logfile>
  duckdb -unsigned "$2" -c "
    LOAD '$1';
    CALL harbor_serve(bind := '127.0.0.1', port := $3, token := '$token');
    CALL harbor_wait();
  " > "$4" 2>&1 &
  echo $!
}

wait_up() { # wait_up <port> <pid> <name>
  for _ in $(seq 1 80); do
    curl -sS -m 1 "http://127.0.0.1:$1/ready" >/dev/null 2>&1 && return 0
    kill -0 "$2" 2>/dev/null || break
    sleep 0.25
  done
  echo "differential: $3 never came up" >&2
  return 1
}

old_pid=$(start "$old_ext" "$work/old.duckdb" "$old_port" "$work/old.log")
new_pid=$(start "$new_ext" "$work/new.duckdb" "$new_port" "$work/new.log")

wait_up "$old_port" "$old_pid" "v1 harbor" || { cat "$work/old.log" >&2; exit 1; }
wait_up "$new_port" "$new_pid" "harbor"  || { cat "$work/new.log" >&2; exit 1; }

old_version=$(curl -sS -m 5 -H "Authorization: Bearer $token" \
  --data '{"sql":"SELECT harbor_version() AS v"}' "http://127.0.0.1:$old_port/sql" \
  | python3 -c 'import sys,json;print(next((json.loads(l)["values"][0] for l in sys.stdin if "\"row\"" in l), "?"))' 2>/dev/null)
new_version=$(curl -sS -m 5 -H "Authorization: Bearer $token" \
  --data '{"sql":"SELECT * FROM harbor_version()"}' "http://127.0.0.1:$new_port/sql" \
  | python3 -c 'import sys,json;print(next((json.loads(l)["values"][0] for l in sys.stdin if "\"row\"" in l), "?"))' 2>/dev/null)

echo "v1 harbor $old_version on :$old_port   vs   harbor $new_version on :$new_port"
echo

python3 "$here/test/scripts/differential.py" \
  --old-port "$old_port" --new-port "$new_port" --token "$token" "${@:2}"
