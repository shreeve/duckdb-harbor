<p align="center">
  <img src="https://github.com/shreeve/duckdb-harbor/raw/main/duckdb-harbor-social.png" alt="DuckDB Harbor" width="600">
</p>

# duckdb-harbor

> **Many clients, one DuckDB, over plain HTTP. `POST` a statement, read NDJSON
> back.**

DuckDB is embedded: one process opens the file, and that process holds an
exclusive lock. So "let my app talk to my DuckDB" normally means picking a
language binding and living inside that one process, forever.

DuckDB Harbor is `harbor`, a small Rust binary that opens a DuckDB database and
serves it over HTTP. Point it at a file and that one process becomes a server:
many clients, any language, all at once — no driver, no client library, no wire
protocol to implement. It ships with `pilot`, a companion CLI that speaks the
same protocol for a duckdb-shell-class REPL over the socket.

If it can speak HTTP and parse JSON, it can query your database.

```console
$ curl -s localhost:9495/sql -H 'Authorization: Bearer …' \
       -d '{"sql":"SELECT id, total FROM orders LIMIT 2"}'
{"type":"schema","columns":[{"name":"id","duckdbType":"BIGINT","lossless":true},
                            {"name":"total","duckdbType":"DECIMAL(10,2)","lossless":true,
                             "decimal":{"width":10,"scale":2}}]}
{"type":"row","values":[1,"19.99"]}
{"type":"row","values":[2,"4.50"]}
{"type":"end","rowCount":2,"timeMs":3}
```

One `schema` message, one `row` per row, one `end`. Rows go out as DuckDB
produces them, so a client can start on row one while the server is still
producing the last one.

Ten routes. That is the whole surface — two of them for queries, three so a
transaction can outlive one request, one to stop a statement that is running,
one to read the schema without asking five questions, one that says who a
berth is, one cheap activity pulse for an interactive Pilot, and one graceful
fleet shutdown route:

```
POST /sql                  run one statement, stream the result as NDJSON
                           (Accept: application/json for one document instead)
GET  /ready                can this server answer a query? no credential required
GET  /catalog              the whole schema — tables, keys, unique constraints,
                           indexes, sequences — as one stable JSON document
GET  /info                 berth identity — name, harbor + DuckDB versions, pid
GET  /keepalive            keep an idle-exit berth alive while Pilot has a prompt
DELETE /shutdown           authenticated drain, checkpoint, and stop
POST /sql/sessions/new     take a connection and hold it, for a transaction
DELETE /sql/sessions/<id>  give it back
GET  /sessions             what is holding one, and for how long
DELETE /sql/queries/<id>   stop a statement the caller named when it sent it
```

`POST /sql` streams by default. Send `Accept: application/json` and the same
result comes back as one document instead:

```json
{"ok":true,
 "columns":[{"name":"id","duckdbType":"INTEGER","lossless":true}],
 "data":[[1],[2]],
 "rowCount":2,
 "timeMs":3}
```

Same columns, same values, same encoder — only the framing differs. It is worth
asking for when the result is small and a single `JSON.parse` is simpler than
reading lines; it is the wrong choice for anything large, because a JSON
document is not valid until its last byte, so nothing can be flushed as it is
built. Harbor holds at most 32 MiB for one and refuses past that with a `406`
naming NDJSON as the remedy. Streaming has no such limit.

The one thing one-shot does better: since nothing has been sent when the last
row lands, a failure is still a real status code. The same query that streams a
`200` with an `{"type":"error"}` line at the end answers `400` in this shape.

`/ready` normally runs `SELECT 1` through an ordinary executor and answers `200
{"status":"ready"}` or `503`. Under sustained worker saturation, the dedicated
probe lane asks the control connection instead, so a load balancer can still
distinguish busy from dead. It is not a process-liveness check: a process can be
running while its database path is broken. Verdicts are cached for one second,
so polling costs at most one probe query per second however often it is asked.

## Stopping a statement

A statement that has entered DuckDB does not come back until it is done, and
harbor runs a small, bounded number at once. So a query nobody wants any more
is not a slow request — it is a connection out of service, and enough of them
are the whole server.

