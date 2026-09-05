# DuckTable UI specification

The layout follows the proven three-pane database-client shape: sidebar,
content with tabs, inspector. What differs from the incumbents is what
Harbor-native and DuckDB-only make possible: berths instead of connection
forms, DuckDB's real object model instead of a lowest common denominator,
and a spreadsheet-style live edit mode built on explicit safety rules.

## Window layout

```
+------------------------------------------------------------------------+
| [sidebar toggle]  berth pill (o)   breadcrumb        [timing] [inspect] |
+----------------+-------------------------------------------+-----------+
| DATABASES      | tab: Invoice | tab: query 1 | +           | ROW       |
|  o medlabs     +-------------------------------------------+           |
|  o scratch     | #  InvoiceId  CustomerId  InvoiceDate     | (selected |
|  - archive     | 1  1          2 ->        2021-01-01 ...  |  row's    |
|                | 2  2          4 ->        2021-01-02 ...  |  values,  |
| TABLES         | 3  3          8 ->        2021-01-03 ...  |  shown    |
|  Invoice   13k |                                           |  verti-   |
|  Track    3.5k |                                           |  cally)   |
|  ...           |                                           |           |
|  SEQUENCES     +-------------------------------------------+           |
|                | Data|Structure  filter columns            |           |
|                |        1 ms . 1-500 of 5,410 rows .       |           |
|                |        9 columns . |< < 500 per > >|      |           |
+----------------+-------------------------------------------+-----------+
```

The diagram is the working target: the tab strip, the JSON view, and
the edit modes below are roadmap, not shipped. On-screen vocabulary is
the user's, not Harbor's: the sidebar says DATABASES and TABLES even
though the code and docs keep saying berth internally.

Three vertical panes. Sidebar and inspector collapse independently;
divider positions persist (the inspector width saves at the end of each
drag). Minimum window 720x420.

Collapsing is offered three redundant ways, all reaching the same
state: toolbar toggle buttons at each end using the split-rectangle
panel glyph (the Finder/Xcode convention; never a hamburger, which
reads as a menu, and never a bare chevron, which reads as navigation),
with the shaded side naming the pane and an active tint when hidden;
dragging a divider past the pane's minimum snaps it closed, and
double-clicking a divider toggles; and Cmd+0 (sidebar) / Cmd+Alt+0
(inspector). Collapse animates briefly; state persists per window.

Keyboard shortcuts are written as Cmd here; on Linux and Windows every
Cmd reads as Ctrl.

The File menu offers the two pieces of information a user can start with.
**Open Database File…** chooses a DuckDB path. **Open Database URL…** asks for a
sidebar name, host, and port. Host defaults to `localhost`, which
DuckTable resolves explicitly to IPv4 `127.0.0.1` and connects to directly. Any
other host tells DuckTable to create an SSH tunnel to that host and forward its
Harbor loopback port. A tunneled row remains a normal DATABASES row and says
“Connects over SSH to <host>” in its tooltip. Its context menu says **Remove
Database**, which forgets the saved route and closes its tunnel without sending
a shutdown request to the database server.

The Edit menu owns Data-grid row operations. **New Row** (Cmd+N) creates an
all-DEFAULT draft and enters its first useful writable cell. **Delete Row**
(Cmd+D) stages the selected row for deletion. The row remains visibly ghosted
and undoable until the final Cmd+S commit boundary.

## Sizing

Rigidity is part of feeling native: content pushes back instead of
flowing, and nothing ever mushes the way a web page does. Every surface
answers "how big, and what happens when content overflows?" from these
rules, in order:

1. **A component class declares its size as a design decision.**
   Runtime content never drives layout: one long value must not inflate
   a card. Deriving a size from content is legitimate only for closed,
   design-time-known sets (a label column sized to its longest label),
   never for user data.
2. **Siblings presented in the same slot share the slot's size**, so
   swapping content never reflows the frame. The berth identity card is
   the reference case: every berth presents in the same 440pt-minimum
   card, so switching berths moves nothing.
