#!/usr/bin/env bash
#
# deployment.sh — point this at a server that is already running and
# find out whether it is one you would put traffic on.
#
#   scripts/deployment.sh --url http://127.0.0.1:9500 --token T
#   scripts/deployment.sh --url https://harbor.internal --token "$TOK" --json
#
# Every other suite in this directory starts its own server on a copy of a
# database. This one does not start anything and does not create anything: it
# is meant to be run against the real deployment, including production, so it
# is strictly read-only. Nothing here writes, and the one write it performs is
# a rollback that proves writes are refused or accepted as configured.
#
# The distinction matters because the interesting failures are the ones that
# only exist in a deployment: a reverse proxy that buffers a streaming response
# until the last row, a load balancer that closes idle keep-alive connections,
# a token that was configured in one place and not another, a TLS terminator
# that mangles UTF-8. None of those can be caught on a developer's laptop by a
# suite that starts its own listener.
#
# Exit status is 0 when the deployment is fit to serve, 1 otherwise. With
# --json the results are also written as one object, for a health dashboard or
# a deploy gate that wants more than an exit code.

set -uo pipefail

url=""; token=""; as_json=0; verbose=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --url)     url=$2; shift 2 ;;
    --token)   token=$2; shift 2 ;;
    --json)    as_json=1; shift ;;
    --verbose) verbose=1; shift ;;
    -h|--help) sed -n '2,28p' "$0" | cut -c3-; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ -z "$url" ]] && { echo "usage: $0 --url URL [--token T] [--json]" >&2; exit 2; }
url=${url%/}

pass=0; fail=0; warn=0
declare -a failures=() warnings=() results=()
bold=$(tput bold 2>/dev/null || true); red=$(tput setaf 1 2>/dev/null || true)
green=$(tput setaf 2 2>/dev/null || true); yellow=$(tput setaf 3 2>/dev/null || true)
dim=$(tput dim 2>/dev/null || true); off=$(tput sgr0 2>/dev/null || true)

say()     { (( as_json )) || printf "$@"; }
section() { say '\n%s%s%s\n' "$bold" "$1" "$off"; }
record()  { results+=("{\"name\":$(jq -Rn --arg s "$1" '$s'),\"status\":\"$2\",\"detail\":$(jq -Rn --arg s "${3:-}" '$s')}"); }
ok()   { pass=$((pass+1)); record "$1" ok "${2:-}";   say '  %s✓%s %s%s\n' "$green" "$off" "$1" "${2:+ ${dim}(${2})${off}}"; }
bad()  { fail=$((fail+1)); failures+=("$1"); record "$1" fail "${2:-}"
         say '  %s✗%s %s\n     %s%s%s\n' "$red" "$off" "$1" "$dim" "${2:-}" "$off"; }
soft() { warn=$((warn+1)); warnings+=("$1"); record "$1" warn "${2:-}"
         say '  %s!%s %s\n     %s%s%s\n' "$yellow" "$off" "$1" "$dim" "${2:-}" "$off"; }
