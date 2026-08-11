# harbor

**HTTP `/sql` for DuckDB — SQL in, NDJSON out, no driver**

harbor is a DuckDB extension that turns a running DuckDB process into an HTTP
endpoint. `POST` a statement, read newline-delimited JSON back. No client
library, no wire protocol to implement, no DuckDB embedded in the caller —
if it can speak HTTP and parse JSON, it can query your database.

```console
$ curl -s localhost:9495/sql -H 'Authorization: Bearer …' \
       -d '{"sql":"SELECT * FROM orders LIMIT 2"}'
{"type":"schema","columns":[{"name":"id","duckdbType":"BIGINT","lossless":true},
                            {"name":"total","duckdbType":"DECIMAL(10,2)","lossless":true,
                             "decimal":{"width":10,"scale":2}}]}
{"type":"row","values":[1,"19.99"]}
{"type":"row","values":[2,"4.50"]}
{"type":"end","rowCount":2,"timeMs":3}
```

## Where it fits

DuckDB's ecosystem already covers two audiences. harbor covers the third.

| Extension | Serves | Client needs |
| --- | --- | --- |
| `quack` | other DuckDB instances | DuckDB |
| `ui` | a browser | a browser |
| **`harbor`** | **everything else** | **`curl`** |

All three run in one process, over one database file, at the same time:

```sql
LOAD harbor; CALL harbor_serve(bind := '127.0.0.1', port := 9495, token := '…');
LOAD quack;  CALL quack_serve('quack:127.0.0.1:9496', token = '…');
LOAD ui;     SET GLOBAL ui_local_port = 9497; CALL start_ui_server();
CALL harbor_wait();
```

harbor does not reimplement the other two. `quack` and `ui` are stock,
unmodified, installed from DuckDB's own extension repository, and maintained
by someone else. That is the point.

## HTTP, not HTTPS

harbor speaks plain HTTP and binds loopback by default. This is a design
decision, not a missing feature.

TLS belongs at the edge, where certificates, renewal, and HTTP/2 and /3 are
already solved by software that does nothing else. Put Caddy or nginx in
front and terminate there. Keeping TLS out of the extension means no OpenSSL
link, no certificate handling in-process, and far less code to get wrong.

## The envelope

Three properties are load-bearing:

**It streams.** Rows are written as chunks arrive. A result larger than
memory is not a problem, because no result is ever fully materialised.

**Types survive.** Every column carries its `duckdbType`, plus width and
scale for `DECIMAL` and nested `child`/`fields` for `LIST`/`STRUCT`. A client
can reconstruct exactly what DuckDB had, not a lossy JSON approximation.

**Precision is not silently lost.** Values JSON cannot hold exactly are
quoted rather than emitted as bare numbers. `HUGEINT` and large `BIGINT` go
out as strings — a bare `123456789012345678901234567890` becomes
`1.2345678901234568e+29` the moment any JavaScript client parses it.

## One statement per request

`POST /sql` takes exactly one statement. Two reasons:

Concatenating user input into SQL is a mistake everyone eventually makes;
single-statement parsing makes `; DROP TABLE …` structurally impossible
rather than merely discouraged. And when a request maps to one statement, the
HTTP status code means something — no partial success to report.

Use `params` for values, and open a session when you need `BEGIN`/`COMMIT`
across requests.

## Concurrency

harbor accepts many concurrent connections and executes a small, bounded
number of statements.

That asymmetry is deliberate. DuckDB parallelises a *single* query across
every core, so running hundreds at once produces thrashing and memory
pressure, not throughput. Statements past the limit queue; past the queue
bound they get `503`. Set `threads` and `memory_limit` per process,
especially when several databases share a machine.

## Known limitations

**`TIME WITH TIME ZONE` loses its offset.** `duckdb-rs` decodes the value to a
wall-clock time before harbor sees it, discarding the UTC offset; there is no
way to recover it at this layer. Rather than emit a time that silently means
something else, harbor marks the column `lossless: false` with
`encoding: "time-offset-dropped"`, so a client can detect the loss instead of
trusting a wrong answer. The v1 harbor preserves the offset. Cast to
`TIMESTAMP WITH TIME ZONE` if you need it.

## Testing

`make check` runs everything; `make check_quick` runs the subset that finishes
in under a minute. `scripts/fixture.sh` builds the database they read, either
from a real CSV export or synthesised deterministically.

| suite | what it establishes |
|---|---|
| `cargo test` | the single-statement scanner, over comments, string literals, `E'…'`, quoted identifiers and dollar-quoting |
| `test/sql/harbor.test` | the SQL surface: argument validation, lifecycle ordering, that a stopped server restarts, that the database stays usable while served |
| `scripts/stress.sh` | 111 end-to-end assertions against oracle values read from DuckDB before the server takes the file lock |
| `scripts/spec_types.py` | 80 assertions of the wire format against SPEC §5.4 as written, not against either implementation |
| `scripts/fuzz.py` | 14,000 random values per run, checked against Python's `datetime`/`base64` rather than against harbor |
| `scripts/differential.py` | 204 cases sent to both the v1 harbor and this one, classified `same` / deliberate improvement / unexplained |
| `scripts/resilience.sh` | SIGKILL and WAL replay, descriptor and memory leaks over a soak, abandoned streams, idle connections, slow readers, restart churn |
| `scripts/validate-deployment.sh` | read-only, against a server that is already running — including production |
| `scripts/swarm.py` | concurrent load at rising client counts, mixed reads and writes |

Two of these are worth explaining.

The **differential** suite treats the v1 harbor as the reference for what
clients already expect, not as the specification. Where the two disagree, the
divergence has to be recorded in the runner with a reason before the run goes
green — so "improved" is a decision someone made, never something that drifted.
It currently records four: the `TIME WITH TIME ZONE` limitation above, and
three cases where the v1 harbor is wrong (it answers `500` to a non-ASCII
string literal and `400` to a non-ASCII bound parameter; both were found by
this runner).

The **fuzz** and **spec** suites deliberately never compare harbor to harbor.
An oracle that shares an implementation with the thing it is checking confirms
only that the code is self-consistent.

## Status

**Early.** The Rust rewrite of a working C++ extension
([`duckdb-harbor-v1`](https://github.com/shreeve/duckdb-harbor-v1), v0.5.4), which
remains the version to use today. This repository starts at **0.7.0** to make
the lineage obvious.

## Versioning

Built against the DuckDB **C extension API**, so no DuckDB source tree is
needed to build — `cargo build` and you have an extension.

One caveat worth stating plainly: `duckdb-rs` currently requires the
**unstable** band of that API (`USE_UNSTABLE_C_API=1` in the `Makefile`),
which pins a binary to one exact DuckDB version — `v1.5.5` today. Forward
compatibility across DuckDB releases is *not* available yet, and will not be
until `duckdb-rs` targets the stable band.

In practice this costs much less than it did in C++: a rebuild is `cargo
build --release` in about twenty seconds against no DuckDB checkout, rather
than recompiling a 114 MB engine.

## Why 1.5.5

`ui` is published for `v1.5.5` and is not published for `v2.0.0-alpha`. Until
that changes, 1.5.5 is the version where all three extensions are available
as stock downloads.

The tradeoff: on 1.5.5 a `quack` attach is read-mostly — `SELECT`, `INSERT`,
and DDL work, `UPDATE` and `DELETE` do not. Writes go through `/sql`, which
works the same on either engine.

## License

MIT
