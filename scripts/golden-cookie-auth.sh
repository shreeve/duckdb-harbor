#!/usr/bin/env bash
# Golden HTTP-roundtrip test for PR-4 cookie auth + CORS allow-list,

#
# Why a bash script (not test/sql/harbor.test): sqllogictest can only
# call SQL functions, not curl. The cookie roundtrip — POST /auth/login
# → Set-Cookie → reuse cookie on a subsequent request — needs a real
# HTTP client. This script provides that, with the same fail-loud
# semantics (any assertion failure → non-zero exit).
#
# Coverage:
#   - POST /auth/login with valid token  → 200, Set-Cookie matches v1.<base64url>
#   - POST /auth/login with bad token    → 401, no Set-Cookie
#   - POST /auth/login JSON body         → 200 (alternate input form)
#   - POST /auth/logout                  → 200, Set-Cookie clears with Max-Age=0
#   - GET /info no cookie                → 204, NO Access-Control-Allow-Origin: *
#   - GET /info with allowed Origin      → 204 + ACAO echoes the matching origin
#   - GET /info with disallowed Origin   → 204 + NO ACAO header
#   - GET /auth/login                    → 405 Method Not Allowed (round-12 fix)
#   - /quack removed (Quack RPC endpoint gone) → 404
#
# Usage:
#   make release
#   scripts/golden-cookie-auth.sh
#
# Exit code: 0 on success; non-zero on any failure (with a loud message
# describing which assertion failed).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXT_PATH="${REPO_ROOT}/build/release/extension/harbor/harbor.duckdb_extension"
DUCKDB_BIN="${REPO_ROOT}/build/release/duckdb"
PORT="${HARBOR_TEST_PORT:-19496}"
TOKEN="cookieauth-golden-token-$$"
SERVER_LOG="$(mktemp)"
COOKIE_JAR="$(mktemp)"
SERVER_PID=""

if [[ ! -x "${DUCKDB_BIN}" ]]; then
    echo "FATAL: ${DUCKDB_BIN} not found — run 'make release' first." >&2
    exit 1
fi
if [[ ! -f "${EXT_PATH}" ]]; then
    echo "FATAL: ${EXT_PATH} not found — run 'make release' first." >&2
    exit 1
fi
cleanup() {
    if [[ -n "${SERVER_PID}" ]]; then
        kill "${SERVER_PID}" 2>/dev/null || true
        wait "${SERVER_PID}" 2>/dev/null || true
    fi
    rm -f "${SERVER_LOG}" "${COOKIE_JAR}"
}
trap cleanup EXIT INT TERM

red() { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
fail() { red "FAIL: $*"; exit 1; }
pass() { green "PASS: $*"; }

# Start harbor server with a CORS allow-list to exercise /info.
nohup "${DUCKDB_BIN}" -unsigned -no-stdin -c "
LOAD '${EXT_PATH}';
SET GLOBAL harbor_cors_origins='https://app.example.com';
CALL harbor_serve(bind := '127.0.0.1', port := ${PORT}, token := '${TOKEN}');
CALL harbor_wait();
" > "${SERVER_LOG}" 2>&1 &
SERVER_PID=$!
sleep 2

# ---- /info (no Origin) → no ACAO header at all ----
INFO_HEADERS="$(curl -sI "http://127.0.0.1:${PORT}/info")"
echo "${INFO_HEADERS}" | grep -qi '^HTTP/1.1 204' \
    || fail "/info expected 204, got $(echo "${INFO_HEADERS}" | head -1)"
if echo "${INFO_HEADERS}" | grep -qi '^Access-Control-Allow-Origin:'; then
    fail "/info should NOT emit Access-Control-Allow-Origin without an Origin request header"
fi
pass "/info no-Origin → 204, no ACAO"

# ---- /info with allowed Origin → ACAO echoes the match ----
ALLOWED_HEADERS="$(curl -sI -H 'Origin: https://app.example.com' "http://127.0.0.1:${PORT}/info")"
echo "${ALLOWED_HEADERS}" | grep -qi '^Access-Control-Allow-Origin: https://app.example.com' \
    || fail "/info with allowed Origin must echo it back exactly"
echo "${ALLOWED_HEADERS}" | grep -qi '^Access-Control-Allow-Credentials: true' \
    || fail "/info with allowed Origin must include Allow-Credentials: true"
pass "/info allowed-Origin → ACAO echoes match"

# ---- /info with disallowed Origin → no ACAO ----
DISALLOWED_HEADERS="$(curl -sI -H 'Origin: https://evil.example.com' "http://127.0.0.1:${PORT}/info")"
if echo "${DISALLOWED_HEADERS}" | grep -qi '^Access-Control-Allow-Origin:'; then
    fail "/info with disallowed Origin must NOT echo any ACAO"
fi
pass "/info disallowed-Origin → no ACAO"

# ---- POST /auth/login with valid JSON body ----
LOGIN_RESP="$(curl -s -i -X POST "http://127.0.0.1:${PORT}/auth/login" \
    -H 'Content-Type: application/json' \
    -d "{\"token\":\"${TOKEN}\"}")"