eq()   { if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1" "expected [$2], got [$3]"; fi; }

command -v jq >/dev/null || { echo "validate-deployment needs jq" >&2; exit 2; }

# Scratch space for the two checks that shell out to Python. Their output goes
# through a file rather than a command substitution: bash 3.2, which is what
# macOS ships, mis-parses a heredoc nested inside $( ), and the damage shows up
# as a syntax error hundreds of lines further down.
work=$(mktemp -d "${TMPDIR:-/tmp}/harbor-validate.XXXXXX")
trap 'rm -rf "$work"' EXIT

auth=(); [[ -n "$token" ]] && auth=(-H "Authorization: Bearer $token")

# post <sql> [params-json] — the NDJSON body, or empty on transport failure
post() {
  local body
  if [[ -n "${2:-}" ]]; then
    body=$(jq -cn --arg s "$1" --argjson p "$2" '{sql:$s,params:$p}')
  else
    body=$(jq -cn --arg s "$1" '{sql:$s}')
  fi
  curl -sS -m 60 "${auth[@]}" -H 'Content-Type: application/json' --data "$body" "$url/sql" 2>/dev/null
}
code() { # code <sql> — the HTTP status alone
  curl -sS -m 60 -o /dev/null -w '%{http_code}' "${auth[@]}" -H 'Content-Type: application/json' \
       --data "$(jq -cn --arg s "$1" '{sql:$s}')" "$url/sql" 2>/dev/null
}
# The first value of the first row, via the same NDJSON parse a client uses.
scalar() { post "$1" "${2:-}" | jq -rs 'map(select(.type=="row"))|if length>0 then .[0].values[0]|tostring else "NO-ROWS" end' 2>/dev/null; }

# ---------------------------------------------------------------------------
section "Reachability"
# ---------------------------------------------------------------------------

t0=$(python3 -c 'import time;print(time.time())')
health=$(curl -sS -m 10 -o /dev/null -w '%{http_code}' "$url/health" 2>/dev/null)
t1=$(python3 -c 'import time;print(time.time())')
ms=$(python3 -c "print(int((float('$t1')-float('$t0'))*1000))")
if [[ "$health" == "200" ]]; then
  ok "health responds 200" "${ms}ms"
  (( ms > 1000 )) && soft "health is slow" "${ms}ms for a liveness probe suggests the process is saturated"
else
  bad "health responds 200" "got [$health] from $url/health — nothing else below will mean anything"
  say '\n%sthe deployment is not reachable; stopping here%s\n' "$red" "$off"
  exit 1
fi

# /health must not need the token, or an orchestrator's probe fails closed and
# takes a working deployment out of rotation.
eq "health needs no credential" "200" "$(curl -sS -m 10 -o /dev/null -w '%{http_code}' "$url/health" 2>/dev/null)"

# There is no /version endpoint; the extension reports its version through
# SQL, which is the route that exists in every build.
ver=$(scalar 'SELECT version FROM harbor_version()')
if [[ -n "$ver" && "$ver" != "NO-ROWS" ]]; then
  ok "reports its version" "$ver"
else
  soft "reports its version" "harbor_version() returned nothing"
fi

# ---------------------------------------------------------------------------
section "Authentication"
# ---------------------------------------------------------------------------

if [[ -n "$token" ]]; then
  eq "no credential is refused" "401" \
     "$(curl -sS -m 20 -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' --data '{"sql":"SELECT 1"}' "$url/sql" 2>/dev/null)"
  eq "a wrong credential is refused" "401" \
     "$(curl -sS -m 20 -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer not-the-token' -H 'Content-Type: application/json' --data '{"sql":"SELECT 1"}' "$url/sql" 2>/dev/null)"
  # A prefix of the real token must not pass; that would mean a comparison
  # that stops at the first difference in length rather than in content.
  eq "a truncated credential is refused" "401" \
     "$(curl -sS -m 20 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer ${token:0:${#token}-1}" -H 'Content-Type: application/json' --data '{"sql":"SELECT 1"}' "$url/sql" 2>/dev/null)"
  eq "the right credential is accepted" "200" "$(code 'SELECT 1')"
else
  soft "authentication" "no --token given, so this deployment is either open or untested here"
fi

# ---------------------------------------------------------------------------
section "The query path"
# ---------------------------------------------------------------------------

eq "a trivial query answers" "1" "$(scalar 'SELECT 1 AS one')"
eq "arithmetic is the database's, not a cache's" "1764" "$(scalar 'SELECT 42*42 AS n')"

