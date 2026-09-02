# DuckDB Harbor — the product

Only one process can access a DuckDB file at a time. Harbor fixes that: it
puts a small server in front of the file so all your apps can share it — and
it feels exactly like the duckdb shell, except a database can also serve.

DuckDB Harbor ships one Rust binary with one grammar. This document is the
architecture — what the product is and why it is shaped this way. For usage
see [README.md](README.md).

```
harbor                          → what's running
harbor <db.duckdb>              → open it: REPL / -c "SQL" / stdin;
                                  spawns a refcounted server if none exists
harbor <path/to.sock>           → connect to a server by its socket
harbor http://host:port         → connect to a server over TCP
harbor <db.duckdb> start        → start it yourself, until you leave
```

The doctrine, in one breath: **bare, the server is everyone's — it lives
while anyone is connected; `start`, the server is yours — it lives until you
leave.** Everything below is the machinery that makes those two sentences
true, and nothing else.

The workspace has four first-party crates: **`harbor`** (server engine,
client, and CLI in one binary — the client half lives in `src/repl/` and
never touches DuckDB), **`harbor-common`** (paths, names, permissions,
durations — shared with DuckTable), **`wire`** (the client's protocol types),
and **`justhttp`** (Harbor's synchronous HTTP/1.1 server). The server
implements the same wire shapes directly rather than consuming the `wire`
crate, so protocol changes require tests on both sides.

---

## One binary, engine on demand (~2.2MB until asked to start)

Nothing links `libduckdb`. The generated bindings route every DuckDB C call
through null-initialized function pointers, and `engine/mod.rs` fills them by
`dlopen` — but only on the code paths that open a database. Consequences,
each load-bearing:

- **The client half is pure.** `harbor <sock>` and `harbor http://…` run on
  machines that have no engine at all; a missing library only surfaces when
  this process is asked to *be* the server, and the error names every path it
  searched.
- **The engine swaps by file.** `HARBOR_LIBDUCKDB`, then `../lib` beside the
  binary (the release-archive layout), `~/.local/lib`, and `~/.duckdb/cli/*`
  — DuckDB's own world, disposable and refetchable. Harbor binds the v2 C
  API, so DuckDB 2.0 is the engine floor; one build has served every v2-API
  engine it has met, and the engine pin *is* the dylib.
- **Building needs nothing.** No DuckDB source tree, library, or header —
  the crate ships pregenerated bindings, so `cargo build` works on a bare
  machine and CI needs the engine only to run the suite.

There is no static build: it would cost a 33MB binary and a from-source build
tree for an advantage — needing no dylib — that would also kill the pure
client. Precedent for the shape: `sqlite3` and `duckdb` are one binary that
is both shell and engine; harbor is one binary that is both shell and server.

## One process per database; DuckDB's own lock is the mutex

DuckDB files are single-writer, and the code enforces one-database-per-process
(`open_pool` refuses a second — the pool/lease/cancel machinery is
process-wide statics). That gives crash isolation, per-server engine versions,
and independent drain→CHECKPOINT lifecycles.

There are no lock files. When two servers race for one database — two clients
spawning at once, or an operator typing `start` against a live file — exactly
one gets past `Connection::open`, because DuckDB itself locks the database
file per process. The loser exits before ever touching a socket, and the
winner's socket has a deterministic name, so both racing clients land on the
winner. A mutex the engine already enforces does not need a second
implementation in flock, and the whole revalidate-the-inode protocol that
came with one is gone.

**Mandatory guard**: DuckDB defaults to ~80% RAM / all cores *per instance*.
`start` ships a conservative default (`--memory-limit`, default 2GB;
`--threads`) and prints it at startup. This is a multi-server safety
requirement, not an option.

## No registry — the listening socket is the registration

A server's socket name is **derived, never registered**:
`<basename>-<hash>.sock` in the `0700` runtime dir, where the hash is FNV-1a
over the database's canonical path (symlinks resolved, absolutized; hand-rolled
because the name must be stable across releases, and std's hasher is not).
Every spelling of the same file lands on the same socket; two `data.duckdb`
in different directories never collide; and the basename keeps `ls` readable
while the whole path stays under `sun_path` (the basename yields bytes when
the runtime dir runs deep).

Discovery is therefore `readdir` + `GET /info` per socket — which is exactly
what bare `harbor` prints: database path, pid, live client count, uptime,
address. A socket that refuses the connection is a leftover from a `kill -9`
and is unlinked on sight; any other failure proves nothing and removes
nothing. There are no sidecar JSONs, no token files, no hold files, and no
log directory to sweep, because none of them exist. `/info` itself is the
identity document, with uptime and the client refcount spliced in live.

## The refcounted lifetime (bare) and the owned lifetime (start)

A spawned server's lifetime is its client count, counted where connections
actually live: justhttp increments at accept and decrements — through a
panic-proof drop guard — when the connection's request loop ends. Two
constants, not knobs: a ~30s startup grace (a spawner that dies before its
client connects cannot orphan a server) and a ~3s zero-client linger (curl
bursts and exit/connect races do not flap it). At zero past the window:
drain, `CHECKPOINT`, sweep the socket, exit. The database file stays behind,
checkpointed and self-contained.

The client's half of the contract is the **anchor**: every `harbor <db>`
invocation holds one silent connection for its lifetime, so a human thinking
at a prompt is presence, not absence. No heartbeat, no `/keepalive` route, no
server-side record — the open fd is the whole protocol, and a crashed client
releases it by definition. justhttp deliberately lets an idle connection wait
between requests forever, which is what makes presence expressible at all.
`.open` moors at the new server before releasing the old one.

`start` ignores the refcount entirely. On a terminal it puts the operator at
the helm — the same REPL, dialled at the server's own socket, so what the
operator sees is what any client would get — and leaving the prompt ends the
server. Headless, it runs until `SIGTERM`. Spawn-on-use spawns exactly this
(`current_exe() <db> start` with `HARBOR_EPHEMERAL` in its environment,
detached, stderr to a log beside the socket), so there is one start path
however a server comes to exist.

`curl` works iff something is listening, by design: a bare HTTP client does
not summon a database. An application that wants spawn-on-use runs
`harbor <db> -c "SELECT 1"` once and connects within the startup grace.

## No config file

There is nothing to configure, so there is no file, no schema, no trust gate
on its permissions, no `token-cmd` shelling out, and no "the config could not
be read" failure class. What the old config carried is now either derived
(socket names), spelled at the command line (`start` flags — a systemd unit
is the place a server's desired state gets written down), or deleted
(names-as-services, holds, REPL defaults). A target is a path, a socket, or a
url; a bare word is refused with the law spelled out — a name never contains
a dot or a slash, so an argument carrying one can only be a path. That
classifier is also the safety rule: a typo can never silently become a fresh
empty database served under a name clients trust.

(One nuance for completeness: `harbor-common` still ships a config/fleet
*reader* behind an off-by-default cargo feature, consumed only by DuckTable's
sidebar. Harbor itself never reads or writes any config file.)

## The token law

The unix socket needs no token, and refuses one: the `0700` runtime dir is
the access control, and a second lock on a door only you can reach teaches
people it does something. TCP leaves the filesystem's protection, so `--port`
makes `--token` mandatory — there is no unauthenticated TCP face and no
minted-token machinery, because the operator who opens the one door that
needs a credential supplies it in the same breath. `/ready` is the one
unauthenticated route (a load balancer should not need a credential to learn
up-or-down); on a token'd server everything else, `/info` included, requires
the bearer.

## Caddy is the optional edge; UDS is the default face

Local security = filesystem perms. Remote = Caddy terminates TLS/HTTP3 and
edge auth, proxies to the socket. The client deliberately speaks UDS and
plain `http://` only. A human reaches a remote server through SSH; browser
and application clients go through Caddy.

## Protocol changes are deliberate, not promised away

Pre-1.0, the wire contract improves when the truth improves — compatibility
is a cost we weigh, not a vow. The record so far: 0.20 and 0.21 left the
protocol untouched (DuckTable and the Rip harbor adapter ran unmodified;
`/keepalive`, an additive 0.19 route, left with the idle-exit machinery it
served). 0.22 made the first breaking change, to stop dropping data: TIMETZ
values gained their UTC offset, nested unions gained their tags, and the
`time-offset-dropped`/`union-tag-dropped` schema encodings — which existed
only to confess those losses — were removed. Clients pinned to the old
shapes notice; clients that read the schema's `lossless` flag get strictly
better data. The core routes, events, and error codes are as they were. No
`database` field on `/sql`: a server holds one database; per-session work
is `USE`/ATTACH as plain SQL on a lease.

## The engine is upstream DuckDB 2.0 — no fork

The engine is DuckDB's own `main` line, unmodified: `libduckdb` plus the
`duckdb` CLI. Nothing is forked and nothing is patched — but until 2.0 GA,
one honest wrinkle: DuckDB's published artifact channel
(`artifacts.duckdb.org/latest/…`) is frozen at a build that predates the v2
C API landing upstream, so the library it delivers cannot serve harbor. A
serving engine is therefore *built* from upstream source at one pinned
commit — CI does this (cached; `.github/actions/duckdb` holds the pin and
the recipe), local development does the same, and `make fetch-duckdb` warns
when the artifact it fetched can't serve. At GA this section loses the
wrinkle and everything returns to the published zip. Verified: one harbor
build loads and runs clean against every v2-API engine it has met — tested
compatibility for Harbor's exercised C-API surface, not a claim about every
future ABI. The floor is DuckDB 2.0 by construction: harbor binds the v2 C
API, whose symbols older engines do not export. Database files are a
different matter — a file created by a 1.5-era DuckDB opens as-is, because
2.0's storage layer reads it. (The v2 line parses SQL ~2× slower than 1.5.5
— the new PEG parser; execution is at parity. See the README's Performance
section.)

**Planned for the GA timeframe** — collected here so GA day has one list:
unwind the frozen-channel scaffolding (the shelf derivation in Release.yml,
the Engine workflow and its `engine-<pin>` shelf, this section's wrinkle, and
`fetch-duckdb`'s warning — each is marked at its site), taking care to pin
the release fetch to a *versioned* GA artifact URL rather than the moving
`/latest` channel, so tested-equals-shipped survives the unwind; and revisit
the
prepared-statement cache size. Harbor keeps a small per-connection LRU of
parsed statements, which is what makes repeated statements skip the 2× parse
cost entirely — whether that cache should grow is a tuning question worth
answering against the GA engine's parser, not the alpha's, since upstream is
still optimizing it. Re-run the Linux benchmarks on quiet hardware at the
same time.

**Harbor ships no extension.** A release archive carries harbor and the exact
libduckdb it was tested against — nothing else. The extension door is the
operator's: the loaded engine exports the full C++ ABI, so an extension built
against the *same* engine loads from it —
`harbor db.duckdb start --unsigned --init 'LOAD <ext>'`. Matching extension
to engine is the caller's responsibility, because the C++ ABI admits no other
answer.

## The HTTP layer is first-party: justhttp

Harbor owns its HTTP layer as the first-party workspace crate
`crates/justhttp`, consumed through a plain path dependency. It is not
vendored tiny_http, has no tiny_http dependency, and does not track an
upstream HTTP crate. Lineage is retained only for licensing: justhttp began
from the relevant synchronous HTTP/1.1 core of tiny_http 0.12.0 and is
maintained as Harbor's own crate.

The surface matches Harbor's needs — synchronous workers, Unix sockets, TCP,
streaming from `Read`, a tiny dependency tree, and the connection counter the
refcounted lifetime rides on (incremented at accept, decremented by drop
guard). TLS, WebSocket upgrades, `TestRequest`, unused response APIs, the
notify/secure rendezvous, and `log` are absent. The crate is roughly 2,700
lines in seven one-word files (lib, http, stream, conn, request, response,
pool), edition 2024, `forbid(unsafe_code)`, with three small dependencies.

Two Harbor hardening behaviors are part of justhttp itself: unread request
bodies drain through a fixed 64 KiB buffer, and every accepted socket has a
10-second response write timeout — while the read side deliberately lets an
idle connection wait forever between requests, which is what the anchor
depends on. Regression tests pin the drain and stall behaviors; the suite
also covers keep-alive reuse, chunked streaming, response ordering,
half-close semantics, and byte-at-a-time CRLF framing. The wire-visible
`Server:` header identifies `justhttp`.
