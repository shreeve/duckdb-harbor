# DuckTable design

What DuckTable is built on, the rules its code keeps, and how its interface
behaves. Editing has its own document, [EDITING.md](EDITING.md), and so does
the Query view, [QUERY.md](QUERY.md); where either goes deeper than this one,
it is the authority. Everything here describes the shipping app unless it sits
under **Planned** at the end.

## Positioning

A deliberately small desktop client for DuckDB: connect to a berth, browse the
catalog, page through tables in a fast grid, run SQL, edit values. No charting,
no multi-engine driver matrix, no sync, no plugins.

## Architecture

```
DuckTable (Rust + GPUI, one macOS app bundle)
    |  HTTP over a local socket or IPv4 loopback TCP
    |  optional app-owned OpenSSH local forward
    v
DuckDB Harbor 0.39 or later (required; owns engine, files, versions)
    |
    v
DuckDB  -- ATTACH/scanners reach SQLite, Postgres, MySQL, Parquet, CSV, ...
```

- **Harbor is required, not optional.** The client never links DuckDB and
  never sees a database file's contents. FFI, engine version lock, WAL
  handling and checkpoint hazards are Harbor's job, not the client's.
- **Berths are addressed by name.** Connection works the way `harbor`'s own
  does: a name resolves to a database, opening a local file spawns Harbor for
  it on demand, and Harbor's socket discovery replaces any registry. DuckTable
  consumes Harbor's `wire` protocol crate and `harbor-common` (features
  `config` and `membership`) as path dependencies on the sibling
  `../../../harbor/crates/*`, so the wire contract is checked on both sides of
  every commit. The HTTP layer (blocking client, NDJSON streaming, chunked
  decoding) is DuckTable's own `harbor-client` crate.
- **A database can be opened by file or added by port.** File → Open Database
  File chooses a DuckDB path. File → Open Database URL saves a sidebar name and
  a Harbor host and port. `localhost` means a direct IPv4-loopback connection;
  any other host means SSH. DuckTable runs `/usr/bin/ssh` directly, forwards a
  free local `127.0.0.1` port to that machine's Harbor loopback port, and keeps
  the tunnel inside the connection's reference-counted lifetime. No survey opens
  SSH; selecting the database does. The last connection clone kills and reaps
  the process. `-S none` makes that process the owner, `BatchMode=yes` keeps
  failures visible rather than interactive, and SSH keepalives detect a dead
  path.
- **A connected berth is kept alive by presence, not pulses.** A held
  connection is the keepalive. The lifetime rule is Harbor's own: one `start`
  verb, two lifetimes. A plain start is persistent and runs until stopped; an
  ephemeral start (the way opening a database summons one) leaves once its last
  client disconnects.
- **UI stack.** GPUI 0.2.2 with gpui-component 0.5.1 supplying the virtualized
  table and the code editor, carrying one local patch in
  `vendor/gpui-component`. Both are pre-1.0 and PINNED: an upgrade is a
  deliberate, review-everything event, not a routine bump. View code uses
  gpui-component directly. A widget that fights us is replaced by first-party
  drawing at that call site, the way the grid owns its selection painting and
  cell borders.

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
  editor read position, font and inset from a single module. Two owners of the
  same pixels always drift.
- **One font rule, one owner.** Every control that shows a stored value uses
  the value font from one theme module. An ad-hoc font "looks right" only while
  settings are at their defaults.
- **An async row replacement never silently discards an in-progress edit.**
- **A cancelled connect is not a failed connect.** Cancel updates the UI
  synchronously and fences the in-flight attempt with a generation id; a late
  completion discards itself instead of clobbering newer state.
- **One grid, many sources.** Anything that shows rows IS the Grid: the Data
  view pages a table, the Query view's results pane pages the user's statement
  as a parenthesized subquery. A second, simpler results table would be two
  owners of the same pixels. Editability follows capability: no catalog
  structure means no identity, no identity means no edits. Read-only by
  construction, never by a parallel widget.
- **Preferences are global; stats are per-grid.** Row numbers, NULL tags and
  alignment describe the reader and are set in exactly one place; every grid
  honors them on its next paint. Row counts, timings and page position describe
  one grid's contents and live on that grid; the view's chrome only displays
  the active grid's facts.

### Berth lifecycle