3. **Content adapts within the bounds, by its kind.** A value
   ellipsizes on one line, with the full text recoverable somewhere
   deliberate (tooltip, inspector, copy) and middle truncation
   preferred for paths, whose filename is the payload. Prose meant to
   be read in full (errors, empty states) wraps. Collections (lists,
   trees, the grid, the editor) scroll; a single long item never
   scrolls. A bounded container may grow with content to a declared
   cap, then hand off to scroll.
4. **The window minimum is the sum of the floors** along the widest
   required chain, never a free-standing number. When space truly runs
   out, the response is a deliberate state change (a pane collapses),
   not gradual squeezing.

Grid columns add one semantic floor to content fitting: a column must fit the
draft hint it can display, including the pill and cell insets. `DEFAULT`,
`REQUIRED`, `GENERATED`, and `NULL` therefore remain whole at every zoom and
cannot be clipped by dragging a divider narrower. The floor is known from
catalog metadata before a draft appears, so adding a row never moves columns.

## Sidebar

Two stacked sections, each collapsible.

**DATABASES** lists every berth Harbor knows: live sockets found by
discovery plus spawn-on-demand databases, each with a **status dot —
green running, dim stopped** (one signal, nothing more; the full model
is DESIGN.md's Berth lifecycle), the table count in parentheses for live
berths, and the size on disk right-justified in decimal units (MB/GB,
never MiB). Click opens the berth (attach + start); the attempt is
fenced, so a late completion discards itself and a cancel works
synchronously. Right-click carries the verbs matched to the two axes,
each landing on its own transition (DESIGN.md, Berth lifecycle). The menu
shows, per axis, only the move that applies: **Start** or **Stop**
(running — Start summons the server, Stop drops this window's anchor,
green→dim), **Attach** or **Detach** (membership — add to or remove from
the list, the row fading out on Detach), and the **Auto-start** toggle
(the boot property, a single checked item). Plain verbs only; the nautical vocabulary is frozen at harbor, berth —
and stays INTERNAL: on-screen labels use user words (DATABASES, TABLES,
"database").

**TABLES** shows the connected berth's tables from Harbor's catalog
endpoint as a flat list (schema headings appear only when there is more
than one schema), each row carrying its column count in parentheses and
an SI-rounded exact row count right-justified (13k, 4.6M). Sequences
follow in their own SEQUENCES section. Views, macros, and ATTACHed sibling
catalogs join
the tree when the catalog endpoint carries them. Each section header
has its own filter glyph — a filter field only appears once a section
exceeds 10 items — and a refresh glyph. Refresh refetches then swaps in
one frame; it never blanks the tree while loading, and a failed refresh
keeps the old tree unchanged. **Refresh Tables** is also available from
the View menu and with Cmd+R; all three entrances perform the same action.
A successful staged-edit commit and every completed Query run perform that
same refresh automatically, so row counts and schema changes land without a
second command. If refreshes overlap, only the newest response may replace
the catalog snapshot.

Single click selects; Enter or double-click opens a table in a new tab
(or focuses the already-open tab for that table).

## Tabs

One tab strip, drawn by us. Every tab is permanently bound to the berth
it was opened on and shows that berth when it is not the active one.
Switching berths in the sidebar never retargets an existing tab; a query
always runs against the berth its tab was born on. A tab is a surface:

- **Table**: the grid, opened from the catalog tree.
- **Query**: SQL editor above, results grid below, one-shot EXPLAIN (⌘⇧E).
- **Notebook** (later, not in the v1 build): a vertical stack of
  independent query panes, not a run-in-order document. Each pane is a
  collapsible SQL editor above a flexible result grid; either half
  expands or collapses on its own. A "+" adds a pane above or below.
  Panes run individually; all panes share the notebook's one berth
  session, so temp tables and macros defined in one pane are usable in
  another, and an optional run-all executes top to bottom as a
  convenience, never as enforced dataflow. A pane may later gain a
  third collapsible view (a chart) without changing the model. A
  notebook is a named, saved document, listed in a Notebooks sidebar
  section under Catalog; opening one makes an ordinary tab in the one
  tab strip. There is no second-level tab hierarchy. Panes reuse the
  same editor and grid components as the other surfaces; grids follow
  the same editability rules. Nothing in v1 blocks this and nothing in
  v1 depends on it.

Cmd+T new query tab, Cmd+W closes, Cmd+1..9 select. A tab with staged
edits or unsaved query text shows a dot and confirms before closing.
Query text is preserved across restart (session restore); staged edits
are not, and the close/quit confirmation says so.

## Grid

Virtualized in both axes; scale target is wide analytics results. Row
numbers on the left show ABSOLUTE positions (page 2 at 500/page starts
at 501). Header sorting (click cycles asc/desc/none, shift-click adds
with priority numbers) is roadmap; the visible→schema column map is
already shaped for it. NULL renders as a dimmed tag, distinct from
empty string. All values render in the one value font; the inline
editor will share the cell's exact geometry so entering edit mode moves
nothing.

Rows fetch in explicit server-side pages. The size defaults to 500 and
cycles 500 / 5,000 / 50,000 — one decade apart, so each step is a
different kind of read. A fetched page REPLACES the rows in one frame
(never appended, never stitched), and the client never materializes an
unbounded result. The status distinguishes a known total ("1–500 of
5,410 rows") from an unknown one ("1–500 rows").

A cell whose column has a single-column foreign key with resolvable
metadata shows a follow arrow that opens the referenced row (roadmap);
when the metadata is absent or composite the arrow simply does not
appear.

Bottom bar per table: the Data | Structure view switcher (JSON view
later), the raw-SQL filter toggle, the Columns popover (search past 10
columns, Show all / Hide all, full-row click targets), the Data view's
Add Row button (a staged draft appears above the fetched page), and the
right-anchored status line: `1 ms · 1–500 of 5,410 rows · 9 columns ·
|< < 500 per > >|`. The ordering is the anti-jump rule: in a
right-justified cluster an element only moves when something to its
RIGHT changes width — so the pager, the only interactive element, is
rightmost (constant-width glyphs pinned to the corner; neither page
flips nor table switches move the click targets), the column count
sits beside the row range it describes, and the per-page text stays
leftmost. The filter is one
raw SQL WHERE strip under the header (applied on Enter, refetching page
1 with a fresh count); structured per-column filters layer on later.

Display preferences are global, not per-table: row numbers, numeric
right-alignment, NULL-tag visibility. They live as a quiet toggle
cluster on the right of the grid header strip (title left, toggles and
the inspector glyph right), each with a tooltip, persisted in
`~/.config/ducktable/prefs.json`. They sit on the grid rather than in
the sidebar because controls live nearest what they change. Structure
shows the table's columns (PK and NOT NULL chips, defaults) and DDL;
the two views are exclusive — a schema change reshapes the data view,
so they never render side by side.

### Editability

Editing follows capability, never appearance. A grid is editable only
when Harbor reports the object is writable and there is a stable row
identity: a primary key, or DuckDB rowid where it exists. Every UPDATE
and DELETE is keyed by that identity, never by display position, and is
refused client-side if it would not affect exactly one row. Views,
results of arbitrary queries, and read-only attached sources render with
editing absent, not disabled-and-surprising.

Cell entry rules: typed text parses against the column type; invalid
input keeps the editor open with an inline error, never silently coerces
or reverts. NULL is entered by an explicit control (and distinct from
the string "NULL" and from empty). Nested types (LIST, STRUCT, MAP)
open in the inspector's row editor rather than the inline cell.

### Edit modes

The mode is chosen per tab, defaults to staged, and is remembered per
berth as the default for new tabs. It is always visible in the bottom
bar and the status line.

- **Staged**: edits, inserts and deletes accumulate locally, keyed by
  row identity. The change list previews parameterized statements (not
  string-assembled SQL); Cmd+S commits them in one transaction,
  all-or-nothing. On failure the transaction rolls back, the local
  changes remain intact, and the error names the failing row. Esc
  discards a cell; the list discards any row.
- **Live**: every confirmed cell edit executes an immediate single-row
  conditional UPDATE: identity match plus the expectation that the cell
  still holds the value the grid last saw. Zero rows affected means
  someone else changed it; the cell shows a conflict with both values
  and takes the server's, never overwrites blind. Cmd+Z (grid focus)
  walks an undo stack issuing the same conditional reverse UPDATEs; a
  conflict invalidates that undo entry. The status line shows LIVE.
  Live mode covers updates to existing rows; insert and delete go
  through staged semantics even in live mode.

Ship order within v1: staged lands first; live lands only on top of the
same identity, capability and conditional-update machinery, never as a
shortcut around it. An async refresh or pagination change never
discards an in-progress edit in either mode.

## Query surface

The editor uses DuckDB keywords and Harbor-side completion
(sql_auto_complete). Cmd+Enter runs the statement under the cursor,
Cmd+Shift+Enter runs all; statements split client-side (QUERY.md). A running query
shows elapsed time and a Cancel action (Cmd+.), which cancels through
Harbor; responses are fenced so a stale response can never replace a
newer result. Multi-statement runs execute in order, stop at the first
error, and keep one result set per completed statement in a segmented
result view; errors show the statement and position. Closing the tab
cancels its running query.

Results are read-only in v1. Copy (cells, rows, with headers) and
export of the fetched result to CSV or Parquet via DuckDB `COPY` are
part of the query flow, not extras. EXPLAIN (⌘⇧E) is a one-shot — not a
sticky toggle — that renders the server's plan text verbatim in a
monospaced pane; plans are text, not diagrams.

## Inspector (right panel)

Details only; no AI chat in v1. The inspector is ROW-LEVEL: the
selected row's values shown vertically, and nothing else. Berth-level
facts keep their own homes at their own urgency — versions, database
path, and size on the berth identity card; row counts in the grid's
footer. Mixing a row being edited with the database engine's version
number puts two data urgency levels in one pane. The pane slots in
BESIDE the table, below the grid's header strip (a resizable split
whose width persists), so opening it never shifts the title row; it
accompanies the Data view only. Cmd+Alt+0 toggles it, as does the
panel glyph on the header strip. The row editor and the grid's
inline editor are one editing session with one owner: opening one
closes the other, and both write through the same staged/live pipeline.
If a refresh replaces the selected row, the inspector shows the new
values and clearly drops nothing silently: a pending edit stays pending
and marked.

## States

Every surface defines empty, loading, and failed:

- Harbor unreachable: full-window state with the reason and a retry. Never a
  blank pane.
- No berths: points at how to open one — drag a `.duckdb` file in, ⌘O,
  or `harbor <db> start` at a shell.
- Berth start failure: inline in the sidebar row, with stderr excerpt.
- Connection lost mid-session: status line goes red, tabs keep their
  data with a stale badge, reconnect retries with backoff; staged edits
  survive a reconnect.
- Empty schema, empty table, empty result: say so, in one line.
- Errors carry copyable detail — an explicit copy tile, not
  drag-select, since painted labels have no OS text-selection (DESIGN.md,
  "Explicit copy, never selection"). Timing labels engine time
  explicitly; end-to-end time appears alongside when they differ
  materially.

## Keyboard map (v1)

- Cmd+K: berth switcher (fuzzy)
- Cmd+P: table switcher (fuzzy, within connected berth)
- Cmd+R: Refresh Tables
- Cmd+N: New Row (Data grid)
- Cmd+D: stage Delete Row (Data grid)
- Cmd+T / Cmd+W: new query tab / close tab
- Cmd+Enter / Cmd+Shift+Enter: run statement / run all
- Cmd+. : cancel running query
- Cmd+S: commit staged changes
- Cmd+Z: focus-scoped undo (text in the editor; reverse UPDATE only
  with grid focus in live mode)
- Cmd+C: copy selection (grid: cells/rows, with-headers variant)
- Cmd+F: filter bar; Esc walks: cancel edit > clear filter > close panel

## Appearance and theming

The look aims for modern macOS warmth, not editor-minimalism: an accent
color that does real work (selection, focus ring, the LIVE indicator),
soft hierarchy between panes, rounded controls, friendly empty states.
Density is functional, not aesthetic: the data grid and editor stay
compact and monospaced in every theme; the chrome around them carries
the personality.

Every color in the app resolves through semantic tokens (background,
surface, accent, text, muted, success, warning, danger, grid lines,
selection), never hardcoded values. Themes are token sets: light and
dark ship first, plus about three more (e.g. a warm paper light, a
midnight blue dark, a high-contrast). The accent is themeable
independently. Value rendering (NULL tags, conflict cells, dirty-cell
marks) uses tokens too, so every theme keeps the same meaning.

## Motion

Animation is felt, not seen: a transition exists to prevent a jarring
snap, never to announce itself. If a user notices "there's an animation
here," it is too slow. Durations follow human motion perception, scaled
to the size of the thing moving:

- **~100ms** is the floor. Below it, a fade reads as a hard cut rather
  than motion.
- **120-200ms** is the pocket for an *in-place* micro-fade — an icon
  swap, a hover, a copy tile's check reverting, a small element breathing
  in. **150ms is the one default** here, shared as `chrome::QUICK_FADE_MS`
  so these can't drift apart. (350ms, tried first for the copy tile, read
  as "look, a crossfade" — the tell that it was ~2x too slow.)
