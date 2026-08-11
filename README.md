<p align="center">
  <img src="https://github.com/shreeve/duckdb-harbor/raw/main/duckdb-harbor-social.png" alt="DuckDB Harbor" width="600">
</p>

# duckdb-harbor

> **Many clients, one DuckDB, over plain HTTP. `POST` a statement, read NDJSON
> back.**

DuckDB is embedded: one process opens the file, and that process holds an
exclusive lock. So "let my app talk to my DuckDB" normally means picking a
language binding and living inside that one process, forever.

DuckDB Harbor is a DuckDB extension, written in Rust, that turns that one
process into an HTTP server. Many clients, any language, all at once — no
driver, no client library, no wire protocol to implement.

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

Two routes. That is the whole surface:

```
POST /sql      run one statement, stream the result as NDJSON
GET  /health   liveness, no credential required
```

## Get it running

Nothing to compile. Download the extension for your platform from the
[releases page](https://github.com/shreeve/duckdb-harbor/releases), then point
DuckDB at it. `-unsigned` is required, since this is not in DuckDB's extension
registry:

```console
$ duckdb -unsigned mydata.duckdb
```
```sql
LOAD '/path/to/harbor.duckdb_extension';
CALL harbor_serve(bind := '127.0.0.1', port := 9495);  -- returns the address
CALL harbor_wait();                                    -- blocks until stopped
```

That is the whole SQL surface, four table functions: `harbor_serve`,
`harbor_stop`, `harbor_wait`, and `harbor_version`.

For running a database as a long-lived service, the bundled launcher handles
the pieces that are easy to get wrong — the shutdown checkpoint and its
ordering, and a `checkpoint_threshold` that keeps weeks of committed rows out
of an unfolded WAL:

```console
$ duckdb-harbor mydata.duckdb --port 9495 --extension ./harbor.duckdb_extension
harbor: serving on http://127.0.0.1:9495  token=4abf6e3696b19601…
```

It finds the extension beside itself or in `~/.duckdb/extensions` if you would
rather not pass `--extension`; `HARBOR_EXTENSION` works too. With no `--token`
it mints one and prints it as the server binds — the only time it is shown, and
`HARBOR_TOKEN` is honoured if set. It runs in the foreground, the way launchd,
systemd and Docker want it, and `SIGTERM` and `Ctrl-C` drain in-flight requests
and `CHECKPOINT` before exiting, so the next open never replays a WAL.

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
  headers: { Authorization: `Bearer ${token}` },
  body: JSON.stringify({ sql: "SELECT id, total FROM orders WHERE id > ?",
                         params: [100] }),
});
for await (const chunk of res.body) {
  for (const line of new TextDecoder().decode(chunk).trim().split("\n")) {
    const msg = JSON.parse(line);
    if (msg.type === "row") console.log(msg.values);
  }
}
```

## Performance

DuckDB answers the query; DuckDB Harbor's job is to stay out of the way. It
sustains thousands of requests per second across concurrent clients on a
laptop, with sub-millisecond overhead at low concurrency.

| clients | req/s | p50 | p95 | p99 | non-200 | wrong answers |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 3,269 | 0.20 ms | 0.58 ms | 0.74 ms | 0 | 0 |
| 4 | 7,012 | 0.50 ms | 1.18 ms | 1.40 ms | 0 | 0 |
| 16 | 9,096 | 1.66 ms | 2.90 ms | 3.60 ms | 0 | 0 |

Mean of five 10-second runs per level on an idle M-series laptop, 20% of
requests being `INSERT`s, connections reused, throughput taken from wall-clock
across the level rather than summed from per-request timings. Run-to-run spread
was under 4% at every level.

Every read in that run was checked against an answer taken from the database
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
request arriving when every worker is busy waits for one to come free.

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

| Extension | Serves | Client needs |
| --- | --- | --- |
| `quack` | other DuckDB instances | DuckDB |
| `ui` | a browser | a browser |
| **`harbor`** | **everything else** | **`curl`** |

All three run in one process, over one database file, at the same time. DuckDB
Harbor does not reimplement the other two — `quack` and `ui` are stock and
unmodified.

## Known limitations

**No query timeout and no cancellation.** A runaway query holds its worker
until it finishes. Six of them and the server stops answering. This is the
largest gap.

**`TIME WITH TIME ZONE` loses its offset.** DuckDB's Arrow exporter discards it
before DuckDB Harbor sees the value, so times at different offsets become
indistinguishable. The column is marked `"lossless": false` rather than
returning a time that silently means something else. Recover the offset with
`date_part('timezone', t)`, or cast to `VARCHAR`.

**`TIME_NS` and `VARIANT` are refused with `400`.** Neither can cross the Arrow
boundary the Rust client uses. Cast to `VARCHAR` and the value comes back
intact.

**A transaction cannot span requests.** Pooled connections are not pinned to a
client, so a `BEGIN` has no way to be committed by a later request — it is
rolled back before the connection is reused. Send a transaction as one
statement.

**Bodies are capped at 8 MiB**, declared or delivered; over that is a `413`.
There is no rate limiting, no CORS, and no request logging — defensible for a
loopback service behind a proxy, worth knowing before it faces a browser.

**Signal handling is Unix only.** On Windows there is no `SIGTERM` drain and no
checkpoint on exit; `harbor_stop` is the way out.

**One exact DuckDB version per binary.** A build is pinned to **v1.5.5**, and a
different engine refuses to load it rather than risk mis-reading it.

## Working on it

Building is only needed to change it. DuckDB Harbor is Rust against DuckDB's
**C extension API**, so no DuckDB source tree is required — a Rust toolchain
and `make release` produce the artifact. It is two files:
[`src/lib.rs`](src/lib.rs) is the extension entry, its four table functions,
and the whole HTTP server; [`bin/duckdb-harbor`](bin/duckdb-harbor) runs a
database as a service.

`make check` runs the suite and `make check_quick` runs the subset that
finishes in about a minute; run [`test/scripts/fixture.sh`](test/scripts/fixture.sh)
once first to build the database the suites read. It is heavily tested: 11
suites and roughly 4,100 lines of tests against about 2,200 lines of product.
Every answer is checked against an oracle that is not DuckDB Harbor — values
read from the database file before the server takes the lock, Python's own
`datetime` and `base64` for fuzzed values, and the v1 harbor for differential
runs, where any divergence has to be recorded with a reason before the run goes
green. An oracle that shares an implementation with the thing it checks
confirms only that the code is self-consistent.

## Status

Early. No release is published yet, so for the moment the artifact has to be
built with `make release`; the download path above is how it will work, and is
already what the launcher expects. The
[v1 harbor](https://github.com/shreeve/duckdb-harbor-v1) is the C++
implementation this replaces.

## License

MIT.
