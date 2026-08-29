# justhttp

**Just HTTP.** A small, synchronous HTTP/1.1 server library in Rust —
TCP and unix sockets, streaming responses, keep-alive, pipelining — and
nothing else. No TLS, no websockets, no HTTP/2, no plugins, no feature
flags. The edge proxy owns encryption; this crate owns fast, correct
HTTP over a socket.

justhttp is the HTTP layer under [`harbor`](../../README.md), created and
maintained here as a first-party workspace crate. Harbor depends directly on
this crate by path; justhttp is not vendored and has no upstream HTTP crate to
track. Its historical ancestry is recorded only for attribution in **Lineage
and license** below.

## What it does

- `Server::http("127.0.0.1:9495")` / `Server::http_unix(path)` — one
  listener, TCP or UDS
- Blocking `recv()` (plus `try_recv()` / `incoming_requests()`): each
  `Request` carries method, url, headers, and a lazy body reader
- `request.respond(response)` — responses stream from any `Read` impl;
  unknown-length responses are framed chunked, so a client reads row one
  while the server produces row N
- Keep-alive and pipelining handled internally: requests from one
  connection are answered in order, connections are reused, and a
  half-closed socket is shut down cleanly

## What it refuses to do

TLS (`https` is the edge proxy's job), `Connection: upgrade` /
websockets, HTTP/2 and /3, routing, cookies, multipart, compression
negotiation. Every one of these is either another program's job or a
layer above.

## Hardening carried in the source

Eight behaviors here are deliberate local hardening, born in production.
Every one of them has a regression test; a failure in `tests/drain.rs` or
the `head`, `first_request` and `stall` modules of `tests/suite.rs` is a
security regression, not a flake.

1. **Bounded body drain.** Discarding an unread request body streams
   through a fixed 64 KiB buffer — never an allocation sized by the
   client's declared `Content-Length`. (Upstream allocated the declared
   size, an unauthenticated memory-exhaustion DoS: a request declaring
   `Content-Length: 1000000000` while sending three bytes cost the
   process a gigabyte at drop time.)
2. **Response write timeout.** Every accepted socket gets a 10 s write
   timeout, so a client that stops reading its response cannot pin a
   worker thread inside `write` forever.
3. **Bounded request head.** A request line or header line is capped at
   8 KiB and a request at 128 headers; over either is `431` and a close.
   The line buffer grew a byte at a time with no ceiling, so one socket
   sending `X-Junk: ` and never stopping took RSS from 30 MB to 1.5 GB in
   under five seconds — before routing, and so before any credential was
   asked for.
4. **Read timeout.** Every accepted socket gets a 5 s read timeout, and
   the whole request head gets 10 s from its first byte. Before the first
   byte the connection is only idle, so it waits on the connection clocks
   below; after it, a client cannot hold a serving thread at one byte per
   minute. This is also what bounds the drain in *time* rather than only
   in memory.
5. **Unambiguous framing.** Two `Content-Length` headers that disagree, a
   `Content-Length` beside a `Transfer-Encoding`, or a `Content-Length`
   that is not a number are all `400` and a close. Each is a
   request-smuggling primitive: whenever this server and a proxy in front
   of it can resolve a request differently, they eventually will.
6. **`TE` does not choose the server's buffering.** An unknown-length
   response is always chunked on HTTP/1.1, whatever the client's `TE`
   says. Identity framing has to read the whole body to discover its
   length, so honoring `TE: identity` handed control of this process's
   memory to the caller — measured at +316 MB of RSS from adding one
   header to one query. (RFC 7230 dropped `identity` from `TE` anyway.)
   HTTP/1.0, which has no chunked encoding, streams an unknown length to
   the connection close instead of buffering it, and `conn.rs` closes
   after every 1.0 request so that delimiter always arrives.
7. **Connection idle clocks.** A connection that has never sent a byte is
   closed after 60 s. Once it has served one request it is a keep-alive
   client and gets 5 minutes between requests — far longer than any pooled
   client's own idle timeout, so a REPL at its prompt or a pool between
   queries is untouched. Neither clock existed: an anonymous caller could
   hold sockets, and a thread apiece, for as long as it liked, and one
   unauthenticated `/ready` was enough to buy that right permanently
   (measured: 120 connections holding 120 threads and 240 descriptors,
   still answering after 100 s idle).
8. **Transient accept failures are retried.** `ECONNABORTED` (the peer
   reset before `accept` took it) and descriptor or buffer exhaustion
   (`EMFILE`/`ENFILE`/`ENOBUFS`/`ENOMEM`) leave the listener perfectly
   usable, so the accept loop backs off and tries again. Leaving the loop
   closes the listening socket for the life of the process; the server
   then sits there alive, accepting nothing. Only a failure meaning the
   listener is *gone* ends the loop, and that one is surfaced through
   `recv()` so the host can say so — as does a transient failure that has
   persisted for a full minute, which is no longer a storm.

## Layout

Seven one-word files, ~2,650 lines, edition 2024, `forbid(unsafe_code)`,
three tiny dependencies (`ascii`, `chunked_transfer`, `httpdate`):

| File | Owns |
|---|---|
| `lib.rs` | `Server`, the accept loop (socket timeouts and accept retry live here), `recv`/`recv_timeout`/`unblock` |
| `http.rs` | `Method`, `StatusCode`, `Header`, `HttpVersion` (strict, smuggling-hardened parsing) |
| `stream.rs` | TCP/UDS listeners and the half-close connection stream |
| `conn.rs` | per-connection request sequencing: keep-alive, pipelining, response ordering, request-head bounds and framing checks |
| `request.rs` | `Request`, lazy body readers (the bounded drain lives here) |
| `response.rs` | `Response`, transfer-encoding choice, chunked/identity framing |
| `pool.rs` | the accept-side task pool and the message queue behind `recv()` |

## Tests

`cargo test -p justhttp` (from the repo root) runs the default suite — green on
macOS and Linux. Two files:
`tests/suite.rs`, one module per property (`basic`, `input`, `network`,
`keepalive` — connection reuse + chunked streaming, `buffering` —
backpressure, `prompt` — latency properties, `unblock`, `unix`, and
`stall` — the write-timeout test, `#[ignore]`d because it takes ~35 s by
design: `cargo test -p justhttp --test suite -- --ignored`); and
`tests/drain.rs`,
the DoS regression, alone in its own binary because its measuring global
allocator must not see other tests' allocations.

## Lineage and license

justhttp was originally derived from the synchronous HTTP/1.1 core of
tiny_http 0.12.0 (MIT OR Apache-2.0) and relicensed here under its MIT option.
That is historical lineage, not a current dependency, vendoring relationship,
or upstream synchronization policy. Harbor's crate removed the unrelated
surface and has since evolved under first-party ownership while retaining the
delicate HTTP semantics that its tests pin. See [LICENSE](LICENSE) for combined
attribution, PLAN.md D12 for the decision record, and the **justhttp** section
of [TODO.md](../../TODO.md) for the maintenance ledger.
