#!/usr/bin/env bash
# Golden HTTP-roundtrip test for PR-6 admin handlers.
#
# Coverage:
#   - Public probes: /health, /info, /ready
#   - Default-deny on __HARBOR_ADMIN__:* without a custom authz fn:
#       /whoami, /tables, /schema, /checkpoint, /sessions, /interrupt,
#       /sql/cancel all return 403 even with a valid bearer; own-session
#       create/delete (__HARBOR_SELF__ scope) still work
#   - With harbor_allow_admin_without_authz=true:
#       /whoami returns identity JSON
#       /tables returns the catalog
#       /schema/<db>/<table> on missing table → 404; on existing table → 200
#       /checkpoint succeeds (returns iso ts + wal_state_available:false)
#       /sessions returns the snapshot (initially empty; populated after
#         POST /sql/sessions/new)
#       /interrupt rejects non-JSON Content-Type with 415, missing
#         sessionId with 400, unknown sid with 404, valid sid with 200
#       /sql/cancel mirrors /interrupt
#   - With a custom authz fn that grants per-resource:action:
#       /whoami granted, /tables denied, /sessions granted — proves
#       the resource:action grammar is decidable per route
#   - Cookie-authenticated mutating POSTs require an Origin/Referer in
#     harbor_cors_origins:
#       /interrupt with cookie + missing Origin → 403
#       /interrupt with cookie + allowed Origin → 200
#   - Body-limit on /interrupt with oversized body → 413
#   - Path-param identifier safety: /schema with table name containing
#     a single quote does not cause a SQL syntax error and does not
#     leak the raw identifier into a SQL string (404 SAFE)
#
# Usage:
#   make release
#   scripts/golden-admin-roundtrip.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXT_PATH="${REPO_ROOT}/build/release/extension/harbor/harbor.duckdb_extension"
DUCKDB_BIN="${REPO_ROOT}/build/release/duckdb"
PORT="${HARBOR_ADMIN_TEST_PORT:-19508}"
TOKEN="admin-golden-token-$$"
SERVER_LOG="$(mktemp)"
COOKIE_JAR="$(mktemp)"
SERVER_PID=""

cleanup() {
    if [[ -n "${SERVER_PID}" ]]; then
        kill "${SERVER_PID}" 2>/dev/null || true
        wait "${SERVER_PID}" 2>/dev/null || true
    fi
    rm -f "${SERVER_LOG}" "${COOKIE_JAR}" /tmp/harbor-admin-*.json /tmp/harbor-admin-*.body
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

# Phase 1 — server WITHOUT admin-bypass: prove the default-deny matrix.
nohup "${DUCKDB_BIN}" -unsigned -no-stdin -c "
LOAD '${EXT_PATH}';
SET GLOBAL harbor_cors_origins='https://app.example.com';
SET GLOBAL harbor_max_request_body_bytes=1024;
-- harbor_allow_admin_without_authz NOT set -> default-deny path
CALL harbor_serve(bind := '127.0.0.1', port := ${PORT}, token := '${TOKEN}');
CALL harbor_wait();
" > "${SERVER_LOG}" 2>&1 &
SERVER_PID=$!
sleep 2

curl -sf -o /dev/null "http://127.0.0.1:${PORT}/info" || {
    echo "--- server log ---" >&2; cat "${SERVER_LOG}" >&2
    fail "server did not start"
}
pass "server started (default-deny mode)"

# /health, /info, /ready are PUBLIC even without authz config.
[[ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:${PORT}/health)" == "200" ]] \
    || fail "/health expected 200"
[[ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:${PORT}/info)" == "204" ]] \
    || fail "/info expected 204"
[[ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:${PORT}/ready)" == "200" ]] \
    || fail "/ready expected 200"
pass "public probes (/health, /info, /ready) all 200/204"

# Default-deny on __HARBOR_ADMIN__:* with valid bearer but no custom authz fn.
# Each route should return 403 FORBIDDEN.
for path in /whoami /tables /sessions; do
    code="$(curl -s -o /tmp/harbor-admin-deny.body -w '%{http_code}' \
        -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT}${path}")"
    [[ "${code}" == "403" ]] || fail "${path} expected 403 in default-deny mode (got ${code})"
    grep -q '"errorCode":"FORBIDDEN"' /tmp/harbor-admin-deny.body \
        || fail "${path} missing FORBIDDEN error code"
done
pass "default-deny: /whoami /tables /sessions all return 403"

# /schema is also gated.
code="$(curl -s -o /tmp/harbor-admin-deny.body -w '%{http_code}' \
    -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT}/schema/main/whatever")"
[[ "${code}" == "403" ]] || fail "/schema expected 403 in default-deny mode"
pass "default-deny: /schema returns 403"

# Mutating POSTs likewise.
for path in /checkpoint /interrupt; do
    code="$(curl -s -o /tmp/harbor-admin-deny.body -w '%{http_code}' -X POST \
        -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
        -d '{}' "http://127.0.0.1:${PORT}${path}")"
    [[ "${code}" == "403" ]] || fail "${path} expected 403 in default-deny mode (got ${code})"
done
code="$(curl -s -o /tmp/harbor-admin-deny.body -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
    -d '{"sessionId":"x"}' "http://127.0.0.1:${PORT}/sql/cancel")"
