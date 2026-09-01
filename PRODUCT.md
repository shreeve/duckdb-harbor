# DuckDB Harbor — the product

DuckDB Harbor ships two Rust binaries: `harbor` owns DuckDB databases and their
HTTP/UDS fleet lifecycle; `pilot` is the engine-free client. This document is
the architecture — what the product is and why it is shaped this way. For
usage see [README.md](README.md).

```
~/.config/harbor/config.toml      ← desired state: yours to edit
~/.local/state/harbor/runtime/    ← actual state: harbor's to write, safe to delete
├── sales.sock                    ← berth: one harbor serve process, one .duckdb
├── sales.lock                    ← process-lifetime ownership mutex
├── sales.token                   ← per-berth bearer token, 0600
├── sales.json                    ← identity sidecar (pid, paths, versions)
└── log/sales.log

harbor add ./sales.duckdb         → name it: a name is a service
harbor show                       → reconcile config, sidecars and locks
harbor stop sales                 → drain, CHECKPOINT, hold it stopped
pilot sales                       → duckdb-CLI-class REPL; raises the service if down
pilot ./ad-hoc.duckdb             → summon a temp berth, connect; it leaves when idle
```

The workspace has five first-party crates: **`harbor`** (bin + library),
**`pilot`** (the client, links no engine), **`harbor-common`** (paths, the
config schema, fleet reconciliation — the vocabulary both binaries share),
**`wire`** (Pilot's protocol types), and **`justhttp`** (Harbor's synchronous
HTTP/1.1 server). Harbor implements the same wire shapes directly rather than
consuming the `wire` crate, so protocol changes require tests on both sides.

---

## One engine binary, dynamically linked (the engine swaps by dylib)

