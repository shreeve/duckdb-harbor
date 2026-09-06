# Harbor changelog

Harbor release tags use `vX.Y.Z`. Entries are ordered by signed tag date,
newest first. Separately tagged DuckDB engine mirrors are build artifacts, not
Harbor releases, and are not included here.

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