[[ "${code}" == "403" ]] || fail "/sql/cancel expected 403 in default-deny mode (got ${code})"
pass "default-deny: /checkpoint /interrupt /sql/cancel all return 403"

# Own-session lifecycle is __HARBOR_SELF__-scoped, NOT admin — it must
# work on this same default-deny server with nothing but a valid bearer.
SELF_SESS="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql/sessions/new" \
    -H "Authorization: Bearer ${TOKEN}" -d '')"
SELF_SID="$(echo "${SELF_SESS}" | sed -E 's/.*"sessionId":"([^"]+)".*/\1/')"
[[ -n "${SELF_SID}" && "${SELF_SID}" != "${SELF_SESS}" ]] \
    || fail "own-session create should succeed in default-deny mode: ${SELF_SESS}"
code="$(curl -s -o /dev/null -w '%{http_code}' -X DELETE \
    -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT}/sql/sessions/${SELF_SID}")"
[[ "${code}" == "200" ]] || fail "own-session delete expected 200 in default-deny mode (got ${code})"
pass "own-session create/delete (__HARBOR_SELF__) unaffected by admin default-deny"

# Round-19 follow-up: /ready is PUBLIC and must not echo DuckDB error
# detail in its body. Happy-path is "{ok:true}" only; failure shape is
# bare "{ok:false}" only. We can't easily simulate a failure, but
# we can assert the happy-path body is exactly the bare ok envelope.
READY_BODY="$(curl -s "http://127.0.0.1:${PORT}/ready")"
[[ "${READY_BODY}" == '{"ok":true}' ]] || fail "/ready ok body must be exactly {ok:true} (got ${READY_BODY})"
pass "/ready ok body is detail-free"

# Round-19 follow-up: stricter Content-Type rejection. With admin-bypass
# off we can't invoke the route body, but we CAN exercise the CT check
# precedence: auth/authz on /interrupt fires first (a 403 result here
# would mean the CT check ran AFTER auth which is fine; we just need
# both 415-paths covered in the bypass-mode block below).

# Wrong/missing token still 401 (not 403) — auth precedes authz.
code="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer wrong-token" "http://127.0.0.1:${PORT}/whoami")"
[[ "${code}" == "401" ]] || fail "/whoami with wrong token expected 401 (got ${code})"
code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${PORT}/whoami")"
[[ "${code}" == "401" ]] || fail "/whoami with no auth expected 401 (got ${code})"
pass "auth precedes authz: invalid/missing token returns 401, not 403"

kill "${SERVER_PID}" 2>/dev/null || true
wait "${SERVER_PID}" 2>/dev/null || true
SERVER_PID=""
sleep 1

