# The Query view

A berth-scoped SQL scratchpad above a read-only results grid. One ruling
governs the design: **DuckDB's PEG grammar is the bible.** DuckDB has its own
SQL philosophy, and the grammar files at `src/parser/peg/grammar/` are its law.
Everything here derives from them or from the engine itself; no third-party
SQL parser appears anywhere. This document extends EDITING.md and never
contradicts it. Everything here ships unless it sits under **Planned** at the
end.

## The five laws

1. **One scratchpad per database, and it is never lost.** The Query view is
   the third segment of the view switcher, scoped to the berth, not the table.
   Its text saves to disk on every change and comes back across restarts.
   There is no unsaved state, so there is no save dialog and no dirty dot.
2. **⌘Enter sends; nothing else does.** In the grid, ⌘Enter and ⌘S both
   commit, because staged edits are local work and the send payload at once.
   In the editor, ⌘Enter runs the marked statement, and typing only ever edits
   a scratch that saves itself.
3. **What ⌘Enter will send is visible before you press it.** The send mark,
   a band in the gutter spanning the statement that owns the caret, is the
   editor's twin of the grid's amber staged cell: the consequence of the send
   key is always on screen first.
4. **The editor is not a gatekeeper; the engine judges.** ⌘Enter sends what
   is marked, half-typed or not, and shows the engine's verdict verbatim.
   Nothing client-side blocks a run, and nothing client-side invents a
   verdict: tree-sitter ERROR nodes drive no UI, because the lens must never
   impersonate the judge.
5. **Results are a snapshot that snaps.** A run's results replace the pane in
   one frame. They are read-only; editing powers would arrive only with row
   identity, through the grid's capability gate, never as a special case.

## The window

Query is the third view, **Structure | Data | Query**: what the table is, what
it holds, what you ask. Data, the default, sits in the middle, so both
neighbors are one ⌥-arrow away. The ⌥←/⌥→ carousel rolls through all three,
and ⌘1/⌘2/⌘3 address them directly. Landing on Query focuses the editor, the
symmetry of landing on Data focusing the grid.

The view belongs to the berth. Whichever table you came from, Query shows the
berth's one scratchpad; switching tables never touches it, and it is there as
soon as the berth connects. Several queries are several statements in the one
scratchpad.

The editor sits above the results, with a draggable split whose position
persists. The editor is gpui-component's code editor with line numbers and
tree-sitter-duckdb highlighting, in the value font and on the same zoom ladder
as every data surface: the editor, the results and the Data grid share one text
size per zoom step. Each theme carries a matching highlight theme drawn from its
own colors (keywords in the primary color, strings in success, numbers in
warning, comments muted and italic).

The bottom bar keeps the switcher on the left. The Data view's filter strip and
Columns popover are absent here, not disabled. The status line follows the
same anti-jump order as the Data view's.

## Run semantics

**⌘Enter**, with the editor focused, sends the marked statement. An empty
buffer or a caret with no statement answers `nothing to run`.

**The marked statement** runs from its first token through its terminating
semicolon and any same-line trailing comment. In the whitespace and comments
between statements, the statement **above** owns the caret: you just finished
typing it, and Enter then ⌘Enter must send what you wrote, not the next one
down. Only before the first statement does the caret look downward. The mark
moves with the caret in the same frame.

**Splitting** happens in the client, because the wire takes one statement per
request, and there is one boundary: a top-level `;`, aware of quotes, comments
and dollar quotes, exactly where the engine's own parser would cut. Blank lines
never divide: FROM-first syntax makes every keyword heuristic lie eventually,
and a wrong split can leave a runnable prefix. The terminator belongs to its
statement, and the payload sheds it along with any same-line trailing comment.

**Each run stands alone.** A run is one request with no session, so it
auto-commits on its own, as it would in the duckdb CLI. Nothing carries from
one run to the next: `BEGIN` in one run and `ROLLBACK` in the next do nothing
together, the `ROLLBACK` fails with "no transaction is active", and a temp
table is gone by the next run. A transaction here has to be the one statement.

**One run at a time.** ⌘Enter during a run answers `already running…` rather
than queueing. Results are fenced, so a late result can never replace a newer
one.

Every completed run refreshes the sidebar catalog and the open Data grid
afterward, fetch first and swapped in one frame: arbitrary SQL can change
tables in ways no client can classify. The Query result itself stays the
snapshot that run returned; DuckTable never reruns SQL on its own. ⌘R, Refresh
Tables, does the same refresh without running anything or replacing the
result.

## Results

The results pane is the ordinary `Grid`, built without identity, so read-only
falls out of the capability gate rather than a fork. NULL tags, the value font
and copy all carry over. Row numbers are result ordinals.

