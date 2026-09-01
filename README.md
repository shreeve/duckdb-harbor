# DuckDB Harbor

Only one process can open a DuckDB file at a time. This repository ships the
two programs that fix that, together because they are built together:

- **[`harbor/`](harbor/)** — a small server that puts a DuckDB file behind
  plain HTTP so all your apps can share it, and a DuckDB-style shell in the
  same 2MB binary. Many clients, one DuckDB. Start with
  [harbor/README.md](harbor/README.md).
- **[`ducktable/`](ducktable/)** — the native macOS desktop face for those
  servers: a fast, minimal table client built on GPUI. Start with
  [ducktable/README.md](ducktable/README.md).

Each directory is its own Cargo workspace with its own version and releases;
DuckTable's client crates build Harbor's protocol crates from source, so the
wire contract is checked on both sides of every commit.

Install harbor:

```sh
curl -fsSL https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/install.sh | bash
```

MIT.