# Phase 2 — server WITH admin-bypass: prove the routes work.
PORT2="$((PORT + 1))"
nohup "${DUCKDB_BIN}" -unsigned -no-stdin -c "
LOAD '${EXT_PATH}';
SET GLOBAL harbor_cors_origins='https://app.example.com';
SET GLOBAL harbor_max_request_body_bytes=512;
SET GLOBAL harbor_allow_admin_without_authz=true;
CALL harbor_serve(bind := '127.0.0.1', port := ${PORT2}, token := '${TOKEN}');
CALL harbor_wait();
" > "${SERVER_LOG}" 2>&1 &
SERVER_PID=$!
sleep 2
curl -sf -o /dev/null "http://127.0.0.1:${PORT2}/info" || fail "phase-2 server did not start"
pass "server started (admin-bypass mode)"

# /whoami returns identity JSON (bearer).
WHOAMI="$(curl -sf -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT2}/whoami")"
echo "${WHOAMI}" | grep -q '"ok":true' || fail "/whoami missing ok:true"
echo "${WHOAMI}" | grep -q '"auth_source":"bearer"' || fail "/whoami missing auth_source:bearer"
echo "${WHOAMI}" | grep -qE '"principal":"[0-9a-f]{64}"' || fail "/whoami principal is not 64-hex"
pass "/whoami returns identity JSON with bearer"

# /tables returns the catalog (initially empty for user tables, but the
# system tables filter happens client-side; we just check shape).
TABLES="$(curl -sf -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT2}/tables")"
echo "${TABLES}" | grep -q '"ok":true' || fail "/tables missing ok:true"
echo "${TABLES}" | grep -q '"tables":\[' || fail "/tables missing tables array"
pass "/tables returns array shape"

# /schema/<db>/<table>: missing table → 404 NOT_FOUND.
code="$(curl -s -o /tmp/harbor-admin.body -w '%{http_code}' \
    -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT2}/schema/memory/never_existed")"
[[ "${code}" == "404" ]] || fail "/schema on missing table expected 404 (got ${code})"
grep -q '"errorCode":"NOT_FOUND"' /tmp/harbor-admin.body \
    || fail "/schema 404 missing NOT_FOUND"
pass "/schema on missing table returns 404 NOT_FOUND"

# Path-param identifier safety: a table name with a single quote must
# not break SQL — bound parameter handling means it just doesn't match
# any table and we get 404. Without bound params this would either
# 500 or accept malicious SQL.
code="$(curl -s -o /tmp/harbor-admin.body -w '%{http_code}' \
    -H "Authorization: Bearer ${TOKEN}" \
    "http://127.0.0.1:${PORT2}/schema/memory/foo'%3Bdrop")"
[[ "${code}" == "404" ]] || fail "/schema with quoted name expected 404 (got ${code}) — possible SQL injection"
pass "/schema path-param identifier safety: quoted names route to 404, not SQL error"

# Create a real table via /sql so /schema and /tables have something
# to find. The /sql endpoint will reject __HARBOR_ADMIN__: prefix
# input but not normal SQL like CREATE TABLE.
curl -sf -X POST "http://127.0.0.1:${PORT2}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"CREATE TABLE people (id INTEGER, name VARCHAR)"}' > /dev/null
SCHEMA="$(curl -sf -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT2}/schema/memory/people")"
echo "${SCHEMA}" | grep -q '"name":"id"' || fail "/schema missing column id"
echo "${SCHEMA}" | grep -q '"name":"name"' || fail "/schema missing column name"
echo "${SCHEMA}" | grep -q '"type":"INTEGER"' || fail "/schema missing INTEGER type"
echo "${SCHEMA}" | grep -q '"schema":"main"' || fail "/schema should report schema:main"
pass "/schema/<db>/<table> on existing table returns columns array"

# /tables now sees the new table.
TABLES2="$(curl -sf -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT2}/tables")"
echo "${TABLES2}" | grep -q '"name":"people"' || fail "/tables should now include people"
pass "/tables reflects newly-created table"