Two verb pairs and a boot property, each matched to what it changes. On-screen
labels stay plain; the nautical harbor/berth vocabulary stays internal.

- **Two verb pairs, one noun.** *Attach/Detach* is membership: whether a
  database is on the list, a config entry remembered across quit.
  *Start/Stop* is running: whether its server is green right now.
  *Autostart* is a property, not an action: the OS runs `start` at login.
- **Open = attach + start.** Bringing a database in (drag-drop, ⌘O) is one
  gesture: add it and run it. A start that fails on a real database still
  attaches (the row lands dim and shows the error); only a file that is not a
  database attaches nothing.
- **Stop stops the server.** It sends `POST /shutdown`, as `harbor <db> stop`
  does, so the server stops for every client, not only this window. The row
  goes from green to dim, or leaves the list if the server was ephemeral, and a
  view that was showing it returns to idle. A remote row has no Stop: removing
  it forgets the route and closes the tunnel without a shutdown.
- **Autostart restarts only a crash.** Under launchd the login item sets
  `RunAtLoad` and `KeepAlive` on unsuccessful exit only; under systemd it is
  `Restart=on-failure`. A deliberate stop stays stopped.
- **Status is one signal, rendered per surface.** Green is running, dim is
  stopped, and the mark carries nothing else. DuckTable paints a dot beside a
  plain name; the terminal listing tints the name instead.

### Motion and feel

Reusable techniques, each named once and reached for anywhere. The umbrella
law is EDITING.md's "content snaps, chrome fades"; durations are under
**Motion** below.

- **Atomic swap, single writer.** When two visuals must move together
  (selection ring and row wash), one code path mutates both in one frame and
  nobody else writes. Where a library binding would be a second writer, a
  keystroke interceptor runs first and does the whole job (grid.rs).
- **Interceptor before bindings.** GPUI dispatches interceptors, then
  bindings, then listeners, and a consumed keystroke skips the rest. It is the
  tool for "this key must not do what the widget thinks": the grid's arrows,
  the query editor's ⌘Enter (query.rs).
- **Generation fence.** Every async producer carries the sequence number it
  was born with, and a result lands only if it still matches. Connects, table
  selects, query runs and copy-flash timers all discard themselves when late
  (app.rs, query.rs, copy_button.rs).
- **Three-phase feedback.** For work that is usually fast: change nothing for
  the first beat (about 300ms) and swap atomically if it finishes; only a slow
  run earns ticking progress and a faded, never blanked, prior state;
  completion is always one atomic swap (query.rs runs).
- **Ghost width lock.** When a state change alters a label's weight or text,
  an invisible ghost of the widest variant owns the layout and the visible
  label overlays it, so state never resizes chrome (chrome.rs `seg_tile`).
- **Always-present chrome.** Separators and slots never appear or disappear.
  They occupy their pixels in every state and change only color or alpha
  (chrome.rs `seg_sep`).
- **Crossfade.** Two elements stacked on one clock with opposite opacity,
  played inside a state window its own timer closes. An instant vanish beside
  a fade-in reads as a glitch (chrome.rs `crossfade`).
- **Explicit copy, never selection.** Painted labels have no OS text
  selection, so any copy-worthy value (a database path, DDL, an error) carries
  its own copy tile: glyph, then a green "Copied" check, then a crossfade home.
  The widget owns its text, clipboard write and timers (copy_button.rs).

## Interface

### Window

Three panes: the sidebar, the content, and the inspector. The content shows
one of three views of the selected table, **Structure | Data | Query**, chosen
by the switcher at the left of the bottom bar, by ⌘1/⌘2/⌘3, or by the ⌥←/⌥→
carousel. Data is the default. The inspector opens beside the Data grid only.

The sidebar width, the inspector's open state and width, the Structure view's
columns/DDL divider and the Query view's editor/results split all persist.

On-screen vocabulary is the user's, not Harbor's: the sidebar says DATABASES
and TABLES although the code says berth.

Menus:

- **File:** Open Database File…, Open Database URL…
- **Edit:** New Row, Duplicate Row, Delete Row, which act on the Data grid
  (EDITING.md).
- **View:** Structure, Data, Query; Refresh Tables; Previous and Next Table;
  Row Numbers, Right-Align Numbers, NULL Tags; Toggle Inspector; Zoom In, Zoom
  Out, Actual Size; Fit Column Widths; Toggle Full Screen.