**Paging.** A result pages like a table, because the grid's FROM target is the
statement itself in parentheses: `SELECT * FROM (statement) LIMIT … OFFSET …`,
for the SELECT-shaped family (select, with, from, values, table, or a
parenthesized statement). The pager, size cycling and jump-to-last all work.

A run costs at most the two queries the Data view pays for a table, and usually
one. Page 0 fetches `size + 1` rows, and a result that fits the page is its own
exact count; only the extra row's arrival proves there is more, and only then
does `count(*)` run for the total. The page query doubles as the wrap probe: if
it fails, because the statement is not really SELECT-shaped or does not parse,
the statement runs bare, so an error always quotes the user's own SQL, never the
wrapper's. A statement that cannot be wrapped keeps its whole result as one
page, with the pager hidden. The costs are named: a big result runs its plan
twice, and deep OFFSET pages re-skip rows, as table paging does.

**A statement with no result set** reports `ok · 2 ms` in the status line
rather than showing an empty grid.

**Errors** show the engine's message verbatim in the results pane.

**Feedback is three-phase.** For the first 300ms of a run nothing on screen
changes, so a fast query's verdict and results land together in one frame. A
run still going at 300ms earns a ticking `running… 1.2 s` line, and the prior
results fade, stale but never blanked. Completion is always one atomic swap.

## Persistence

- **Scratch:** `~/.config/ducktable/scratch/<berth>.sql`, written on every
  change to the editor. The files are small, so the write is not debounced.
- **History:** `~/.config/ducktable/history/<berth>.ndjson`, one line per run
  with its text, time, duration and row count or error. It is captured before
  any UI reads it, because history never captured cannot be recovered. It keeps
  the newest 10,000 entries, pruned when the view is created.

## `crates/duckdb-lang`, the lens

A tree-sitter grammar for DuckDB, derived from DuckDB's PEG grammar, which
serves as its coverage checklist, node-naming spec and keyword source. None
existed elsewhere.

- **Node names are DuckDB's PEG rule names, snake_cased** (`select_statement`,
  `star_expression`, `qualify_clause`), so a change in `select.gram` points at
  the rule to touch. It is not a fork of DerekStride's SQL grammar, whose
  highlighting is welded to hundreds of `keyword_*` nodes and to Postgres JSON
  operators; only its dollar-quote scanner and file layout are borrowed (MIT,
  attributed).
- **Keyword discipline mirrors DuckDB's own.** Only reserved keywords are
  grammar tokens. Unreserved, column, function and type keywords parse as
  identifiers, as DuckDB's own `ColId` design does, and highlight through
  predicate lists.
- **No runtime PEG interpretation.** The `.gram` files are deliberately
  approximate at the token level, and a faithful executor would be a parser
  that cannot recover mid-keystroke. The engine over the wire is the validity
  oracle, version-exact for the attached database.
- The generated `parser.c` is vendored and compiled by `cc` in `build.rs`.

## Planned

Designed, not built.

- **More ways to send.** A selection sends exactly the selected text; ⌘⇧Enter
  runs every statement top to bottom and stops at the first error; ⌘. cancels
  through Harbor; ⌘L switches to Query from anywhere; ⌘Enter works from the
  results pane too.
- **A held session.** One Harbor session opened at the first run and held while
  the berth stays connected, so temp tables, macros and `BEGIN`…`COMMIT` carry
  between runs. A dead session would reopen on the next run with a note that
  its temporary state is gone.
- **Several results.** One result per completed statement of a run-all, as
  chips above the grid (`2 · SELECT · 500 rows · 12 ms`). An error would carry
  its statement and position, and clicking it would move the caret there.
- **Completion.** Local at keystroke time, so popovers cost no round trip:
  keywords from DuckDB's own lists, functions fetched once at connect,
  schema objects and columns from the catalog. `.` after a name opens its
  members, ⌃Space summons, Tab accepts, and Enter always inserts a newline. The
  engine's `sql_auto_complete` would join as a re-ranker that never removes
  what is showing. The logic would live in a pure `sql-intel` crate: a context
  classifier over the PEG-named tree, a ranking tuple, and idle-time
  diagnostics through `json_serialize_sql`.
- **Editing keys.** ⌘/ toggles `--` comments; ⇧⌥F formats through the engine's
  own `duckdb_format_sql`; ⌃R opens a history popover that inserts, never
  runs.
- **EXPLAIN.** ⌘⇧E as a one-shot that explains the marked statement and shows
  the plan text verbatim. A sticky toggle that rewrote every send would lie
  about what ⌘Enter sends.
- **Grammar upkeep.** A `grammar-sync` task that pins a DuckDB tag,
  regenerates the keyword layer and diffs the `.gram` files per release, with a
  parse corpus from DuckDB's own tests gating the ERROR-node rate in CI.
- **Editable results.** When a result proves identity (a base table with a key
  or a rowid), the grid's staging lights up through the capability gate, and
  `SELECT * FROM t` becomes the Data view with extra steps.