# /checkpoint behavior depends on whether other write transactions are
# active. Right after CREATE TABLE, the catalog manager may still hold
# a snapshot — DuckDB returns "Cannot CHECKPOINT" and we map it to
# 409 CONFLICT (operators retry or use FORCE CHECKPOINT through /sql).
# Either 200 (no contention) or 409 (contention) is acceptable; the
# test asserts that we never return 5xx for this well-known case.
CK_OUT="$(curl -s -i -X POST -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT2}/checkpoint")"
CK_CODE="$(echo "${CK_OUT}" | head -1 | awk '{print $2}')"
case "${CK_CODE}" in
  200)
    echo "${CK_OUT}" | grep -q '"checkpointed_at"' || fail "/checkpoint 200 missing checkpointed_at"
    echo "${CK_OUT}" | grep -q '"wal_state_available":false' \
        || fail "/checkpoint 200 missing wal_state_available:false"
    ;;
  409)
    echo "${CK_OUT}" | grep -q '"errorCode":"CONFLICT"' \
        || fail "/checkpoint 409 missing CONFLICT errorCode"
    ;;
  *)
    fail "/checkpoint expected 200 or 409, got ${CK_CODE}"
    ;;
esac
pass "/checkpoint returns 200 (success) or 409 CONFLICT (transactions in flight) — never 5xx"

# /interrupt: 415 without Content-Type, 400 missing sessionId, 404
# unknown sid.
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT2}/interrupt" -d '{}')"
[[ "${code}" == "415" ]] || fail "/interrupt without CT expected 415 (got ${code})"
# Round-19 follow-up: stricter Content-Type — `application/jsonjunk` was
# previously accepted by the prefix-only check.
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/jsonjunk' \
    "http://127.0.0.1:${PORT2}/interrupt" -d '{"sessionId":"x"}')"
[[ "${code}" == "415" ]] || fail "/interrupt with bogus CT 'application/jsonjunk' expected 415 (got ${code})"
# But the canonical charset suffix must still pass.
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json; charset=utf-8' \
    "http://127.0.0.1:${PORT2}/interrupt" -d '{"sessionId":"deadbeef"}')"
[[ "${code}" == "404" ]] || fail "/interrupt with 'application/json; charset=utf-8' expected to pass CT check (got ${code})"
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
    "http://127.0.0.1:${PORT2}/interrupt" -d '{}')"
[[ "${code}" == "400" ]] || fail "/interrupt missing sessionId expected 400 (got ${code})"
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
    "http://127.0.0.1:${PORT2}/interrupt" -d '{"sessionId":"deadbeef"}')"
[[ "${code}" == "404" ]] || fail "/interrupt unknown sid expected 404 (got ${code})"
pass "/interrupt: 415 (no CT or junk CT), pass with charset suffix, 400 (no sid), 404 (unknown sid)"

# Body-limit: harbor_max_request_body_bytes=512; 600-byte body → 413.
BIG="$(printf 'x%.0s' $(seq 1 600))"
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
    "http://127.0.0.1:${PORT2}/interrupt" -d "{\"sessionId\":\"${BIG}\"}")"
[[ "${code}" == "413" ]] || fail "/interrupt oversize body expected 413 (got ${code})"
pass "/interrupt body-limit (harbor_max_request_body_bytes=512) returns 413"

# /sessions: snapshot is empty initially.
SESS_EMPTY="$(curl -sf -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT2}/sessions")"
echo "${SESS_EMPTY}" | grep -q '"sessions":\[\]' || fail "/sessions should be empty initially"
pass "/sessions empty initially"

# Create a session via /sql/sessions/new and verify /sessions populates.
SESS_NEW="$(curl -sf -X POST "http://127.0.0.1:${PORT2}/sql/sessions/new" \
    -H "Authorization: Bearer ${TOKEN}" -d '')"
SID="$(echo "${SESS_NEW}" | sed -E 's/.*"sessionId":"([^"]+)".*/\1/')"
[[ -n "${SID}" && "${SID}" != "${SESS_NEW}" ]] || fail "/sql/sessions/new failed to return sessionId"
SESS="$(curl -sf -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT2}/sessions")"
echo "${SESS}" | grep -q "\"session_id\":\"${SID}\"" || fail "/sessions missing newly-created sid"
echo "${SESS}" | grep -qE '"principal":"[0-9a-f]{64}"' || fail "/sessions missing 64-hex principal"
echo "${SESS}" | grep -qE '"age_s":[0-9]+' || fail "/sessions missing age_s"
echo "${SESS}" | grep -q '"in_flight":false' || fail "/sessions missing in_flight:false"
echo "${SESS}" | grep -q '"last_query_truncated":false' || fail "/sessions missing last_query_truncated"
pass "/sessions reflects newly-created session with all instrumentation fields"

# /interrupt on a real session — 200 ok.
INTR="$(curl -sf -X POST -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
    "http://127.0.0.1:${PORT2}/interrupt" -d "{\"sessionId\":\"${SID}\"}")"