### Sizing

Rigidity is part of feeling native: content pushes back instead of flowing,
and nothing mushes the way a web page does. Every surface answers "how big,
and what happens when content overflows?" from these rules, in order:

1. **A component class declares its size as a design decision.** Runtime
   content never drives layout: one long value must not inflate a card.
   Deriving a size from content is legitimate only for closed, design-time
   sets (a label column sized to its longest label), never for user data.
2. **Siblings presented in the same slot share the slot's size**, so swapping
   content never reflows the frame. Every berth presents in the same
   440pt-minimum identity card, so switching berths moves nothing.
3. **Content adapts within the bounds, by its kind.** A value ellipsizes on
   one line, with the full text recoverable somewhere deliberate (tooltip,
   inspector, copy), and a path truncates in the middle because its filename is
   the payload. Prose meant to be read whole (errors, empty states) wraps.
   Collections (lists, the grid, the editor) scroll; a single long item never
   does. A bounded container may grow with content to a declared cap, then
   scroll.
4. **The window minimum is the sum of the floors** along the widest required
   chain, never a free-standing number. When space runs out, the response is a
   deliberate state change (a pane collapses), not gradual squeezing.

Grid columns add one floor to content fitting: a column fits the draft hint it
can display, pill and cell insets included, so `DEFAULT`, `REQUIRED`,
`GENERATED` and `NULL` stay whole at every zoom and cannot be clipped by
dragging a divider. The floor comes from catalog metadata before a draft
appears, so adding a row never moves columns.

### Sidebar

Two stacked sections.

**DATABASES** lists every berth Harbor knows: live sockets found by discovery,
databases on the list, and saved remotes. Each row has a status dot, the table
count in parentheses when it is running, and its size on disk right-justified
in decimal units (MB, GB). A filter field appears once the list passes ten
rows. Clicking a row opens the berth; the attempt is fenced, so a late
completion discards itself and a cancel works at once. Right-click offers,
per axis, only the move that applies: **Start** or **Stop**, **Attach** or
**Detach**, and the **Autostart** checkmark. A tunneled row says "Connects
over SSH to <host>" in its tooltip, and its menu offers **Remove Database**.

**TABLES** shows the connected berth's tables from Harbor's `/catalog` as a
flat list, with schema headings only when there is more than one schema. Each
row carries its column count and an SI-rounded row count (13k, 4.6M).
Sequences follow in their own SEQUENCES section. A filter field appears past
ten tables. **Refresh Tables** (⌘R, or the View menu) refetches the catalog and
the open Data page and swaps them in one frame; it never blanks the tree, and a
failed refresh keeps the old one. A successful commit and every completed
Query run refresh the same way. When refreshes overlap, only the newest may
replace a snapshot.

A single click selects a table and keeps the current view. A double click
selects it and switches to Data.

### Grid

Virtualized in both axes. Row numbers show absolute positions: page 2 at 500
per page starts at 501. NULL renders as a dimmed tag, distinct from the empty
string. Every value renders in the one value font, and the inline editor
shares the cell's exact geometry, so entering an edit moves nothing.

Rows arrive in explicit server-side pages. The size defaults to 500 and cycles
500, 5,000, 50,000, a decade apart so each step is a different kind of read. A
fetched page replaces the rows in one frame, never appended or stitched, and
the client never holds an unbounded result. The status line tells a known
total ("1–500 of 5,410 rows") from an unknown one ("1–500 rows").

Display preferences are global: row numbers, right-aligned numbers and NULL
tags, toggled from the grid's header strip, the View menu, or ⌘7/⌘8/⌘9, and
kept in `~/.config/ducktable/prefs.json`.

The Structure view shows the table's columns (key and NOT NULL chips,
defaults) and its DDL, all from the `/catalog` document. Structure and Data
never render side by side: a schema change reshapes the data.

What can be edited, and how, is EDITING.md.

### Bottom bar

The view switcher sits at the left. The Data view adds the raw-SQL filter
toggle, the Columns popover (search past ten columns, Show all and Hide all,
full-row click targets) and the Add Row button. The right-anchored status line
reads `1 ms · 1–500 of 5,410 rows · 9 columns · |< < 500 per > >|`.

