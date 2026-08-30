#!/usr/bin/env bash
#
# fleet.sh — the summon and hold law, end to end, through the real binaries.
#
#   test/scripts/fleet.sh
#
# The law under test is the product's one sentence: a name is a service — it
# starts on use and runs until you say stop — and a path is a session. stop
# writes a hold that every client summon must refuse, whatever spelling would
# raise the berth (the name, or its database's path), and only `harbor start`
# lifts it. add and forget must be true inverses, and a forget must never eat
# a remote entry or the database file. These behaviors live in the seam
# between two binaries, which is exactly where no unit test can see.

set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
harbor=${FLEET_HARBOR:-$here/target/release/harbor}
pilot=${FLEET_PILOT:-$here/target/release/pilot}
[[ -x $harbor && -x $pilot ]] || { echo "fleet: build first (make harbor pilot)" >&2; exit 77; }

work=$(mktemp -d "${TMPDIR:-/tmp}/harbor-fleet.XXXXXX")
export HARBOR_HOME="$work"
export HARBOR_BIN="$harbor" # pilot summons through this exact binary
cleanup() {
  "$harbor" stop x >/dev/null 2>&1
  pkill -f "harbor serve.*$work" 2>/dev/null
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

# A short temp window so "the temp leaves on its own" is waitable.
printf '[defaults]\ntemp-idle-exit = "1s"\n' >"$work/config.toml"

echo "— a path is a session"
check "a typed path summons and creates" 0 "42" \
  "$pilot" --mode csv -c "SELECT 42 AS answer" "$work/x.duckdb"
check "a second pilot joins the same berth" 0 "42" \
  "$pilot" --mode csv -c "SELECT 42 AS answer" "$work/x.duckdb"
check "a bare word is never a path" 1 "nothing running" \
  "$pilot" -c "SELECT 1" nosuchname
if [[ -f $work/nosuchname ]]; then bad "a bare word conjured a file"; else ok "no file conjured for a bare word"; fi
sleep 3 # the temp's 1s idle window passes; the berth leaves on its own

echo "— a name is a service"
check "add names the database" 0 "added" "$harbor" add "$work/x.duckdb"
check "the name starts on use" 0 "42" \
  "$pilot" --mode csv -c "SELECT 42 AS answer" x
check "and stays up after the client leaves" 0 "running" "$harbor" show
check "the panel shows the service, not a temp" 0 "never" "$harbor" show x

echo "— stop is a hold"
check "stop says it held" 0 "held" "$harbor" stop x
check "the name refuses to start on use" 1 "stopped by hand" "$pilot" -c "SELECT 1" x
check "the path refuses too" 1 "stopped by hand" "$pilot" -c "SELECT 1" "$work/x.duckdb"
check "stopping a held berth is honest, not a no-op" 0 "held" "$harbor" stop x
check "start says it lifted the hold" 0 "was held" "$harbor" start x
check "and the berth answers again" 0 "42" \
  "$pilot" --mode csv -c "SELECT 42 AS answer" x

echo "— forget is add's inverse, and only that"
printf '\n[connection.remote]\nurl = "http://127.0.0.1:1"\n' >>"$work/config.toml"
check "forget refuses a remote entry" 1 "yours to remove" "$harbor" forget remote
check "stop, then forget clears the name" 0 "held" "$harbor" stop x
check "forget sweeps registry and config entry" 0 "config entry" "$harbor" forget x
check "the name is unknown afterwards" 1 "no database named" "$harbor" show x
if grep -q 'connection.x' "$work/config.toml"; then bad "config entry survived forget"; else ok "config entry gone"; fi
if grep -q 'connection.remote' "$work/config.toml"; then ok "the remote entry was not touched"; else bad "forget ate the remote entry"; fi
if [[ -f $work/x.duckdb ]]; then ok "the database file was not touched"; else bad "forget removed the database file"; fi

echo
if ((fails)); then
  echo "fleet: $fails failing"
  exit 1
fi
echo "fleet: all green"
