#!/usr/bin/env bash
#
# stress.sh — drive harbor over HTTP until something breaks.
#
#   scripts/stress.sh                       # uses ../sample.duckdb
#   scripts/stress.sh path/to/data.duckdb
#   SOAK=30 scripts/stress.sh               # add a 30s sustained-load phase
#
# The suite talks to the server the way a client does: curl in, NDJSON out.
# It knows nothing about Rust, so it is equally valid against the C++ harbor —
# which is the point. It is the acceptance harness for the rewrite, not a set
# of unit tests that only prove this implementation agrees with itself.
#
# Correctness claims are checked against an oracle rather than against
# harbor's own earlier answers: every expected value is computed by the DuckDB
# CLI *before* the server starts, because DuckDB's file lock is exclusive and
# there is no second reader once harbor holds it. A test that only compares
# harbor to harbor cannot catch a systematic encoding bug.
#
# The database is copied to a scratch directory first, so write tests are free
# to write and the original is never touched.

set -uo pipefail

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
src_db=${1:-$here/../sample.duckdb}
port=${PORT:-9499}
token=${TOKEN:-stress-$$}
soak=${SOAK:-0}
base="http://127.0.0.1:$port"
timeout=${TIMEOUT:-120}

if [[ ! -f "$src_db" ]]; then
  echo "stress: no database at $src_db" >&2
  exit 2
fi
if [[ ! -x "$here/bin/harbor" ]]; then
  echo "stress: $here/bin/harbor is missing or not executable" >&2
  exit 2
fi
if [[ ! -f "$here/build/release/harbor.duckdb_extension" ]]; then
  echo "stress: extension not built — run 'make release'" >&2
  exit 2
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/harbor-stress.XXXXXX")
db="$work/stress.duckdb"
log="$work/server.log"
cp "$src_db" "$db"

pass=0
fail=0
declare -a failures=()

cleanup() {
  if [[ -n "${server_pid:-}" ]] && kill -0 "$server_pid" 2>/dev/null; then
    kill -TERM "$server_pid" 2>/dev/null
    for _ in $(seq 1 60); do kill -0 "$server_pid" 2>/dev/null || break; sleep 0.25; done
  fi
  [[ "${KEEP:-0}" == 1 ]] || rm -rf "$work"
}
trap cleanup EXIT

bold=$(tput bold 2>/dev/null || true)
red=$(tput setaf 1 2>/dev/null || true)
green=$(tput setaf 2 2>/dev/null || true)
dim=$(tput dim 2>/dev/null || true)
off=$(tput sgr0 2>/dev/null || true)

section() { printf '\n%s%s%s\n' "$bold" "$1" "$off"; }
ok()      { pass=$((pass + 1)); printf '  %s✓%s %s\n' "$green" "$off" "$1"; }
bad()     { fail=$((fail + 1)); failures+=("$1"); printf '  %s✗%s %s\n     %s%s%s\n' "$red" "$off" "$1" "$dim" "$2" "$off"; }

# eq <name> <expected> <actual>
eq() {
  if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1" "expected [$2], got [$3]"; fi
}

# ---------------------------------------------------------------------------
# HTTP helpers
# ---------------------------------------------------------------------------

