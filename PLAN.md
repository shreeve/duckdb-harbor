# Harbor — decision record

Harbor is a single Rust binary that starts, serves, and talks to DuckDB
databases — fleet manager, HTTP/UDS server, and companion CLI. The plan that
built it shipped: Phases 0–2 are complete and it is pre-production behind Caddy.
What remains here is the **decision record** — the rationale the code cites by
number (`PLAN.md D5`, `D8`, …). For usage see [MANUAL.md](MANUAL.md); for
maintenance items see [TODO.md](TODO.md).

```
~/.harbor/                        ← the harbor IS a directory (no daemon)
├── sales.sock                    ← berth: one harbor serve process, one .duckdb
├── sales.token                   ← per-berth bearer token, 0600
├── sales.json                    ← identity sidecar (pid, paths, versions)
└── log/sales.log

harbor add ./sales.duckdb         → spawn a detached berth, bind the socket
harbor ls                         → readdir + /ready probe + /info
harbor stop sales                 → drain, CHECKPOINT, socket gone
pilot sales                       → duckdb-CLI-class REPL over UDS (or https via Caddy)
pilot ./ad-hoc.duckdb             → summon an ephemeral berth, connect (D9)
```

The workspace is three crates: **`harbor`** (bin + library — the engine folded
in beside the CLI), **`wire`** (the frozen protocol contract, shared with
pilot), and **`pilot`** (the client, links no engine).

---

## Decisions (with rationale)

