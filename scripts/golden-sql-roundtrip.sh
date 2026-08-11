#!/usr/bin/env bash
# Golden HTTP-roundtrip test for PR-5 /sql.
#
# Coverage:
#   - Auth: bearer, cookie, invalid token
#   - CORS preflight on /sql
#   - Default NDJSON row mode
#   - NDJSON chunk mode
#   - One-shot JSON mode
#   - Request validation: missing sql, multi-statement, __HARBOR_ADMIN__, oversize body
#   - Params: implicit `$1`, typed wrapper DECIMAL, typed NULL
#   - Sessions: create, transaction state across requests, delete, foreign/not-found 404
#     (own-session scope — no admin grant; plus unauthenticated local-dev mode)
#   - /auth/logout?destroy_sessions=true destroys owned SQL sessions
#   - Representative type encodings: BIGINT smart number/string, DECIMAL string, INTERVAL object, BLOB base64, JSON text string
#
# Usage:
#   make release
#   scripts/golden-sql-roundtrip.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXT_PATH="${REPO_ROOT}/build/release/extension/harbor/harbor.duckdb_extension"
DUCKDB_BIN="${REPO_ROOT}/build/release/duckdb"
PORT="${HARBOR_SQL_TEST_PORT:-19506}"
TOKEN="sql-golden-token-$$"
SERVER_LOG="$(mktemp)"
COOKIE_JAR="$(mktemp)"
SERVER_PID=""

cleanup() {
    if [[ -n "${SERVER_PID}" ]]; then
        kill "${SERVER_PID}" 2>/dev/null || true
        wait "${SERVER_PID}" 2>/dev/null || true
    fi
    rm -f "${SERVER_LOG}" "${COOKIE_JAR}" /tmp/harbor-sql-*.json /tmp/harbor-sql-*.ndjson /tmp/harbor-sql-big-body.json
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

nohup "${DUCKDB_BIN}" -unsigned -no-stdin -c "
LOAD '${EXT_PATH}';
SET GLOBAL harbor_cors_origins='https://app.example.com';
SET GLOBAL harbor_max_request_body_bytes=1024;
-- Session create/delete are __HARBOR_SELF__-scoped (own-session
-- lifecycle), allowed for any authenticated caller under the default
-- nop authz. Deliberately NO harbor_allow_admin_without_authz here —
-- the session assertions below double as the regression test that
-- own-session ops need zero admin grants.
CALL harbor_serve(bind := '127.0.0.1', port := ${PORT}, token := '${TOKEN}');
CALL harbor_wait();
" > "${SERVER_LOG}" 2>&1 &
SERVER_PID=$!
sleep 2

# Ensure server is listening.
curl -sf -o /dev/null "http://127.0.0.1:${PORT}/info" || {
    echo "--- server log ---" >&2
    cat "${SERVER_LOG}" >&2
    fail "server did not start"
}
pass "server started"

# ---- CORS preflight ----
PREFLIGHT="$(curl -s -i -X OPTIONS "http://127.0.0.1:${PORT}/sql" \
    -H 'Origin: https://app.example.com' \
    -H 'Access-Control-Request-Method: POST' \
    -H 'Access-Control-Request-Headers: Authorization, Content-Type')"
echo "${PREFLIGHT}" | grep -qi '^HTTP/1.1 204' \
    || fail "OPTIONS /sql expected 204"
echo "${PREFLIGHT}" | grep -qi '^Access-Control-Allow-Origin: https://app.example.com' \
    || fail "OPTIONS /sql must echo allowed Origin"
echo "${PREFLIGHT}" | grep -qi '^Access-Control-Allow-Credentials: true' \
    || fail "OPTIONS /sql must include Allow-Credentials"
pass "OPTIONS /sql CORS preflight"

PREFLIGHT_SESS="$(curl -s -i -X OPTIONS "http://127.0.0.1:${PORT}/sql/sessions/new" \
    -H 'Origin: https://app.example.com' \
    -H 'Access-Control-Request-Method: POST' \
    -H 'Access-Control-Request-Headers: Authorization, Content-Type')"
