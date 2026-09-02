<p align="center">
  <a href="harbor/"><img src="harbor/duckdb-harbor-social.png" alt="DuckDB Harbor" width="600"></a>
</p>

<p align="center">
  <b><a href="harbor/">DuckDB Harbor</a></b> — many clients, one DuckDB, over plain HTTP.<br>
  One small binary that serves a DuckDB file to everything, and is its own modern shell.
</p>

<p align="center">
  <a href="ducktable/"><img src="ducktable/social-ducktable.png" alt="DuckTable" width="600"></a>
</p>

<p align="center">
  <b><a href="ducktable/">DuckTable</a></b> — the native macOS desktop face for Harbor servers.<br>
  Query, browse, and edit your data. Nothing else.
</p>

---

Two products, one repo. DuckTable requires Harbor and builds Harbor's protocol
crates from the tree beside it, so the wire contract is checked on both sides
of every commit. Each directory is its own Cargo workspace with its own
version and releases: harbor tags are `v*`, DuckTable tags are `ducktable-v*`.

Install harbor:

```sh
curl -fsSL https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/install.sh | bash
```

Install DuckTable (macOS, Apple Silicon):

```sh
curl -fsSL https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/ducktable/scripts/install.sh | bash
```

MIT.
