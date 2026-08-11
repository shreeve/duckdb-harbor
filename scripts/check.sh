#!/usr/bin/env bash
#
# check.sh — run every suite, in the order that fails cheapest first.
#
#   make check                    everything
#   make check SUITES="spec fuzz" just those
#
# Invoked by the `check` target in the Makefile; run it directly if you want
# the arguments. The ordering is deliberate — unit tests take seconds and catch
# the compile-level mistakes, the HTTP suites take a minute, and resilience
# takes several because it kills and restarts the server repeatedly. A failure
# early means not paying for the rest.
#
# The differential suite needs the C++ harbor built alongside; when it is not
# there it skips rather than fails, because the reference implementation is not
# a build dependency of this one.

set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
# Look in the checkout before the parent directory. Locally the fixture tends
# to sit beside the repo, in CI scripts/fixture.sh writes it into the checkout,
# and a default that only knew one of those failed every CI run with "no
# database" while the build and the fixture step had both succeeded.
db=${DB:-}
if [[ -z "$db" ]]; then
  for candidate in "$here/sample.duckdb" "$here/../sample.duckdb"; do
    [[ -f "$candidate" ]] && { db=$candidate; break; }
  done
  db=${db:-$here/sample.duckdb}
fi
# Ask the kernel for a port nobody is using rather than hoping a fixed one is
# free. A leftover server from an earlier debugging session — or another
# checkout running its own suite — otherwise fails the whole run at startup
# with "address already in use", which looks like a harbor bug and is not one.
port=${PORT:-$(python3 -c 'import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()')}
token=${TOKEN:-check-$$}
suites=${SUITES:-unit abi types sqllogic stress spec fuzz differential deploy swarm resilience}

bold=$(tput bold 2>/dev/null || true); red=$(tput setaf 1 2>/dev/null || true)
green=$(tput setaf 2 2>/dev/null || true); dim=$(tput dim 2>/dev/null || true)
off=$(tput sgr0 2>/dev/null || true)

declare -a failed=() skipped=() passed=()
work=$(mktemp -d "${TMPDIR:-/tmp}/harbor-check.XXXXXX")
server_pid=""
cleanup() {
  [[ -n "$server_pid" ]] && kill -TERM "$server_pid" 2>/dev/null
  sleep 0.5
  [[ -n "$server_pid" ]] && kill -KILL "$server_pid" 2>/dev/null
  [[ "${KEEP:-0}" == 1 ]] || rm -rf "$work"
}
trap cleanup EXIT

banner() { printf '\n%s══ %s %s%s\n' "$bold" "$1" "$(printf '═%.0s' $(seq 1 $((60 - ${#1}))))" "$off"; }
run() { # run <name> <command...>
  local name=$1; shift
  [[ " $suites " == *" $name "* ]] || return 0
  banner "$name"
  if "$@"; then passed+=("$name"); printf '%s✓ %s%s\n' "$green" "$name" "$off"
  else failed+=("$name"); printf '%s✗ %s%s\n' "$red" "$name" "$off"; fi
}
skip() { skipped+=("$1"); printf '%s— %s skipped: %s%s\n' "$dim" "$1" "$2" "$off"; }

[[ -f "$db" ]] || { echo "check: no database at $db (set DB=...)" >&2; exit 2; }

# ---------------------------------------------------------------------------
# The suites that need no server
# ---------------------------------------------------------------------------

run unit     cargo test --release
# Reads the built artifact rather than a running server: the ABI stamp decides
# which DuckDB versions will load it at all, and nothing else here looks at it.
run abi      "$here/scripts/abi.sh"
run types    "$here/scripts/type_coverage.py"
run sqllogic make -C "$here" test_release

# ---------------------------------------------------------------------------
# One server, shared by the read-only HTTP suites, on its own copy
# ---------------------------------------------------------------------------

needs_server=0
for s in stress spec fuzz deploy swarm; do
  [[ " $suites " == *" $s "* ]] && needs_server=1
done

if (( needs_server )); then
  cp "$db" "$work/check.duckdb"
  "$here/bin/harbor" "$work/check.duckdb" --port "$port" --token "$token" --workers 8 \
      >"$work/server.log" 2>&1 &
  server_pid=$!
  up=0
  for _ in $(seq 1 80); do
    curl -sS -m 1 "http://127.0.0.1:$port/health" >/dev/null 2>&1 && { up=1; break; }
    kill -0 "$server_pid" 2>/dev/null || break
    sleep 0.25
  done
  if (( ! up )); then
    printf '%scheck: the server did not come up%s\n' "$red" "$off"
    cat "$work/server.log"
    exit 1
  fi
fi

# stress.sh starts its own server, because it needs oracle values read from the
# database before anything takes the file lock.
run stress "$here/scripts/stress.sh" "$db"

run spec  "$here/scripts/spec_types.py" --port "$port" --token "$token"
run fuzz  "$here/scripts/fuzz.py"       --port "$port" --token "$token"
run deploy "$here/scripts/validate-deployment.sh" --url "http://127.0.0.1:$port" --token "$token"
# Load runs against the same server as the read-only suites rather than
# standing up its own. There was a second copy of the start-and-wait logic in
# the CI workflow doing exactly this, and it was the only place that could not
# get a server up — one proven path is better than two, one of which is only
# exercised in CI.
run swarm  "$here/scripts/swarm.py" --port "$port" --token "$token" \
           --levels "${SWARM_LEVELS:-1,4,16}" --seconds "${SWARM_SECONDS:-10}" \
           --write-pct "${SWARM_WRITE_PCT:-20}"

[[ -n "$server_pid" ]] && { kill -TERM "$server_pid" 2>/dev/null; wait "$server_pid" 2>/dev/null; server_pid=""; }

# ---------------------------------------------------------------------------
# The suites that manage their own servers
# ---------------------------------------------------------------------------

if [[ " $suites " == *" differential "* ]]; then
  old_ext=${OLD_EXT:-$here/../duckdb-harbor-v1/build/release/extension/harbor/harbor.duckdb_extension}
  if [[ -f "$old_ext" ]]; then
    run differential "$here/scripts/differential.sh" "$db"
  else
    skip differential "the C++ harbor is not built at $old_ext (set OLD_EXT=...)"
  fi
fi

run resilience "$here/scripts/resilience.sh" "$db"

# ---------------------------------------------------------------------------

banner "summary"
# ${a[@]+"${a[@]}"} rather than "${a[@]}": bash 3.2, which is what macOS
# ships, counts an empty array as unset under `set -u` and aborts the summary
# — the one part of the run that has to print no matter what happened.
for s in ${passed[@]+"${passed[@]}"};   do printf '  %s✓%s %s\n' "$green" "$off" "$s"; done
for s in ${skipped[@]+"${skipped[@]}"}; do printf '  %s—%s %s\n' "$dim" "$off" "$s"; done
for s in ${failed[@]+"${failed[@]}"};   do printf '  %s✗%s %s\n' "$red" "$off" "$s"; done
printf '\n%s%d passed, %d failed, %d skipped%s\n' \
       "$bold" "${#passed[@]}" "${#failed[@]}" "${#skipped[@]}" "$off"
(( ${#failed[@]} > 0 )) && exit 1
exit 0
