<p align="center">
  <img src="duckdb-harbor-social.png" alt="DuckDB Harbor" width="600">
</p>

# duckdb-harbor

> **Many clients, one DuckDB, over plain HTTP. `POST` a statement, read NDJSON back.**

DuckDB is embedded: one process opens the file, and that process holds an
exclusive lock. Harbor is a DuckDB extension, written in Rust, that turns that
one process into an HTTP endpoint so everything else can reach it — from any
language, with no driver, no client library, and no wire protocol to implement.

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

Two routes. That is the whole surface:

```
POST /sql      run one statement, stream the result as NDJSON
GET  /health   liveness, no credential required
```

## Quick start

```sql
LOAD '/path/to/harbor.duckdb_extension';
CALL harbor_serve(bind := '127.0.0.1', port := 9495);
CALL harbor_wait();
```

Or run a database as a service, which is what the bundled launcher is for:

```console
$ bin/harbor mydata.duckdb --port 9495
```

It runs in the foreground and holds the terminal, because that is what launchd,
systemd, Docker, and a shell job all want. With no `--token` harbor mints one
and reports it in the address it returns; pass `--token` or set `HARBOR_TOKEN`
to choose your own. `SIGTERM` and `Ctrl-C` drain in-flight requests and
`CHECKPOINT` before exiting, so the next open never replays a WAL, and
`checkpoint_threshold` defaults to 1MB rather than DuckDB's 16MB so a long-lived
server does not accumulate weeks of committed rows in a WAL that has never been
folded in.

## Performance

DuckDB answers the query; harbor's job is to stay out of the way. Measured by
[`scripts/swarm.py`](scripts/swarm.py) against a real database, 20% of requests
being `INSERT`s, connections reused, throughput taken from wall-clock across the
level rather than summed from per-request timings:

| clients | req/s | p50 | p95 | p99 | non-200 | wrong answers |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 2,567 | 0.2 ms | 0.8 ms | 1.1 ms | 0 | 0 |
| 4 | 5,433 | 0.6 ms | 1.5 ms | 1.9 ms | 0 | 0 |
| 16 | 7,947 | 1.9 ms | 3.3 ms | 4.3 ms | 0 | 0 |

Streaming matters more than the rate for large results. A 300,000-row result
completes in **71 ms**, and the first row reaches the client at **10 ms** —
before the query has finished running. Nothing is materialised first, so a
result larger than memory is not a problem, and a client can start work on row
one while the server is still producing row 300,000.

Every reply above was also checked for correctness, not just for status. A
benchmark that does not verify its answers is measuring the wrong thing.

## Why it looks like this

**Plain HTTP, on purpose.** Harbor binds loopback and speaks HTTP, not HTTPS.
TLS belongs at the edge, where certificates, renewal, and HTTP/2 and /3 are
already solved by software that does nothing else. Put Caddy or nginx in front
and terminate there. Keeping TLS out of the extension means no OpenSSL link, no
certificate handling in-process, and far less code to get wrong.

**One statement per request.** `POST /sql` takes exactly one. Concatenating user
input into SQL is a mistake everyone eventually makes; single-statement parsing
makes `; DROP TABLE …` structurally impossible rather than merely discouraged.
It also keeps status codes meaningful — there is no partial success to report.
Use `params` for values.

**Types survive the trip.** Every column carries its `duckdbType`, plus width
and scale for `DECIMAL` and nested `child`/`fields` for `LIST` and `STRUCT`, so
a typed client can reconstruct exactly what DuckDB had rather than a lossy JSON
approximation. Values JSON cannot hold exactly are quoted instead of emitted as
bare numbers — a bare `123456789012345678901234567890` silently becomes
`1.2345678901234568e+29` in any JavaScript client. Where something genuinely
cannot survive, the column says so with `"lossless": false` rather than
returning a plausible wrong answer.

**Many connections, few queries.** Harbor accepts many concurrent connections
and executes a small, bounded number of statements. That asymmetry is
deliberate: DuckDB parallelises a *single* query across every core, so running
hundreds at once produces thrashing and memory pressure, not throughput.
Statements past the limit queue; past the queue bound they get `503`, which is
backpressure working rather than a failure.

## Where it fits

DuckDB's ecosystem already covers two audiences. Harbor covers the third.

| Extension | Serves | Client needs |
| --- | --- | --- |
| `quack` | other DuckDB instances | DuckDB |
| `ui` | a browser | a browser |
| **`harbor`** | **everything else** | **`curl`** |

All three run in one process, over one database file, at the same time:

```sql
LOAD '…/harbor.duckdb_extension';
CALL harbor_serve(bind := '127.0.0.1', port := 9495, token := '…');
LOAD quack;  CALL quack_serve('quack:127.0.0.1:9496', token = '…');
LOAD ui;     SET GLOBAL ui_local_port = 9497; CALL start_ui_server();
CALL harbor_wait();
```

Harbor does not reimplement the other two. `quack` and `ui` are stock,
unmodified, and maintained by someone else. That is the point — and it is most
of why this is 2,027 lines where its C++ predecessor is 9,728 across thirty
files. Nearly the whole reduction came from not doing three things, rather
than from writing denser code.

