# Harbor vs. Quack

> **This document is evidence, not positioning.** Every protocol claim below was
> verified against a running Quack server with a man-in-the-middle proxy on the
> wire, and every performance number was measured, not estimated. Versions,
> method and reproduction steps are in [Appendix: how this was tested](#appendix-how-this-was-tested).
> Where a claim rests on reading source rather than observing bytes, it says so.

The one-line distinction:

> **Quack connects databases. Harbor connects applications.**

Quack is a well-engineered protocol and it is faster than Harbor at moving bulk
result sets. Neither of those facts is in dispute here, and this document does
not argue otherwise. What it argues is that the two systems answer different
questions, and that the axis Harbor is usually defended on — text versus
binary — is the wrong one.

## What Quack actually is, on the wire

Quack is a **stateful session protocol that uses HTTP as a transport**. That is
a sharper and more useful description than "a binary protocol," and it is the
root of every practical difference below.

Observed, not inferred:

* One endpoint, `POST /quack`, default port 9494.
* Requests are `Content-Type: application/octet-stream`. Responses are
  `Content-Type: application/vnd.duckdb`. The asymmetry is real; DuckDB's own
  documentation says `application/duckdb`, which matches neither.
* Bodies are DuckDB's internal `BinarySerializer` format — field-id tagged,
  the same machinery used for its on-disk structures.
* Every message carries `MessageHeader { type, connection_id, client_query_id }`.
* `Access-Control-Allow-Origin: *`, with a source comment conceding this is
  "very liberal."

The message set is small and explicitly connection-oriented
([`src/include/quack_message.hpp`](https://github.com/duckdb/duckdb-quack/blob/main/src/include/quack_message.hpp),
[`quack_message.json`](https://github.com/duckdb/duckdb-quack/blob/main/src/include/quack_message.json)):
`CONNECTION_REQUEST/RESPONSE`, `PREPARE_REQUEST/RESPONSE`,
`FETCH_REQUEST/RESPONSE`, `SEND_DATA_REQUEST/RESPONSE`, `CANCEL_REQUEST`,
`HEARTBEAT_REQUEST`, `ACKNOWLEDGEMENT`, `DISCONNECT`, `SUCCESS_RESPONSE`,
`ERROR_RESPONSE`. Tags 5, 6 and 13 are already retired — the protocol has
history.

It is more sophisticated than a request/response wrapper:

* **Single round trip for small results.** `PREPARE_RESPONSE` carries the first
  chunks inline via an `inline_rows` hint, so a small query needs no `FETCH`.
* **Windowed fetch.** `FETCH_REQUEST { uuid, batch_index, ack_index }` is a
  sliding window, not a naive pull loop.
* **Credit-based ingest.** `SEND_DATA_RESPONSE` returns an `accept_budget` the
  client must respect — real backpressure on bulk insert.
* **Client-generated query ids.** `PREPARE_REQUEST` carries a `hugeint_t`
  `query_uuid`, so `CANCEL_REQUEST` needs no prior server round trip.
* **Lease-based liveness.** The client proposes `heartbeat_timeout_seconds`;
  expiry actively aborts running statements.

Credit where due: that is a careful design.

## The three findings that matter

These are the differences that will not be closed by a patch release, because
they follow from the protocol's shape rather than its maturity.

### 1. Authorization is invisible to the edge

The token travels as `auth_string` **inside the body** of `CONNECTION_REQUEST`.
The server evaluates it as SQL through a pluggable setting —
`SELECT <quack_authentication_function>(session_id, auth_string, token)`,
default `quack_check_token`
([`src/quack_server.cpp:579`](https://github.com/duckdb/duckdb-quack/blob/main/src/quack_server.cpp)).
That extensibility is a genuine feature: per-user auth in SQL is a nice idea.

But the observed consequence is severe. **One client query produced five
`POST /quack` requests, and only the first carried the token.** The other four
were authenticated solely by an opaque `connection_id`
(`BAA0972AC8F986746A3E24913096E7AF`) buried in a binary body.

To a reverse proxy, load balancer, API gateway or WAF, those five requests are
indistinguishable, and four of them contain no credential at all. Authorization
state lives in server memory. Caddy cannot gate this. Harbor's
`Authorization: Bearer` header is checked by anything that speaks HTTP.

### 2. Failures return HTTP 200

Verified against a live server with a deliberately wrong token:

```
HTTP/1.1 200 OK
Content-Type: application/vnd.duckdb
Access-Control-Allow-Origin: *
```

…with `Authentication failed` as a bare string in the body. `ErrorResponse`
carries a single `string` from `error.RawMessage()` — no code, no class, no
retryable flag — and the POST handler never sets a status
([`src/quack_http_server.cpp:225`](https://github.com/duckdb/duckdb-quack/blob/main/src/quack_http_server.cpp)).

Harbor, same query, same minute: `HTTP/1.1 401 Unauthorized`.

This is not a tidiness complaint. Nothing above the codec can tell success from
failure: not a proxy, not a metrics sidecar, not `fail2ban`. **A brute-force
attempt against a Quack server appears in access logs as a run of successful
requests.**

### 3. Nobody owns the file

`quack_serve()` is a function called inside an already-running DuckDB. The
protocol says nothing about which process owns the database file, who starts it,
what happens when it dies, or who holds DuckDB's single-writer lock.

That gap is the whole of Harbor. It showed up unprompted during testing: a
second writer was refused while Harbor held the file —

```
IO Error: Could not set lock on file "bench.duckdb":
Conflicting lock is held in /Users/shreeve/.local/bin/harbor (PID 28438)
```

— which is the product stated in one error message.

## Measured performance

1,000,000 rows × 3 columns (`BIGINT`, `DECIMAL`, `VARCHAR`), same data, same
machine, localhost, warm:

| | Quack | Harbor NDJSON | Harbor + zstd-1 |
| --- | ---: | ---: | ---: |
| Wire bytes | 34,935,381 | 57,037,316 | **2,698,430** |
| Relative to Quack | 1.00× | 1.63× | **0.077× (13× smaller)** |
| Wall time (best of 3) | 0.09 s | 0.21 s | not measured |

Three honest readings of that table:

**NDJSON costs only 1.63× the bytes of DuckDB's native binary format** —
not the 3–5× that "text is bloated" intuition suggests. Harbor sends the schema
once and then positional rows (`{"type":"row","values":[...]}`) with no repeated
keys, which removes most of JSON's structural overhead.

**Quack was roughly 2× faster against the encoder measured here, and the true
gap was somewhat wider than shown.** The comparison favors Harbor: `curl` wrote
bytes to a file without parsing JSON, while the DuckDB client materialized a
full temp table and paid process startup. That was Harbor within 2–3× of a
native binary protocol, before any encoder optimization.

**Compression inverts the bandwidth argument.** Quack ships uncompressed. With
`Content-Encoding: zstd` at level 1, Harbor moves **13× less data than Quack**.
Stated with the caveat it deserves: this synthetic data (sequential integers,
`row-N` labels) is unusually compressible, and 3–6× is the realistic range on
production data — which still puts NDJSON ahead of uncompressed binary on any
real network.

That headroom has since been spent. The encoder now writes digits directly into
the output buffer with no per-cell allocation, and fetch and encode run
pipelined on separate threads. Re-measured on the same machine and shape
(current tree, DuckDB `v2.0.0-dev83323`): the million rows arrive in
**~0.06 s** wall, best of three — a 3.5× improvement that lands at Quack's
recorded figure, with the caveat that the two client paths still differ as
described in the appendix. The `not measured` cell is also settled: with
`Content-Encoding: zstd` the wall time is indistinguishable from identity,
because compression rides the writer thread while the engine produces the next
chunk. The wall-time gap in the table was the old encoder, not the protocol.

## The real comparison

| Area | Harbor | Quack + `CONNECT` |
| --- | --- | --- |
| Protocol shape | Stateless request/response; sessions opt-in | Stateful connection with heartbeat lease |
| Primary client | Any HTTP-capable program | Another DuckDB |
| Payload | JSON / NDJSON | `application/vnd.duckdb` binary |
| Client dependency | HTTP + JSON parser | DuckDB, or a Quack implementation |
| Auth visible to proxy | Yes — `Authorization: Bearer` | No — in-body, first request only |
| Failure visible to proxy | Yes — real status codes | No — HTTP 200 |
| Browser use | Natural with `fetch()` | Wasm or a hand-written JS codec |
| Shell use | `curl` works | DuckDB client required |
| Bulk transfer | 1.63× bytes; 13× smaller compressed | Fastest uncompressed |
| Remote catalog | SQL endpoint and `/catalog` | Native `ATTACH`/`CONNECT` with pushdown |
| Type fidelity | JSON with explicit type metadata | Native, fully lossless |
| Process ownership | The entire point | Out of scope |
| Human client | the same binary — `harbor <db>` | DuckDB CLI |
| Desktop UI | DuckTable | Not the proposition |

## Where Quack is better

This section is not a courtesy. A pitch that concedes nothing invites a reader
to disprove one claim and discard the rest.

**Native catalog federation.** `ATTACH 'quack:host'` then `CONNECT` makes a
remote database behave like DuckDB inside DuckDB, and DuckDB 2.0 routes whole
queries to it through a remote pushdown optimizer. For DuckDB-to-DuckDB
federation and analytical composition this is excellent, and Harbor offers
remote SQL execution rather than transparent federation. Harbor should not try
to imitate this.

**Bulk binary transport.** For very large typed results moving between DuckDB
instances, native chunk transfer skips text encoding and parsing entirely, and
that CPU advantage compounds at 100M-row scale. Harbor's encoder rewrite closed
the measured wall-time gap at 1M rows, but the client must still parse JSON that
a native client never produces — compression changed the bandwidth story, not
the compute one.

**Full type fidelity by construction.** Harbor spends real effort on lossless
JSON encoding for decimals, nested types and oversized integers. Quack gets it
for free.

**First-party momentum.** Quack graduates to stable in DuckDB 2.0. `CONNECT`
and the remote optimizer will receive upstream investment that Harbor will not.
This is the item to take most seriously — not because the protocol competes with
Harbor, but because a first-party server with a lifecycle story eventually
might.

Two caveats in Quack's favor, stated plainly: DuckDB documents Quack as **beta**
and expects breaking changes, and the shipped build negotiates **protocol
version 1** while the source tree is already at `QUACK_VERSION = 3`. The
HTTP-200-on-error behavior in particular could be fixed at any time. The
in-body auth and the absent lifecycle are architectural and will not be.

## What Harbor is actually for

The application does not need to know which DuckDB client library exists for its
language, whether that library matches the server version, how to distribute
`libduckdb`, how DuckDB serializes vectors, or how to implement Quack. It sends
SQL and reads JSON.

Underneath that, Harbor is a supervisor, and that is the part nobody else ships:
bounded query concurrency; streamed results without materializing; prepared
statement caching; pinned transactional sessions with separate worker and
session capacity; client-named cancellation; statement deadlines;
abandoned-session reclamation; readiness that tests the database rather than the
process; graceful drain, checkpoint and shutdown; per-database process
isolation; Unix sockets locally and Caddy at the edge; sealed mode, memory and
spill limits; spawn-on-use with a refcounted lifetime; and server discovery
with no supervising daemon and no registry — the listening socket is the
registration.

The client protocol stays small and versioned —
[`crates/wire/src/lib.rs`](crates/wire/src/lib.rs) defines a handful of request
types and four streaming events: `schema`, `row`, `end`, `error`.

The same binary completes it for humans: `harbor <db>` is a DuckDB-style REPL
with highlighting and completion, join-or-spawn on a database path, no engine
loaded in the client half, and real Ctrl-C cancellation translated into
`DELETE /sql/queries/<id>`. DuckTable is the native desktop face on the same
servers.

## Conclusion

Quack is not a weaker Harbor and Harbor is not a weaker Quack. Quack turns
DuckDB into a database other DuckDBs can use. Harbor turns a DuckDB file into a
service that anything can use, and takes responsibility for the process that
owns it.

The strongest form of the argument is not "JSON versus binary" — that is
Quack's ground and it wins there on CPU. It is:

> Quack is a session protocol wearing HTTP. Harbor is an HTTP service.
>
> Everything follows from that: whether your proxy can authorize a request,
> whether your monitoring can see a failure, whether a browser can talk to it,
> and whether anything owns the file when the client goes away.

Or, in the README's own words: **many clients, one DuckDB, over plain HTTP.**

DuckDB declaring this the year of DuckDB-as-a-server does not obsolete Harbor.
It vastly increases the number of people who will understand why it exists.

---

## Appendix: how this was tested

**Date:** 2026-09-01. **Platform:** macOS, arm64.

| Component | Version |
| --- | --- |
| DuckDB CLI | `v2.0.0-alpha38195` (Cyanoptera, `8cbdaba6ac`) |
| `quack` extension | `b2f2d10`, installed from `core` |
| Harbor / Pilot | `0.19.2` |

**Method.** A Quack server was started with
`CALL quack_serve('quack:localhost:9494', token => '…')`. A Python TCP proxy sat
between client and server, logging raw bytes in both directions; the DuckDB
client connected through it with `ATTACH 'quack:127.0.0.1:9493'`. All headers,
status codes, request counts and token positions quoted above are from those
captures. Harbor was run as `harbor serve bench.duckdb --port 9495 --token …`
and driven with `curl`.

**Wire bytes** were counted by the proxy for Quack (server→client, 34,935,381)
and as the response body size for Harbor (57,037,316). Compressed sizes are
`zstd -1` (2,698,430) and `gzip -1` (8,588,385) over Harbor's exact response
bytes.

**Timings** are best-of-three wall clock. They are *not* strictly comparable:
Harbor's figure is `curl` writing the response to a file with no JSON parsing,
while Quack's is a DuckDB client materializing the result into a temp table,
including process startup. The comparison therefore understates Quack's CPU
advantage, and is reported that way deliberately.

**Reproducing it** requires only the versions above; no code in this repository
was modified for these measurements.