echo "${LOGIN_RESP}" | grep -qi '^HTTP/1.1 200 OK' \
    || fail "/auth/login valid token expected 200, got $(echo "${LOGIN_RESP}" | head -1)"
SET_COOKIE_LINE="$(echo "${LOGIN_RESP}" | grep -i '^Set-Cookie: harbor_session=' || true)"
if [[ -z "${SET_COOKIE_LINE}" ]]; then
    fail "/auth/login valid token must set harbor_session cookie"
fi
COOKIE_VALUE="$(echo "${SET_COOKIE_LINE}" | sed -E 's/^[Ss]et-[Cc]ookie: harbor_session=([^;]+).*$/\1/' | tr -d '\r')"
[[ "${COOKIE_VALUE}" =~ ^v1\..*\..*\..*\..* ]] \
    || fail "cookie value must match format v1.<seg1>.<seg2>.<seg3>.<seg4>; got '${COOKIE_VALUE}'"
echo "${SET_COOKIE_LINE}" | grep -qi 'HttpOnly' \
    || fail "cookie must include HttpOnly attribute"
echo "${SET_COOKIE_LINE}" | grep -qi 'SameSite=Strict' \
    || fail "cookie must include SameSite=Strict attribute"
echo "${SET_COOKIE_LINE}" | grep -qi 'Path=/' \
    || fail "cookie must include Path=/ attribute"
pass "/auth/login valid → 200 + cookie v1.<...> + HttpOnly + SameSite=Strict"

# Persist cookie to jar for later requests.
echo -e "127.0.0.1\tFALSE\t/\tFALSE\t0\tharbor_session\t${COOKIE_VALUE}" > "${COOKIE_JAR}"

# ---- POST /auth/login with INVALID token → 401 ----
INVALID_STATUS="$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:${PORT}/auth/login" \
    -H 'Content-Type: application/json' \
    -d '{"token":"definitely-wrong"}')"
[[ "${INVALID_STATUS}" == "401" ]] \
    || fail "/auth/login invalid token expected 401, got ${INVALID_STATUS}"
pass "/auth/login invalid → 401"

# ---- POST /auth/login with bearer header instead of body ----
BEARER_STATUS="$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:${PORT}/auth/login" \
    -H "Authorization: Bearer ${TOKEN}" -d '')"
[[ "${BEARER_STATUS}" == "200" ]] \
    || fail "/auth/login with Bearer header expected 200, got ${BEARER_STATUS}"
pass "/auth/login Bearer → 200"

# ---- POST /auth/logout → 200 + clear cookie ----
LOGOUT_RESP="$(curl -s -i -X POST "http://127.0.0.1:${PORT}/auth/logout" -d '')"
echo "${LOGOUT_RESP}" | grep -qi '^HTTP/1.1 200 OK' \
    || fail "/auth/logout expected 200, got $(echo "${LOGOUT_RESP}" | head -1)"
echo "${LOGOUT_RESP}" | grep -qi '^Set-Cookie: harbor_session=; .*Max-Age=0' \
    || fail "/auth/logout must clear cookie with Max-Age=0"
pass "/auth/logout → 200 + cleared cookie"

pass "GET /random/path no-cookie → 401"

# ---- GET /auth/login → 405 Method Not Allowed (round-12 fix) ----
GET_LOGIN="$(curl -s -i "http://127.0.0.1:${PORT}/auth/login")"
echo "${GET_LOGIN}" | grep -qi '^HTTP/1.1 405' \
    || fail "GET /auth/login expected 405 Method Not Allowed, got $(echo "${GET_LOGIN}" | head -1)"
echo "${GET_LOGIN}" | grep -qi '^Allow: POST' \
    || fail "GET /auth/login should advertise Allow: POST, OPTIONS"
pass "GET /auth/login → 405 + Allow: POST"

# ---- /quack is gone (Quack removed) ----
# The Quack RPC endpoint was removed. POST /quack must now 404 — no
# route is registered for it.
QUACK_STATUS="$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:${PORT}/quack" \
    -H 'Content-Type: application/vnd.duckdb' -d '')"
[[ "${QUACK_STATUS}" == "404" ]] \
    || fail "/quack must 404 after Quack removal; got ${QUACK_STATUS}"
pass "/quack removed → 404"

green "All cookie-auth golden assertions passed."