## The implementation

Rust, against DuckDB's **C extension API**, so no DuckDB source tree is needed
to build — `cargo build` and you have an extension. Two files:

| file | lines | |
| --- | --: | --- |
| [`src/lib.rs`](src/lib.rs) | 1,940 | the extension entry, its four table functions, and the whole HTTP server |
| [`bin/harbor`](bin/harbor) | 87 | run a database as a service |

One source file is a deliberate choice, not laziness. The engine and the four
table functions that front it are useless apart, and the module boundary
between them existed only to be crossed — eight `pub` markers whose entire
purpose was to let the other half call in. Removing it removed them, and
removed the blanket `#![allow(dead_code)]` that boundary had carried, which
turned out to be hiding a request counter nothing ever incremented.

Against **3,613 lines of tests** — roughly 1.8 lines of test per line of
product, and the larger half of the repository.

## Testing

`make check` runs everything; `make check_quick` runs the subset that finishes
in under a minute. [`scripts/fixture.sh`](scripts/fixture.sh) builds the
database they read, either from a real CSV export or synthesised
deterministically, so CI needs no binary fixture in git.

| suite | what it establishes |
|---|---|
| `cargo test` | the single-statement scanner and the BIGNUM decoder, over comments, string literals, `E'…'`, quoted identifiers and dollar-quoting |
| [`scripts/abi.sh`](scripts/abi.sh) | the built artifact's metadata footer, the C API version it requests, and whether a different DuckDB engine loads it |
| [`scripts/type_coverage.py`](scripts/type_coverage.py) | every DuckDB type is exercised by some case, or excused with a reason |
| [`test/sql/harbor.test`](test/sql/harbor.test) | the SQL surface: argument validation, lifecycle ordering, that a stopped server restarts |
| [`scripts/stress.sh`](scripts/stress.sh) | 117 assertions against oracle values read from DuckDB before the server takes the file lock |
| [`scripts/spec_types.py`](scripts/spec_types.py) | 88 assertions of the wire format against the spec as written |
| [`scripts/fuzz.py`](scripts/fuzz.py) | 14,000 random values a run, checked against Python's `datetime` and `base64` |
| [`scripts/differential.py`](scripts/differential.py) | 217 cases sent to both the v1 harbor and this one, classified rather than diffed |
| [`scripts/validate-deployment.sh`](scripts/validate-deployment.sh) | 48 read-only checks against a server already running — safe to point at production |
| [`scripts/swarm.py`](scripts/swarm.py) | concurrent load at rising client counts, every answer verified |
| [`scripts/resilience.sh`](scripts/resilience.sh) | `SIGKILL` and WAL replay, descriptor and memory leaks over a soak, abandoned streams, idle connections, slow readers, restart churn, a locked database |

Three of these are worth explaining.

The **differential** suite treats the v1 harbor as the reference for what
clients already expect, not as the specification. Where the two disagree, the
divergence must be recorded in the runner with a reason before the run goes
green — so "improved" is always a decision someone made, never something that
drifted. It currently records twenty-two, including two cases where v1 is
wrong, both found by this runner.

The **fuzz** and **spec** suites deliberately never compare harbor to harbor.
An oracle that shares an implementation with the thing it is checking confirms
only that the code is self-consistent.

The **type coverage** suite exists because the two worst bugs found so far were
both types no suite covered: `BIGNUM` shipped emitting base64 of DuckDB's
private storage, and `TIME_NS` panicked the executor thread — returning `200`
with an empty body while permanently removing a connection from the pool. It
now works from DuckDB's own type list, so the next added type fails a run
before it reaches anyone's data.

## Known limitations

**`TIME WITH TIME ZONE` loses its offset.** DuckDB's Arrow exporter discards it
before harbor sees the value, keeping the local wall clock, so times at
different offsets become indistinguishable. Harbor marks the column
`"lossless": false` rather than emit a time that silently means something else.
Recover the offset with `date_part('timezone', t)`, or cast to `VARCHAR`.

**`TIME_NS` and `VARIANT` are refused with `400`.** Neither can cross the Arrow
boundary the Rust client uses. Cast to `VARCHAR` and the value comes back
intact.

**One exact DuckDB version per binary.** Harbor is stamped
`C_STRUCT_UNSTABLE` and pinned to **v1.5.5**, because `duckdb-rs` reads every
result through three functions in the unstable band of the C API. That band has
no ordering guarantee across releases. A stable-ABI build of this same binary
does load into DuckDB v2 and serve correctly — [`scripts/abi.sh`](scripts/abi.sh)
asserts it — so the pin lifts the day `duckdb-rs` stops needing the unstable
surface, and that suite fails when it does.

## Status

Early, and not yet published to community-extensions. Build it with
`make release` and `LOAD` the artifact by path. The
[v1 harbor](https://github.com/shreeve/duckdb-harbor-v1) is the C++
implementation this replaces.

## License

MIT.