echo "${INTR}" | grep -q '"ok":true' || fail "/interrupt valid sid missing ok:true"
echo "${INTR}" | grep -q "\"session_id\":\"${SID}\"" || fail "/interrupt missing session_id echo"
pass "/interrupt valid sid returns 200 with ok:true"

# /sql/cancel mirrors /interrupt — same shape, distinct authz string
# so a custom authz could grant one without the other.
CANCEL="$(curl -sf -X POST -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
    "http://127.0.0.1:${PORT2}/sql/cancel" -d "{\"sessionId\":\"${SID}\"}")"
echo "${CANCEL}" | grep -q '"ok":true' || fail "/sql/cancel valid sid missing ok:true"
pass "/sql/cancel valid sid returns 200 with ok:true"

# Cookie-authenticated mutating POSTs require allowed Origin. First
# log in to get a cookie.
COOKIE_VAL="$(curl -s -i -X POST "http://127.0.0.1:${PORT2}/auth/login" \
    -H "Authorization: Bearer ${TOKEN}" -d '' \
    | tr -d '\r' | awk '/^Set-Cookie: harbor_session=/{print substr($2, index($2, "=") + 1); exit}' \
    | sed 's/;.*//')"
[[ -n "${COOKIE_VAL}" ]] || fail "could not obtain harbor_session cookie"

# /interrupt cookie + no Origin → 403.
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Cookie: harbor_session=${COOKIE_VAL}" -H 'Content-Type: application/json' \
    "http://127.0.0.1:${PORT2}/interrupt" -d "{\"sessionId\":\"${SID}\"}")"
[[ "${code}" == "403" ]] || fail "/interrupt cookie + no Origin expected 403 (got ${code})"
# /interrupt cookie + allowed Origin → 200.
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Cookie: harbor_session=${COOKIE_VAL}" -H 'Content-Type: application/json' \
    -H 'Origin: https://app.example.com' \
    "http://127.0.0.1:${PORT2}/interrupt" -d "{\"sessionId\":\"${SID}\"}")"
[[ "${code}" == "200" ]] || fail "/interrupt cookie + allowed Origin expected 200 (got ${code})"
# /interrupt cookie + disallowed Origin → 403.
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Cookie: harbor_session=${COOKIE_VAL}" -H 'Content-Type: application/json' \
    -H 'Origin: https://evil.example.com' \
    "http://127.0.0.1:${PORT2}/interrupt" -d "{\"sessionId\":\"${SID}\"}")"
[[ "${code}" == "403" ]] || fail "/interrupt cookie + disallowed Origin expected 403 (got ${code})"
pass "/interrupt cookie auth: no Origin / allowed Origin / disallowed Origin → 403/200/403"

# OPTIONS preflight on /interrupt (allowed origin → 204 with ACAO).
PREF="$(curl -s -i -X OPTIONS "http://127.0.0.1:${PORT2}/interrupt" \
    -H 'Origin: https://app.example.com' \
    -H 'Access-Control-Request-Method: POST' \
    -H 'Access-Control-Request-Headers: Authorization, Content-Type')"
echo "${PREF}" | grep -qi '^HTTP/1.1 204' || fail "OPTIONS /interrupt expected 204"
echo "${PREF}" | grep -qi '^Access-Control-Allow-Origin: https://app.example.com' \
    || fail "OPTIONS /interrupt must echo allowed Origin"
pass "OPTIONS /interrupt CORS preflight"

# OPTIONS preflight on /sql/cancel.
PREF="$(curl -s -i -X OPTIONS "http://127.0.0.1:${PORT2}/sql/cancel" \
    -H 'Origin: https://app.example.com' \
    -H 'Access-Control-Request-Method: POST' \
    -H 'Access-Control-Request-Headers: Authorization, Content-Type')"
