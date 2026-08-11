#!/usr/bin/env bash
# Golden test for the SIGTERM shutdown checkpoint.
#
# Why this exists: DuckDB only folds the WAL into the database file once
# the WAL passes checkpoint_threshold (16 MiB by default). A server that
# writes steadily but modestly can run for weeks with every committed
# row living in the WAL and a near-empty .duckdb file — and if the
# process is killed there, that WAL is the only copy of the data. (A WAL
# that then fails to replay, which DuckDB has open bugs around, takes
# the whole database with it.) So harbor catches SIGTERM: harbor_wait
# stops the listener and runs CHECKPOINT before the process exits.
#
# Coverage:
#   - A file-backed server accumulates a non-empty .wal from /sql writes
#   - SIGTERM makes the process exit on its own (harbor_wait returns)
#   - The .wal is gone afterwards — the shutdown checkpoint ran
#   - Every committed row is readable from the database file alone
#   - SIGINT is deliberately NOT handled here: it belongs to the duckdb
#     CLI (interactive query cancel), so this test does not assert on it
#
# Usage:
#   make release
#   scripts/golden-shutdown-checkpoint.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXT_PATH="${REPO_ROOT}/build/release/extension/harbor/harbor.duckdb_extension"
DUCKDB_BIN="${REPO_ROOT}/build/release/duckdb"
PORT="${HARBOR_SHUTDOWN_TEST_PORT:-19515}"
TOKEN="shutdown-golden-token-$$"
WORK_DIR="$(mktemp -d)"
DB_FILE="${WORK_DIR}/shutdown.duckdb"
SERVER_LOG="${WORK_DIR}/server.log"
SERVER_PID=""

cleanup() {
    if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
        kill -9 "${SERVER_PID}" 2>/dev/null || true
        wait "${SERVER_PID}" 2>/dev/null || true
    fi
    rm -rf "${WORK_DIR}"
}
trap cleanup EXIT INT TERM

red() { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
fail() { red "FAIL: $*"; exit 1; }
pass() { green "PASS: $*"; }

if [[ ! -x "${DUCKDB_BIN}" ]]; then
    fail "${DUCKDB_BIN} not found — run 'make release' first"
fi
if [[ ! -f "${EXT_PATH}" ]]; then
    fail "${EXT_PATH} not found — run 'make release' first"
fi

# A file-backed database is the whole point — an in-memory one has no
# WAL to lose.
"${DUCKDB_BIN}" "${DB_FILE}" -c "CREATE TABLE t(i INTEGER, s VARCHAR);" > /dev/null

"${DUCKDB_BIN}" "${DB_FILE}" -c "
  LOAD '${EXT_PATH}';
  CALL harbor_serve(bind := '127.0.0.1', port := ${PORT}, token := '${TOKEN}');
  CALL harbor_wait();
" > "${SERVER_LOG}" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 60); do
    if curl -fsS -m 2 "http://127.0.0.1:${PORT}/health" > /dev/null 2>&1; then
        break
    fi
    sleep 0.5
done
curl -fsS -m 2 "http://127.0.0.1:${PORT}/health" > /dev/null 2>&1 \
    || fail "server never became healthy — log: $(cat "${SERVER_LOG}")"
pass "file-backed server is serving on ${PORT}"

# Enough rows to make a WAL, nowhere near the 16 MiB auto-checkpoint
# threshold — exactly the profile that silently loses data on a kill.
curl -fsS -m 10 "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"INSERT INTO t SELECT i, '"'"'row'"'"'||i FROM range(5000) tbl(i)"}' > /dev/null \
    || fail "insert over /sql failed"

[[ -s "${DB_FILE}.wal" ]] || fail "expected a non-empty WAL before shutdown (nothing to checkpoint = vacuous test)"
pass "writes accumulated in the WAL (below the auto-checkpoint threshold)"

kill -TERM "${SERVER_PID}"
for _ in $(seq 1 60); do
    kill -0 "${SERVER_PID}" 2>/dev/null || break
    sleep 0.5
done
if kill -0 "${SERVER_PID}" 2>/dev/null; then
    fail "process still running 30s after SIGTERM — harbor_wait did not return"
fi
wait "${SERVER_PID}" 2>/dev/null || true
pass "SIGTERM returned harbor_wait and the process exited on its own"

[[ -f "${DB_FILE}.wal" ]] && fail "WAL still present after SIGTERM — the shutdown checkpoint did not run"
pass "WAL folded into the database file on shutdown"

COUNT="$("${DUCKDB_BIN}" -noheader -csv "${DB_FILE}" -c "SELECT count(*) FROM t;" 2>/dev/null | tr -d '[:space:]')"
[[ "${COUNT}" == "5000" ]] || fail "expected 5000 rows readable from the database file, got '${COUNT}'"
pass "all committed rows survive in the database file alone"

green "All shutdown-checkpoint golden assertions passed."