The order is the anti-jump rule: in a right-justified cluster an element moves
only when something to its right changes width. So the pager, the only
interactive element, is rightmost, with constant-width glyphs pinned to the
corner, and neither a page flip nor a table switch moves a click target. The
column count sits beside the row range it describes.

The filter is one raw SQL `WHERE` strip under the header, applied on Enter,
which refetches page 1 with a fresh count.

### Inspector

Row-level only: the selected row's values, shown vertically, read-only. Berth
facts keep their own homes (versions, path and size on the berth identity card;
row counts in the status line), so two levels of urgency never share a pane.
The inspector sits beside the table below the header strip, so opening it
never shifts the title row. ⌘I toggles it, as does the panel glyph on the
header strip.

### States

Every surface defines empty, loading and failed, and none of them is a blank
pane. Empty schemas, tables and results say so in one line. Errors carry their
detail in a copy tile, since painted labels cannot be selected. Timing labels
engine time explicitly.

### Keyboard

The app-level keys. The Data grid's full grammar (typing, Enter, Tab,
arrows, ⌘S, ⌘Z, ⌘⌫, ⌃⇧N, ⌘⇧⌫) is EDITING.md's; the Query view's is QUERY.md's.

| Key | Does |
| --- | --- |
| ⌘O | Open Database File |
| ⌘R | Refresh Tables: the catalog and the open Data page |
| ⌘1 / ⌘2 / ⌘3 | Structure / Data / Query |
| ⌥← / ⌥→ | previous / next view, rolling over at the ends |
| ⌘N / ⌘D | New Row / Duplicate Row |
| ⌘I | toggle the inspector |
| ⌘7 / ⌘8 / ⌘9 (or ⌥7 / ⌥8 / ⌥9) | row numbers / right-aligned numbers / NULL tags |
| ⌘= / ⌘- / ⌘0 | zoom in / out / actual size |
| ⌘⇧F | fit column widths |
| ⌃⌘F | full screen |
| ⌘Q | quit |

### Appearance

Modern macOS warmth rather than editor minimalism: an accent that does real
work (selection, focus), soft hierarchy between panes, rounded controls. The
grid and editor stay compact and monospaced in every theme; the chrome around
them carries the personality.

Every color resolves through semantic tokens in `theme.rs`, and no other file
names one. The themes are one ThemeSet file, `assets/themes/ducktable.json`:
Duck Light, Duck Dark, Paper, Midnight and High Contrast, each with a matching
syntax-highlight theme. Value rendering (NULL tags, staged cells) uses tokens
too, so every theme keeps the same meanings.

### Motion

Animation is felt, not seen: a transition exists to prevent a jarring snap,
never to announce itself. If a user notices "there's an animation here," it is
too slow. Durations follow motion perception, scaled to the size of the thing
moving:

- **About 100ms** is the floor. Below it, a fade reads as a hard cut.
- **120–200ms** is for an in-place micro-fade: an icon swap, a hover, a copy
  tile's check reverting. **150ms is the one default**, shared as
  `chrome::QUICK_FADE_MS` so these cannot drift apart. 350ms, tried first for
  the copy tile, read as "look, a crossfade", the tell that it was twice too
  slow.
- **200–250ms** is for structural enter and leave: a whole row fading out of
  the list, the layout reflowing. A departure is a bigger motion than an
  in-place swap and the eye tracks it, so it runs a little longer on purpose.
  The departing-row fade is 220ms, and this is the one reason a fade is not the
  shared 150.
- **250–300ms** suits larger moves only: a pane, a card, a sheet.
- **Over 400ms** feels laggy for anything small.

Two things look like durations but are not motion, and keep their own numbers:

- A continuous **loop**: a spinner's full turn at about 800ms reads as
  "working" without spinning frantically.
- A **dwell**: how long a "Copied" confirmation holds before it reverts, about
  1.2s. It is a readability budget: the person copied and is looking right at
  it, so it need not linger, and longer starts to read as stuck.

The constants live at their use sites: copy_button.rs (`FADE_MS` is
`QUICK_FADE_MS`, `HOLD_MS` is 1200), the berth stop spinner (an 800ms turn with
a 150ms fade-in), and the departing-row fade (220ms).

## Components

gpui-component supplies what fits directly: the virtualized table (delegate
based, column resizing, loading states), the code editor, menus, popovers,
dialogs, resizable splits, the theme registry and the platform title bar.

