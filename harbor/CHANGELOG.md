# Harbor changelog

Harbor release tags use `vX.Y.Z`. Entries are ordered by release date,
newest first. Separately tagged DuckDB engine mirrors are build artifacts, not
Harbor releases, and are not included here.

## 0.36.2 — 2026-09-11

- **A summoned server no longer leaves while a client is still there.** The
  mooring that keeps a spawned server ashore was a connection that opened and
  never sent a request, and such a connection is reclaimed after sixty
  seconds — its silence is indistinguishable from an anonymous caller sitting
  on a descriptor. A repl whose human paused to think, or a backup between
  passes, therefore lost its berth and met `cannot reach harbor: No such file
  or directory` on the next statement. The mooring now asks `/ready` once and
  holds the answered connection, renewing every 240 seconds inside the
  server's 300-second idle clock, so the berth stays for as long as the
  client does. This reaches every client that summons a server: the repl,
  piped and `-c` scripts, `backup`, and `restore`.
- The lifecycle suite holds its mooring past the first-request timeout
  instead of for a few seconds, putting the clock that reclaims a silent
  connection inside what the test can see.

## 0.36.1 — 2026-09-11

- Keeps Windows' native canonical database paths for file access, server
  identity, and configuration. The `\\?\` prefix is hidden only when
  rendering paths in the banner and fleet display, preserving long-path
  support while keeping displayed paths readable.
- Adds a regression covering native canonical paths for existing and new
  databases under long directory names, plus Windows drive/UNC display cases.
  Release builds run the shared path tests on each supported platform.
- Sends the round-trip suite's large SQL fixtures through stdin so Linux
  argument-size limits cannot prevent the backup checks from running.
- Completes the 0.36.0 backup notes and corrects the description of config
  permission and symlink protections.

## 0.36.0 — 2026-09-11

- **A backup reads one snapshot from its first pass to its last.** `backup`
  may export a table again after scanning its first export for blank records
  or deciding that its types need another format. These passes previously
  used separate requests that could see different committed data. They now
  share one transaction. A session opened with `{"purpose":"backup"}` has no
  statement-idle timeout; it lives in a 60-second window that
  `POST /sql/sessions/<id>/renew` extends, and the client renews it from a heartbeat every twenty seconds,
  through SQL and file work alike. Renewals travel the control path, so a busy
  worker cannot starve them. Losing the lease aborts the backup, even
  mid-response, rather than resuming on a snapshot the first pass never saw,
  and an expired or released session cannot be revived. Ordinary sessions
  cannot be renewed; `/sessions` reports `renewable`. An older server without
  the route is refused with an explicit ask to upgrade.
- Backup loader rewriting handles quoted schemas and identifiers, apostrophes,
  and newlines using the shared SQL scanner. Loader filenames remain relative
  so a backup restores after its directory is moved. File scans use bounded
  buffers, and table-type metadata is indexed once for the rewrite passes.
- A table whose types cannot round-trip through either supported format is
  refused, including combinations of text-incompatible types and
  parquet-incompatible types. Failed exports remove their incomplete directory.
  Sequence counters remain nontransactional; exact alignment with exported
  rows requires quiescing sequence users.
- The backup heartbeat replaces the ordinary five-minute session ceiling,
  while the operator's statement timeout still limits each export statement.
- **One statement per request, decided before any of it runs.** A body with
  two statements used to run the leading ones and answer for the last; it is
  now refused with `400` and nothing executes.
- **A connection is replaced, not rolled back, before another caller sees
  it.** `ROLLBACK` undoes a transaction and nothing else — a `SET VARIABLE`,
  a temp table, and a `PREPARE` survived it onto the next request.
  Any statement that can leave such state, including one wrapped in
  `EXPLAIN ANALYZE`, now costs the worker a fresh engine connection, and a
  released session gets the same treatment. The parsed-statement cache goes
  with the connection.
- **Operator settings are locked once the berth is up.** Memory, threads and
  spill are fixed at `start`, and DuckDB's own `allowed_configs` and
  `lock_configuration` now enforce it — a wrapped `SET threads` or a `SET
  lock_configuration=false` is refused by the engine rather than by a keyword
  check. Other settings registered at startup remain changeable unless
  initialization imposed stricter locks. Load
  extensions whose settings must be tunable during `--init`; settings that
  arrive later are outside the allowed list.
- **Limits that answer instead of truncating.** A request body over the limit
  is refused with `413` on both endpoints, chunked or not, where before the
  excess was quietly cut off and parsed. The parsed-statement cache keeps at
  most 64 texts and 1 MiB of SQL, and does not retain a statement over 64
  KiB, so a one-off bulk `INSERT` runs but cannot pin every connection's cache.
- **`config.toml` writes take a lock.** Concurrent membership updates each
  land through a lock spanning read, modification, and atomic replacement.
  Temporary files use exclusive creation. On Unix, the lock file is opened
  without following symlinks, owned config directories are secured to `0700`,
  and foreign-owned directories, permission failures, and unsafe config files
  are refused. This does not reject symlinks throughout the entire config path.
- The Windows banner prints `C:\...` rather than the `\\?\C:\...` that
  `canonicalize` returns.
- New `regressions` suite, run on an isolated one-worker server so connection
  reuse is deterministic; `make test` now runs the workspace with all
  features in release mode.

## 0.35.0 — 2026-09-09

- Adds `harbor <db> backup [dir]` and `harbor <new.duckdb> restore <dir>`.
  A `.duckdb` file is only as portable as the engine that wrote it, so copying
  one is a snapshot, not a backup. `backup` writes the durable thing —
  `schema.sql`, `load.sql`, and one tab-separated file per table — greppable,
  diffable, and readable by anything.
- The dialect is three values and one escape: a bare `NULL` is a real null, a
  quoted `"NULL"` is the string, an empty field is an empty string. Quotes are
  always allowed and rarely required — the reader takes a bare field and a
  written `""` the same way, so a backup stays editable by hand.
- One exception, and it is a row of data rather than a matter of taste: a
  one-column table holding an empty string writes an empty LINE, and every CSV
  reader skips those — the row would not come back and nothing would say so.
  `FORCE_QUOTE` takes a column list rather than a predicate, so it is spent per
  FILE: a table whose export contains a blank record is written again with
  every value quoted, and no other file pays for it. The test is the failure
  itself rather than a proxy — a blank record between rows, walked
  quote-aware, so a blank line inside a multi-line value is left alone.
- The backup directory is self-contained. Each `COPY` names its file and
  nothing more, so it can be moved, renamed, copied to another machine or
  committed to a repo and still restore — an absolute path would have nailed
  it to the machine that wrote it.
- `restore` builds a **new** database and refuses one that exists, since a
  restore that can overwrite can be run at the wrong moment and destroy what
  it was meant to protect. It is also where `--block-size` applies, block size
  being fixed at creation. Both verbs act on contents rather than lifetime, so
  they stand alone and combine with no other verb. Retention, rotation,
  scheduling, compression and remote targets stay out: cron, a filesystem and
  `rsync` already do those.
- `backup` takes `--format tsv|parquet` and `--strict`. Neither format holds
  every type and the holes are not the same shape: text loses a `UNION`'s tag
  (the restore then refuses) and retypes a `VARIANT`'s contents (it does not),
  while parquet refuses a negative `INTERVAL` outright and normalises a
  `TIMETZ` to UTC — `12:00:00+02:30` returns as `09:30:00+00`, the same
  instant, a different value, and nothing said. Each hole is the other
  format's solid ground, so a table the chosen format cannot carry is written
  in the other one and named out loud, with `load.sql` recording the format
  per table. `--strict` refuses instead of swapping. The rule under all three
  is the same: never write something that will not come back.
- New `roundtrip` suite: back up and restore every type in the shared corpus,
  a schema of constraints, indexes, views and sequences, the strings that
  attack the format, and a seeded fuzz of random tables — then attach both
  databases and ask DuckDB whether anything differs, rather than reading the
  export back and finding the writer agrees with the writer.
- `--block-size` now also works on the summon — `harbor <db> --block-size 64k
  -c "..."` shapes the database that call creates, instead of the size being
  reachable only through an explicit `start`. A size that reached nothing,
  because a server was already up or the file already existed, says so.

## 0.34.0 — 2026-09-09

- **`USE` outside a session is now refused instead of silently discarded.**
  It sets the current database on the connection it runs on; that connection
  is pooled, and a request carries exactly one statement, so nothing could
  ever follow it there. It reported success and was thrown away, every time.
  The refusal names the session that does persist (`POST /sql/sessions`) and
  the qualified names — `database.schema.table` — that need no session at all.
  Inside a session `USE` is unchanged.
- Adds `--block-size` and the matching `block-size` config key: the block size
  for a database the call CREATES, from 16k to 256k. A block is both the unit
  of allocation and the window a column's compression works in, and the
  default suits few large tables rather than many small ones — a dozen
  near-empty tables cost 9.5 MiB at 256k. It travels as an open-time option
  because a later `SET` cannot reach it.
- `default_block_size` under `[settings]` is now dropped and reported rather
  than emitted as a `SET` that succeeds and changes nothing, and a server
  asked for a block size the file does not have says so — block size is fixed
  when a database is created, so a config naming another one was a wish that
  read like a setting.
- Fixes statements that expand to a group of statements — `COPY FROM DATABASE`
  and `IMPORT DATABASE` — which could not run at all. The result schema was
  read before anything had stepped, and for these the result-producing member
  of the group is not prepared until something does. A failing group statement
  now reports its own complaint rather than the readiness error that was only
  how it surfaced.
- The `unit` suite runs `--workspace --all-features`. `default-members` and two
  off-by-default features had been hiding 101 tests — all of `crates/justhttp`
  and all of the config reader — which compiled but never ran.

## 0.33.1 — 2026-09-06

- A verb typed without a database (`harbor restart`) now names the attached
  databases in its redirect, with the command spelled out for the first one,
  instead of showing only the file form.

## 0.33.0 — 2026-09-06

- Makes `autostart` a service: the login item is loaded the moment it is
  installed, so the server starts now under launchd or systemd, at every
  login, and again after a crash (`KeepAlive` on failure only; systemd
  `Restart=on-failure`). A clean `stop` stays stopped until the next login.
- Adds `restart`, which bounces a database under its login item with a fresh
  read of config.toml, and `autostart off`, which drops the login item and
  leaves a running server alone (`autostart off stop` takes both down).
- Stops a plain `start` or `stop` from removing the login item; only
  `autostart off` and `detach` do.
- Refuses start options on `autostart` and on a login item's `restart`,
  pointing at the `[connection.<name>]` entry a login item actually reads.
- Sends the login item's output to the berth's log under `runtime/log/`.
- Makes `start` at a terminal bring the server up in the background and
  return — under the database's login item when it has one, otherwise as a
  detached process that runs until `stop`. The prompt-as-server helm is
  gone: a prompt is what bare `harbor <db>` opens. Headless `start` still
  serves in place until SIGTERM, and `--foreground` asks for that shape at a
  terminal. `stop` returns once the server has actually gone quiet, so
  `stop` followed by `start` — by hand or inside `restart` — always meets a
  free database.
- Names a database by the config key that lists it, wherever a name is
  derived — the login item, the stopped row, `attach`, `detach` — so an entry
  such as `[connection.warehouse]` pointing at `inventory.duckdb` never grows
  an `inventory` twin and never boots without its settings.
- Fixes the systemd unit, which ordered itself after the target that wants
  it — a cycle systemd resolves by dropping a job — and sends the unit's
  output to the berth's log like the LaunchAgent does.
- Carries `HARBOR_HOME` and the XDG home variables into the login item when
  the installing shell had them set, so the manager's server and the CLI
  agree on where the socket is.
- Makes `start` at a terminal a success when the server is already up, the
  way `systemctl start` treats an active unit; headless it stays a refusal,
  so a manager or a spawn never reads a clean exit as a server it did not
  get. A running server announces the config key that lists it in `/info`,
  so its bare name is the same word whether it is running or stopped.
- Unloads a login item whose server fails to come up within 15s, instead of
  letting the manager retry it every ten seconds until logout; the item stays
  registered for the next login and the next `start`.
- Lists attached databases that are not running as dimmed `stopped` rows in
  bare `harbor`, with the file path and whether a login item will bring them
  back. Their names and footnote numbers now resolve everywhere a running
  one's do, so `harbor medlabs start`, `harbor 2 autostart` and a bare
  `harbor medlabs` all work with nothing running.

## 0.32.1 — 2026-09-05

- Shows the installed Harbor CLI version as a caption joined to the fleet
  table printed by bare `harbor`.
- Adds a `VERSION` column with the version reported by each running database
  server, making processes that still need a restart immediately visible.

## 0.32.0 — 2026-09-04

- Adds generated-column metadata to `/catalog` through `generated` and
  `generationExpression` fields.
- Documents `httpfs` initialization using Harbor's boot-SQL support.

## 0.31.1 — 2026-09-03

- Publishes synchronized Harbor patch packages and versioned installation
  examples for the 0.31 release line.

## 0.31.0 — 2026-09-03

- Makes full `/catalog` row counts exact by counting every table in one
  ordered `UNION ALL` query.
- Adds `/catalog?style=lite` for fast inventory requests without counts,
  columns, constraints, indexes, DDL, or sequences.
- Keeps full catalog responses as the complete schema document with exact
  database and WAL sizes.

## 0.30.0 — 2026-09-03

- Restricts Harbor's TCP listener to IPv4 loopback.
- Aligns configured addresses, sidecar discovery, and `/info` with the
  loopback-only TCP contract.

## 0.29.0 — 2026-09-03

- Establishes the current direct Harbor connection model across the CLI,
  server, clients, configuration, installers, tests, and documentation.
- Simplifies local socket and HTTP connection setup to the endpoint alone.

## 0.28.3 — 2026-09-03

- Fixes supervised TCP startup when endpoint settings are supplied entirely
  by configuration and the service environment.

## 0.28.2 — 2026-09-03

- Prevents a systemd or launchd login item from disarming itself when it
  starts its configured database.
- Moves the test sandbox under `/tmp` so Unix socket paths fit platform limits.

## 0.28.1 — 2026-09-02

- Shows the server's resolved name in the interactive prompt.
- Improves errors when a bare word does not identify a running database.

## 0.28.0 — 2026-09-02

- Adds a URL column to the fleet display whenever a database exposes a TCP
  listener.
- Allows running databases and lifecycle verbs to resolve socket paths, URLs,
  unique names, or fleet footnote numbers.
- Adds consistent uninstall support to release installers.

## 0.27.0 — 2026-09-02

- Adds an optional TCP listener alongside the always-present Unix socket.
- Teaches the HTTP server to operate multiple listeners and records which
  transport accepted each request.
- Reports the active TCP port through `/info` and packages ICU and JSON with
  the DuckDB engine.

## 0.26.1 — 2026-09-02

- Gives an empty fleet the same table frame and headers as a populated fleet,
  followed by a clear “Nothing running” status.

## 0.26.0 — 2026-09-02

- Adds typed per-database configuration for memory, threads, workers,
  statement timeout, temporary storage, access mode, logging, and TCP
  exposure.
- Adds verbatim boot SQL and arbitrary `[connection.*.settings]` values for
  DuckDB and extension configuration.
- Makes explicit starts and autostart consistently honor saved settings while
  on-demand local opens remain socket-based.

## 0.25.0 — 2026-09-02

- Adds the order-independent `start`, `stop`, `attach`, `detach`, and
  `autostart` lifecycle grammar.
- Reports a server's ephemeral lifetime through `/info` so restarts preserve
  how it was started.
- Cleans up obsolete lifecycle surfaces and standardizes current command
  vocabulary across code, installers, and documentation.

## 0.22.0 — 2026-09-01

- Preserves `TIMETZ` UTC offsets in ISO output.
- Preserves union tags at every nested depth.
- Encodes DuckDB's `24:00:00` end-of-day value without wrapping it to
  midnight.
- Single-sources the DuckDB engine pin and hardens release artifact checks.

## 0.21.0 — 2026-09-01

- Moves execution to DuckDB's first-party v2 C API with generated bindings,
  chunk-level streaming, cached parsed statements, and direct value encoding.
- Pipelines fetching and encoding for substantially faster large results and
  compresses responses with negotiated Zstandard.
- Adds support for nanosecond time values, textual `VARIANT` and `GEOMETRY`,
  and structurally encoded tuples.
- Loads `libduckdb` only when database work begins and moves Harbor into the
  shared monorepo under `harbor/`.

## 0.20.0 — 2026-09-01

- Makes one Harbor binary both the command-line client and database server.
- Adds refcounted ephemeral servers that remain available while clients are
  connected and retire after the final client leaves.
- Derives server identity from the database path and removes registry and
  timer-based lifecycle state.
- Loads the DuckDB engine on demand rather than at process launch.

## 0.19.1 — 2026-08-31

- Makes stopping an already-stopped known database idempotent.
- Strengthens the distinction between configured database names and direct
  file paths throughout the prompt and lifecycle commands.
- Adds operator stop holds, centralized sidecar discovery, clearer fleet
  footnotes, and expanded lifecycle tests.

## 0.19.0 — 2026-08-30

- Treats a configured name as a persistent service and a direct path as a
  temporary database session.
- Makes temporary databases identify themselves clearly in fleet output.
- Keeps autostart an explicit property of configured databases.

## 0.18.0 — 2026-08-30

- Reworks fleet management around the `show`, `start`, `forget`, and `doctor`
  commands with shared discovery and plain-language output.
- Expands `/catalog` with database sizes, table row statistics, and engine DDL,
  plus a lightweight inventory form.
- Introduces a shared common crate for configuration, paths, state, and UI
  rules used across Harbor clients.
- Moves release installation into the current user's home on every platform.

## 0.15.0 — 2026-08-29

- Hardens HTTP parsing with bounded request heads and bodies, framing checks,
  connection deadlines, and recoverable listener errors.
- Closes statement-smuggling edge cases involving carriage returns and dollar
  signs inside identifiers.
- Improves `/catalog` index metadata, nested union honesty, map typing, and
  `FLOAT` rendering.
- Strengthens shutdown, readiness, terminal safety, and hostile-input test
  coverage.

## 0.14.0 — 2026-08-27

- Moves user configuration to `~/.config/harbor/config.toml` and runtime fleet
  state to its protected `runtime/` directory.
- Ships an example configuration file in release archives.
- Adds bootstrap installers that migrate a stopped earlier installation and
  enforce private directory permissions.
- Makes database creation explicit and improves Pilot conflict reporting.

## 0.13.2 — 2026-08-26

- Improves Pilot REPL lifecycle handling and completion behavior.
- Makes stress tests tolerate bounded 503 load shedding.
- Reconciles platform documentation with the supported release targets.

## 0.13.1 — 2026-08-24

- Closes the completion menu when statement punctuation or another word
  boundary is typed, preventing stale suggestions from consuming Enter.

## 0.13.0 — 2026-08-24

- Adds a per-connection prepared-statement cache and batches the HTTP response
  hot path, nearly doubling small-statement throughput in benchmarks.
- Reduces allocations in request parsing, result encoding, and response
  framing.
- Adds `version`, `-V`, and `--version` commands to Harbor and Pilot.

## 0.12.0 — 2026-08-24

- Replaces the external HTTP implementation with Harbor's first-party
  `justhttp` HTTP/1.1 crate.
- Preserves wire behavior while matching prior throughput and removes the
  patched HTTP dependency from the tree.

## 0.11.1 — 2026-08-17

- Makes the release installer safe when invoked through `sudo`, keeping
  user-space files owned by the invoking user.
- Updates artifact actions used by the release workflow.

## 0.11.0 — 2026-08-17

- Consolidates the server and CLI into the standalone Harbor binary with
  `wire` and `pilot` companion crates.
- Builds against the official DuckDB 2.0 development engine and removes
  extension-era build machinery.
- Adds self-contained release archives for macOS ARM64, Linux AMD64/ARM64,
  and Windows AMD64/ARM64.
- Adds reproducible engine fetching, release smoke tests, and a separate full
  CI suite.

## 0.9.1 — 2026-08-15

- Adds unique constraints to the `/catalog` schema document.

## 0.9.0 — 2026-08-15

- Adds `GET /catalog` as one structured call for tables, columns, constraints,
  indexes, sequences, views, macros, and attached catalogs.
- Adds scripted, repeatable release assembly for all supported native targets.

## 0.8.2 — 2026-08-13

- Replaces the static health response with `/ready`, which verifies the
  database through the normal query path.
- Adds negotiated single-document JSON responses alongside streaming NDJSON.
- Adds leased sessions for multi-request transactions and cancellation for
  running or timed-out statements.
- Updates the release matrix to macOS ARM64, Linux AMD64/ARM64, and Windows
  AMD64/ARM64.

## 0.8.1 — 2026-08-12

- Adds an interactive REPL that becomes the default at a terminal.
- Adds opt-in request logging with status and end-to-end request duration.
- Improves standalone extension discovery and compatibility across DuckDB
  builds.
- Adds one concise startup summary table and fixes untyped `NULL` decoding on
  DuckDB v2.

## 0.7.0 — 2026-08-11

- Introduces Harbor's HTTP `/sql` server, readiness checks, lifecycle commands,
  graceful checkpointing, and keep-alive connections.
- Adds streaming DuckDB result encoding, transaction cleanup after abandoned
  streams, and broad protocol and resilience tests.
- Publishes native release binaries for Linux AMD64/ARM64, macOS AMD64/ARM64,
  and Windows AMD64.