# post <sql> [params-json] — send one statement, print the NDJSON body
post() {
  local body
  body=$(SQL="$1" PARAMS="${2-}" python3 -c '
import json, os, sys
d = {"sql": os.environ["SQL"]}
p = os.environ.get("PARAMS") or ""
if p:
    d["params"] = json.loads(p)
sys.stdout.write(json.dumps(d))')
  curl -sS -m "$timeout" -H "Authorization: Bearer $token" \
       -H 'Content-Type: application/json' --data-binary "$body" "$base/sql"
}

# status <sql> [params-json] — send one statement, print only the HTTP status
status() {
  local body
  body=$(SQL="$1" PARAMS="${2-}" python3 -c '
import json, os, sys
d = {"sql": os.environ["SQL"]}
p = os.environ.get("PARAMS") or ""
if p:
    d["params"] = json.loads(p)
sys.stdout.write(json.dumps(d))')
  curl -sS -m "$timeout" -o /dev/null -w '%{http_code}\n' \
       -H "Authorization: Bearer $token" --data-binary "$body" "$base/sql"
}

# raw <body> — send an arbitrary body, print "<status> <body>"
raw() {
  curl -sS -m "$timeout" -w '\n%{http_code}' -H "Authorization: Bearer $token" \
       --data-binary "$1" "$base/sql"
}

# nd <expr> — evaluate a Python expression over an NDJSON envelope on stdin.
# Bound names: lines, schema, rows, end, err, cols.
nd() {
  python3 -c '
import sys, json
lines = []
for line in sys.stdin:
    line = line.strip()
    if line:
        try:
            lines.append(json.loads(line))
        except json.JSONDecodeError:
            print("MALFORMED-JSON: " + line[:120]); sys.exit(0)
schema = next((l for l in lines if l.get("type") == "schema"), None)
rows   = [l["values"] for l in lines if l.get("type") == "row"]
end    = next((l for l in lines if l.get("type") == "end"), None)
err    = next((l for l in lines if l.get("type") == "error"), None)
cols   = (schema or {}).get("columns", [])
v0     = rows[0][0] if rows else None
typed  = "%s:%s" % (type(v0).__name__, v0)
try:
    print(eval(sys.argv[1]))
except Exception as e:
    print("EVAL-ERROR: %s" % e)
' "$1"
}

# scalar <sql> — the first value of the first row, as text
scalar() { post "$1" | nd 'rows[0][0] if rows else "NO-ROWS"'; }

# oracle <sql> — the same query answered by the DuckDB CLI, out of band.
# -csv quotes any field containing a comma, so undo that: the value harbor
# returns is the bare string, not its CSV rendering.
oracle() {
  duckdb -readonly -csv -noheader "$db" -c "$1" 2>/dev/null | tail -1 \
    | python3 -c 'import sys; v = sys.stdin.read().rstrip("\n"); print(v[1:-1].replace("\"\"", "\"") if len(v) > 1 and v[0] == v[-1] == "\"" else v)'
}

# ---------------------------------------------------------------------------
# Ground truth, computed before the server takes the lock
# ---------------------------------------------------------------------------

section "Oracle (DuckDB CLI, before harbor holds the file lock)"

# bash 3.2 ships on macOS and has no associative arrays, so these are plain
# variables. Each is the answer harbor will be held to.
exp_n_sites=$(oracle   "SELECT count(*) FROM sites")
exp_n_cohorts=$(oracle "SELECT count(*) FROM cohorts")
exp_n_plans=$(oracle   "SELECT count(*) FROM plans")
exp_n_rules=$(oracle   "SELECT count(*) FROM rules")
exp_clients=$(oracle   "SELECT count(DISTINCT client) FROM sites")
exp_payers=$(oracle    "SELECT count(DISTINCT payer) FROM plans")
exp_join4=$(oracle     "SELECT count(*) FROM sites s JOIN rules r USING (client) JOIN plans p USING (client) JOIN cohorts c USING (client)")
exp_active=$(oracle    "SELECT count(*) FROM rules WHERE status = 'Active'")
exp_top_client=$(oracle "SELECT client FROM plans GROUP BY 1 ORDER BY count(*) DESC, client LIMIT 1")
exp_digest=$(oracle    "SELECT md5(string_agg(account, '|' ORDER BY account)) FROM sites")
exp_states=$(oracle    "SELECT string_agg(DISTINCT state, ',' ORDER BY state) FROM sites WHERE state IS NOT NULL")
exp_cross=$(oracle     "SELECT count(*) FROM plans a CROSS JOIN rules b")

printf '  %ssites=%s cohorts=%s plans=%s rules=%s clients=%s cross=%s%s\n' \
  "$dim" "$exp_n_sites" "$exp_n_cohorts" "$exp_n_plans" "$exp_n_rules" \
  "$exp_clients" "$exp_cross" "$off"

if [[ -z "$exp_n_sites" ]]; then
  echo "stress: oracle produced nothing — is $db a valid DuckDB file?" >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# Start the server
# ---------------------------------------------------------------------------

section "Startup"

"$here/bin/harbor" "$db" --port "$port" --token "$token" --workers 6 >"$log" 2>&1 &
server_pid=$!

up=0
for _ in $(seq 1 80); do
  if curl -sS -m 1 "$base/health" >/dev/null 2>&1; then up=1; break; fi
  kill -0 "$server_pid" 2>/dev/null || break
  sleep 0.25
done
if [[ $up != 1 ]]; then
  echo "stress: server never came up. log:" >&2
  cat "$log" >&2
  exit 1
fi
ok "server listening on $base (pid $server_pid)"

# ---------------------------------------------------------------------------
section "Liveness and authentication"
# ---------------------------------------------------------------------------

eq "/health answers without a token" "200" \
   "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "$base/health")"
eq "/health body" '{"status":"ok"}' "$(curl -sS -m 5 "$base/health")"
eq "/sql refuses a missing token" "401" \
   "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' --data '{"sql":"SELECT 1"}' "$base/sql")"
eq "/sql refuses a wrong token" "401" \
   "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer wrong' \
        --data '{"sql":"SELECT 1"}' "$base/sql")"
eq "/sql refuses a malformed Authorization header" "401" \
   "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' -H "Authorization: $token" \
        --data '{"sql":"SELECT 1"}' "$base/sql")"
eq "/sql accepts the right token" "200" "$(status 'SELECT 1')"
eq "unknown path is 404" "404" \
   "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "$base/nope")"
eq "GET /sql is 404" "404" \
   "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "$base/sql")"

# ---------------------------------------------------------------------------
section "Envelope shape"
# ---------------------------------------------------------------------------

eq "schema first, end last" "schema|end" \
   "$(post 'SELECT 1 AS a' | nd '"%s|%s" % (lines[0]["type"], lines[-1]["type"])')"
eq "rowCount agrees with the rows sent" "True" \
   "$(post 'SELECT * FROM sites' | nd 'end["rowCount"] == len(rows)')"
eq "rowCount matches the oracle" "$exp_n_sites" \
   "$(post 'SELECT * FROM sites' | nd 'end["rowCount"]')"
eq "empty result still frames" "schema|0|end" \
   "$(post 'SELECT * FROM sites WHERE 1=0' | nd '"%s|%s|%s" % (lines[0]["type"], end["rowCount"], lines[-1]["type"])')"
eq "every column carries a duckdbType" "True" \
   "$(post 'SELECT * FROM plans LIMIT 1' | nd 'all("duckdbType" in c and "name" in c for c in cols)')"
eq "timeMs is present and numeric" "True" \
   "$(post 'SELECT 1' | nd 'isinstance(end.get("timeMs"), int)')"
eq "row width matches column count" "True" \
   "$(post 'SELECT * FROM sites LIMIT 5' | nd 'all(len(r) == len(cols) for r in rows)')"
eq "response is chunked, not buffered" "chunked" \
   "$(curl -sS -m 30 -D - -o /dev/null -H "Authorization: Bearer $token" \
        --data '{"sql":"SELECT * FROM plans"}' "$base/sql" \
      | tr -d '\r' | awk -F': ' 'tolower($1)=="transfer-encoding"{print $2}')"

# ---------------------------------------------------------------------------
section "Type fidelity"
# ---------------------------------------------------------------------------

eq "DECIMAL carries width and scale" "10|2" \
   "$(post "SELECT 19.99::DECIMAL(10,2) AS d" | nd '"%s|%s" % (cols[0]["decimal"]["width"], cols[0]["decimal"]["scale"])')"
eq "DECIMAL value keeps trailing zeros" "str:4.50" \
   "$(post "SELECT 4.5::DECIMAL(10,2) AS d" | nd 'typed')"
eq "large BIGINT is quoted, not truncated" "str:9007199254740993" \
   "$(post 'SELECT 9007199254740993::BIGINT AS n' | nd 'typed')"
eq "small BIGINT stays a number" "int:42" \
   "$(post 'SELECT 42::BIGINT AS n' | nd 'typed')"
eq "HUGEINT survives" "str:123456789012345678901234567890" \
   "$(post 'SELECT 123456789012345678901234567890::HUGEINT AS n' | nd 'typed')"
eq "UUID is canonical" "str:6ba7b810-9dad-11d1-80b4-00c04fd430c8" \
   "$(post "SELECT '6ba7b810-9dad-11d1-80b4-00c04fd430c8'::UUID AS u" | nd 'typed')"
eq "BLOB is base64" "str:aGk=" \
   "$(post "SELECT 'hi'::BLOB AS b" | nd 'typed')"
eq "TIMESTAMP keeps microseconds" "str:2026-08-11T07:24:05.123456" \
   "$(post "SELECT '2026-08-11 07:24:05.123456'::TIMESTAMP AS t" | nd 'typed')"
eq "DATE is ISO" "str:2026-08-11" \
   "$(post "SELECT '2026-08-11'::DATE AS d" | nd 'typed')"
eq "NULL is JSON null" "NoneType:None" \
   "$(post 'SELECT NULL::VARCHAR AS v' | nd 'typed')"
eq "non-finite DOUBLE degrades to null" "NoneType:None" \
   "$(post "SELECT 'inf'::DOUBLE AS f" | nd 'typed')"
eq "LIST nests its child type" "VARCHAR" \
   "$(post "SELECT ['a','b'] AS l" | nd 'cols[0]["child"]["duckdbType"]')"
eq "LIST value round-trips" "True" \
   "$(post "SELECT ['a','b'] AS l" | nd "rows[0][0] == ['a', 'b']")"
# The SQL goes through a variable rather than being written inline. bash 3.2 —
# which is what macOS ships — brace-expands `{'a': 1, 'b': 'x'}` even inside
# the double quotes of a "$( ... )" used as a command argument, splitting it
# into two words and running the whole pipeline twice. Assignment is not a
# brace-expansion context, so this sidesteps it.
struct_sql="SELECT {'a': 1, 'b': 'x'} AS s"
eq "STRUCT names its fields" "a|b" \
   "$(post "$struct_sql" | nd '"|".join(f["name"] for f in cols[0]["fields"])')"
# Same brace-expansion hazard, so the expected value is spelled with brackets
# and tuples rather than a dict literal.
eq "STRUCT value round-trips" "True" \
   "$(post "$struct_sql" | nd "sorted(rows[0][0].items()) == [('a', 1), ('b', 'x')]")"
eq "unicode survives the encoder" "str:héllo — 世界 🦆" \
   "$(post "SELECT 'héllo — 世界 🦆' AS s" | nd 'typed')"
eq "control characters are escaped" "True" \
   "$(post "SELECT 'a' || chr(9) || 'b' || chr(10) || 'c' AS s" | nd "rows[0][0] == 'a\\tb\\nc'")"

# ---------------------------------------------------------------------------
section "Real queries against the loaded data"
# ---------------------------------------------------------------------------

eq "count(sites)"           "$exp_n_sites"   "$(scalar 'SELECT count(*) FROM sites')"
eq "count(cohorts)"         "$exp_n_cohorts" "$(scalar 'SELECT count(*) FROM cohorts')"
eq "count(plans)"           "$exp_n_plans"   "$(scalar 'SELECT count(*) FROM plans')"
eq "count(rules)"           "$exp_n_rules"   "$(scalar 'SELECT count(*) FROM rules')"
eq "distinct clients"       "$exp_clients"   "$(scalar 'SELECT count(DISTINCT client) FROM sites')"
eq "distinct payers"        "$exp_payers"    "$(scalar 'SELECT count(DISTINCT payer) FROM plans')"
eq "four-way join"          "$exp_join4" \
   "$(scalar 'SELECT count(*) FROM sites s JOIN rules r USING (client) JOIN plans p USING (client) JOIN cohorts c USING (client)')"
eq "filtered count"         "$exp_active" \
   "$(scalar "SELECT count(*) FROM rules WHERE status = 'Active'")"
eq "GROUP BY + ORDER BY"    "$exp_top_client" \
   "$(scalar 'SELECT client FROM plans GROUP BY 1 ORDER BY count(*) DESC, client LIMIT 1')"
eq "md5 over ordered aggregate" "$exp_digest" \
   "$(scalar "SELECT md5(string_agg(account, '|' ORDER BY account)) FROM sites")"
eq "string_agg DISTINCT"    "$exp_states" \
   "$(scalar "SELECT string_agg(DISTINCT state, ',' ORDER BY state) FROM sites WHERE state IS NOT NULL")"
eq "CTE" "$exp_clients" \
   "$(scalar 'WITH c AS (SELECT DISTINCT client FROM sites) SELECT count(*) FROM c')"
eq "window function" "1" \
   "$(scalar 'SELECT rn FROM (SELECT row_number() OVER (ORDER BY account) AS rn FROM sites) WHERE rn = 1')"
eq "HAVING" "True" \
   "$(post 'SELECT client FROM plans GROUP BY 1 HAVING count(*) > 5' | nd 'len(rows) > 0')"
eq "UNNEST of a list" "3" \
   "$(scalar 'SELECT count(*) FROM (SELECT unnest([1,2,3]) AS x)')"
eq "regexp filter runs" "True" \
   "$(post "SELECT count(*) AS n FROM sites WHERE regexp_matches(coalesce(name,''), '[A-Za-z]')" | nd 'int(rows[0][0]) >= 0')"
eq "LEFT JOIN preserves the left side" "$exp_n_plans" \
   "$(scalar 'SELECT count(*) FROM plans p LEFT JOIN rules r USING (client)')"
eq "UNION ALL" "$(( $exp_n_sites + $exp_n_rules ))" \
   "$(scalar 'SELECT count(*) FROM (SELECT client FROM sites UNION ALL SELECT client FROM rules)')"
eq "correlated subquery" "True" \
   "$(post 'SELECT count(*) AS n FROM sites s WHERE EXISTS (SELECT 1 FROM rules r WHERE r.client = s.client)' | nd 'int(rows[0][0]) > 0')"
eq "PIVOT-shaped conditional aggregate" "True" \
   "$(post "SELECT count(*) FILTER (WHERE status = 'Active') AS a, count(*) AS n FROM rules" | nd 'int(rows[0][0]) <= int(rows[0][1])')"

# ---------------------------------------------------------------------------
section "Parameter binding"
# ---------------------------------------------------------------------------

eq "integer param" "42" "$(post 'SELECT ? + 1 AS n' '[41]' | nd 'rows[0][0]')"
eq "string param" "hello" "$(post 'SELECT ? AS s' '["hello"]' | nd 'rows[0][0]')"
eq "null param" "None" "$(post 'SELECT ?::VARCHAR AS s' '[null]' | nd 'repr(rows[0][0])')"
eq "float param" "2.5" "$(post 'SELECT ? AS f' '[2.5]' | nd 'rows[0][0]')"
eq "boolean param" "True" "$(post 'SELECT ? AS b' '[true]' | nd 'rows[0][0]')"
eq "two params" "ab" "$(post 'SELECT ? || ? AS s' '["a","b"]' | nd 'rows[0][0]')"
eq "param filters real data" "True" \
   "$(post 'SELECT count(*) AS n FROM plans WHERE client = ?' "[\"$exp_top_client\"]" | nd 'int(rows[0][0]) > 0')"
eq "a param that looks like SQL is data, not SQL" "0" \
   "$(post 'SELECT count(*) AS n FROM sites WHERE client = ?' "[\"' OR 1=1 --\"]" | nd 'rows[0][0]')"
eq "params must be an array" "400" \
   "$(raw '{"sql":"SELECT 1","params":{"a":1}}' | tail -1)"

# ---------------------------------------------------------------------------
section "One statement per request"
# ---------------------------------------------------------------------------

eq "a second statement is rejected" "400" "$(status 'SELECT 1; DROP TABLE sites')"
eq "and the table is still there" "$exp_n_sites" "$(scalar 'SELECT count(*) FROM sites')"
eq "no whitespace before it either" "400" "$(status 'SELECT 1;DROP TABLE sites')"
eq "a trailing comment counts as a second statement" "400" "$(status 'SELECT 1; -- sneaky')"
eq "an empty second statement is still a second statement" "400" "$(status 'SELECT 1;;')"
eq "a semicolon inside a literal is data" ";drop" "$(post "SELECT ';drop' AS s" | nd 'rows[0][0]')"
eq "a doubled quote does not end the literal" "it's; fine" \
   "$(post "SELECT 'it''s; fine' AS s" | nd 'rows[0][0]')"
eq "a dollar-quoted body is data" "a; b" "$(post 'SELECT $tag$a; b$tag$ AS s' | nd 'rows[0][0]')"
eq "a semicolon in a quoted identifier is data" "200" "$(status 'SELECT 1 AS "a;b"')"
eq "a semicolon in a block comment is data" "200" "$(status '/* a; b */ SELECT 1')"
eq "nested block comments are handled" "200" "$(status '/* a /* b; c */ d */ SELECT 1')"
eq "a trailing semicolon is fine" "200" "$(status 'SELECT 1;')"
eq "trailing whitespace after the semicolon is fine" "200" "$(status 'SELECT 1;   ')"

# ---------------------------------------------------------------------------
section "Error handling"
# ---------------------------------------------------------------------------

eq "malformed JSON is 400"        "400" "$(raw 'not json at all' | tail -1)"
eq "missing sql key is 400"       "400" "$(raw '{"params":[1]}' | tail -1)"
eq "empty sql is 400"             "400" "$(raw '{"sql":"   "}' | tail -1)"
eq "sql of the wrong type is 400" "400" "$(raw '{"sql":123}' | tail -1)"
eq "unknown table is 400"         "400" "$(status 'SELECT * FROM does_not_exist')"
eq "syntax error is 400"          "400" "$(status 'SELECT FROM WHERE')"
eq "wrong param count is 400"     "400" "$(status 'SELECT ? + ?' '[1]')"
eq "an error body is an error envelope" "error|sql_error" \
   "$(post 'SELECT * FROM does_not_exist' | nd '"%s|%s" % (err["type"], err["code"])')"
eq "the error message survives" "True" \
   "$(post 'SELECT * FROM does_not_exist' | nd '"does_not_exist" in err["message"]')"
eq "the server is still healthy after all that" "200" \
   "$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "$base/health")"

# ---------------------------------------------------------------------------
section "Volume and streaming"
# ---------------------------------------------------------------------------

eq "cross join row count" "$exp_cross" \
   "$(scalar 'SELECT count(*) FROM plans a CROSS JOIN rules b')"

big_rows=$(post 'SELECT a.ef_id, b.name FROM plans a CROSS JOIN rules b' | nd 'end["rowCount"] if end else "NO-END"')
eq "a $exp_cross-row body streams to completion" "$exp_cross" "$big_rows"

eq "500k generated rows arrive intact" "500000" \
   "$(post 'SELECT i, i::VARCHAR AS s, i * 2 AS d FROM range(500000) t(i)' | nd 'end["rowCount"]')"
eq "the last row of 500k is correct" "[499999, '499999', 999998]" \
   "$(post 'SELECT i, i::VARCHAR AS s, i * 2 AS d FROM range(500000) t(i)' | nd 'repr(rows[-1])')"
eq "a 64-column result is well formed" "64" \
   "$(post "SELECT * FROM (SELECT $(python3 -c 'print(", ".join("%d AS c%d" % (i, i) for i in range(64)))')) LIMIT 1" \
      | nd 'len(cols)')"
eq "a 1 MB single value survives" "1048576" \
   "$(post "SELECT repeat('x', 1048576) AS big" | nd 'len(rows[0][0])')"
eq "a wide row with mixed types is well formed" "True" \
   "$(post "SELECT 1::BIGINT a, 'x' b, 1.5::DOUBLE c, [1,2] d, {'k':'v'} e, NULL f" | nd 'len(rows[0]) == 6')"

# Time to first byte should be a small fraction of total on a long stream. If
# the server buffered the whole result, the two would be nearly equal.
curl -sS -m "$timeout" -o /dev/null \
  -w '%{time_starttransfer} %{time_total}' -H "Authorization: Bearer $token" \
  --data '{"sql":"SELECT i, repeat(i::VARCHAR, 40) AS pad FROM range(2000000) t(i)"}' \
  "$base/sql" > "$work/timing.txt"
read -r ttfb total < "$work/timing.txt"
streamed=$(python3 -c "print('yes' if $total > 0 and $ttfb < $total * 0.75 else 'no')")
if [[ $streamed == yes ]]; then
  ok "rows start arriving before the query finishes (ttfb ${ttfb}s of ${total}s)"
else
  bad "rows start arriving before the query finishes" "ttfb ${ttfb}s vs total ${total}s — looks buffered"
fi

# ---------------------------------------------------------------------------
section "Concurrency"
# ---------------------------------------------------------------------------

# More clients than workers, so the excess genuinely queues.
concurrent=16
rm -f "$work"/c.*
pids=""
for i in $(seq 1 $concurrent); do
  ( post "SELECT $i AS w, count(*) AS n FROM plans a CROSS JOIN rules b" \
      | nd "'%s|%s' % (rows[0][0], rows[0][1])" > "$work/c.$i" ) &
  pids="$pids $!"
done
for p in $pids; do wait "$p"; done
concurrent_ok=0
for i in $(seq 1 $concurrent); do
  [[ "$(cat "$work/c.$i" 2>/dev/null)" == "$i|$exp_cross" ]] && concurrent_ok=$((concurrent_ok + 1))
done
eq "$concurrent concurrent queries all return their own correct answer" "$concurrent" "$concurrent_ok"

# Readers and writers at the same time, on different connections.
post 'CREATE OR REPLACE TABLE hammer(i BIGINT, who VARCHAR)' > /dev/null
rm -f "$work"/w.* "$work"/r.*
pids=""
for i in $(seq 1 8); do
  ( for j in $(seq 1 10); do
      status 'INSERT INTO hammer VALUES (?, ?)' "[$j, \"w$i\"]" >> "$work/w.$i"
    done ) &
  pids="$pids $!"
done
for i in $(seq 1 4); do
  ( for _ in $(seq 1 10); do status 'SELECT count(*) FROM hammer' >> "$work/r.$i"; done ) &
  pids="$pids $!"
done
for p in $pids; do wait "$p"; done
eq "80 concurrent inserts all landed" "80" "$(scalar 'SELECT count(*) FROM hammer')"
eq "every insert returned 200" "80" "$(cat "$work"/w.* | grep -c '^200$')"
eq "every concurrent read returned 200" "40" "$(cat "$work"/r.* | grep -c '^200$')"
eq "writes are visible across connections" "8" "$(scalar 'SELECT count(DISTINCT who) FROM hammer')"

# ---------------------------------------------------------------------------
section "Abuse"
# ---------------------------------------------------------------------------

eq "an oversized body does not take the server down" "True" \
   "$(python3 -c 'print("x" * 200000)' > "$work/huge.txt"; \
      curl -sS -m 30 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $token" \
        --data-binary "@$work/huge.txt" "$base/sql" | grep -q '^4' && echo True || echo False)"
eq "a client that hangs up mid-stream does not wedge a worker" "200" \
   "$(curl -sS -m 1 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $token" \
        --data '{"sql":"SELECT i FROM range(5000000) t(i)"}' "$base/sql" >/dev/null 2>&1; \
      sleep 1; curl -sS -m 10 -o /dev/null -w '%{http_code}' "$base/health")"
