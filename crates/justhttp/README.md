# justhttp

**Just HTTP.** A small, synchronous HTTP/1.1 server library in Rust —
TCP and unix sockets, streaming responses, keep-alive, pipelining — and
nothing else. No TLS, no websockets, no HTTP/2, no plugins, no feature
flags. The edge proxy owns encryption; this crate owns fast, correct
HTTP over a socket.

justhttp is the HTTP layer under [`harbor`](../../README.md), a workspace
crate maintained as first-party code — permanently: there is no upstream
to track. It began as a TLS-stripped
[tiny_http 0.12.0](https://github.com/tiny-http/tiny-http) and is kept
deliberately smaller than its ancestor (see **Lineage** below).

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

Two behaviors here are deliberate local hardening, born in production:

1. **Bounded body drain.** Discarding an unread request body streams
   through a fixed 64 KiB buffer — never an allocation sized by the
   client's declared `Content-Length`. (Upstream allocated the declared
   size, an unauthenticated memory-exhaustion DoS: a request declaring
   `Content-Length: 1000000000` while sending three bytes cost the
   process a gigabyte at drop time.)
2. **Response write timeout.** Every accepted socket gets a 10 s write
   timeout, so a client that stops reading its response cannot pin a
   worker thread inside `write` forever. The read side is untouched —
   keep-alive connections still wait indefinitely between requests.

## Layout

Seven one-word files, ~2,650 lines, edition 2024, `forbid(unsafe_code)`,
three tiny dependencies (`ascii`, `chunked_transfer`, `httpdate`):

| File | Owns |
|---|---|
| `lib.rs` | `Server`, the accept loop (write-timeout lives here), `recv`/`recv_timeout`/`unblock` |
| `http.rs` | `Method`, `StatusCode`, `Header`, `HttpVersion` (strict, smuggling-hardened parsing) |
| `stream.rs` | TCP/UDS listeners and the half-close connection stream |
| `conn.rs` | per-connection request sequencing: keep-alive, pipelining, response ordering |
| `request.rs` | `Request`, lazy body readers (the bounded drain lives here) |
| `response.rs` | `Response`, transfer-encoding choice, chunked/identity framing |
| `pool.rs` | the accept-side task pool and the message queue behind `recv()` |

## Tests

`cargo test -p justhttp` (from the repo root) runs the whole suite — green
on macOS and Linux. Two files:
`tests/suite.rs`, one module per property (`basic`, `input`, `network`,
`keepalive` — connection reuse + chunked streaming, `buffering` —
backpressure, `prompt` — latency properties, `unblock`, `unix`, and
`stall` — the write-timeout test, `#[ignore]`d because it takes ~35 s by
design: `cargo test -p justhttp --test suite -- --ignored`); and
`tests/drain.rs`,
the DoS regression, alone in its own binary because its measuring global
allocator must not see other tests' allocations.

## Lineage and license

Derived from tiny_http 0.12.0 (MIT OR Apache-2.0), used and relicensed
here under its MIT option. Removed from the ancestor: TLS (features,
`src/ssl/`, `Server::https`, and the optional openssl/rustls/zeroize
dependencies), the websocket upgrade path and `ReadWrite` trait, test
scaffolding (`TestRequest`), the HTTPS notify/secure rendezvous, unused
response constructors and getters, the `log` dependency, examples,
benches, and upstream CI — roughly a quarter of the crate. The delicate
cores (response ordering, half-close, keep-alive state machine,
byte-at-a-time CRLF framing) moved verbatim. See [LICENSE](LICENSE) for
the combined attribution, PLAN.md D12 for the decision record, and the
**justhttp** section of [TODO.md](../../TODO.md) for the maintenance
ledger.