Name a statement when you send it, and you can stop it:

```console
$ curl -s localhost:9495/sql -H "$auth" \
       -d '{"sql":"SELECT count(*) FROM huge","queryId":"report-7"}' &
$ curl -s -X DELETE localhost:9495/sql/queries/report-7 -H "$auth"
{"cancelled":true}
```

When cancellation lands before streaming begins, the statement answers `499`
with `{"code":"cancelled"}` — nginx's code, because there is no standard one
for "the caller withdrew" and neither `400` nor `500` is true. If a streaming
response already began with `200`, cancellation arrives as its final NDJSON
error event instead; an HTTP status cannot be changed after its headers were
sent. Cancelling something that already finished is `{"cancelled":false}`, not
an error: by the time a Stop button is pressed, the query it refers to may well
be over.

The id is chosen by the caller rather than issued by harbor, and it has to be:
the response does not begin until the statement is streaming or done, so an id
in the reply would arrive too late to be any use. It is refused with a `409`
while a statement of that name is already running, so two live queries can
never share one name and make a cancel a coin flip.

**A deadline is the backstop.** `{"timeoutMs": N}` on a request, or
`HARBOR_STATEMENT_TIMEOUT_MS` for a whole deployment, stops a statement without
anyone having to ask. There is no default, deliberately: harbor streams
300,000-row results and is used for queries that take minutes on purpose, so a
default deadline would break correct programs to catch incorrect ones. With no
deployment cap, zero on a request means no limit. When a deployment cap is set,
it is a hard ceiling: a request may ask for less time, but neither a larger value
nor zero can opt out of the operator's limit.

Explicit cancellation remains reachable when every executor is inside a long
statement: after sustained saturation, a connection-free probe lane accepts
query cancellation, session release, readiness, and inspection requests. The
reaper is the independent backstop. It runs on its own thread and never touches
HTTP, so deadlines are still enforced if no cancellation request arrives or a
client disappears. If a deployment's worry is runaway queries rather than
impatient users, set `HARBOR_STATEMENT_TIMEOUT_MS` or
`--statement-timeout <duration>`.

Two smaller things follow from the same machinery. Releasing a session whose
statement is still running now stops it — `{"released":false,"cancelling":true}`
— and the connection comes back on the reaper's next tick, where before the
release was simply refused. And a lease that blows its TTL while busy is
reclaimed, where the reaper used to skip it: the one lease that most needed
taking back, wedged inside a runaway statement, was the one it could never take.

Cancelling a statement inside a transaction aborts that transaction, exactly as
it does in Postgres. Harbor does not paper over it — the next statement gets
`Current transaction is aborted (please ROLLBACK)` until you do. Rolling back
silently would let the statement after a cancellation commit in autocommit
under a client that still believed it was in a transaction.

## Transactions

A transaction lives on a connection and HTTP requests do not, so one request
per statement means no transaction can span two. A session bridges that: a
connection pinned to you until you commit, roll back, or stop answering.

```console
$ sid=$(curl -s localhost:9495/sql/sessions/new -H "$auth" | jq -r .sessionId)
$ post() { curl -s localhost:9495/sql -H "$auth" -d "{\"sql\":\"$1\",\"sessionId\":\"$sid\"}"; }
$ post "BEGIN"
$ post "INSERT INTO orders (total) VALUES (19.99) RETURNING id"
$ post "INSERT INTO order_items (order_id, price) VALUES (1, 19.99)"
$ post "COMMIT"
$ curl -s -X DELETE localhost:9495/sql/sessions/$sid -H "$auth"
```

This is PgBouncer's transaction pooling, or ActiveRecord checking a connection
out of its pool — with an HTTP request where they have a socket and a thread.
Three things follow from that, and they are the parts worth knowing:

**Sessions draw from their own connections.** `HARBOR_POOL_SIZE` (default 16)
is opened at load and split: the workers take theirs, sessions get the rest. A
pool serving both would run out of workers the moment enough clients held
transactions open, and then answer nothing at all. With none free, opening a
session is a `503` with `Retry-After` — queries keep working throughout.