eq "the server still answers correctly afterwards" "$exp_n_sites" \
   "$(scalar 'SELECT count(*) FROM sites')"
eq "deeply nested SQL is handled, not crashed" "True" \
   "$(status "$(python3 -c 'print("SELECT * FROM (" * 40 + "SELECT 1 AS x" + ") t" * 40)')" | grep -qE '^(200|400)$' && echo True || echo False)"

# ---------------------------------------------------------------------------
if [[ "$soak" -gt 0 ]]; then
section "Soak (${soak}s of sustained load)"
# ---------------------------------------------------------------------------
  deadline=$(( $(date +%s) + soak ))
  pids=""
  rm -f "$work"/soak.*
  for i in $(seq 1 8); do
    ( n=0; bad_n=0
      while [[ $(date +%s) -lt $deadline ]]; do
        got=$(scalar 'SELECT count(*) FROM sites')
        [[ "$got" == "$exp_n_sites" ]] || bad_n=$((bad_n + 1))
        n=$((n + 1))
      done
      echo "$n $bad_n" > "$work/soak.$i" ) &
    pids="$pids $!"
  done
  for p in $pids; do wait "$p"; done
  total_n=0; total_bad=0
  for f in "$work"/soak.*; do
    read -r n b < "$f"; total_n=$((total_n + n)); total_bad=$((total_bad + b))
  done
  eq "$total_n requests over ${soak}s, none wrong" "0" "$total_bad"
  printf '  %s%.0f requests/second across 8 clients%s\n' "$dim" "$(python3 -c "print($total_n / $soak)")" "$off"
