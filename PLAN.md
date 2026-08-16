# Harbor Fleet Plan

**Harbor becomes a single Rust binary** that starts, serves, and talks to DuckDB
databases — fleet manager, HTTP/UDS server, and interactive CLI in one artifact.
The extension is retired (D5); the binary is the only artifact. This plan
was produced from a three-seat internal design review (architecture, CLI/UX,
protocol/deployment) grounded in the v0.9.1 source, MANUAL.md, and the DuckDB
2.0-dev checkout.

```
~/.harbor/                        ← the harbor IS a directory (no daemon)
├── sales.sock                    ← berth: one harbor serve process, one .duckdb
├── sales.token                   ← per-berth bearer token, 0600
├── sales.json                    ← identity sidecar (pid, paths, versions)
├── metrics.sock  metrics.token  metrics.json
└── log/sales.log

harbor add ./sales.duckdb         → spawn a berth, bind the socket
harbor ls                         → readdir + /ready probe + /info
harbor stop sales                 → drain, CHECKPOINT, socket gone
pilot sales                       → duckdb-CLI-class REPL over UDS (or https via Caddy)
pilot ./ad-hoc.duckdb             → summon an ephemeral berth, connect (D9)
```

---

## 1. Decisions (with rationale)

### D1. One binary, DuckDB statically embedded
`harbor` links libduckdb via the `duckdb` crate's `bundled` feature. The current
extension code is already source-compatible: the server machinery uses only the
library API (`Connection`, `InterruptHandle`, `try_clone`, params, logical
types) plus three tiny ffi helpers — the `loadable-extension` feature changes
how symbols link, not the Rust API. Distribution collapses from
zip + `FORCE INSTALL` + `-unsigned` + 35KB bash launcher to `curl && chmod +x`.
The version pin becomes *internal* (a harbor release = one DuckDB version, one
file, unmismatchable) instead of the extension's two-artifacts-must-agree
unstable-band pin. Expect a ~50–90MB binary; N berths share text pages.

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
resurrected on top of harbor-core. The embedded binary loses no co-residency:
`harbor serve --ui --quack` loads those extensions into its own DuckDB (same
composition the bash launcher performs today; `allow_unsigned_extensions` on
the 2.0 channel). The deployed v0.9.1 extension keeps working during
migration; it just stops being built.

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
body field; don't
implement it.

### D8. Two binaries: `harbor` (engine + fleet) and `pilot` (pure client)
The CLI never needs libduckdb — it speaks the protocol. Split along linkage:
`harbor` = `serve` (the only subcommand linking DuckDB) + fleet verbs
(`add/ls/stop/rm/doctor`: filesystem, spawning, HTTP probes). `pilot` = REPL +
one-shot `pilot sql`, connecting by berth name (via `~/.harbor/`), socket
path, or `http://` on a trusted network. **Pilot is TLS-free by design
(decided 2026-08-16): https/edge auth is Caddy's job for browser and app
clients; a human with pilot reaches a remote host over ssh.** Consequences: pilot is ~3–5MB,
installs on machines that never host a database, and is **DuckDB-version-
agnostic** — one client for a mixed 1.5.5/2.0 fleet, while each harbor build
pins one engine. Precedent: `psql`/`postgres`, `redis-cli`/`redis-server`.
A small shared **`harbor-protocol`** crate (envelope types, error codes,
request shapes) is the wire contract in compilable form — harbor-core encodes
it, pilot decodes it, drift is a compile error. Dependency quarantine stays
perfect: reedline/clap never in the server tree, tiny_http never in the client.

