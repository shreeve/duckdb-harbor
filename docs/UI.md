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
| BERTHS      v  | tab: Invoice | tab: query 1 | +           | DETAILS   |
|  o medlabs     +-------------------------------------------+           |
|  o scratch     | #  InvoiceId  CustomerId  InvoiceDate     | Size      |
|  - archive     | 1  1          2 ->        2021-01-01 ...  |  Data     |
|                | 2  2          4 ->        2021-01-02 ...  |  Index    |
| filter [....]  | 3  3          8 ->        2021-01-03 ...  | Stats     |
|                |                                           |  Rows     |
| CATALOG        |                                           | Metadata  |
|  v memory      |                                           |  DuckDB   |
|    v main      |                                           |  Harbor   |
|      Tables    |                                           | Row       |
|       Invoice  |                                           |  (selected|
|       Track    |                                           |   values) |
|      Views     |                                           |           |
|      Sequences +-------------------------------------------+           |
|      Macros    | Data | Structure | JSON   [staged|LIVE]   |           |
|  > chinook_pg  | columns filters   1,000  < 1/1 >          |           |
+----------------+-------------------------------------------+-----------+
| status: medlabs . 128 of 5,410 rows . engine 1ms . STAGED  |           |
+------------------------------------------------------------------------+
```

Three vertical panes. Sidebar and inspector collapse independently and
auto-collapse when the window is too narrow to keep the content pane at
least 480pt wide; divider positions persist. Minimum window 640x400.

Keyboard shortcuts are written as Cmd here; on Linux and Windows every
Cmd reads as Ctrl.

## Sidebar

Two stacked sections, each collapsible.

**Berths** lists every berth Harbor knows: configured berths and
spawn-on-demand aliases, each with a status dot (connected, running,
stopped). Click connects; connecting shows a spinner with a cancel that
works synchronously (the attempt is fenced; a late completion discards
itself). Context menu: connect, disconnect, start, stop, rename. Add and
remove edit Harbor config and ship after v1; until then the empty state
says where the config lives. Stopping or disconnecting a berth that has
tabs with staged edits or a running query lists what would be lost and
asks. Plain verbs only; the nautical vocabulary is frozen at harbor,
pilot, berth.

**Catalog** shows the connected berth's object tree from Harbor's catalog
endpoint, mirroring DuckDB's model: catalog > schema > Tables, Views,
Sequences, Macros. No Triggers section; DuckDB has none. An ATTACHed
database (DuckDB file, SQLite file, Postgres) appears as a sibling
catalog. Parquet, CSV and JSON reached through table functions or views
are not catalogs and are not invented as tree nodes; they appear where
they really live (a view in its schema) and are otherwise the query
surface's business. One filter field narrows both sections. A refresh
refetches then swaps, preserving expansion and selection; it never blanks
the tree while loading, and a failed refresh keeps the old tree with a
stale badge. DDL run from a query tab refreshes the affected nodes.

Single click selects; Enter or double-click opens a table in a new tab
(or focuses the already-open tab for that table).

## Tabs

One tab strip, drawn by us. Every tab is permanently bound to the berth
it was opened on and shows that berth when it is not the active one.
Switching berths in the sidebar never retargets an existing tab; a query
always runs against the berth its tab was born on. A tab is a surface:

- **Table**: the grid, opened from the catalog tree.
- **Query**: SQL editor above, results grid below, EXPLAIN toggle.
- **Notebook** (later, not in the v1 build): a vertical document of SQL
  cells, each followed by its own result grid, run individually
  (Cmd+Enter) or top to bottom, sharing the tab's berth session so
  temp tables and variables carry between cells. Cells reuse the same
  editor and grid components as the other surfaces. The tab model is
  the extension point; nothing in v1 blocks it and nothing in v1
  depends on it.

Cmd+T new query tab, Cmd+W closes, Cmd+1..9 select. A tab with staged
edits or unsaved query text shows a dot and confirms before closing.
Query text is preserved across restart (session restore); staged edits
are not, and the close/quit confirmation says so.

## Grid

Virtualized in both axes; scale target is wide analytics results. Row
numbers on the left. Headers sort: click cycles asc/desc/none,
shift-click adds with visible priority numbers. Sorting and filtering a
table tab re-run the query server-side; in a query-results grid they
operate on the fetched rows only and the bar says so. NULL renders as a
dimmed tag, distinct from empty string. All values render in the one
value font; the inline editor shares the cell's exact geometry so
entering edit mode moves nothing.

Rows fetch in server-side pages (default 1,000). The status line always
distinguishes "128 of 5,410 rows" from "first 1,000 (total unknown)";
fetching more is an explicit action, and the client never materializes
an unbounded result.

A cell whose column has a single-column foreign key with resolvable
metadata shows a follow arrow that opens the referenced row; when the
metadata is absent or composite the arrow simply does not appear.

Bottom bar per table tab: Data | Structure | JSON views, column
show/hide, filters, page size, pagination. Structure shows the table's
DDL and columns; JSON shows the current page as JSON. In a query-results
grid, Structure and edit mode are absent and JSON shows the fetched
result.

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
Cmd+Shift+Enter runs all; statements split server-side. A running query
shows elapsed time and a Cancel action (Cmd+.), which cancels through
Harbor; responses are fenced so a stale response can never replace a
newer result. Multi-statement runs execute in order, stop at the first
error, and keep one result set per completed statement in a segmented
result view; errors show the statement and position. Closing the tab
cancels its running query.

Results are read-only in v1. Copy (cells, rows, with headers) and
export of the fetched result to CSV or Parquet via DuckDB `COPY` are
part of the query flow, not extras. The EXPLAIN toggle renders the
server's plan text verbatim in a monospaced pane; plans are text, not
diagrams.

## Inspector (right panel)

Details only; no AI chat in v1. Sections mirror what a berth can
answer: Size (data, index, total), Statistics (row count), Metadata
(DuckDB version, Harbor version, berth name), and a Row section showing
the selected row's values vertically. The row editor and the grid's
inline editor are one editing session with one owner: opening one
closes the other, and both write through the same staged/live pipeline.
If a refresh replaces the selected row, the inspector shows the new
values and clearly drops nothing silently: a pending edit stays pending
and marked.

## States

Every surface defines empty, loading, and failed:

- Harbor unreachable or auth failed: full-window state with the reason,
  a retry, and where the config/token lives. Never a blank pane.
- No berths: points at Harbor config.
- Berth start failure: inline in the sidebar row, with stderr excerpt.
- Connection lost mid-session: status line goes red, tabs keep their
  data with a stale badge, reconnect retries with backoff; staged edits
  survive a reconnect.
- Empty schema, empty table, empty result: say so, in one line.
- Errors carry copyable detail. Timing labels engine time explicitly;
  end-to-end time appears alongside when they differ materially.

## Keyboard map (v1)

- Cmd+K: berth switcher (fuzzy)
- Cmd+P: table switcher (fuzzy, within connected berth)
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

## Non-goals

Charting, dashboards, AI chat, multi-engine drivers, iOS, collaborative
editing, berth add/remove UI. The notebook is deferred, not rejected;
the tab model is its door.