echo "${PREFLIGHT_SESS}" | grep -qi '^HTTP/1.1 204' \
    || fail "OPTIONS /sql/sessions/new expected 204"
echo "${PREFLIGHT_SESS}" | grep -qi '^Access-Control-Allow-Methods: .*POST' \
    || fail "OPTIONS /sql/sessions/new must allow POST"
pass "OPTIONS /sql/sessions/new CORS preflight"

# ---- NDJSON row mode ----
curl -sfN -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"SELECT 42 AS answer, '\''hello'\'' AS greeting"}' \
    > /tmp/harbor-sql-row.ndjson
grep -q '"type":"schema"' /tmp/harbor-sql-row.ndjson || fail "row mode missing schema"
grep -q '"duckdbType":"INTEGER"' /tmp/harbor-sql-row.ndjson || fail "row mode missing INTEGER schema"
grep -q '"values":\[42,"hello"\]' /tmp/harbor-sql-row.ndjson || fail "row mode missing row values"
grep -q '"type":"end"' /tmp/harbor-sql-row.ndjson || fail "row mode missing end"
pass "POST /sql NDJSON row mode"

# ---- NDJSON chunk mode ----
curl -sfN -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Accept: application/x-ndjson; shape=chunk' \
    -H 'Content-Type: application/json' \
    -d '{"sql":"FROM range(3) SELECT range AS i"}' \
    > /tmp/harbor-sql-chunk.ndjson
grep -q '"type":"chunk"' /tmp/harbor-sql-chunk.ndjson || fail "chunk mode missing chunk line"
grep -q '"rows":\[\[0\],\[1\],\[2\]\]' /tmp/harbor-sql-chunk.ndjson || fail "chunk mode rows mismatch"
pass "POST /sql NDJSON chunk mode"

# ---- One-shot JSON ----
curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d '{"sql":"SELECT 1 AS x, 2 AS y"}' \
    > /tmp/harbor-sql-oneshot.json
grep -q '"ok":true' /tmp/harbor-sql-oneshot.json || fail "one-shot missing ok"
grep -q '"kind":"select"' /tmp/harbor-sql-oneshot.json || fail "one-shot missing kind"
grep -q '"data":\[\[1,2\]\]' /tmp/harbor-sql-oneshot.json || fail "one-shot data mismatch"
pass "POST /sql one-shot JSON"

# ---- Validation failures ----
code="$(curl -s -o /tmp/harbor-sql-missing.json -w '%{http_code}' \
    -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{}')"
[[ "${code}" == "400" ]] || fail "missing sql expected 400, got ${code}"
grep -q '"errorCode":"BAD_REQUEST"' /tmp/harbor-sql-missing.json || fail "missing sql error code"
pass "missing sql rejected"

code="$(curl -s -o /tmp/harbor-sql-multi.json -w '%{http_code}' \
    -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"SELECT 1; SELECT 2;"}')"
[[ "${code}" == "400" ]] || fail "multi-statement expected 400, got ${code}"
grep -q '"errorCode":"BAD_REQUEST"' /tmp/harbor-sql-multi.json || fail "multi-statement error code"
pass "multi-statement rejected"

code="$(curl -s -o /tmp/harbor-sql-admin.json -w '%{http_code}' \
    -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"__HARBOR_ADMIN__:checkpoint:create"}')"
[[ "${code}" == "400" ]] || fail "__HARBOR_ADMIN__ expected 400, got ${code}"
pass "__HARBOR_ADMIN__ reserved prefix rejected"

code="$(curl -s -o /dev/null -w '%{http_code}' \
    -X POST "http://127.0.0.1:${PORT}/sql" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"__HARBOR_SELF__:sessions:create"}')"
[[ "${code}" == "400" ]] || fail "__HARBOR_SELF__ expected 400, got ${code}"
pass "__HARBOR_SELF__ reserved prefix rejected"