- **200-250ms** is for *structural* enter/leave — a whole row fading out
  of the list, the layout reflowing. A departure is a bigger motion than
  an in-place swap and the eye tracks it, so it runs a hair longer on
  purpose (the departing-row fade is 220ms). This is the one reason a
  fade is NOT the shared 150.
- **250-300ms** suits larger moves only — a pane, a card, a sheet.
- **>400ms** feels laggy for anything small; reserve it for deliberate,
  attention-worthy transitions, which this app has none of.

Two things that look like durations but are NOT motion, and keep their
own numbers:

- A continuous **loop** (a spinner) — a full turn at ~800ms reads as
  "working" without spinning frantically.
- A **dwell** timer — how long a "Copied" confirmation holds before it
  reverts (~1.2s). A readability budget: "can the eye catch the word."
  You initiated the copy and are looking right at it, so it need not
  linger; longer starts to read as "stuck," not "confirmed." Never
  confuse it with the 150ms crossfade that follows it.

Concrete constants live at their use sites (copy_button.rs
FADE_MS=`QUICK_FADE_MS` / HOLD_MS=1200; the berth stop spinner 800ms turn
with a 150ms fade-in; the departing-row fade 220ms). This section is the
why they cluster where they do.

## Platform fit

One design, three thin platform layers; the theme and layout are
identical everywhere, and the platform work is behavioral:

- Window chrome: traffic lights inset on macOS; native caption
  behaviors (snap layouts, min/max/close placement) on Windows; the
  window manager's decoration preference on Linux. Mockups treat this
  region as platform chrome, never drawing one platform's controls
  into another's build.
- Menus: native menu bar on macOS; in-window menus elsewhere.
- Keymaps are per-platform defaults, not a blind Cmd-to-Ctrl swap, and
  platform text-editing conventions are preserved.
- Chrome text uses the platform UI font; the data and editor fonts are
  ours everywhere.
- File dialogs, clipboard, notifications, URL opening are native.
- Light/dark follows the OS setting on all three platforms.

## Non-goals

Charting, dashboards, AI chat, multi-engine drivers, iOS, collaborative
editing, berth add/remove UI. The notebook is deferred, not rejected;
the tab model is its door.