What we own on top of it:

1. **The grid's editing layer.** Inline editing, the shared cell and editor
   geometry, staged-cell marks and the staging pipeline, built on the delegate
   API. It is the app's core engineering and follows the rules above.
2. **SQL highlighting.** The editor's tree-sitter highlighter bundles no SQL
   grammar, so the first-party `duckdb-lang` crate (tree-sitter-duckdb, see
   QUERY.md) is registered with it.
3. **Small custom drawing.** Berth status dots, NULL tags, draft hints, the
   segmented switcher.

**Column virtualization is measured, not assumed.** Both axes virtualize: rows
through `uniform_list`, columns through `virtual_list`, with `render_td` called
only for visible cells. The probe, `crates/ducktable/examples/wide_probe.rs`,
is a self-driving 500-column by 100,000-row table that sweeps six scroll
patterns and prints frame-time statistics. Re-run it on every gpui-component
upgrade:

```
cargo run --release -p ducktable --example wide_probe
```

On an M-series Mac at 60 Hz, release build, every phase held the 16.7ms vsync
interval: p50 16.7ms, p95 no worse than 17.6ms, and no frame over 33ms in any
phase, random jumps on both axes at once included. RSS was 128.9 MB with no row
data stored, the framework-plus-window baseline. The table is the grid's
display and scroll foundation, and no column-windowing wrapper is needed.

## DuckDB facts (measured)

- EXPLAIN returns two columns (`explain_key`, `explain_value`). The box art is
  a 2D layout, `FORMAT JSON` carries no cost data, and `EXPLAIN ANALYZE`
  output is not parseable. Render the server's text; do not redraw plans.
- DDL that depends on a dropped index cannot run inside the same transaction
  (the dependency stays visible in the transaction). Index rebuilds are
  sequential auto-commit statements; an aborted transaction rolls back
  atomically.
- A rowid names a position, not a row. A checkpoint that compacts deleted rows
  renumbers the rest, which is why a keyless table's identity pairs the rowid
  with a hash of the row (EDITING.md).

## Code layout

One file per surface, one owner per piece of state. `app.rs` is the root entity
and the only mutator of connection state (phase, attempt fence, selection).
The surfaces (`sidebar.rs`, `content.rs`, `grid.rs`, `structure.rs`,
`inspector.rs`, `query.rs`, `footer.rs`) read state and call back into `app.rs`
methods, never mutating it themselves. `edits.rs` is the staging model and
`sql.rs` every hand-written query; both are pure. All colors resolve through
`theme.rs`. `main.rs` is the entry point, menus and key bindings.

The rule this protects: one type that owns every surface decays into an
extension-file sprawl nobody can navigate, and a surface that mutates shared
state from inside a render callback is how two owners of one fact are born.
When a surface's file grows past what one reader holds, it splits by
subsurface (grid: rendering, editing, selection), not by line count.

## Planned

Designed, not built. Nothing shipping depends on these, and nothing shipping
blocks them.

- **Tabs.** One tab strip, each tab bound for life to the berth it was opened
  on: a table tab or a query tab. ⌘T opens a query tab, ⌘W closes, and the
  number keys move from the views to the tabs.
- **Notebook.** A vertical stack of independent query panes, each an editor
  above a result grid, sharing one berth session so temp tables and macros
  carry between panes. An optional run-all goes top to bottom as a
  convenience, never as enforced dataflow. A notebook is a named, saved
  document that opens as an ordinary tab.
- **Grid.** Header sorting (click cycles ascending, descending, none;
  shift-click adds a key with a priority number), a follow arrow on
  single-column foreign keys, a JSON view beside Data and Structure, and
  per-column structured filters over the raw strip.
- **Live edit mode.** Each confirmed cell runs a single-row conditional
  UPDATE, matching both the identity and the value the grid last saw; zero rows
  affected is a conflict that shows both values and takes the server's. It is
  deferred until it clears an adversarial review (EDITING.md).
- **Inspector and Structure editing.** A row editor in the inspector that
  shares the grid's one editing session, and a schema editor in Structure.
- **Catalog.** Views, macros and attached catalogs in the TABLES tree once
  `/catalog` carries them.
- **Other platforms.** Linux and Windows builds with native window chrome and
  menus, and per-platform keymaps rather than a blind ⌘-to-Ctrl swap.