python3 - <<'PY' >/tmp/harbor-sql-big-body.json
print('{"sql":"' + ('x' * 2000) + '"}')
PY
code="$(curl -s -o /tmp/harbor-sql-big-response.json -w '%{http_code}' \
    -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    --data-binary @/tmp/harbor-sql-big-body.json)"
[[ "${code}" == "413" ]] || fail "oversized body expected 413, got ${code}"
pass "oversized body rejected"

# ---- Auth failures and cookie auth ----
code="$(curl -s -o /tmp/harbor-sql-auth.json -w '%{http_code}' \
    -X POST "http://127.0.0.1:${PORT}/sql" \
    -H 'Authorization: Bearer wrong-token' \
    -H 'Content-Type: application/json' \
    -d '{"sql":"SELECT 1"}')"
[[ "${code}" == "401" ]] || fail "invalid token expected 401, got ${code}"
pass "invalid bearer rejected"

LOGIN="$(curl -s -i -X POST "http://127.0.0.1:${PORT}/auth/login" \
    -H 'Content-Type: application/json' \
    -d "{\"token\":\"${TOKEN}\"}")"
COOKIE="$(echo "${LOGIN}" | grep -i '^Set-Cookie: harbor_session=' | sed -E 's/^[Ss]et-[Cc]ookie: harbor_session=([^;]+).*$/\1/' | tr -d '\r')"
[[ -n "${COOKIE}" ]] || fail "failed to obtain harbor_session cookie"
curl -sfN -X POST "http://127.0.0.1:${PORT}/sql" \
    -b "harbor_session=${COOKIE}" \
    -H 'Origin: https://app.example.com' \
    -H 'Content-Type: application/json' \
    -d '{"sql":"SELECT 7 AS cookie_ok"}' \
    > /tmp/harbor-sql-cookie.ndjson
grep -q '"values":\[7\]' /tmp/harbor-sql-cookie.ndjson || fail "cookie-auth /sql did not return row"
pass "cookie auth works for /sql"

# ---- Params ----
curl -sfN -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"SELECT $1::INTEGER + 1 AS n","params":[41]}' \
    > /tmp/harbor-sql-param.ndjson
grep -q '"values":\[42\]' /tmp/harbor-sql-param.ndjson || fail "implicit param failed"
pass "implicit params work"

curl -sfN -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"SELECT $1 AS d, $2 AS maybe_null","params":[{"value":"123.4567","type":"DECIMAL(18,4)"},{"value":null,"type":"INTEGER"}]}' \
    > /tmp/harbor-sql-typed-param.ndjson
grep -q '"duckdbType":"DECIMAL(18,4)"' /tmp/harbor-sql-typed-param.ndjson || fail "typed decimal schema missing"
grep -q '"values":\["123.4567",null\]' /tmp/harbor-sql-typed-param.ndjson || fail "typed param row mismatch"
pass "typed-wrapper params work"

# ---- Representative type encodings ----
curl -sfN -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"SELECT 9223372036854775807::BIGINT AS big, 12345.6789::DECIMAL(18,4) AS dec_val, INTERVAL 1 YEAR + INTERVAL 2 DAYS + INTERVAL 3 SECONDS AS iv, '\''hello world'\''::BLOB AS b, '\''{\"a\":1}'\''::JSON AS j"}' \
    > /tmp/harbor-sql-types.ndjson
grep -q '"9223372036854775807"' /tmp/harbor-sql-types.ndjson || fail "BIGINT max should be string"
grep -q '"12345.6789"' /tmp/harbor-sql-types.ndjson || fail "DECIMAL should be string"
grep -q '"months":12,"days":2,"micros":"3000000"' /tmp/harbor-sql-types.ndjson || fail "INTERVAL object mismatch"
grep -q '"aGVsbG8gd29ybGQ="' /tmp/harbor-sql-types.ndjson || fail "BLOB base64 mismatch"
grep -q '"{\\"a\\":1}"' /tmp/harbor-sql-types.ndjson || fail "JSON column should be JSON-text string"
pass "representative type encodings"