**Every session has a deadline.** HTTP has no reliable close signal, so a
client that vanishes mid-transaction looks exactly like one that is thinking,
and a timer is the only way that connection ever comes back. Ask for a lifetime
with `{"ttlMs": N}`; harbor caps it at five minutes and answers with what it
granted, alongside the thirty-second idle timeout it enforces regardless. When
a session is reclaimed its transaction is rolled back, and so is one released
with a transaction still open.

**One statement at a time.** A second statement sent while the first is running
gets a `409`: a transaction is a sequence, and two of them interleaving inside
one is something no client could reason about.

`GET /sessions` shows what is held — age, idle time, statements, whether a
transaction is open — and the connection accounting behind it. Free plus live
plus in-flight always equals total; `balanced` is that checked at the moment
you asked. A pool leaks connections silently and the symptom shows up weeks
later as "everything hangs", so the arithmetic is worth being able to read.

Note that DuckDB resolves write conflicts optimistically: two transactions
touching the same row do not queue, the second is refused the moment it writes.
The answer is to run the transaction again, which is what `rip/db` does for
you.

## Get it running

Two binaries: **`harbor`** (the server and fleet manager) and **`pilot`** (the
client). `harbor` links an external `libduckdb`; the same build has been
verified against DuckDB 1.5.5 and current 2.0 nightlies by resolving a
compatible library at runtime. A build needs a libduckdb to link against.
`make fetch-duckdb` pulls DuckDB's official v2 nightly into
`~/.duckdb/cli/2.0.0/`; then:

```console
$ make fetch-duckdb                       # libduckdb + duckdb CLI -> ~/.duckdb/cli/2.0.0/
$ make harbor pilot                       # -> target/release/{harbor,pilot}
$ harbor serve mydata.duckdb --token secret
harbor 0.13.4: berth "mydata" serving mydata.duckdb on ~/.harbor/mydata.sock (duckdb v2.0.0-alpha38195, memory_limit 2GB)
```

`make bootstrap` does the whole thing in one shot — fetch the engine into `~/.duckdb`,
build and install `harbor` + `pilot` onto PATH in `/usr/local/bin`, and build the
matched DuckDB UI extension. Building the UI additionally requires a compatible
`duckdb-ui` checkout (`DUCKDB_UI_DIR`, default `~/Data/Code/duckdb-ui`), a C++
toolchain, `gh`, and OpenSSL. With those prerequisites present, it takes an empty
`~/.duckdb` to a working fleet.