### D9. Pilot spawns the owner on demand (ephemeral berths)
`pilot ./sales.duckdb` recovers the old `duckdb sales.duckdb` muscle memory
via the agent pattern (gpg-agent/emacsclient): resolve the target — name in
`config.toml` (D10a) → that entry; live berth name → registry socket; URL →
connect; file path → connect to the live berth whose sidecar claims that
path, else exec `harbor serve` from PATH (pilot never links DuckDB), poll
`/ready`, connect. This factors out DuckDB's single-writer
lock pain: a second `pilot` on the same file joins the same berth instead of
"database is locked". Pilot-spawned berths run with `--idle-exit <dur>`: zero
connections AND zero leases for the window → drain, CHECKPOINT, unlink, exit —
robust against pilot crashes, no refcounting, no orphans. (Not the rejected
socket activation: idle-exit is opt-in per spawn, exits through the normal
checkpoint path; permanent berths never idle-exit.) `--moor` / `harbor add`
promotes ephemeral → permanent. No cleverer lifecycle rules than these.

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
ambient discovery), with a one-line notice. ssh_config/pg_service precedent: `pilot medlabs` resolves
`[connection.medlabs]` → `url` (https via Caddy; path-prefix shapes fine) or
`path` (spawn-on-demand alias with per-entry `idle-exit`), plus `[defaults]`
for REPL prefs (mode, timer, maxrows). Credentials never touch argv: per
connection `token-file` (default) or `token-cmd` (prints token — keychain/
1Password); inline `token` and URL userinfo accepted but documented as the
lazy path. Lives in `$HARBOR_HOME` (default `~/.harbor/`, already 0700, file
0600) beside sockets/tokens/history — the harbor stays one directory. Bare
`pilot` lists address book ∪ live registry (remotes, berths, ready-state,
versions): the fleet dashboard from any machine, including database-less ones.

