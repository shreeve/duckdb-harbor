<p align="center">
  <img src="https://github.com/shreeve/ducktable/raw/main/social-ducktable.png" alt="DuckTable" width="600">
</p>

# DuckTable

> **A fast, minimal desktop client for DuckDB. Query, browse, and edit your
> data. Nothing else.**

DuckTable speaks to [DuckDB Harbor](https://github.com/shreeve/duckdb-harbor)
and requires it. It never links DuckDB, never opens a database file, and never
asks you for a path. You connect to a berth by name; Harbor owns the engine,
the files, and the versioning. Local files work through Harbor's
spawn-on-demand aliases, the same way `pilot` opens them.

## Screenshots

One window, three views — and three of the built-in themes. Click any
shot for full resolution.

**Structure** — every column with its type, attributes, and defaults,
and the table's DDL below, drawn as the engine would write it. *(Duck
Light theme.)*

[![Structure view](https://raw.githubusercontent.com/shreeve/ducktable/main/docs/shots/01-structure.png)](https://raw.githubusercontent.com/shreeve/ducktable/main/docs/shots/01-structure.png)

**Data** — 27k rows paged 5,000 at a time in 28ms, with row numbers,
right-aligned numerics, and NULL tags each one keystroke away. *(Paper
theme.)*

[![Data view](https://raw.githubusercontent.com/shreeve/ducktable/main/docs/shots/02-data.png)](https://raw.githubusercontent.com/shreeve/ducktable/main/docs/shots/02-data.png)

**Query** — a SQL scratchpad above live results. Statements wear bands
in the gutter, line numbers restart per statement to match the engine's
own `LINE n` diagnostics, and ⌘Enter sends the statement under the
caret. *(Midnight theme.)*

[![Query view](https://raw.githubusercontent.com/shreeve/ducktable/main/docs/shots/03-query.png)](https://raw.githubusercontent.com/shreeve/ducktable/main/docs/shots/03-query.png)

*(GitHub won't run scripts in a README, so the real slideshow lives at
[`docs/shots/index.html`](docs/shots/index.html) — open it locally for
arrow-key navigation, Escape to leave.)*

## Install

```console
$ curl -fsSL https://raw.githubusercontent.com/shreeve/ducktable/main/scripts/install.sh | sh
```

One command, Apple Silicon, no Gatekeeper dialog — the script drops
`DuckTable.app` into `/Applications` from the latest release. (Downloading
the zip in a browser instead will trip Gatekeeper's quarantine; if you go
that way, allow it under System Settings → Privacy & Security → Open Anyway.)

On Intel, or to build from source: clone the repo and run
`scripts/macos-app.sh release`.

## Why Harbor-only

- One wire protocol, one type system, one EXPLAIN dialect.
- No C++ FFI, no bundled DuckDB, no engine version lock. The client is a
  single static binary per platform.
- DuckDB's ATTACH and scanner ecosystem (SQLite, Postgres, MySQL, Parquet,
  CSV, JSON, Iceberg, DuckLake) means one engine already reads most of your
  data. DuckTable inherits all of it without a driver matrix.

## What it is not

No charting. No 30-driver matrix. No sync. No plugin ABI. The "lite" is the
feature.

## Stack

Rust, [GPUI](https://crates.io/crates/gpui) (Zed's GPU-accelerated UI
framework), and [gpui-component](https://github.com/longbridge/gpui-component)
for the virtualized data table and code editor. Dependencies are pinned to
crates.io releases, not git.

## Status

Early releases, moving fast. Browsing works today — the fleet sidebar, the
paged data grid, filters, and the Structure view; editing lands next.
Requires Harbor 0.18+. See `docs/DESIGN.md` for the architecture and roadmap.

## License

MIT.
