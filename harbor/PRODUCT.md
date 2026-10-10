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
harbor <db.duckdb> start        → bring it up in the background, until you stop it
```

The doctrine, in one breath: **bare, the server is everyone's — it lives
while anyone is connected; `start`, the server is yours — it lives until you
stop it.** Everything below is the machinery that makes those two sentences
true, and nothing else.

The workspace has five first-party crates: **`harbor`** (server engine,
client, and CLI in one binary — the client half lives in `src/repl/` and
never touches DuckDB), **`harbor-common`** (paths, names, permissions,
durations — shared with DuckTable), **`wire`** (the client's protocol types),
**`harbor-http`** (the client's transport, sessions and summoning, shared with
DuckTable) and **`justhttp`** (Harbor's synchronous HTTP/1.1 server). The
server implements the same wire shapes directly rather than consuming the
`wire` crate, so protocol changes require tests on both sides.

---

## One binary, engine on demand

Nothing links `libduckdb`. The generated bindings route every DuckDB C call
through null-initialized function pointers, and `engine/mod.rs` fills them by
`dlopen` — but only on the code paths that open a database. Consequences,
each load-bearing:

- **The client half is pure.** `harbor <sock>` and `harbor http://…` run on
  machines that have no engine at all; a missing library only surfaces when
  this process is asked to *be* the server, and the error names every path it
  searched.