### D10b. Defense in depth on auth
Token required on ALL faces by default, including UDS (the janus lesson:
sockets get their modes widened; a proxy misconfiguration must not equal full
DB access). `--token ''` remains the explicit opt-out. Per-berth token files
(`~/.harbor/<name>.token`, 0600) beside the sockets — no shared credentials
store. Caddy injects/passes the berth token with *replace* semantics
(`header_up Authorization …`) — harbor 401s duplicate Authorization headers.
`/ready` stays the only unauthenticated route; `/info` requires auth (it leaks
paths and pids). *(Resolved disagreement: architecture seat wanted /info open;
protocol seat's leak argument wins.)*

---

## 2. Workspace layout

```
Cargo.toml                 # [workspace], resolver = "2", default-members = ["crates/harbor"]
crates/
├── harbor-protocol/       # lib — the wire contract: envelope/event types,
│                          #   error codes, request shapes. No I/O. Shared by
│                          #   harbor-core (encode) and pilot (decode).
├── harbor-core/           # lib — ~90% of today's lib.rs, near-verbatim:
│                          #   pool, leases, cancellation, timeouts, signal/
│                          #   checkpoint, worker/routing, /sql /catalog /ready
│                          #   handlers, NDJSON schema/value emitter, KEYWORDS
│                          #   duckdb = { default-features = false, features = ["vtab"] }
│                          #   link mode chosen by leaf-crate feature passthrough
├── harbor/                # bin — duckdb {bundled, vtab}; ~50–90MB (the engine)
│   ├── serve              #   open db → open_pool → start(UDS and/or TCP) → wait
│   │                      #   flags: --memory-limit --threads --idle-exit --ui
│   │                      #   --quack --socket-mode/-group --idle-exit
│   └── add/ls/stop/rm/    #   fleet verbs (filesystem + spawning + HTTP probes;
│       doctor             #   no duckdb linkage — serve is the only user of it)
├── pilot/                 # bin — pure protocol client, no duckdb; ~3–5MB
│   ├── repl / sql         #   interactive + one-shot; berth name, path, or URL;
│   │                      #   spawns `harbor serve --idle-exit` for raw paths (D9)
│   ├── sqllex             #   SQL tokenizer (mirrors DuckDB PEG token stream)
│   └── duckbox            #   table renderers
```

(No harbor-ext crate: the extension retires per D5. The root extension
package and its extension-ci-tools machinery are deleted at the end of
Phase 1, once the binary passes the dual-target suite.)

- No "connection provider" trait: both link modes yield the same
  `duckdb::Connection`; `open_pool(Connection)` is already the abstraction.
- `start()` grows a UDS listener (tiny_http 0.12 `Server::http_unix`) beside
  TCP. **Verify `http_unix` + graceful `unblock` early — load-bearing.**
  Fallback: ~100-line hand-rolled accept loop.
- Transitional build gotcha (Phase 1 only, while the root extension package
  still exists beside crates/harbor): `bundled` and `loadable-extension`
  cannot feature-unify — build them in separate cargo invocations. The gotcha
  dies with the extension (D5).
- Windows: TCP-only, no registry (cfg(unix) throughout today); out of scope.

## 3. Registry contract

- Directory `$HARBOR_HOME` else `~/.harbor/` (0700). Per berth: `<name>.sock`
  (0600 default; `--socket-mode/--socket-group` for the janus-style shared
  case, applied after bind before accept, printed at startup), `<name>.token`,
  `<name>.json`, `<name>.lock`, `log/<name>.log`.
- Names: db file stem normalized to `[a-z0-9_-]{1,64}`; `--name` overrides;
  unique per directory (flock-enforced).
- `<name>.json` (fast path for `ls`; socket is the truth):
  `{name, pid, db, socket, harborVersion, duckdbVersion, startedAt, port}` —
  written via tmp+rename.

## 4. Protocol

### 4.1 Current surface = protocol 1 (frozen, byte-stable)
POST `/sql` (body: `sql`, `params`, `sessionId`, `queryId` ≤128, `timeoutMs`;
Accept-negotiated NDJSON stream `schema`/`row`/`end`|`error` or one-shot JSON ≤32MiB);
POST `/sql/sessions/new`, DELETE `/sql/sessions/<id>`, DELETE `/sql/queries/<id>`;
GET `/sessions`, `/catalog`, `/ready` (unauth). Error envelope
`{"type":"error","code","message"}` everywhere — **codes are the interface**
(the harborAdapter classifies on them); never rename one.

### 4.2 Additions
- **GET `/info`** (auth): `{protocolVersion: 1, name, harborVersion,
  duckdbVersion, database, databases[], pid, uptimeMs, mode:
  "binary"|"extension", grammar: bool}`. Feature probe for clients: 404 ⇒ old
  server, plain protocol 1. Implemented in core ⇒ both artifacts serve it.
  `protocolVersion` bumps only on breaking change (ideally never); additive
  features are capability booleans here.
- **GET `/grammar`** (auth): the `inlined_grammar.gram` (+ keyword lists)
  **compiled in at build time** for the linked DuckDB — the only way it
  provably matches. `ETag` + `X-Harbor-DuckDB-Version` headers. 404
  `no_grammar` on 1.5.5 builds. Powers Phase-3 client-side smarts with zero
  drift by construction.
- Reserved, not implemented: `POST /admin/checkpoint`; `"catalog"` body field.
- Wanted later (CLI progress bars): opt-in `{"type":"progress",…}` NDJSON
  event, gated by a request flag. Server-side item; not a blocker.

## 5. The CLI/REPL (`pilot`)

Crate stack: **reedline** (highlighter/validator/completer traits, menus,
vi+emacs, external editor) + crossterm + nu-ansi-term + unicode-width + serde +
**ureq** (rustls) for https + **hand-rolled HTTP/1.1-over-UnixStream** (~150
lines; no tokio/hyper — one request in flight, blocking is simpler) + clap.
History: reedline `FileBackedHistory` at `~/.harbor/history`.

- **Highlighting (tier 1)**: `sqllex`, a faithful Rust port of DuckDB's PEG
  *tokenizer semantics* (`base_tokenizer.cpp`, `token_type.hpp`): KEYWORD /
  STRING / NUMBER / OPERATOR / IDENTIFIER / COMMENT / TERMINATOR +
  `unterminated` flags; handles dollar-quoting, nested comments, `''`/`""`
  escapes, maximal-munch operators. Colors mirror the duckdb shell defaults
  (keyword green, literals yellow, comments gray, lexical errors red),
  overridable via `.highlight`. Keyword `.list` files (527 entries) vendored
  from the pinned DuckDB commit by `scripts/sync-keywords.sh`; harbor-core's
  existing `KEYWORDS` table moves into `sqllex` so server and REPL can't
  disagree. The PEG matcher is packrat-over-*tokens*, so this tokenizer is
  deliberately the Phase-3 foundation, not throwaway.
- **Completion, three lanes**: (A) dot-commands + berth names — client-side;
  (B) default for SQL: server-side `sql_auto_complete(?)` via `/sql` with
  params (sub-ms over UDS; fine on explicit Tab even over TLS); (C) fallback:
  `/catalog` cache (fetched at connect, refreshed after DDL-ish statements),
  used when the server misses a **150ms deadline** or for any as-you-type
  hinting. If measured RTT p50 > 80ms, cache answers first and the server
  result refines. Tab always answers.
- **Display modes**: P1 `duckbox` (default), `csv`, `json`, `jsonlines`,
  `markdown`, `line`, `list`, `trash`; P2 `table box ascii column insert quote
  html`; P3 `latex`. Duckbox is **hand-rolled** (~600–800 lines; no crate does
  the type row, middle-column pruning with a `…` column, first/last-half row
  elision with a `·` row, per-class styling, footer grammar — and
  `box_renderer.cpp` is a complete spec). Note: current source uses *square*
  corners `┌┬┐` — match source, not memory.
- **Streaming policy**: boxed modes retain O(display) — first ⌈maxrows/2⌉ rows
  + ring buffer of last ⌊maxrows/2⌋, count everything, drop the middle; render
  after `end` (authoritative `rowCount`/`timeMs`). Pipe modes (`csv`, `json*`,
  `line`, `list`, …) stream each `row` event, O(1) memory. Non-TTY duckbox
  hints `.mode csv` once.
- **Dot-commands day one**: `.help .quit .mode .maxrows .maxwidth .timer
  .nullvalue .open <berth|path|url> .databases` (fleet-aware: enumerates
  sockets + configured remotes, shows ready-state/version) `.tables .schema`
  (from `/catalog`, no round-trip) `.read .highlight`. Phase 2: `.output
  .once .headers .keymode .pager`.
- **Ctrl-C**: never kills the REPL. Client-generated `queryId` per statement;
  Ctrl-C fires `DELETE /sql/queries/<id>` on a side connection (cancel is
  race-proof server-side), drains to terminal event, prints `Interrupted`.
  Second Ctrl-C drops the connection. Ctrl-D at empty prompt exits.
- **Progress**: spinner + elapsed, then `streaming… N rows` once rows flow
  (the counter is free — we're consuming the stream). Real `progress` events
  when the protocol grows them.
- **Paging**: boxed output taller than the terminal pipes through `$PAGER`
  (default `less -SRFX`). Streaming modes never page.
- **Timer**: server `end.timeMs` + client wall time when they diverge
  (makes the network tax visible on remote berths).

## 6. Deployment

- **systemd**: template unit `harbor@.service` (User/Group harbor,
  `KillSignal=SIGTERM`, `TimeoutStopSec=30`, `Restart=on-failure`, hardening:
  `NoNewPrivileges`, `ProtectSystem=strict`, `ReadWritePaths`). Shared-uid
  socket access (janus case) = shared group + `--socket-group/--socket-mode
  0660`; harbor owns and prints socket perms; `harbor doctor` verifies them
  (late-EACCES is the failure mode to design against).
- **launchd**: per-berth plist, `KeepAlive.SuccessfulExit=false`,
  `ExitTimeOut=30`. Generated and loaded by `harbor add --boot`; units are
  artifacts, never hand-maintained. Headless Linux: document `loginctl
  enable-linger`.
- **Caddy**: prefer host-per-berth (`mydata.example.com { reverse_proxy
  unix/….sock { header_up Authorization "Bearer {env.HARBOR_TOKEN_MYDATA}" } }`)
  — no prefix stripping, adapter base-URL unchanged. Path-per-berth
  (`/db/<name>/*` + `uri strip_prefix`) documented as the fallback shape.

## 7. Migration from v0.9.1

Compatibility mechanism = **shared server code**, not a port: harbor-core is
the same code serving both artifacts. Per database: (1) `harbor add` → binary
berth on its socket; (2) repoint the edge upstream (same base URL, same token —
zero client change); (3) stop the old extension process (its SIGTERM path
already checkpoints). One database at a time; never both against the same file.
`bin/duckdb-harbor` bash launcher ships one release as an exec-shim, then
retires. Old servers 404 on `/info`; clients degrade gracefully.

## 8. Testing

- **Dual-target matrix**: today's Python suites already speak "URL + token".
  Add `HARBOR_TARGET=extension|binary` to `check.sh` (changes only server
  bootstrap) and a UDS option in the HTTP helper. Every protocol suite runs
  unchanged against both artifacts and both faces.
- **Version matrix**: `{duckdb 1.5.5, 2.0.0-dev} × {extension, binary}` in CI
  (heavy suites in one cell). `/info.duckdbVersion` available for the rare
  version-conditional assertion.
- **PEG-readiness pass** (`peg.py`, 2.0 cells): run the entire SQL corpus
  (harbor.test, suite statements, fixture DDL, fuzz corpus) through
  `check_peg_parser(?)` via `/sql`. Green = harbor's tested surface parses
  identically under 2.0's default parser; failures name the statement. Doubles
  as the Phase-3 grammar corpus.
- **Fleet suite** (`fleet.sh`, temp `$HOME`): add two berths → distinct
  identities; SIGKILL → stale socket detected, reclaimed only after failed
  dial; SIGTERM → exit 0, row present, **WAL gone** (checkpoint proof); lease
  survives client reconnect (server state keyed by id, not connection);
  stop/rm → socket+token cleaned, sibling unaffected.

## 9. Roadmap

**Phase 0 — spikes (de-risk before committing)**
1. `duckdb-rs` bundled/lib-dir build against the 2.0-dev C API: port
   `emit_column_schema`/`emit_value` against 2.0 headers. *Biggest unknown.*
   → **CLOSED, both halves PASS.** spikes/bundled: embedded v1.5.5 via the
   crate's `bundled` feature (40MB binary). spikes/linked2: the SAME
   duckdb-rs version (~1.10505, incl. `vtab`) compiles and runs against the
   2.0-dev libduckdb from our checkout via `DUCKDB_LIB_DIR`/
   `DUCKDB_INCLUDE_DIR` — the plain C API holds across the 1.5.5 → 2.0 line;
   the version-pin pain was the loadable-extension pointer band, now retired
   with the extension. **Release channels (2.0 preferred): `harbor` links
   our prebuilt 2.0-dev libduckdb (static for shipping; dylib fine for dev),
   `harbor-1.5` uses `bundled` from crates.io. Same code, link-time choice —
   and the fleet model (D2) lets both serve side by side.** Remaining build
   engineering: static-link flags for the shipped 2.0 binary, and re-verify
   whenever we roll the pinned 2.0 build forward.
2. tiny_http `Server::http_unix` + `unblock` graceful-shutdown behavior.
   → **PASSES** (spikes/uds: binds, serves sequential requests, `unblock()`
   releases a blocked `recv()` in ~65µs).
3. Workspace feature-split build (`bundled` vs `loadable-extension`) in CI.
   → workspace exists (root package + crates/*; spikes excluded); the split
   lands with the harbor-ext move in Phase 1.

**Phase 1 — the binary** (workspace refactor + fleet + serve)
→ **COMPLETE 2026-08-16.** The retirement is done: `src/` (extension),
`extension-ci-tools/`, the bash launcher, and the extension-only suites
(abi, sqllogic, differential, release) are deleted; the workspace is pure
(`harbor`, `harbor-core`, `harbor-protocol`, `harbor-pilot`); `make check`
boots the binary (2.0 channel preferred via `HARBOR_LAUNCHER`, bundled
fallback); `--init` replaced the planned `--ui/--quack` (one flag, any
extension); resilience.sh is parked for a Phase 2 rebuild (it tested the
retired launcher's REPL — pilot's job now). Earlier landing notes: harbor-core extracted verbatim (4.1k lines,
compiles clean, UDS listener added); `harbor` binary serves both channels
(bundled 1.5.5: 33MB; linked 2.0-dev: 1.8MB + dylib) with
serve/add/ls/stop/rm, the D3 registry (flock claim, json sidecar, token
files, 2GB memory default), and SIGTERM drain+CHECKPOINT verified. Dual-
target proof: spec, types, fuzz, cancel, sessions, catalog all PASS against
the binary (suites gained the `HARBOR_LAUNCHER` axis); extension target
still green. → **Tail landed same day**: GET `/info` (auth, uptimeMs live,
404-absence = version probe),
`--idle-exit` (D9 reaper: countable-request clock + lease guard; /ready
does not count), `make binary/binary2/fleet-check`. Remaining: extension
deletion + check.sh binary-default + MANUAL rewrite (the retirement
ceremony), `--ui/--quack` flags, `--boot` units (Phase 3).
Core moves to harbor-core (~90% verbatim); `harbor serve` (UDS+TCP, memory/
thread defaults, `--ui/--quack`); `add/ls/stop/rm` + registry
contract; `/info`; dual-target suite green (extension vs binary — the
compatibility proof), THEN the extension package, Makefile machinery, and
bash launcher are deleted (D5). *Exit: MANUAL.md's install section is
`curl && harbor add`, and the repo builds nothing but the binary crates.*

**Phase 2 — pilot, the REPL daily driver** (~2.5–3 kLOC)
`harbor-protocol` crate; transport + envelope decoder; reedline shell
(`;`-continuation validator, highlighter, history); `sqllex` + vendored
keywords; completion lanes A/B/C; duckbox + P1 modes with head/tail
retention; day-one dot-commands incl. fleet-aware `.databases`; the
`config.toml` address book with token-file/token-cmd (D10a); spawn-on-demand
for file paths (`--idle-exit` berths, D9); spinner; Ctrl-C cancel; timer;
pager. *Exit: you stop reaching for the official CLI against harbor
berths — including `pilot ./file.duckdb` replacing `duckdb ./file.duckdb`.*
→ **COMPLETE 2026-08-16.** All of the above shipped and pty-verified, plus
`.open` (switch berths mid-session), `.read file.sql` and multi-statement
buffers through one quote/comment/dollar-aware splitter, `.keymode vi|emacs`,
and `$PAGER` (default `less -SRFX`) for tall duckbox output. The spinner
rides the cancel ticks — the same `on_tick` that fires DELETE paints
elapsed time, so a berth that hasn't produced headers yet still shows life.
TLS deliberately absent (D6/D8: Caddy owns the edge; ssh is the human path).
`.output/.once/.headers`, progress events, and semantic highlighting remain
Phase 3/4. 1.4MB binary, 9 unit tests, 10 protocol suites green.

**Phase 3 — polish + edge** (~1.5–2 kLOC)
Remaining display modes; `.output/.once/.headers`; result-value highlighting;
bracket matching; hints; progress protocol event (server+client); `--boot`
unit generation; Caddy recipes in MANUAL; `harbor doctor`.

**Phase 4 — PEG smarts** (~3–5 kLOC)
`GET /grammar`; Rust packrat interpreter for the DuckDB PEG dialect (standard
PEG + `List()`/`Parens()` macros + special matcher tokens, per
`src/parser/peg/README.md`) consuming `sqllex` tokens; parse-to-cursor
client-side category completion (kills remote-latency); semantic highlighting
(TABLE_NAME/COLUMN_NAME/SCALAR_FUNCTION); pre-submit error underline.

## 10. Risks & open questions

| # | Risk | Mitigation |
|---|------|-----------|
| 1 | ~~No published duckdb-rs for 2.0-dev; C-API compat unverified~~ **RESOLVED** by spikes/linked2: pinned duckdb-rs drives both 1.5.5 (bundled) and 2.0-dev (`DUCKDB_LIB_DIR`) | Residual: pregenerated 1.5.5 bindings won't expose NEW 2.0 C-API entry points; re-run the spike when rolling the 2.0 build forward |
| 2 | tiny_http UDS support (`http_unix`, graceful `unblock`) unproven for our shutdown path | Phase-0 spike; ~100-line accept-loop fallback acceptable given how little of tiny_http is used |
| 3 | N berths × DuckDB's 80%-RAM default = OOM | Conservative per-berth defaults in 1.0 (D2), printed at startup |
| 4 | `cargo build --workspace` feature-unification foot-gun | `default-members`, Makefile, separate CI jobs |
| 5 | PEG grammar subsystem still moving in 2.0-dev (README paths stale, negative lookahead TODO) | Grammar work stays Phase 4; `/grammar` is compiled-in per build so drift is structural, not operational |
| 6 | UI extension on 2.0-dev is our own unsigned fork (DUCKDB-UI-V2-COMPAT.md) | Unchanged from today; `--ui` implies `allow_unsigned_extensions` on that channel |
| 7 | reedline API churn between minors | Pin; vendor-patch if needed |
| 8 | Windows | Declared TCP-only, no registry; out of scope for fleet 1.0 |

Open questions: does `/info` include the absolute db path when exposed through
Caddy (current answer: yes, but the edge only routes `/sql`+`/catalog` —
policy lives in the Caddyfile)?
Does `harbor rm` delete the database file (current answer: never — it removes
the berth, not the boat)?
