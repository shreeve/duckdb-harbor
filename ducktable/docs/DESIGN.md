# DuckTable design

The founding decisions, and the design rules this codebase starts with.

## Positioning

A deliberately small desktop client for DuckDB: connect to a berth, browse the
catalog, run queries, see results in a fast grid, read EXPLAIN plans, edit
values. No charting, no multi-engine driver matrix, no sync, no plugins.

## Architecture

```
DuckTable (Rust + GPUI, single static binary)
    |  HTTP over a local socket or loopback TCP
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
- **Berths are addressed by name.** Connection UX mirrors `harbor`'s own: a
  name resolves to a database, spawn-on-demand launches Harbor for a local
  file, and Harbor 0.20+'s socket discovery replaces registries. Harbor is
  MIT and first-party: DuckTable consumes the `wire` protocol crate and
  `harbor-common` (paths, fleet-state semantics, feature `fleet`) as plain
  path dependencies into the sibling `../../../harbor/crates/*` — the
  monorepo checks the wire contract on both sides of every commit. The
  HTTP layer (blocking client, NDJSON streaming, chunked decoding) lives in
  DuckTable's own client crate.
- **A connected berth is kept alive by presence, not pulses.** Under Harbor
  0.20+ a held connection *is* the keepalive — the old `GET /keepalive`
  route died with the idle-exit machinery it served. The lifetime rule is
  harbor's own: one `start` verb, two lifetimes. A plain start is
  persistent and runs until stopped; an ephemeral start (spawned with
  `HARBOR_EPHEMERAL`, the way opening a database summons one) self-retires
  once its last client disconnects. See Berth lifecycle in Design rules.
- **UI stack**: GPUI (pinned crates.io release, currently 0.2.2) with
  gpui-component (0.5.1) supplying the virtualized Table and the code editor.
  Both are pre-1.0, and the versions are PINNED — an upgrade is a
  deliberate, review-everything event, not a routine bump. View code uses
  gpui-component directly (blanket wrapper modules bought indirection
  without insulation and were dropped); the churn hedge is the pin plus
  the rule that any widget which fights us gets replaced by first-party
  drawing at that call site, the way the grid already owns its selection
  painting and cell borders.

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
  synchronously and fences the in-flight attempt with a generation id; a late
  completion discards itself instead of clobbering newer state.
- **One grid, many sources.** Anything that shows rows IS the Grid — the
  Data view pages a table, the Query view's results pane pages the user's
  statement as a parenthesized subquery. A second "simpler" results table
  is two owners of the same pixels (see rule three). Editability follows
  capability: no catalog structure means no key, no key means no Edits —
  read-only by construction, never by a parallel widget.
- **Preferences are global; stats are per-grid.** Row numbers, NULL tags,
  and alignment describe the reader and are set in exactly one place —
  every grid honors them, self-healing on its next paint. Row counts,
  timings, and page position describe one grid's contents and live on
  that grid; the view's chrome merely displays the active grid's facts.

Berth lifecycle — two verb pairs and a boot flag, each verb matched to
what it changes. The vocabulary is settled and shipped; on-screen labels
stay plain and the nautical harbor/berth stays internal. All of it is
live: drag-in Open (attach+start), Stop, Detach, and the Auto-start
toggle, over harbor's single `start` verb (the old `serve` is gone — see
"kept alive by presence" under Architecture).

- **Two verb pairs, one noun.** *attach/detach* is membership — whether
  a database is on the list, a config entry remembered across quit.
  *start/stop* is running — whether its server is green right now.
  *auto-start* is a property, not an action: the OS (launchd/systemd)
  runs `start` at boot. Verbs act, the noun persists — the parts of
  speech lining up with what is transient vs stored is the sign the
  split is right. Flipping the noun still takes a verb (the toggle), like
  "favorite" the property vs "Favorite it" the act.
- **Open = attach + start.** Bringing a database in (drag-drop, ⌘O) is
  one gesture: add it and run it. There is no bare "attach" affordance —
  dim is a RESTING state reached by stopping or by relaunch, never a row
  someone deliberately creates. A start that fails on a real database
  still attaches (lands dim, shows the error); only a non-database
  attaches nothing.
- **The refcount lives in start/stop, never in attach/detach.** start
  holds a client anchor (refcount++); stop drops it (refcount-- → the
  server departs only if it was the last holder). So stop is safe by
  construction: it closes MY view and can never nuke a server another
  client still holds.
- **`serve` is dead — it is `start`.** The old owned-vs-refcounted CLI
  modes collapse into one verb: `start` runs until `stop`. "Always up
  with nobody present" is not a mode, it is *auto-start* — the OS firing
  `start` at boot (launchd `RunAtLoad`, never `KeepAlive`, or a stop
  would just relaunch). start and stop are symmetric across CLI and REST
  (`harbor <db> start` ↔ POST /shutdown).
- **Status is one signal, rendered per surface.** green = running, dim =
  stopped — the mark carries nothing else. DuckTable paints a dot with a
  plain name beside it; the terminal fleet table tints the NAME instead
  (a dot there would be redundant with the color). Same meaning, form
  suited to the surface — and no anchor/lock glyph: a status mark should
  inform, not decorate.

Motion and feel — the reusable techniques, each minted once, named, and
meant to be reached for anywhere (the umbrella law is EDITING.md's
"content snaps, chrome fades"; durations live in UI.md's Motion):

- **Atomic swap, single writer.** When two visuals must move together
  (selection ring + row wash), one code path mutates both in one frame;
  nobody else writes. Where a library binding would be a second writer, a
  keystroke interceptor runs first and does the whole job (grid.rs).
- **Interceptor before bindings.** gpui dispatches interceptors, then
  bindings, then listeners; a consumed keystroke skips the rest. The tool
  for "this key must NOT do what the widget thinks" — the grid's arrows,
  the query editor's ⌘Enter (query.rs).
- **Generation fence.** Every async producer carries the sequence number
  it was born with; a result may only land if it still matches. Connects,
  table selects, query runs, copy-flash timers — anything late discards
  itself (app.rs, query.rs, copy_button.rs).
- **Three-phase feedback.** For work that is usually fast: change nothing
  for the first beat (~300ms) and swap atomically if it finishes; only a
  slow run earns ticking progress and a faded (never blanked) prior
  state; completion is always one atomic swap (query.rs runs).
- **Ghost width lock.** When a state change alters text weight or
  content, an invisible ghost at the widest variant owns the layout and
  the visible label overlays it — state can never resize chrome
  (chrome.rs seg_tile).
- **Always-present chrome.** Separators and slots never appear or
  disappear — they occupy their pixels in every state and change only
  color/alpha, so nothing can shift (chrome.rs seg_sep).
- **Crossfade.** Two elements stacked on one clock, opposite opacity
  directions, played inside a state window its own timer closes — an
  instant vanish beside a fade-in reads as a glitch (chrome.rs
  crossfade; first user: the copy tile's check → copy revert).
- **Explicit copy, never selection.** Painted labels have no OS
  text-selection, so any copy-worthy value — a database path, DDL, an
  error detail — carries its own self-confirming copy tile (glyph →
  green check "Copied" → crossfade home), never relies on drag-select +
  ⌘C. The widget owns its text, clipboard write, and flash timers; the
  host just drops the entity in (copy_button.rs; used by the DDL block
  and the connected-berth info card's path).

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

1. **Shell**: window, berth picker (names from socket discovery), connect
   flow with cancel, status line.
2. **Catalog**: schema tree from Harbor `/catalog`.
3. **Grid**: starts with the wide-table probe (see COMPONENTS.md), then
   results in the wrapped virtualized Table; value font; NULL
   presentation; copy.
4. **Editor**: SQL editing with DuckDB keywords, Cmd+Enter to run, statement
   splitting server-side.
5. **EXPLAIN**: plan pane rendering the two-column text.
6. **Edits**: inline cell editing, identity-keyed commits, save preview.

Each phase ships usable. Anything not on this list needs a reason to exist.