fi

# ---------------------------------------------------------------------------
section "Shutdown and durability"
# ---------------------------------------------------------------------------

post 'CREATE OR REPLACE TABLE durable AS SELECT i FROM range(1000) t(i)' > /dev/null
eq "wrote 1000 rows before shutdown" "1000" "$(scalar 'SELECT count(*) FROM durable')"

# The WAL should exist while running with data uncheckpointed, and be gone
# after a clean stop. That is the whole point of the SIGTERM handler.
kill -TERM "$server_pid"
for _ in $(seq 1 60); do kill -0 "$server_pid" 2>/dev/null || break; sleep 0.25; done
if kill -0 "$server_pid" 2>/dev/null; then
  bad "SIGTERM stops the server" "still running after 15s"
  kill -KILL "$server_pid" 2>/dev/null
else
  ok "SIGTERM stops the server"
fi
unset server_pid

if [[ -f "$db.wal" ]]; then
  bad "the WAL is folded in on shutdown" "$db.wal still exists ($(wc -c < "$db.wal") bytes)"
else
  ok "the WAL is folded in on shutdown"
fi

eq "the data is there when the file is reopened" "1000" \
   "$(duckdb -readonly -csv -noheader "$db" -c 'SELECT count(*) FROM durable' 2>/dev/null | tail -1)"
eq "the concurrent inserts are there too" "80" \
   "$(duckdb -readonly -csv -noheader "$db" -c 'SELECT count(*) FROM hammer' 2>/dev/null | tail -1)"
eq "the original tables are unharmed" "$exp_digest" \
   "$(duckdb -readonly -csv -noheader "$db" -c "SELECT md5(string_agg(account, '|' ORDER BY account)) FROM sites" 2>/dev/null | tail -1)"

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

printf '\n%s%d passed, %d failed%s\n' "$bold" "$pass" "$fail" "$off"
if (( fail > 0 )); then
  printf '\n%sfailures:%s\n' "$red" "$off"
  for f in "${failures[@]}"; do printf '  - %s\n' "$f"; done
  [[ "${KEEP:-0}" == 1 ]] && printf '\nartifacts kept in %s\n' "$work"
  exit 1
fi
exit 0