Because duckdb-rs uses *pregenerated* bindings, the SAME dynamically-linked
`harbor` bytes drive whichever `libduckdb.dylib` they resolve at runtime
(`@rpath` / `@loader_path`) — verified by one binary reporting v1.5.5 beside
the 1.5.5 dylib and v2.0.0-alpha beside the 2.0 one. Harbor is **dynamic and
compatible across the tested line**: one build serves a mixed DuckDB
1.5.5/2.0-nightly fleet, with the engine chosen by the dylib it links
(`make install` puts harbor + pilot on PATH in `/usr/local/bin`; harbor's
baked absolute rpath resolves the engine in `~/.duckdb`, DuckDB's own world).
Windows resolves `duckdb.dll` beside the executables. There is no static
build: it would cost a 33MB binary and a 17GB from-source build tree for an
advantage — needing no sibling dylib — the swap model removes. Distribution is
the release archive plus its sibling dylib; the engine pin *is* the dylib.

## One process per database (the fleet is N `harbor serve` processes)

DuckDB files are single-writer; the code enforces one-database-per-process
(`open_pool` refuses a second, by design — the pool/lease/cancel machinery is
process-wide statics). That gives crash isolation, per-berth DuckDB versions
(a 1.5.5 berth beside a 2.0-dev berth), independent SIGTERM→drain→CHECKPOINT
lifecycles, and the daemonless registry. **One berth, one database, one listen
endpoint, and one token by default — no multi-database mode.** ATTACH is
available to clients as plain SQL through `/sql` — an engine feature harbor
neither promotes nor babysits.

**Mandatory guard**: DuckDB defaults to ~80% RAM / all cores *per instance*.
`harbor serve` ships a conservative default (`--memory-limit`, default 2GB;
`--threads`) and prints it at startup. This is a fleet-safety requirement, not
an option.

## No daemon — the filesystem is the registry

`<runtime>/<name>.json` is the discoverable identity and dial record for both
UDS and TCP berths; liveness is a `GET /ready` probe. The process-lifetime
`flock(LOCK_EX|LOCK_NB)` on `<name>.lock` is the ownership mutex. Once `serve`
holds that lock, an existing socket path belongs to no live process under that
name and can be replaced before bind. Clean shutdown removes the socket and
JSON sidecar but deliberately leaves the lock inode in place. `harbor show`
reads sidecars and reports dead entries; it does not silently reap them. The
same flock is proof-of-death for `stop`/`forget`: if we can take it, the
recorded pid may have been recycled, so Harbor does not signal it. `forget` is
the explicit cleanup operation — registry files and the config entry, never
the database file.

## Spawn, don't fork; persist via the init system

`harbor start` runs `current_exe() serve …` detached in a new process group,
with stdin null and stdout/stderr appended to `<runtime>/log/<name>.log`, then
polls `/ready` before reporting success. Identical on macOS/Linux. Harbor
never becomes a supervisor. Boot persistence belongs in launchd or a systemd
user unit. **No socket activation**: a database should not cold-start (WAL
replay) per request. An always-on unit and an explicitly ephemeral
`--idle-exit` berth are separate operating choices.

## Caddy is the optional edge; UDS is the default face

Local security = filesystem perms + token. Remote = Caddy terminates TLS/HTTP3
and edge auth, proxies to the socket. Pilot deliberately speaks UDS and plain
`http://` only. A human reaches a remote berth through SSH or runs Pilot on
the host; browser and application clients may use HTTPS through Caddy.

## Protocol changes are additive only

The Rip harborAdapter runs unmodified. The fleet routes are `/info`,
`/keepalive`, and `/shutdown`, with zero changes to existing routes, events,
fields, or error codes. No `database` field on `/sql`: a berth holds one
database; per-session work is `USE`/ATTACH as plain SQL on a lease (a lease is
a pinned connection — exactly what it was built for). `"catalog"` is reserved
as a future optional body field, not implemented.

## Two binaries: `harbor` (engine + fleet) and `pilot` (pure client)

The CLI never needs libduckdb — it speaks the protocol. Split along linkage:
`harbor` = `serve` (the only subcommand linking DuckDB) + fleet verbs
(`add/start/show/expose/stop/forget/doctor`: filesystem, config edits,
spawning, HTTP probes).
`pilot` = REPL plus one-shot `pilot <target> -c "SQL"`, connecting by berth
name (via the runtime registry), socket path, database path, or `http://` on a
trusted network. **Pilot is TLS-free by design: https/edge auth is Caddy's job
for browser and app clients; a human with pilot reaches a remote host over
ssh.** Consequences: pilot is ~1.5MB, installs on machines that never host a
database, and is **DuckDB-version-agnostic** — one client for a mixed
1.5.5/2.0 fleet, while each harbor build resolves whichever engine sits beside
it. Precedent: `psql`/`postgres`, `redis-cli`/`redis-server`. The small
**`wire`** crate contains the request, envelope, error-code, and endpoint
types Pilot consumes. Harbor owns its encoder and request parser directly, so
drift is caught by cross-side protocol tests rather than by a shared Rust
dependency. Dependency quarantine stays clean: reedline never enters the
server tree, and justhttp never enters the client.

## A path summons a temp database; a name is a service

The semantics follow how the target is written. A **name** is a service: it
starts on use and runs until the operator says `harbor stop`. `harbor add
<db.duckdb> [name]` writes the config entry that makes it one; from then on
`pilot <name>` raises the berth if it is down and leaves it running on exit.
`harbor stop` is the operator's last word — it drains, CHECKPOINTs, and
writes a hold, and a held name refuses every client's autostart (`pilot
<name>` answers with the `harbor start` hint rather than a berth) until
`harbor start` lifts it. A **path** is a session: `pilot ./sales.duckdb`
gives the `duckdb sales.duckdb` muscle memory via the agent pattern
(gpg-agent/emacsclient) — connect to the live berth whose sidecar claims
that file, else run `harbor start ... --idle-exit` from PATH (Pilot never
links DuckDB), wait for readiness, connect. This factors out DuckDB's
single-writer lock pain: a second Pilot on the same file joins the same
berth instead of reporting "database is locked".

These temp databases run with `--idle-exit <dur>` (`[defaults]
temp-idle-exit` sets the window; 90s otherwise — a named service never reads
it): no active countable requests and no live sessions for the window →
drain, CHECKPOINT, depart. A departure leaves the harbor as the berth found
it — no runtime record, nothing in `harbor show` — and the database file
stays behind, checkpointed and self-contained. `harbor show` marks the
living ones — `● running (temp 1m30s)` — so a berth that
appears and leaves on its own always says so. Idle keep-alive TCP
connections are not reference-counted. The lifecycle is robust against
Pilot crashes and has no client refcount. An interactive Pilot sends cheap
authenticated `/keepalive` activity pulses while it waits at the prompt —
every third of the idle window, capped at 10s; these hold no DuckDB
connection and leave no server-side record, so exiting or crashing Pilot
simply lets the same idle window resume. One-shot Pilot invocations do not
pulse after their query.

## One config, two readers, one writer: `~/.config/harbor/config.toml`

**Zero-config is the local default**: with no config file, `pilot <berth>`
(registry socket), `pilot ./file.duckdb` (join-or-spawn), bare `pilot` (what
is openable), and `harbor start ./file.duckdb` all work — first run is
`curl && harbor add ./sales.duckdb && pilot sales`, no files edited by hand.
The config has exactly one writer: `add`, `expose`, and `forget` edit it
through a TOML document model that preserves the operator's comments and
ordering, behind the same trust gate reads use, and never land bytes the
shared schema would refuse.

The split that matters is desired versus actual, not client versus server.
`~/.config/harbor/config.toml` is **desired** state: what you have, and how
you want it started. `~/.local/state/harbor/runtime/` is **actual** state:
what is running right now, discovered by readdir and never configured. Config
is edited by a human; runtime is written by harbor and is safe to delete.

**Harbor reads the config too.** A berth's desired state does not fit on a
command line — sixteen `serve` flags, repeatable `--init` SQL, memory limits,
sealed mode — and a server whose settings cannot be written down is a server
nobody can reproduce: `harbor start medlabs` has to mean something. The
load-bearing rule: **harbor never reads a credential from config.** Its
deserializer has no `token`/`token-file`/`token-cmd` field, so
`resolve_token` — which runs `sh -c` — stays in pilot, and no config file can
make the server shell out.

One namespace, so "where do I configure medlabs" has one answer: an entry with
a `path` is a local berth harbor can start, one with a `url` is a remote
harbor never touches, and one with both is reported rather than guessed at.
The berth's name is the section key, never the database file's stem. Name
collisions: an explicit config entry shadows any live local berth of the same
name (ssh_config precedent: explicit mapping beats ambient discovery). Pilot
prints a notice when the local berth has a UDS socket; a same-name TCP sidecar
is shadowed silently. `pilot medlabs` resolves `[connection.medlabs]` →
path-free plain-HTTP `url`, or `path` (joins the running berth, or raises the
service — a name starts on use; only a `harbor stop` hold keeps it down),
plus `[defaults]` for REPL prefs
(mode, timer, maxrows,
nullvalue, theme, appearance). Credentials can come from `--token`,
`HARBOR_TOKEN`, inline config `token`, `token-file`, or `token-cmd`; URL
userinfo is not parsed. Harbor creates `$HARBOR_HOME` as mode 0700 and minted
token files as 0600; a user-authored config file keeps whatever permissions
its creator gave it, but pilot refuses to read one that is group- or
world-writable, sits in such a directory, or is owned by another user —
`token-cmd` runs through `sh -c` and `url` chooses who receives the bearer
token, so a writable config is a program, not a preference. Bare `pilot`
lists the live local registry; configured remotes are resolved when named,
not merged into that fleet view.

## Defense in depth on auth

Token required on ALL faces by default, including UDS (sockets get their
modes widened; a proxy misconfiguration must not equal full DB access).
`--token ''` is the explicit opt-out. Per-berth token files
(`<runtime>/<name>.token`, 0600) beside the sockets — no shared credentials
store. Caddy injects/passes the berth token with *replace* semantics
(`header_up Authorization …`) — harbor 401s duplicate Authorization headers.
`/ready` is the only unauthenticated route; `/info` requires auth (it leaks
paths and pids).

## The engine is DuckDB's official v2.0-dev nightly — no fork

The 2.0 line has no stable release yet, but DuckDB publishes it as an
official nightly: `artifacts.duckdb.org/latest/duckdb-binaries-<plat>.zip`, a
bundle of `libduckdb` (+ headers) and the `duckdb` CLI, rebuilt from `main`
daily. That is the whole engine the fleet needs, from upstream, so there is
nothing to build and nothing to fork. `/latest/` is the v2.0-dev channel; if
`main` ever rolls past 2.0, pin the v2.0 channel URL.

**CI links that nightly**, via `.github/actions/duckdb` — one composite
action both the quick and full jobs share, so they cannot drift. No version
pinned, no nested-archive naming in the workflow: it tracks whatever `main`
built last, which is the 2.0 line harbor targets, so every run tests the real
thing. Verified: a harbor built against one alpha links and runs clean
against newer alphas and against upstream stable `v1.5.5` — the same binary
answers `select version()` as the nightly via its baked rpath and as `v1.5.5`
via `DYLD_LIBRARY_PATH`, with the full check suite green on the nightly. The
C API is compatible for Harbor's exercised surface across those builds; this
is tested compatibility, not a claim about every past or future DuckDB ABI.
There is no "1.5 build" and no "2.0 build" — one artifact, and the engine is
whichever `libduckdb` it resolves at load. (The v2 nightlies do parse SQL
~2× slower than 1.5.5 — the new PEG parser; execution is at parity. See the
README's Performance section for the decomposition.)

`make fetch-duckdb` (→ `scripts/fetch-duckdb.sh`) pulls the same official
nightly into `~/.duckdb/cli/2.0.0/` for local work; `make bootstrap` chains it
with the binary build and `install`, taking an empty `~/.duckdb` to a working
fleet with no other prerequisites.

**Harbor ships no extension.** A release archive carries harbor, pilot, and
the exact libduckdb they were built against — nothing else. A server that
serves one database over HTTP does not need anything else in the box. The
extension door is the operator's: harbor holds one `libduckdb` in-process and
that library exports the full C++ ABI, so an extension built against the
*same* engine loads and resolves its symbols from it —
`harbor serve db.duckdb --unsigned --init 'LOAD <ext>'`. Matching the
extension to the linked engine is the caller's responsibility, because the
C++ ABI admits no other answer. Harbor resolves `LOAD` by name in `~/.duckdb`
and never patches extension source while loading it.

## The HTTP layer is first-party: justhttp

Harbor owns its HTTP layer as the first-party workspace crate
`crates/justhttp`, consumed through a plain path dependency. It is not
vendored tiny_http, has no tiny_http dependency, and does not track an
upstream HTTP crate. Lineage is retained only for licensing: justhttp began
from the relevant synchronous HTTP/1.1 core of tiny_http 0.12.0 and is
maintained as Harbor's own crate.

The surface matches Harbor's needs — synchronous workers, Unix sockets, TCP,
streaming from `Read`, and a tiny dependency tree. TLS, WebSocket upgrades,
`TestRequest`, unused response APIs, the notify/secure rendezvous, and `log`
are absent. The crate is roughly 2,650 lines in seven one-word files (lib,
http, stream, conn, request, response, pool), edition 2024,
`forbid(unsafe_code)`, with three small dependencies.

Two Harbor hardening behaviors are part of justhttp itself: unread request
bodies drain through a fixed 64 KiB buffer, and every accepted socket has a
10-second response write timeout. Regression tests pin both behaviors
(`tests/drain.rs` and the separately run ignored `stall` test). The suite
also covers keep-alive reuse, chunked streaming, response ordering,
half-close semantics, and byte-at-a-time CRLF framing. The wire-visible
`Server:` header identifies `justhttp`.