# ---- Sessions + transaction state ----
SESS="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql/sessions/new" -H "Authorization: Bearer ${TOKEN}" -d '')"
SID="$(echo "${SESS}" | sed -E 's/.*"sessionId":"([^"]+)".*/\1/')"
[[ -n "${SID}" ]] || fail "session create returned no sessionId"

curl -sf -X POST "http://127.0.0.1:${PORT}/sql" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' -d "{\"sql\":\"BEGIN\",\"sessionId\":\"${SID}\"}" >/dev/null
curl -sf -X POST "http://127.0.0.1:${PORT}/sql" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' -d "{\"sql\":\"CREATE TEMP TABLE t (i INT)\",\"sessionId\":\"${SID}\"}" >/dev/null
curl -sf -X POST "http://127.0.0.1:${PORT}/sql" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' -d "{\"sql\":\"INSERT INTO t VALUES (1),(2),(3)\",\"sessionId\":\"${SID}\"}" >/dev/null
curl -sfN -X POST "http://127.0.0.1:${PORT}/sql" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' -d "{\"sql\":\"SELECT count(*) AS n FROM t\",\"sessionId\":\"${SID}\"}" \
    > /tmp/harbor-sql-session.ndjson
grep -q '"values":\[3\]' /tmp/harbor-sql-session.ndjson || fail "session transaction state not visible"
pass "session transaction state survives requests"

# ---- DML ... RETURNING must use select-shaped response ----
# Vanilla INSERT/UPDATE/DELETE return DuckDB's single-column "Count"
# pseudo-result and should be write-shaped. The same statements with
# RETURNING produce real columns and must be surfaced as select-shaped
# results with columns/data intact.
curl -sf -X POST "http://127.0.0.1:${PORT}/sql" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d "{\"sql\":\"CREATE TEMP TABLE returning_t (id INTEGER, name VARCHAR)\",\"sessionId\":\"${SID}\"}" >/dev/null

INSERT_RETURNING="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d "{\"sql\":\"INSERT INTO returning_t VALUES (42, 'r') RETURNING *\",\"sessionId\":\"${SID}\"}")"
echo "${INSERT_RETURNING}" | jq -e \
    '.ok == true and .kind == "select" and (.columns | length) == 2 and .columns[0].name == "id" and .columns[1].name == "name" and .data == [[42,"r"]] and .rowCount == 1' \
    >/dev/null || fail "INSERT ... RETURNING should be select-shaped: ${INSERT_RETURNING}"
pass "INSERT ... RETURNING surfaces columns/data"

UPDATE_RETURNING="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d "{\"sql\":\"UPDATE returning_t SET name = 'rr' WHERE id = 42 RETURNING id, name\",\"sessionId\":\"${SID}\"}")"
echo "${UPDATE_RETURNING}" | jq -e \
    '.ok == true and .kind == "select" and (.columns | length) == 2 and .data == [[42,"rr"]] and .rowCount == 1' \
    >/dev/null || fail "UPDATE ... RETURNING should be select-shaped: ${UPDATE_RETURNING}"
pass "UPDATE ... RETURNING surfaces columns/data"

DELETE_RETURNING="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d "{\"sql\":\"DELETE FROM returning_t WHERE id = 42 RETURNING id, name\",\"sessionId\":\"${SID}\"}")"
echo "${DELETE_RETURNING}" | jq -e \
    '.ok == true and .kind == "select" and (.columns | length) == 2 and .data == [[42,"rr"]] and .rowCount == 1' \
    >/dev/null || fail "DELETE ... RETURNING should be select-shaped: ${DELETE_RETURNING}"
pass "DELETE ... RETURNING surfaces columns/data"

DEL="$(curl -s -i -X DELETE "http://127.0.0.1:${PORT}/sql/sessions/${SID}" -H "Authorization: Bearer ${TOKEN}")"
echo "${DEL}" | grep -qi '^HTTP/1.1 200' || fail "session DELETE expected 200"
echo "${DEL}" | grep -q '"ok":true' || fail "session DELETE missing ok"
PREFLIGHT_DEL="$(curl -s -i -X OPTIONS "http://127.0.0.1:${PORT}/sql/sessions/${SID}" \
    -H 'Origin: https://app.example.com' \
    -H 'Access-Control-Request-Method: DELETE' \
    -H 'Access-Control-Request-Headers: Authorization')"
echo "${PREFLIGHT_DEL}" | grep -qi '^HTTP/1.1 204' \
    || fail "OPTIONS /sql/sessions/<id> expected 204"
echo "${PREFLIGHT_DEL}" | grep -qi '^Access-Control-Allow-Methods: .*DELETE' \
    || fail "OPTIONS /sql/sessions/<id> must allow DELETE"
pass "OPTIONS /sql/sessions/<id> CORS preflight"
code="$(curl -s -o /tmp/harbor-sql-session-gone.json -w '%{http_code}' \
    -X POST "http://127.0.0.1:${PORT}/sql" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' -d "{\"sql\":\"SELECT 1\",\"sessionId\":\"${SID}\"}")"
[[ "${code}" == "404" ]] || fail "using deleted session expected 404, got ${code}"
pass "session delete + not-found behavior"

# ---- logout?destroy_sessions=true ----
SESS2="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql/sessions/new" -H "Authorization: Bearer ${TOKEN}" -d '')"
SID2="$(echo "${SESS2}" | sed -E 's/.*"sessionId":"([^"]+)".*/\1/')"
[[ -n "${SID2}" ]] || fail "second session create returned no sessionId"
curl -sf -X POST "http://127.0.0.1:${PORT}/auth/logout?destroy_sessions=true" \
    -b "harbor_session=${COOKIE}" -d '' >/tmp/harbor-sql-logout.json
code="$(curl -s -o /tmp/harbor-sql-session-destroyed.json -w '%{http_code}' \
    -X POST "http://127.0.0.1:${PORT}/sql" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' -d "{\"sql\":\"SELECT 1\",\"sessionId\":\"${SID2}\"}")"
[[ "${code}" == "404" ]] || fail "logout destroyed session expected 404, got ${code}"
pass "logout destroy_sessions removes owned SQL sessions"

echo
# ============================================================================
# PR-7d — full nested-type Mode B param parser (LIST/ARRAY/STRUCT/MAP)
# ============================================================================

# LIST<INTEGER> with nulls
RESP="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"LIST<INTEGER>","value":[1,2,null]}]}')"
echo "${RESP}" | grep -q '"data":\[\[\[1,2,null\]\]\]' || fail "PR-7d LIST<INTEGER>: ${RESP}"
pass "PR-7d Mode B LIST<INTEGER> with null element"

# Nested LIST<LIST<VARCHAR>>
RESP="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"LIST<LIST<VARCHAR>>","value":[["a"],["b","c"]]}]}')"
echo "${RESP}" | grep -q '\[\["a"\],\["b","c"\]\]' || fail "PR-7d nested LIST: ${RESP}"
pass "PR-7d Mode B nested LIST<LIST<VARCHAR>>"

# ARRAY<INTEGER, 3>
RESP="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"ARRAY<INTEGER, 3>","value":[10,20,30]}]}')"
echo "${RESP}" | grep -q '\[10,20,30\]' || fail "PR-7d ARRAY<INTEGER,3>: ${RESP}"
pass "PR-7d Mode B ARRAY<INTEGER, 3>"

# ARRAY length mismatch → BAD_REQUEST
code="$(curl -s -o /tmp/harbor-sql-arr-bad.json -w '%{http_code}' \
    -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"ARRAY<INTEGER, 3>","value":[1,2]}]}')"
[[ "${code}" == "400" ]] || fail "PR-7d ARRAY length mismatch expected 400 (got ${code})"
grep -q '"errorCode":"BAD_REQUEST"' /tmp/harbor-sql-arr-bad.json \
    || fail "PR-7d ARRAY length mismatch missing BAD_REQUEST errorCode"
pass "PR-7d ARRAY length mismatch → 400 BAD_REQUEST"

# STRUCT(a INTEGER, b VARCHAR, c LIST<DOUBLE>) with nested LIST
RESP="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"STRUCT(a INTEGER, b VARCHAR, c LIST<DOUBLE>)","value":{"a":1,"b":"hi","c":[1.5,2.5]}}]}')"
echo "${RESP}" | grep -q '"a":1' || fail "PR-7d STRUCT field a: ${RESP}"
echo "${RESP}" | grep -q '"b":"hi"' || fail "PR-7d STRUCT field b"
echo "${RESP}" | grep -q '\[1.5,2.5\]' || fail "PR-7d STRUCT field c LIST<DOUBLE>"
pass "PR-7d Mode B STRUCT with nested LIST<DOUBLE> field"

# Case-insensitive STRUCT field lookup
RESP="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"STRUCT(name VARCHAR, age INTEGER)","value":{"NAME":"alice","AGE":30}}]}')"
echo "${RESP}" | grep -q '"name":"alice"' || fail "PR-7d STRUCT case-insensitive lookup: ${RESP}"
pass "PR-7d STRUCT case-insensitive field lookup"

# Duplicate STRUCT field (case-insensitive collision) → BAD_REQUEST
code="$(curl -s -o /tmp/harbor-sql-struct-dup.json -w '%{http_code}' \
    -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"STRUCT(a INTEGER)","value":{"A":1,"a":2}}]}')"
[[ "${code}" == "400" ]] || fail "PR-7d STRUCT duplicate-key expected 400 (got ${code})"
grep -q 'duplicate field' /tmp/harbor-sql-struct-dup.json \
    || fail "PR-7d STRUCT duplicate-key error message missing 'duplicate field'"
pass "PR-7d STRUCT duplicate field (case-insensitive collision) → 400"

# Extra STRUCT field → BAD_REQUEST
code="$(curl -s -o /tmp/harbor-sql-struct-extra.json -w '%{http_code}' \
    -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"STRUCT(a INTEGER)","value":{"a":1,"unexpected":2}}]}')"
[[ "${code}" == "400" ]] || fail "PR-7d STRUCT extra-key expected 400 (got ${code})"
grep -q 'unexpected' /tmp/harbor-sql-struct-extra.json \
    || fail "PR-7d STRUCT extra-key error must mention the bad field name"
pass "PR-7d STRUCT extra field → 400"

# Missing STRUCT field → NULL (the test value omits 'b')
RESP="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"STRUCT(a INTEGER, b VARCHAR)","value":{"a":7}}]}')"
echo "${RESP}" | grep -q '"a":7' || fail "PR-7d STRUCT missing-field decoded: ${RESP}"
echo "${RESP}" | grep -q '"b":null' || fail "PR-7d STRUCT missing field 'b' should be null"
pass "PR-7d STRUCT missing field → NULL (not BAD_REQUEST)"

# MAP<VARCHAR, INTEGER> as array-of-pairs
RESP="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"MAP<VARCHAR, INTEGER>","value":[["alpha",1],["beta",2]]}]}')"
echo "${RESP}" | grep -q '\["alpha",1\]' || fail "PR-7d MAP entry alpha: ${RESP}"
echo "${RESP}" | grep -q '\["beta",2\]' || fail "PR-7d MAP entry beta"
pass "PR-7d Mode B MAP<VARCHAR, INTEGER> as array-of-pairs"

# Nested DECIMAL inside STRUCT (round-25 catch: comma in DECIMAL(10,2)
# must NOT split the STRUCT field list).
RESP="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"STRUCT(a DECIMAL(10,2), b INTEGER)","value":{"a":"123.45","b":99}}]}')"
echo "${RESP}" | grep -q '"a":"123.45"' || fail "PR-7d STRUCT(DECIMAL): ${RESP}"
echo "${RESP}" | grep -q '"b":99' || fail "PR-7d STRUCT(DECIMAL) sibling field b"
pass "PR-7d STRUCT(a DECIMAL(10,2), b INTEGER) — comma inside DECIMAL not split"

# UNION explicitly unsupported → BAD_REQUEST with helpful message
code="$(curl -s -o /tmp/harbor-sql-union.json -w '%{http_code}' \
    -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"UNION(a INTEGER, b VARCHAR)","value":1}]}')"
[[ "${code}" == "400" ]] || fail "PR-7d UNION expected 400 (got ${code})"
grep -q 'UNION.*not supported' /tmp/harbor-sql-union.json \
    || fail "PR-7d UNION error must say 'not supported'"
pass "PR-7d UNION → 400 with explicit not-supported message"

# Whitespace-tolerant type strings (round-25)
RESP="$(curl -sf -X POST "http://127.0.0.1:${PORT}/sql" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d '{"sql":"SELECT $1 AS x","params":[{"type":"LIST < INTEGER >","value":[1,2,3]}]}')"
echo "${RESP}" | grep -q '\[1,2,3\]' || fail "PR-7d whitespace-tolerant type: ${RESP}"
pass "PR-7d whitespace-tolerant type strings (LIST < INTEGER >)"

echo
# ============================================================================
# Unauthenticated mode (token := NULL) — sessions + transactions work,
# owned by the synthetic harbor.local-dev principal.
# ============================================================================

PORT3="$((PORT + 2))"
SERVER3_LOG="$(mktemp)"
nohup "${DUCKDB_BIN}" -unsigned -no-stdin -c "
LOAD '${EXT_PATH}';
CALL harbor_serve(bind := '127.0.0.1', port := ${PORT3}, token := NULL);
CALL harbor_wait();
" > "${SERVER3_LOG}" 2>&1 &
SERVER3_PID=$!
sleep 2
curl -sf -o /dev/null "http://127.0.0.1:${PORT3}/info" || {
    echo "--- unauth server log ---" >&2; cat "${SERVER3_LOG}" >&2
    fail "unauth server did not come up"
}

USESS="$(curl -sf -X POST "http://127.0.0.1:${PORT3}/sql/sessions/new" -d '')"
USID="$(echo "${USESS}" | sed -E 's/.*"sessionId":"([^"]+)".*/\1/')"
[[ -n "${USID}" && "${USID}" != "${USESS}" ]] || fail "unauth session create failed: ${USESS}"
pass "unauth mode: POST /sql/sessions/new with no credentials"

curl -sf -X POST "http://127.0.0.1:${PORT3}/sql" \
    -H 'Content-Type: application/json' -d "{\"sql\":\"BEGIN\",\"sessionId\":\"${USID}\"}" >/dev/null
curl -sf -X POST "http://127.0.0.1:${PORT3}/sql" \
    -H 'Content-Type: application/json' -d "{\"sql\":\"CREATE TEMP TABLE ut (i INT)\",\"sessionId\":\"${USID}\"}" >/dev/null
curl -sf -X POST "http://127.0.0.1:${PORT3}/sql" \
    -H 'Content-Type: application/json' -d "{\"sql\":\"INSERT INTO ut VALUES (7)\",\"sessionId\":\"${USID}\"}" >/dev/null
curl -sf -X POST "http://127.0.0.1:${PORT3}/sql" \
    -H 'Content-Type: application/json' -d "{\"sql\":\"COMMIT\",\"sessionId\":\"${USID}\"}" >/dev/null
UROW="$(curl -sfN -X POST "http://127.0.0.1:${PORT3}/sql" \
    -H 'Content-Type: application/json' -d "{\"sql\":\"SELECT i FROM ut\",\"sessionId\":\"${USID}\"}")"
echo "${UROW}" | grep -q '"values":\[7\]' || fail "unauth transaction state lost: ${UROW}"
pass "unauth mode: BEGIN/COMMIT transaction on the session"

UDEL="$(curl -s -i -X DELETE "http://127.0.0.1:${PORT3}/sql/sessions/${USID}")"
echo "${UDEL}" | grep -qi '^HTTP/1.1 200' || fail "unauth session DELETE expected 200"
pass "unauth mode: DELETE /sql/sessions/:id"

kill "${SERVER3_PID}" 2>/dev/null || true
wait "${SERVER3_PID}" 2>/dev/null || true
rm -f "${SERVER3_LOG}"

green "All /sql golden assertions passed."