No toolchain? Each [release](https://github.com/shreeve/duckdb-harbor/releases)
ships one self-contained archive per
platform (osx-arm64, linux-amd64, linux-arm64, windows-amd64, windows-arm64):
harbor + pilot and the exact DuckDB shared library they were built against.
Unix archives also carry the matched `ui` extension and `install.sh`; Windows
archives put `duckdb.dll` beside the two executables and run in place.

### Browser UI

harbor can host the DuckDB UI over a berth. `make ui` builds the `ui` extension
against the *exact* engine harbor runs (out-of-tree — only the extension, seconds,
no engine compile) and installs it where `LOAD ui` finds it by name. Then:

```console
$ harbor serve mydata.duckdb --unsigned \
    --init "LOAD ui" --init "FROM start_ui_server()"     # UI at http://localhost:4213/
```

Because harbor carries `libduckdb` in-process, the dynamically-linked extension
resolves its DuckDB symbols at load, and everything — engine, harbor, extension —
derives from one nightly, so the versions match by construction (PLAN.md D11).

`harbor serve` runs in the foreground. `harbor add mydata.duckdb` spawns a
**detached** berth and returns once it answers `/ready`; `harbor ls` lists the
fleet, `harbor stop <name>` drains and `CHECKPOINT`s, `harbor rm <name>` clears
the registry (never the database file). With no `--token`, a per-berth token is
minted and written to `~/.harbor/<name>.token`.

Exits are clean: `SIGTERM` / `Ctrl-C` drain in-flight requests and `CHECKPOINT`
so the next open never replays a WAL.

Talk to it with `curl` (above), or with the bundled client — a
duckdb-shell-class REPL with syntax highlighting and completion. Tab accepts
the inline suggestion or highlighted panel entry; Down opens the completion
panel, and the arrow keys navigate it:

```console
$ pilot mydata                 # a REPL over the socket
$ pilot mydata -c "SELECT count(*) FROM orders"   # one-shot
```

Remote access is Caddy's job at the edge (TLS + auth); harbor itself speaks
plain HTTP over a unix socket or a loopback TCP port.

A bearer token grants the ability to run SQL, and ordinary DuckDB SQL can read
host files or load extensions. For a berth reachable by an untrusted token
holder, `--sealed` disables host-file access and community extensions.
`--max-temp-size` bounds disk spill, and `--statement-timeout` places the hard
statement ceiling described above. These are independent of Caddy's transport
and HTTP policy.

### Request logging

`--log` writes one line per HTTP request to stderr:

```
harbor: 2026-08-12T04:31:07Z 127.0.0.1 POST /sql 200 12ms
```

Timestamp, peer, method, path, status, duration — measured to the last body
byte rather than the first, so a slow query and a slow client both show. Off by
default. The SQL itself is never logged: it arrives in the request body, it can
be megabytes, and on this endpoint it is as likely to hold customer data as the
tables it reads.

stderr, not stdout, so it stays clear of anything a client reads. Send it
wherever the log belongs — `2>>/var/log/harbor.log`, a pipe, or a supervisor's
collector. There is no `--log FILE`: rotation and permissions are the shell's
job, and it does them better than harbor would.

## Any language

There is nothing to install on the client side. Shell:

```console
$ curl -sN localhost:9495/sql -H "Authorization: Bearer $TOKEN" \
       -d '{"sql":"SELECT count(*) FROM orders"}'
```

Python, standard library only — NDJSON means one message per line, so the
response reads as it arrives:

```python
import http.client, json

conn = http.client.HTTPConnection("127.0.0.1", 9495)
conn.request("POST", "/sql", json.dumps({"sql": "SELECT id, total FROM orders"}),
             {"Authorization": f"Bearer {token}"})

for line in conn.getresponse():
    msg = json.loads(line)
    if msg["type"] == "row":
        print(msg["values"])
```

JavaScript, with `fetch` — and `params`, which is how values are passed:

```js
const res = await fetch("http://127.0.0.1:9495/sql", {
  method: "POST",
  headers: {
    Authorization: `Bearer ${token}`,
    "Content-Type": "application/json",
  },
  body: JSON.stringify({ sql: "SELECT id, total FROM orders WHERE id > ?",
                         params: [100] }),
});

const decoder = new TextDecoder();
let pending = "";
for await (const chunk of res.body) {
  pending += decoder.decode(chunk, { stream: true });
  const lines = pending.split("\n");
  pending = lines.pop();
  for (const line of lines) {
    if (!line.trim()) continue;
    const msg = JSON.parse(line);
    if (msg.type === "row") console.log(msg.values);
  }
}
pending += decoder.decode();
if (pending.trim()) {
  const msg = JSON.parse(pending);
  if (msg.type === "row") console.log(msg.values);
}
```

## Performance

DuckDB answers the query; DuckDB Harbor's job is to stay out of the way. It
sustains tens of thousands of requests per second across concurrent clients
on a laptop, with sub-100µs round trips at low concurrency.

**harbor 0.13.0, DuckDB v2.0.0 nightly** (alpha38195), eight workers, pure
read path — `POST /sql` with `{"sql":"select 1"}` over keep-alive loopback
TCP, 10-second `oha` runs, every response a 200:

| clients | req/s | p50 | p99 |
|--:|--:|--:|--:|
| 1 | 10,914 | 0.09 ms | 0.12 ms |
| 4 | 28,167 | 0.14 ms | 0.22 ms |
| 16 | 44,079 | 0.24 ms | 0.61 ms |

The HTTP layer is not the ceiling: `GET /ready` — the same plumbing with no
SQL — measures ~99,000 req/s at 16 clients. Most of the per-request engine
cost is amortized by the per-connection prepared-statement cache (below);
0.13.0 also coalesced each response head into a single buffered write, set
`TCP_NODELAY`, and removed most per-request allocations from the HTTP layer.

An earlier, deliberately harsher benchmark — 20% `INSERT`s, every read
checked against an oracle, harbor 0.12.0 (no statement cache), **DuckDB
v1.5.5**, eight workers:

| clients | req/s | p50 | p95 | p99 | non-200 | wrong answers |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 3,269 | 0.20 ms | 0.58 ms | 0.74 ms | 0 | 0 |
| 4 | 7,012 | 0.50 ms | 1.18 ms | 1.40 ms | 0 | 0 |
| 16 | 9,096 | 1.66 ms | 2.90 ms | 3.60 ms | 0 | 0 |

Mean of five 10-second runs per level on an idle M-series laptop, connections
reused, throughput taken from wall-clock across the level rather than summed
from per-request timings. Run-to-run spread was under 4% at every level.

The engine version belongs beside the numbers, because it moves them. The same
harbor build on a **v2.0.0** nightly gets roughly half this on small statements
— 1,352 / 3,667 / 4,739 req/s at the same three levels (alpha37626; still true
of alpha38195). That is not a debug build and it is not harbor. It is v2's [new
PEG parser](https://duckdb.org/2026/08/20/duckdb-20-peg-parser), plus a small
fixed cost per execute — measured by driving each engine directly, no server:
re-executing an already-prepared statement costs +11 µs on v2, while parsing
fresh SQL text costs about 2× v1.5.5, growing with statement size. Execution
itself is at parity or faster (bulk CTAS is quicker on v2 than on 1.5.5).
Before 0.13.0 harbor parsed every request's SQL fresh, paying the parser on
every statement; that was the whole gap. Since 0.13.0 each executor
connection keeps an LRU of prepared statements keyed by statement text, so a
repeated statement skips parse and plan entirely — which is why the pure-read
numbers above sit where they do on a v2 engine. First-seen statement texts
still pay the parser once; upstream is still optimizing it pre-GA, and real
analytical queries never notice either way. Measure against the engine you
deploy.

Every read in the mixed run was checked against an answer taken from the database
file before the server opened it — a benchmark whose oracle is the server it is
benchmarking cannot detect a server that is consistently wrong.

Streaming matters more than the rate for large results. A 300,000-row result
starts arriving in single-digit milliseconds — before the query has finished
running — and completes in well under 100 ms, because nothing is buffered. A
client can start work on row one while the server is still producing row
300,000. (Whether the *query* materialises is DuckDB's business: `ORDER BY`,
hash aggregates and joins all build state first.)

Many connections, few queries: DuckDB Harbor accepts many concurrent
connections and executes a small, bounded number of statements — six by
default, settable with `--workers`. DuckDB parallelises a *single* query across
every core, so running hundreds at once produces thrashing, not throughput. A
request normally waits for a worker. If every worker has been inside a
statement for at least 250 ms, the dedicated probe lane keeps control routes
responsive and may shed new `/sql` or `/catalog` work with a retryable `503`
instead of hiding an unbounded queue behind saturated analytics.

## Why it looks like this

**Plain HTTP, on purpose.** It binds loopback and speaks HTTP, not HTTPS. TLS
belongs at the edge, where certificates, renewal, and HTTP/2 and /3 are already
solved by software that does nothing else. Put Caddy or nginx in front and
terminate there.

**One statement per request.** A second statement is rejected with `400`, and
that check is load-bearing rather than decorative: the Rust DuckDB client
*executes* every statement but the last while merely preparing one, so anything
that gets past it runs. Use `params` for values.

**Types survive the trip.** Every column carries its `duckdbType`, plus width
and scale for `DECIMAL` and nested `child`/`fields` for `LIST` and `STRUCT`, so
a typed client can reconstruct exactly what DuckDB had rather than a lossy JSON
approximation. Values JSON cannot hold exactly are quoted rather than emitted
as bare numbers, so an integer past 2^53 does not silently reprecision in a
JavaScript client. Where something genuinely cannot survive, the column says so
with `"lossless": false` instead of returning a plausible wrong answer.

## Where it fits

DuckDB's ecosystem already covers two audiences. DuckDB Harbor covers the
third.

| Serves | Client needs |
| --- | --- |
| `quack` — other DuckDB instances | DuckDB |
| `ui` — a browser | a browser |
| **`harbor` — everything else** | **`curl`** |

`quack` and `ui` are DuckDB extensions; `harbor` is a standalone server. It can
still load them into its own database with
`harbor serve db.duckdb --unsigned --init 'LOAD ui'`, so one process can answer
HTTP clients, browsers, and other DuckDB instances over one file at once. The
current 2.0 UI build comes from the Harbor-compatible `shreeve/duckdb-ui` fork
until its [upstream compatibility issue](https://github.com/duckdb/duckdb-ui/issues/242)
lands; Harbor does not patch extension source while loading it.

## Known limitations

**`TIME WITH TIME ZONE` loses its offset.** DuckDB's Arrow exporter discards it
before DuckDB Harbor sees the value, so times at different offsets become
indistinguishable. The column is marked `"lossless": false` rather than
returning a time that silently means something else. Recover the offset with
`date_part('timezone', t)`, or cast to `VARCHAR`.

**`TIME_NS` and `VARIANT` are refused with `400`.** Neither can cross the Arrow
boundary the Rust client uses. Cast to `VARCHAR` and the value comes back
intact.

**Bodies are capped at 8 MiB**, declared or delivered; over that is a `413`.
There is no rate limiting and no CORS — defensible for a service behind a proxy,
worth knowing before it faces a browser. Request logging is available with
`--log`, off by default.

**Windows berths use loopback TCP.** Unix keeps its Unix-socket default and
SIGTERM lifecycle. Windows assigns each berth a loopback port, records it in
the same sidecar registry, and uses Harbor's authenticated shutdown route to
drain and checkpoint. The Windows release does not include the UI extension.

**The engine is the linked `libduckdb`, not the binary.** harbor links
dynamically, and the same build has been verified against DuckDB 1.5.5 and a
2.0 development build. Treat that as tested compatibility, not a promise that
an arbitrary past or future DuckDB ABI will work.
`make install` puts harbor + pilot on PATH in `/usr/local/bin`, and harbor's
baked rpath resolves the engine in `~/.duckdb` — DuckDB's own world, disposable
and refetchable. The caveat that comes with that: point it at a library whose
storage format matches the database file.

## Working on it

Building is only needed to change it. The workspace has four first-party
crates:

- **`harbor`** — the server engine and fleet CLI;
- **`pilot`** — the DuckDB-shell-class Harbor client;
- **`wire`** — protocol request and response types consumed by Pilot; and
- **`justhttp`** — Harbor's small synchronous HTTP/1.1 server over TCP and Unix
  sockets.

Harbor currently implements its protocol shapes directly rather than depending
on `wire`, so a wire change needs tests on both sides; drift is not a Rust
compile error. Harbor links an external `libduckdb` rather than embedding one,
so no DuckDB source tree is required — `make fetch-duckdb` fetches a libduckdb
to link against, then `make harbor pilot` builds. The crate ships pregenerated
bindings, so there is no bindgen and no headers to find.

`make unit` runs the fast Rust tests and `make test` runs the full suite. The
full suite expects `sample.duckdb`; create it with
`test/scripts/fixture.sh sample.duckdb` when it is absent. CI performs that
fixture step explicitly. The ten suites use independent oracles where answers
need comparison — values read from the database file before the server takes
the lock, and Python's own `datetime` and `base64` for fuzzed values. An oracle
that shares an implementation with the thing it checks confirms only that the
code is self-consistent.

## Status

Pre-production. harbor and pilot are two small binaries — no Harbor extension,
launcher, or signing dance. Harbor is dynamically linked: the same build has
served DuckDB 1.5.5 and a 2.0 development build by resolving the compatible
`libduckdb` beside it. Deploy remote TCP behind Caddy, which owns TLS and edge
request policy; Harbor independently owns SQL statement deadlines.

## License

MIT.