echo "${PREF}" | grep -qi '^HTTP/1.1 204' || fail "OPTIONS /sql/cancel expected 204"
echo "${PREF}" | grep -qi '^Access-Control-Allow-Origin: https://app.example.com' \
    || fail "OPTIONS /sql/cancel must echo allowed Origin"
pass "OPTIONS /sql/cancel CORS preflight"

# OPTIONS preflight on /checkpoint.
PREF="$(curl -s -i -X OPTIONS "http://127.0.0.1:${PORT2}/checkpoint" \
    -H 'Origin: https://app.example.com' \
    -H 'Access-Control-Request-Method: POST' \
    -H 'Access-Control-Request-Headers: Authorization, Content-Type')"
echo "${PREF}" | grep -qi '^HTTP/1.1 204' || fail "OPTIONS /checkpoint expected 204"
pass "OPTIONS /checkpoint CORS preflight"

kill "${SERVER_PID}" 2>/dev/null || true
wait "${SERVER_PID}" 2>/dev/null || true
SERVER_PID=""
sleep 1

# Phase 3 — server with a CUSTOM AUTHZ FN that grants per-resource:action.
# Proves the resource:action grammar is decidable per route (the SPEC §7
# example pattern: starts_with(query, '__HARBOR_ADMIN__:server:') etc.).
PORT3="$((PORT + 2))"
nohup "${DUCKDB_BIN}" -unsigned -no-stdin -c "
LOAD '${EXT_PATH}';
CREATE MACRO admin_authz(sid, query) AS (
  CASE
    WHEN starts_with(query, '__HARBOR_ADMIN__:server:') THEN true
    WHEN starts_with(query, '__HARBOR_ADMIN__:sessions:') THEN true
    WHEN starts_with(query, '__HARBOR_ADMIN__:catalog:') THEN false
    WHEN starts_with(query, '__HARBOR_ADMIN__:checkpoint:') THEN false
    WHEN starts_with(query, '__HARBOR_SELF__:sessions:create') THEN false
    ELSE true  -- non-admin queries permitted
  END
);
SET GLOBAL harbor_authorization_function='admin_authz';
CALL harbor_serve(bind := '127.0.0.1', port := ${PORT3}, token := '${TOKEN}');
CALL harbor_wait();
" > "${SERVER_LOG}" 2>&1 &
SERVER_PID=$!
sleep 2
curl -sf -o /dev/null "http://127.0.0.1:${PORT3}/info" || fail "phase-3 server did not start"
pass "server started (custom authz fn mode)"

code="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT3}/whoami")"
[[ "${code}" == "200" ]] || fail "custom authz: /whoami should be granted (got ${code})"
code="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT3}/sessions")"
[[ "${code}" == "200" ]] || fail "custom authz: /sessions should be granted (got ${code})"
code="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT3}/tables")"
[[ "${code}" == "403" ]] || fail "custom authz: /tables should be denied (got ${code})"
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
    "http://127.0.0.1:${PORT3}/checkpoint" -d '{}')"
[[ "${code}" == "403" ]] || fail "custom authz: /checkpoint should be denied (got ${code})"
pass "custom authz fn: per-resource gating (server/sessions allowed; catalog/checkpoint denied)"

# The custom policy above denies __HARBOR_SELF__:sessions:create —
# proving policies see and can restrict the own-session scope too.
code="$(curl -s -o /tmp/harbor-admin-deny.body -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT3}/sql/sessions/new" -d '')"
[[ "${code}" == "403" ]] || fail "custom authz: session create should be deniable (got ${code})"
grep -q '"errorCode":"FORBIDDEN"' /tmp/harbor-admin-deny.body \
    || fail "denied session create missing FORBIDDEN error code"
pass "custom authz fn: __HARBOR_SELF__:sessions:create deniable by policy"

kill "${SERVER_PID}" 2>/dev/null || true
wait "${SERVER_PID}" 2>/dev/null || true
SERVER_PID=""
sleep 1