- **The engine swaps by file.** The engine *is* the dylib, found by a fixed
  search (the README's
  [Known limitations](README.md#known-limitations) gives the order) that
  starts with an explicit `HARBOR_LIBDUCKDB`, then `../lib` beside the binary,
  the release-archive layout, and never reads the working directory. Harbor
  binds the v2 C API, so DuckDB 2.0 is the engine floor.
- **Building needs nothing.** No DuckDB source tree, library, or header —
  the crate ships pregenerated bindings, so `cargo build` works on a bare
  machine and CI needs the engine only to run the suite.

There is no static build: it would cost a binary that carries the whole engine
and a from-source build tree for an advantage — needing no dylib — that would
also kill the pure client. Precedent for the shape: `sqlite3` and `duckdb` are
one binary that is both shell and engine; harbor is one binary that is both
shell and server.

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
implementation in flock.

**Mandatory guard**: DuckDB defaults to ~80% RAM / all cores *per instance*.
`start` ships a conservative default (`--memory-limit`, default 2GB;
`--threads`) and prints it at startup. This is a multi-server safety
requirement, not an option.

**Standing settings live in config**: a database's `[connection.*]` entry
supplies its own memory, threads, and boot SQL, so a bare start — a summon, the
login item — honors them without flags; explicit flags override. Each key is
the `start` flag of the same name, so there is no second dialect. `init` is
the open door: any `INSTALL`, `LOAD`, `SET` or secret runs verbatim before
serving, so harbor passes a berth's customizations straight to DuckDB without
knowing what they are, and a `settings` block is the same door in key-value
form. TCP exposure (`port`) is honored only by an explicit start, so opening a
database never opens a door. The keys are listed in the README's
[config.toml](README.md#configtoml).

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
what bare `harbor` prints: the installed CLI version, then each database's
path, serving Harbor version, pid, live client count, uptime, and address. A
socket that refuses the connection is a leftover from a `kill -9` and is
unlinked on sight; any other failure proves nothing and removes nothing. The
listening socket is the only runtime registration; `/info` is the identity
document, with uptime and the client refcount spliced in live.

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
invocation holds one connection open for its lifetime, so a human thinking
at a prompt is presence, not absence. No route of its own and no server-side
record — the open connection is the whole protocol, and a crashed client
releases it by definition. The anchor asks `/ready` once, which marks the
connection as a client's and buys it the server's five-minute idle clock, and
every four minutes moors a fresh connection before letting the old one go, so
the count never touches zero; justhttp's
[connection clocks](crates/justhttp/README.md#hardening-carried-in-the-source)
close a connection that never speaks. `.open` moors at the new server before
releasing the old one.

`start` ignores the refcount entirely. At a terminal it brings the server up
in the background and returns: under the database's login item when it has
one (launchd or systemd owns the process from the first second), otherwise
as a detached child that runs until `stop`. Headless, it serves in place
until `SIGTERM`; `--foreground` asks for that at a terminal. Spawn-on-use
and a terminal `start` are the same launch (`current_exe() <db> start`,
detached, output to a log under `runtime/log`) — the summon adds
`HARBOR_EPHEMERAL` to its child's environment and a hand start does not —
so there is one start path however a server comes to exist. The prompt is
what bare `harbor <db>` is for; `start` never opens one.

`curl` works iff something is listening, by design: a bare HTTP client does
not summon a database. An application that wants spawn-on-use runs
`harbor <db> -c "SELECT 1"` once and connects within the startup grace.

## Shared config; derived runtime identity

`~/.config/harbor/config.toml` names local paths and remote URLs, and gives
each local berth standing resource limits, boot SQL, DuckDB settings, and an
optional loopback port. Harbor and DuckTable read the same schema. Harbor
refuses a file or containing directory writable by another user before
applying any setting, because boot SQL can load code, and a file that will not
load stops a start rather than letting a server come up without its `sealed`
or its statement ceiling.

Runtime identity remains derived rather than registered: a database path maps
to its socket name, and live discovery reads those sockets. `attach` and
`detach` edit desired membership in config; they do not create a second live
registry.

## Backup is contents, not a file copy

`backup` and `restore` are the two verbs that act on a database's contents
rather than its lifetime, so they stand alone and take no other verb. Backup
writes `EXPORT DATABASE` as tab-separated files with the dialect pinned — bare
`NULL` is a null, quoted `"NULL"` is the string, an empty field is an empty
string — because the artifact has to be greppable, diffable and readable by
anything, which a `.duckdb` written by one engine build is not. Quotes are
allowed everywhere and required almost nowhere; the one file that gets them is
a single-column table holding an empty string, which written plain would be an
empty line, and every CSV reader skips those. Restore reads that directory
into a **new** file and refuses an existing one: a restore that can overwrite
can be run at the wrong moment and destroy what it was meant to protect, so
putting the result into place stays a human act. It is also where
`--block-size` is applied, block size being fixed at creation. Both work
under a temporary name and rename at the end, so a backup or a restored file
appears whole or not at all.

A `VARIANT` column travels as JSON text. `harbor restore` loads it back as
documents, so a `CHECK` on the column sees nothing else; for a stock `duckdb`
the decode is `after.sql` (DuckDB's `IMPORT DATABASE` takes only `COPY`, so
it is a file of its own beside `load.sql`). A value that entered as JSON —
which is how a variant is populated in practice — returns exactly; what JSON
has no word for, a `DATE` or `DECIMAL` put inside a variant from SQL, returns
as JSON's nearest type and is named out loud, once per column;
`--format parquet` keeps it.

No format holds every shape, and the holes do not overlap: text loses a
`UNION`'s tag and retypes a `VARIANT` nested inside another type, parquet
refuses a negative `INTERVAL` and normalises a `TIMETZ` to UTC. Two of those
four are silent, which is the reason the verb has an opinion at all. Each
hole is the other format's solid ground, so a table the chosen format cannot
carry is written in the other and named out loud; `--strict` refuses instead.
The invariant is that harbor never writes something that will not come back
without saying so. Out of scope on purpose: retention, rotation, scheduling,
compression, and remote targets — cron, a filesystem and `rsync` already do
those, and doing them here would make harbor a backup product.

## The local access boundary

Unix sockets live in a `0700` runtime directory. TCP binds IPv4 loopback only —
`127.0.0.1` — and is added with `--port` or the matching config entry. Remote
clients arrive through SSH or an edge proxy; Harbor itself does not expose a
non-loopback listener.

Loopback is not the same as trusted: a browser on the machine sends a web
page's requests there too. So the TCP listener refuses any request with an
`Origin` header, or with a `Host` that is a hostname rather than `localhost` or
an address. A page cannot post SQL, and DNS rebinding cannot read the answer.

## Caddy is the optional edge; UDS is the default face

Local security = filesystem perms. Remote = Caddy terminates TLS/HTTP3,
enforces edge policy, and proxies to the socket. A proxy to the TCP port
instead drops `Origin` and sends the upstream's own `Host`, since the browser
check is its job there. The client deliberately speaks UDS and
plain `http://` only. A human reaches a remote server through SSH; browser
and application clients go through Caddy.

## The wire is a commitment

Protocol v1 stays compatible: Rip's ORM, the applications in production on
it, and DuckTable all speak it. A change adds — a field, an event, a route, an
encoding — and never removes or renames one, so a client that reads the
schema's `lossless` flag and passes over what it does not know keeps working
across releases. The [CHANGELOG](CHANGELOG.md) records each addition. There
is no `database` field on `/sql`: a server holds one database, and
per-session work is `USE` or `ATTACH` as plain SQL on a lease.

## The engine is upstream DuckDB 2.0 — no fork

The engine is DuckDB's own 2.0 line, unmodified: `libduckdb` plus the
`duckdb` CLI, as DuckDB publishes them. Nothing is forked, nothing is patched,
and nothing is built here. `make fetch-duckdb`, CI and the release workflow
all fetch through one script: from DuckDB's nightly channel, or, for a build
`DUCKDB_LIB_BUILD` names, whole from this repository's `engine-<build>`
release, where a fixed build stays available after the channel has moved on.
A release archive bundles the engine it was built and tested with. The
floor is DuckDB 2.0 by construction: harbor binds the v2 C API, whose symbols
older engines do not export, and passes over a library without them. Database
files are a different matter — a file created by a 1.5-era DuckDB opens
as-is, because 2.0's storage layer reads it.

Parsing is the dominant engine cost of a small statement, so each connection
keeps a small cache of parsed statements and a repeated one skips the parse.
A cached statement is the parser's output, not a plan: execution binds it
again, so a catalog change is always seen.

**Planned for the GA timeframe** — collected here so GA day has one list:
point the release fetch at a versioned GA artifact rather than a named
pre-release build; revisit the parsed-statement cache size against the GA
engine's parser; and re-run the benchmarks on quiet hardware.

**Harbor ships no extension.** A release archive carries harbor and the exact
libduckdb it was tested against — nothing else. The extension door is the
operator's: the loaded engine exports the full C++ ABI, so an extension built
against the *same* engine loads from it —
`harbor db.duckdb start --unsigned --init 'LOAD <ext>'`. Matching extension
to engine is the caller's responsibility, because the C++ ABI admits no other
answer.

## The HTTP layer is first-party: justhttp

Harbor owns its HTTP layer as the workspace crate
[`justhttp`](crates/justhttp/README.md), a path dependency: not vendored, and
tracking no upstream. It began from the synchronous HTTP/1.1 core of
tiny_http 0.12.0, and that lineage is kept for licensing only. Its surface is
what Harbor needs and nothing more — synchronous workers, Unix sockets and
TCP, responses streamed from `Read`, a handle that tells a handler its client
has gone, and the connection counter the refcounted lifetime rides on
(incremented at accept, decremented by a drop guard). Its hardening, each
behavior with a regression test, is listed in its README. The wire-visible
`Server:` header identifies `justhttp`.
