<p align="center">
  <img src="https://github.com/shreeve/ducktable/raw/main/assets/AppIcon.iconset/icon_512x512.png" alt="DuckTable" width="180">
</p>

# DuckTable

> **A fast, minimal desktop client for DuckDB. Query, browse, and edit your
> data. Nothing else.**

DuckTable speaks to [DuckDB Harbor](https://github.com/shreeve/duckdb-harbor)
and requires it. It never links DuckDB, never opens a database file, and never
asks you for a path. You connect to a berth by name; Harbor owns the engine,
the files, and the versioning. Local files work through Harbor's
spawn-on-demand aliases, the same way `pilot` opens them.

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
