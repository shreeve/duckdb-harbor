# DuckTable design

The founding decisions, and the design rules this codebase starts with.

## Positioning

A deliberately small desktop client for DuckDB: connect to a berth, browse the
catalog, run queries, see results in a fast grid, read EXPLAIN plans, edit
values. No charting, no multi-engine driver matrix, no sync, no plugins.

## Architecture

```
DuckTable (Rust + GPUI, single static binary)
    |  HTTP, token auth (token-file, never argv)
    v
DuckDB Harbor (required; owns engine, files, versions)
    |
    v
DuckDB  -- ATTACH/scanners reach SQLite, Postgres, MySQL, Parquet, CSV, ...
```

- **Harbor is required, not optional.** The client never links DuckDB and
  never sees a database file path. This removes FFI, engine version lock, WAL
  handling, and checkpoint hazards from the client entirely; they are Harbor's
  job.
- **Berths are addressed by name.** Connection UX mirrors `pilot`: a name is
  resolved through Harbor config, spawn-on-demand aliases launch Harbor for a
  local file, and tokens come from `~/.local/state/harbor/runtime/<name>.token`.
  Harbor is MIT and first-party: DuckTable consumes the `wire` protocol crate
  and `harbor-common` (config, paths, berth state semantics) as git
  dependencies pinned by rev, with a `[patch]` for local iteration. The
  HTTP layer (blocking client, NDJSON streaming, chunked decoding) lives in
  DuckTable's own client crate until Harbor hoists it into `harbor-common`.
- **A connected berth is kept alive and its lifetime is respected.** While a
  window holds a connection, DuckTable pulses `GET /keepalive` so an
  idle-exit berth does not retire under an open grid. The lifetime rule is
  pilot's: if DuckTable summoned the berth (spawn-on-demand), closing the
  window lets it retire; if it joined one already running, closing the
  window changes nothing.
- **UI stack**: GPUI (pinned crates.io release, currently 0.2.2) with
  gpui-component (0.5.1) supplying the virtualized Table and the code editor.
  Both are pre-1.0; every gpui-component widget is consumed through a thin
  wrapper module owned by this repo, so a breaking upgrade or a component
  library pivot is a contained diff.

## Design rules

Client architecture:

- **Selection indices are display positions, not row identities.** Any sort
  or per-column value filter makes them diverge. Every read or mutation of a
  selected row resolves through one display-to-identity mapping. Commits are
  keyed by row identity, never by display position.
- **A refresh never clears the cache it is refreshing.** Fetch first, commit
  over the old value. "Has data" and "needs refetch" are separate states; a
  loading flag that discards data is a blank screen.
- **One geometry owner per glyph.** The grid cell renderer and the inline
  editor read position, font, and inset from a single module. Two owners of
  the same pixels always drift.
- **One font rule, one owner.** Every control that shows a stored value uses
  the value font from one theme module. Naming an ad-hoc font "looks right"
  only while settings are at defaults.
- **An async row replacement never silently discards an in-progress edit.**
- **A cancelled connect is not a failed connect.** Cancel updates the UI
  synchronously and fences the in-flight attempt with a token; a late
  completion discards itself instead of clobbering newer state.

DuckDB facts (measured):

- EXPLAIN returns two columns (`explain_key`, `explain_value`); the box art is
  a 2D layout, `FORMAT JSON` carries no cost data, and `EXPLAIN ANALYZE`
  output is not parseable. Render the server's text; do not re-draw plans.
- DDL that depends on a dropped index cannot run inside the same transaction
  (the dependency stays visible in-txn). Index rebuilds are sequential
  auto-commit statements; an aborted transaction rolls back atomically.
- DuckDB v2's PEG parser costs ~2x on parse only; execution is at parity.
  Mitigation when it matters: a prepared-statement LRU.

## Code layout

One file per surface, one owner per piece of state. `app.rs` is the root
entity and the only mutator of connection state (phase, attempt fence,
selection); rendering files (`sidebar.rs`, `content.rs`, and each surface
that follows: tabs, grid, editor, inspector, status bar) read state and
call back into `app.rs` methods, never mutating it themselves. All colors
resolve through `theme.rs`; no other file names one. `main.rs` is only
the entry point.

The rule this protects: a single type that owns every surface decays into
an extension-file sprawl nobody can navigate, and a surface that mutates
shared state from inside a render callback is how two owners of one fact
are born. When a surface's file grows past a few hundred lines, it splits
by subsurface (grid: rendering, editing, selection), not by line count.

## Roadmap

1. **Shell**: window, berth picker (names from Harbor config), connect flow
   with cancel, status line.
2. **Catalog**: schema tree from Harbor `/catalog`.
3. **Grid**: starts with the wide-table probe (see COMPONENTS.md), then
   results in the wrapped virtualized Table; value font; NULL
   presentation; copy.
4. **Editor**: SQL editing with DuckDB keywords, Cmd+Enter to run, statement
   splitting server-side.
5. **EXPLAIN**: plan pane rendering the two-column text.
6. **Edits**: inline cell editing, identity-keyed commits, save preview.

Each phase ships usable. Anything not on this list needs a reason to exist.