### D1. One binary, dynamically linked (the engine swaps by dylib)
Originally the binary embedded DuckDB statically via the `duckdb` crate's
`bundled` feature. That shipped and worked — but the Phase-0 spike proved
something better. Because duckdb-rs uses *pregenerated* bindings, the SAME
dynamically-linked `harbor` bytes drive whichever `libduckdb.dylib` they resolve
at runtime (`@rpath` / `@loader_path`) — verified by one binary reporting
v1.5.5 beside the 1.5.5 dylib and v2.0.0-alpha beside the 2.0 one. So harbor is
**dynamic by default and version-agnostic**: one build serves a mixed fleet, the
engine chosen by the dylib it links (`make install` puts harbor + pilot on PATH
in `/usr/local/bin`; harbor's baked absolute rpath resolves the engine in
`~/.duckdb`, DuckDB's own world). The static `bundled` build was dropped
entirely: it cost a 33MB binary and a 17GB from-source build tree for an
advantage — needing no sibling dylib — the swap model removed. Distribution is
`curl && chmod +x` plus the sibling dylib; the version pin *is* the dylib.

### D2. One process per database (the fleet is N `harbor serve` processes)
DuckDB files are single-writer; the code already enforces one-database-per-
process (`open_pool` refuses a second, by design — the pool/lease/cancel
machinery is process-wide statics). Keeping that gives crash isolation,
per-berth DuckDB versions (a 1.5.5 berth beside a 2.0-dev berth), independent
SIGTERM→drain→CHECKPOINT lifecycles, and the daemonless registry.
**One berth, one database, one socket, one token — no exceptions shipped**
(decided 2026-08-16: an implemented `--attach` flag was removed the same day
to keep the model clean; ~30 lines to restore if a real cross-db-join need
ever arrives). ATTACH remains available to clients as plain SQL through
`/sql` — an engine feature harbor neither promotes nor babysits.

**Mandatory guard**: DuckDB defaults to ~80% RAM / all cores *per instance*.
`harbor serve` ships a conservative default (`--memory-limit`, default 2GB;
`--threads`) and prints it at startup. Not a follow-up — a 1.0 requirement.

### D3. No daemon — the filesystem is the registry
A berth's UDS socket in `~/.harbor/` IS its registration. Discovery = readdir;
liveness = connect + `GET /ready`; identity = `GET /info`. Claim protocol:
`flock(LOCK_EX|LOCK_NB)` on `<name>.lock` is the real mutex (held for process
life); if acquired and a socket file exists, it is stale by definition → unlink
and bind. Never unlink a socket without a failed `/ready` dial first. Clean
shutdown removes socket + json. `harbor ls` reaps stale entries (flock-guarded).
The same flock is proof-of-life for `stop`/`rm`: if we can take it, the berth is
gone and its recorded pid may be recycled, so we do not signal it.

### D4. Spawn, don't fork; persist via the init system
`harbor add` runs `current_exe() serve …` detached (setsid, stdin null,
stderr → `~/.harbor/log/<name>.log`), then polls `/ready` before reporting
success. Identical on macOS/Linux. Boot persistence is a separate verb:
`harbor add --boot` generates and loads a launchd plist / systemd user unit
(`KeepAlive` / `Restart=on-failure`, `ExitTimeOut`/`TimeoutStopSec=30` to honor
the drain+CHECKPOINT window). Harbor never becomes a supervisor. **No socket
activation**: a database should not cold-start (WAL replay) per request, and
idle-exit fights the lease/checkpoint machinery. Plain always-on units.

### D5. The extension retires (decided 2026-08-16)
Once the binary is validated, the loadable-extension artifact is dropped —
with it go extension signing, `-unsigned`, `FORCE INSTALL`, the two-artifact
version-matching dance, the unstable-C-API band pin, the bash launcher, and
the bundled/loadable feature-split build gotcha. Its one niche (bolting HTTP
onto someone else's live DuckDB process) is covered well enough by `--quack`
and by pilot; if ever truly needed, the ~350 lines of vtab glue can be
resurrected on top of harbor's library. The embedded binary loses no
co-residency: `harbor serve --ui --quack` loads those extensions into its own
DuckDB (`allow_unsigned_extensions` on the 2.0 channel). *(This retirement is
also why `harbor-core` later folded back into `harbor`: the split existed to
share server logic with the extension, and nothing else ever linked it.)*

### D6. Caddy is the optional edge; UDS is the default face
Local security = filesystem perms + token. Remote = Caddy terminates TLS/HTTP3
and edge auth, proxies to the socket. The CLI speaks UDS, `http://`, or
`https://` interchangeably — a remote berth is just a URL.

### D7. Protocol changes are additive only
The Rip harborAdapter must run unmodified. Two new endpoints (`/info`,
`/grammar`), zero changes to existing routes, events, fields, or error codes.
No `database` field on `/sql`: a berth holds one database (D2); per-session
work is `USE`/ATTACH as plain SQL on a lease (a lease is a pinned connection —
exactly what it was built for). Reserve `"catalog"` as a future optional
body field; don't implement it.

### D8. Two binaries: `harbor` (engine + fleet) and `pilot` (pure client)
The CLI never needs libduckdb — it speaks the protocol. Split along linkage:
`harbor` = `serve` (the only subcommand linking DuckDB) + fleet verbs
(`add/ls/stop/rm/doctor`: filesystem, spawning, HTTP probes). `pilot` = REPL +
one-shot `pilot sql`, connecting by berth name (via `~/.harbor/`), socket
path, or `http://` on a trusted network. **Pilot is TLS-free by design
(decided 2026-08-16): https/edge auth is Caddy's job for browser and app
clients; a human with pilot reaches a remote host over ssh.** Consequences:
pilot is ~1.5MB, installs on machines that never host a database, and is
**DuckDB-version-agnostic** — one client for a mixed 1.5.5/2.0 fleet, while each
harbor build resolves whichever engine sits beside it. Precedent: `psql`/
`postgres`, `redis-cli`/`redis-server`. A small shared **`wire`** crate
(envelope types, error codes, request shapes) is the wire contract in
compilable form — harbor encodes it, pilot decodes it, drift is a compile
error. Dependency quarantine stays perfect: reedline never in the server tree,
tiny_http never in the client.

### D9. Pilot spawns the owner on demand (ephemeral berths)
`pilot ./sales.duckdb` recovers the old `duckdb sales.duckdb` muscle memory
via the agent pattern (gpg-agent/emacsclient): resolve the target — name in
`config.toml` (D10a) → that entry; live berth name → registry socket; URL →
connect; file path → connect to the live berth whose sidecar claims that
path, else exec `harbor serve` from PATH (pilot never links DuckDB), poll
`/ready`, connect. This factors out DuckDB's single-writer lock pain: a second
`pilot` on the same file joins the same berth instead of "database is locked".
Pilot-spawned berths run with `--idle-exit <dur>`: zero connections AND zero
leases for the window → drain, CHECKPOINT, unlink, exit — robust against pilot
crashes, no refcounting, no orphans. (Not the rejected socket activation:
idle-exit is opt-in per spawn, exits through the normal checkpoint path;
permanent berths never idle-exit.) `--moor` / `harbor add` promotes ephemeral →
permanent. No cleverer lifecycle rules than these.

### D10a. Client address book: `~/.harbor/config.toml` — purely additive
**Zero-config is the local default**: with no config file, `pilot <berth>`
(registry socket), `pilot ./file.duckdb` (join-or-spawn, D9), bare `pilot`
(live berths), and every `harbor` verb all work — first run is
`curl && harbor add && pilot sales`, no files edited. The registry is live
state (what's running, discovered by readdir, never configured); the config
is the address book (how to reach non-local things: remotes, tokens, taste),
read by **pilot only** — the server has no config file, ever (`harbor` =
flags + registry). Name collisions: an explicit config entry shadows a live
local berth of the same name (ssh_config precedent: explicit mapping beats
ambient discovery), with a one-line notice. `pilot medlabs` resolves
`[connection.medlabs]` → `url` (https via Caddy) or `path` (spawn-on-demand
alias with per-entry `idle-exit`), plus `[defaults]` for REPL prefs (mode,
timer, maxrows, nullvalue, theme, appearance). Credentials never touch argv:
per connection `token-file` (default) or `token-cmd` (prints token — keychain/
1Password); inline `token` and URL userinfo accepted but documented as the
lazy path. Lives in `$HARBOR_HOME` (default `~/.harbor/`, 0700, files 0600)
beside sockets/tokens/history — the harbor stays one directory. Bare `pilot`
lists address book ∪ live registry: the fleet dashboard from any machine,
including database-less ones.

### D10b. Defense in depth on auth
Token required on ALL faces by default, including UDS (the janus lesson:
sockets get their modes widened; a proxy misconfiguration must not equal full
DB access). `--token ''` remains the explicit opt-out. Per-berth token files
(`~/.harbor/<name>.token`, 0600) beside the sockets — no shared credentials
store. Caddy injects/passes the berth token with *replace* semantics
(`header_up Authorization …`) — harbor 401s duplicate Authorization headers.
`/ready` stays the only unauthenticated route; `/info` requires auth (it leaks
paths and pids).

### D11. The engine is DuckDB's official v2.0-dev nightly — no fork
The 2.0 line has no stable release yet, but DuckDB publishes it as an official
nightly: `artifacts.duckdb.org/latest/duckdb-binaries-<plat>.zip`, a bundle of
`libduckdb` (+ headers) and the `duckdb` CLI, rebuilt from `main` daily. That is
the whole engine the fleet needs, from upstream, so there is nothing to build
and nothing to fork. (An earlier fork release existed only because we thought no
2.0 was published; it was a frozen copy of this exact artifact and is now
retired.) `/latest/` is the v2.0-dev channel today — it reported
`v2.0.0-alpha38069` when this was written; if `main` ever rolls past 2.0, pin
the v2.0 channel URL.

**CI links that nightly**, via `.github/actions/duckdb` — one composite action
both the quick and full jobs share, so they cannot drift (which is what once let
the full job rot pointing at a tag upstream never had). No version pinned, no
nested-archive naming in the workflow: it tracks whatever `main` built last,
which is the 2.0 line harbor targets, so every run tests the real thing.
Verified: a harbor built against `alpha37626` links and runs clean against the
newer official `alpha38069`, and against upstream stable `v1.5.5` — the C API is
stable across the line (D1's version-agnostic property, exercised, not asserted).

`make fetch-duckdb` (→ `scripts/fetch-duckdb.sh`) pulls the same official nightly
into `~/.duckdb/cli/2.0.0/` for local work; `make setup` chains it with the
binary build, `install`, and `make ui`, taking an empty `~/.duckdb` to a working
fleet in one command.

`make ui` (→ `scripts/build-ui-extension.sh`) builds the UI extension out-of-tree
against the *exact* nightly just installed: read its commit, fetch DuckDB's
headers at that commit (cached), compile only the ~11 UI sources, link
dynamically (`-undefined dynamic_lookup` — symbols resolve from harbor's
in-process libduckdb at load), and install to
`~/.duckdb/extensions/<version>/<platform>/` so `LOAD ui` resolves it by name.
No engine compile (~6s cached vs the 1,511-file, 20–40 min from-source build);
the version lock the C++ ABI demands is satisfied *by construction*, since
engine, dylib, and extension all derive from one nightly. The UI source is a
duckdb-ui checkout carrying the v2 fixes — our fork until duckdb/duckdb-ui#242
lands, then official. Verified end to end: `harbor serve --unsigned --init 'LOAD
ui' --init 'FROM start_ui_server()'` serves the UI at `http://localhost:4213/`.

**The UI extension can be dynamic — and should be.** DuckDB ships loadable
extensions *statically* (each embeds a private copy of DuckDB, ~37MB, portable
because a stock CLI may have loaded DuckDB `RTLD_LOCAL`). harbor is not a stock
CLI: it holds one `libduckdb.dylib` in-process, and that dylib exports the full
C++ ABI (~29.6k `_ZN6duckdb…` symbols beside the 549 stable `duckdb_*` C-API
ones). So a *dynamically* linked UI extension — built with
`EXTENSION_STATIC_BUILD=0`, an upstream-supported knob, **not** a source patch —
loads into harbor and resolves its DuckDB symbols from that shared library. It
is smaller, and it removes the static model's duplicate-global-state hazard
(two DuckDB runtimes in one address space). Static is forced only on Windows and
z/OS; on macOS/Linux the flag is honored — macOS gets `-undefined
dynamic_lookup`, Linux leaves the symbols for the loader. Trade-off: it is
CLI-incompatible (fine — harbor is the only host) and leans on
incidentally-exported C++ internals, so it is sound *only* under the same-build
guarantee above. Keep the static build as the known-good fallback.