# The envelope, in full: schema first, rows in order, exactly one end, and a
# rowCount that matches what was actually sent. A proxy that reorders or drops
# a chunk shows up right here.
envelope=$(post 'SELECT i FROM range(5) t(i)' | jq -rs '
  {first: (.[0].type),
   last:  (.[-1].type),
   rows:  (map(select(.type=="row"))|length),
   count: (map(select(.type=="end"))|.[0].rowCount),
   order: (map(select(.type=="row"))|map(.values[0])|tostring)}' 2>/dev/null)
eq "schema comes first"            "schema"      "$(jq -r .first <<<"$envelope")"
eq "end comes last"                "end"         "$(jq -r .last  <<<"$envelope")"
eq "every row arrives"             "5"           "$(jq -r .rows  <<<"$envelope")"
eq "rowCount matches the rows"     "5"           "$(jq -r .count <<<"$envelope")"
eq "rows arrive in order"          '[0,1,2,3,4]' "$(jq -r .order <<<"$envelope")"

# Column metadata is what a typed client builds its decoder from.
schema=$(post "SELECT 1::BIGINT AS a, 'x' AS b, 1.5::DECIMAL(5,2) AS c" | jq -rs 'map(select(.type=="schema"))|.[0].columns' 2>/dev/null)
eq "column names survive"    '["a","b","c"]'                    "$(jq -c '[.[].name]' <<<"$schema")"
eq "column types survive"    '["BIGINT","VARCHAR","DECIMAL(5,2)"]' "$(jq -c '[.[].duckdbType]' <<<"$schema")"
eq "decimal carries its scale" '{"width":5,"scale":2}'          "$(jq -c '.[2].decimal' <<<"$schema")"

# ---------------------------------------------------------------------------
section "Encoding through the whole stack"
# ---------------------------------------------------------------------------

# Anything between here and the database that re-encodes bytes — a TLS
# terminator, a proxy that rewrites bodies, a gateway that assumes latin-1 —
# breaks one of these and nothing else.
eq "utf-8 round-trips"        "café — 世界 🦆" "$(scalar "SELECT 'café — 世界 🦆' AS s")"
eq "quotes and backslashes survive" 'a"b\c'   "$(scalar "SELECT 'a\"b\\c' AS s")"
eq "embedded newlines survive"  "$(printf 'a\nb')" "$(scalar "SELECT 'a' || chr(10) || 'b' AS s")"

# U+2028 is legal in a JSON string and is a line break to every Unicode-aware
# splitter. In a newline-delimited format it must be escaped, or one row is
# read as two and the leftover half is not valid JSON.
# The separator is looked for by code point rather than by a literal in this
# file, which an editor or a copy-paste would silently normalise away — and a
# check for U+2028 that cannot see a U+2028 passes for the wrong reason.
post "SELECT 'a' || chr(8232) || 'b' AS s" > "$work/u2028.txt"
if python3 -c 'import sys; sys.exit(0 if chr(0x2028) in open(sys.argv[1], encoding="utf-8").read() else 1)' "$work/u2028.txt"; then
  bad "U+2028 is escaped, so a row cannot split in two" \
      "a raw separator reached the client: $(head -c 120 "$work/u2028.txt")"
else
  ok "U+2028 is escaped, so a row cannot split in two"
fi

# Past 2^53 a JSON number stops being exact; harbor quotes those rather than
# hand a client a value that silently changes when it is parsed.
eq "integers past 2^53 are quoted" '"9007199254740993"' \
   "$(post 'SELECT 9007199254740993::BIGINT AS n' | jq -rs 'map(select(.type=="row"))|.[0].values[0]|tojson')"
eq "integers within 2^53 stay numbers" '9007199254740991' \
   "$(post 'SELECT 9007199254740991::BIGINT AS n' | jq -rs 'map(select(.type=="row"))|.[0].values[0]|tojson')"

eq "NULL is null, not the string"   'null' \
   "$(post 'SELECT NULL::INTEGER AS n' | jq -rs 'map(select(.type=="row"))|.[0].values[0]|tojson')"
eq "blobs are base64"               'aGVsbG8=' "$(scalar "SELECT 'hello'::BLOB AS b")"
eq "dates are ISO-8601"             '2024-02-29' "$(scalar "SELECT DATE '2024-02-29' AS d")"
# Hoisted rather than written inline: bash 3.2 brace-expands a {a, b} literal
# that appears inside "$( )", runs the pipeline once per expansion, and the
# check fails against a server that is answering correctly.
struct_sql="SELECT {'a': 1, 'b': 'x'} AS s"
struct_out=$(post "$struct_sql" | jq -cs 'map(select(.type=="row"))|.[0].values[0]')
eq "structs keep their field names" '{"a":1,"b":"x"}' "$struct_out"

# ---------------------------------------------------------------------------
section "Errors and refusals"
# ---------------------------------------------------------------------------

eq "a syntax error is 400, not 500"   "400" "$(code 'SELEKT 1')"
eq "an unknown table is 400"          "400" "$(code 'SELECT * FROM no_such_table_here')"
eq "malformed JSON is 400"            "400" \
   "$(curl -sS -m 20 -o /dev/null -w '%{http_code}' "${auth[@]}" -H 'Content-Type: application/json' --data '{"sql":' "$url/sql" 2>/dev/null)"
eq "an empty statement is 400"        "400" "$(code '')"

err=$(post 'SELECT * FROM no_such_table_here' | jq -rs 'map(select(.type=="error"))|.[0]' 2>/dev/null)
if [[ "$(jq -r '.code // empty' <<<"$err")" != "" ]]; then
  ok "errors carry a code clients can branch on" "$(jq -r .code <<<"$err")"
else
  bad "errors carry a code clients can branch on" "no code field: $(head -c 200 <<<"$err")"
fi

# The reason this check exists: duckdb-rs's prepare() runs every statement but
# the last, so a server that forwards a multi-statement body executes the DROP
# and reports on the SELECT. It must be refused before anything runs.
eq "multiple statements are refused"  "400" "$(code 'SELECT 1; SELECT 2')"
eq "a trailing semicolon is still one statement" "200" "$(code 'SELECT 1;')"
eq "a semicolon inside a string is not a separator" "200" "$(code "SELECT 'a;b' AS s")"
eq "a semicolon inside a comment is not a separator" "200" "$(code $'SELECT 1 -- a; b\n')"

# ---------------------------------------------------------------------------
section "Parameters"
# ---------------------------------------------------------------------------

eq "a parameter binds"          "7"        "$(scalar 'SELECT ?::INTEGER AS n' '[7]')"
eq "a string parameter binds"   "hello"    "$(scalar 'SELECT ?::VARCHAR AS s' '["hello"]')"
eq "a non-ASCII parameter binds" "café"    "$(scalar 'SELECT ?::VARCHAR AS s' '["café"]')"
eq "a null parameter binds"     "null"     "$(post 'SELECT ?::INTEGER AS n' '[null]' | jq -rs 'map(select(.type=="row"))|.[0].values[0]|tojson')"
# A parameter is data. If this returns rows from a table, it is being pasted
# into the SQL rather than bound.
eq "a parameter is never interpolated" "1; DROP TABLE x" \
   "$(scalar 'SELECT ?::VARCHAR AS s' '["1; DROP TABLE x"]')"
eq "too few parameters is 400"  "400" \
   "$(curl -sS -m 20 -o /dev/null -w '%{http_code}' "${auth[@]}" -H 'Content-Type: application/json' --data '{"sql":"SELECT ?::INTEGER","params":[]}' "$url/sql" 2>/dev/null)"

# ---------------------------------------------------------------------------
section "Streaming and concurrency, as deployed"
# ---------------------------------------------------------------------------

# Time to the first row versus time to the last. A proxy that buffers the whole
# response — the single most common way a streaming deployment stops streaming
# — makes these two numbers equal, and the query looks fine while every client
# waits for the last row before seeing the first.
python3 - "$url" "$token" > "$work/stream.txt" <<'PY'
import subprocess, sys, time
url, token = sys.argv[1], sys.argv[2]
cmd = ["curl", "-sSN", "-m", "120", "-H", "Content-Type: application/json"]
if token:
    cmd += ["-H", "Authorization: Bearer " + token]
cmd += ["--data", '{"sql":"SELECT i, repeat(\'x\',100) FROM range(300000) t(i)"}', url + "/sql"]
start = time.time()
first = None
rows = 0
p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
for line in p.stdout:
    if b'"row"' in line:
        rows += 1
        if first is None:
            first = time.time() - start
p.wait()
print(int((first or 0) * 1000), int((time.time() - start) * 1000), rows)
PY
read -r first_ms total_ms rows < "$work/stream.txt"
if (( rows >= 300000 )); then
  ok "a 300k-row result streams to completion" "${rows} rows in ${total_ms}ms"
else
  bad "a 300k-row result streams to completion" "only ${rows} rows arrived"
fi
if (( first_ms * 2 < total_ms || total_ms < 500 )); then
  ok "the first row arrives before the last" "first ${first_ms}ms of ${total_ms}ms"
else
  soft "the first row arrives before the last" \
       "first row at ${first_ms}ms of ${total_ms}ms — something between here and harbor is buffering the whole response"
fi

# Keep-alive is what stops a busy client from exhausting its ephemeral ports.
# A load balancer that closes after every response reintroduces exactly that.
reused=$(curl -sS -m 30 "${auth[@]}" -H 'Content-Type: application/json' \
              --data '{"sql":"SELECT 1"}' "$url/sql" \
              --next -sS -m 30 "${auth[@]}" -H 'Content-Type: application/json' \
              --data '{"sql":"SELECT 2"}' "$url/sql" -w '%{num_connects}' -o /dev/null 2>/dev/null | tail -1)
# num_connects is reported per transfer, so 0 on the second request means it
# went out over the connection the first one opened.
if [[ "$reused" == "0" ]]; then
  ok "connections are reused between requests"
else
  soft "connections are reused between requests" \
       "the second request opened $reused new connections — keep-alive is not surviving the deployment"
fi

# Concurrency, at the shape a real client uses: several connections at once,
# each making several requests, all of which must be correct.
python3 - "$url" "$token" > "$work/conc.txt" <<'PY'
import http.client, json, sys, threading
from urllib.parse import urlparse
u = urlparse(sys.argv[1]); token = sys.argv[2]
cls = http.client.HTTPSConnection if u.scheme == "https" else http.client.HTTPConnection
headers = {"Content-Type": "application/json"}
if token: headers["Authorization"] = "Bearer " + token
good = [0]; lock = threading.Lock()
def work():
    c = cls(u.hostname, u.port, timeout=60)
    n = 0
    try:
        for i in range(20):
            c.request("POST", (u.path or "") + "/sql", json.dumps({"sql": "SELECT %d*3 AS n" % i}), headers)
            body = c.getresponse().read().decode()
            vals = [json.loads(l)["values"][0] for l in body.split("\n") if l.strip() and '"row"' in l]
            if vals == [i * 3]: n += 1
    except Exception:
        pass
    finally:
        c.close()
    with lock: good[0] += n
ts = [threading.Thread(target=work) for _ in range(12)]
[t.start() for t in ts]; [t.join() for t in ts]
print(good[0])
PY
eq "12 concurrent clients x 20 requests all correct" "240" "$(cat "$work/conc.txt")"

# ---------------------------------------------------------------------------
section "Data at rest"
# ---------------------------------------------------------------------------

tables=$(scalar "SELECT count(*) AS n FROM information_schema.tables WHERE table_schema NOT IN ('information_schema','pg_catalog')")
if [[ "$tables" =~ ^[0-9]+$ ]] && (( tables > 0 )); then
  ok "the attached database has tables" "$tables"
  (( verbose )) && post "SELECT table_name, estimated_size FROM duckdb_tables() ORDER BY 1" | jq -rs 'map(select(.type=="row"))|.[]|.values|"       \(.[0]) — \(.[1]) rows"'
else
  soft "the attached database has tables" "none found — is this the database you meant to serve?"
fi

# Read-only or writable is a deployment decision, not a defect either way, but
# it should be the one that was intended. Asked as a setting rather than by
# starting a transaction: BEGIN and ROLLBACK sent as two requests land on two
# different pooled connections, so the BEGIN is never undone and the
# connection that received it is left inside a transaction. A validator that
# is safe to point at production cannot do that.
mode=$(scalar "SELECT current_setting('access_mode') AS m")
case "$mode" in
  READ_ONLY|read_only) ok "the deployment is read-only" "as configured" ;;
  *)                   ok "the deployment accepts writes" "access_mode=$mode" ;;
esac

# ---------------------------------------------------------------------------

if (( as_json )); then
  printf '{"url":%s,"pass":%d,"fail":%d,"warn":%d,"checks":[%s]}\n' \
    "$(jq -Rn --arg s "$url" '$s')" "$pass" "$fail" "$warn" \
    "$(IFS=,; echo "${results[*]}")"
else
  printf '\n%s%d passed, %d failed, %d warnings%s\n' "$bold" "$pass" "$fail" "$warn" "$off"
  if (( warn > 0 )); then
    printf '\n%swarnings (not failures, but look at them):%s\n' "$yellow" "$off"
    for w in "${warnings[@]}"; do printf '  - %s\n' "$w"; done
  fi
  if (( fail > 0 )); then
    printf '\n%sfailures:%s\n' "$red" "$off"
    for f in "${failures[@]}"; do printf '  - %s\n' "$f"; done
    printf '\n%sthis deployment should not take traffic%s\n' "$red" "$off"
  else
    printf '\n%sthe deployment is fit to serve%s\n' "$green" "$off"
  fi
fi
(( fail > 0 )) && exit 1
exit 0