# Phase 4 (round-19 follow-up) — explicit-NOP-authz still admin-denies.
# This is the security fix round 19 caught: an operator explicitly
# setting `harbor_authorization_function='harbor_nop_authorization'`
# (the built-in default-allow) should NOT thereby gain admin access.
PORT4="$((PORT + 3))"
nohup "${DUCKDB_BIN}" -unsigned -no-stdin -c "
LOAD '${EXT_PATH}';
-- harbor_allow_admin_without_authz NOT set
SET GLOBAL harbor_authorization_function='harbor_nop_authorization';
CALL harbor_serve(bind := '127.0.0.1', port := ${PORT4}, token := '${TOKEN}');
CALL harbor_wait();
" > "${SERVER_LOG}" 2>&1 &
SERVER_PID=$!
sleep 2
curl -sf -o /dev/null "http://127.0.0.1:${PORT4}/info" || fail "phase-4 server did not start"
pass "server started (explicit nop-authz mode — round 19 follow-up)"

# /whoami /tables /sessions /checkpoint /interrupt /sql/cancel must
# all still default-deny — explicit nop must NOT count as "custom".
for path in /whoami /tables /sessions; do
    code="$(curl -s -o /dev/null -w '%{http_code}' \
        -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT4}${path}")"
    [[ "${code}" == "403" ]] || \
        fail "explicit-nop authz: ${path} expected 403 (got ${code}) — round-19 fail-open regression"
done
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
    "http://127.0.0.1:${PORT4}/checkpoint" -d '{}')"
[[ "${code}" == "403" ]] || \
    fail "explicit-nop authz: /checkpoint expected 403 (got ${code}) — round-19 fail-open regression"
pass "explicit nop-authz fn (harbor_nop_authorization) does NOT count as custom — admin still default-denied"

# Same check with harbor_authorization_function explicitly set to
# the harbor-side nop name. Test case-insensitivity at the same time.
kill "${SERVER_PID}" 2>/dev/null || true
wait "${SERVER_PID}" 2>/dev/null || true
SERVER_PID=""
sleep 1
PORT5="$((PORT + 4))"
nohup "${DUCKDB_BIN}" -unsigned -no-stdin -c "
LOAD '${EXT_PATH}';
SET GLOBAL harbor_authorization_function='Harbor_NOP_Authorization';
CALL harbor_serve(bind := '127.0.0.1', port := ${PORT5}, token := '${TOKEN}');
CALL harbor_wait();
" > "${SERVER_LOG}" 2>&1 &
SERVER_PID=$!
sleep 2
curl -sf -o /dev/null "http://127.0.0.1:${PORT5}/info" || fail "phase-5 server did not start"
code="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT5}/whoami")"
[[ "${code}" == "403" ]] || \
    fail "explicit case-mixed nop authz: /whoami expected 403 (got ${code})"
pass "explicit nop-authz fn case-insensitive — Harbor_NOP_Authorization still triggers admin default-deny"

# Round-20 polish: schema-qualified nop fn name must also be recognized.
# Without IsBuiltinNopAuthz's rfind('.')+substr step, `main.harbor_nop_authorization`
# would slip through and re-open the round-19 fail-open.
kill "${SERVER_PID}" 2>/dev/null || true
wait "${SERVER_PID}" 2>/dev/null || true
SERVER_PID=""
sleep 1
PORT6="$((PORT + 5))"
nohup "${DUCKDB_BIN}" -unsigned -no-stdin -c "
LOAD '${EXT_PATH}';
SET GLOBAL harbor_authorization_function='main.harbor_nop_authorization';
CALL harbor_serve(bind := '127.0.0.1', port := ${PORT6}, token := '${TOKEN}');
CALL harbor_wait();
" > "${SERVER_LOG}" 2>&1 &
SERVER_PID=$!
sleep 2
curl -sf -o /dev/null "http://127.0.0.1:${PORT6}/info" || fail "phase-6 server did not start"
code="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer ${TOKEN}" "http://127.0.0.1:${PORT6}/whoami")"
[[ "${code}" == "403" ]] || \
    fail "schema-qualified nop authz: /whoami expected 403 (got ${code}) — round-20 schema-strip regression"
pass "schema-qualified nop-authz fn (main.harbor_nop_authorization) still triggers admin default-deny"

green "All admin-roundtrip golden assertions passed."
