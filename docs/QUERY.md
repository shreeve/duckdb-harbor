# The Query view

The query editor's design, as settled by a three-agent analysis round
(grammar strategy, editor UX, intelligence layer) over the completed
research sweep, under one governing ruling: **DuckDB's PEG grammar is
the bible.** DuckDB has its own SQL philosophy; the grammar files at
`src/parser/peg/grammar/` are its law, and everything here derives from
them or from the engine itself. No third-party SQL parser appears
anywhere in this design. This document extends docs/EDITING.md and
never contradicts it; where docs/UI.md's older Query-surface notes
disagree, the two amendments are flagged inline.

## The five laws

1. **One scratchpad per database, and it is never lost.** The Query
   view is the third segment of the view switcher, scoped to the berth,
   not the table. Its text autosaves keystroke-to-disk and restores
   across restart. There is no unsaved state, so there is no save
   dialog and no dirty dot — the scratch is as durable as the prefs
   file.
2. **⌘Enter sends to the database; ⌘S makes local work durable.** These
   are the two meanings, app-wide. In the grid they coincide — staged
   edits are local work *and* the send payload, so committing is both.
   In the editor they part: ⌘Enter runs, ⌘S flushes the scratch. No
   double-agent: each key keeps its one meaning; the grid was the
   special case where the meanings converge, not the rule.
3. **What ⌘Enter will send is visible before you press it.** The send
   mark — a hairline accent bar in the gutter spanning the statement
   that owns the caret — is the editor's twin of the amber staged cell.
   The app's signature is that the commit key's consequence is always
   on screen first; the editor inherits it whole.
4. **The editor is not a gatekeeper; the engine judges.** ⌘Enter sends
   what you asked for — half-typed, a selected fragment, anything — and
   renders the engine's verdict with its source position. Diagnostics
   advise; nothing client-side ever blocks a run. The corollary:
   nothing client-side ever *invents* a verdict — tree-sitter ERROR
   nodes drive no UI, because the lens must never impersonate the
   judge.
5. **Results are a snapshot that snaps.** A run's results replace the
   results pane in one frame, labeled with what produced them and when.
   They are read-only in v1; editing powers arrive only with row
   identity, through the grid's existing capability gate — never as a
   special case.

## The window

Query joins `ViewMode` as the third segment — the order is
**Structure | Data | Query** (ruled 2026-08-31): what the table is,
what it holds, what you ask — Sequel Pro's arc, with Data, the default
and hub, seated center so both neighbors are one ⌥-arrow away. The
⌥←/⌥→ carousel rolls through all three; landing on Query focuses the
editor, the exact symmetry of landing on Data focusing the grid.
⌘1/⌘2/⌘3 address the segments directly (Finder's idiom) — held on
loan: when the roadmap tab strip ships, the number keys migrate to
tabs, the same pattern ⌥↑/↓ follows.

The view is **berth-scoped**. Whichever table you arrived from, Query
shows the berth's one scratchpad; switching tables never touches it,
and it is available the moment a berth connects, table or no table.
One scratchpad with statement navigation, not tabs — multiple queries
are multiple statements. The tab strip and the notebook remain the
roadmap doors UI.md already framed.

Layout: **editor above, results below**, a draggable horizontal split
whose position persists per window (UI.md's divider rules apply: drag
past minimum snaps closed, double-click toggles). Before the first run
the results pane sits collapsed to its one-line status strip — a
deliberate state, not a squeezed grid — and the first run opens it to
the persisted split. The editor is gpui-component's `code_editor` with
line numbers and tree-sitter-duckdb highlighting, in the value font —
borderless (the pane's 12px inset is the frame; focus is the caret),
and sized by the same zoom ladder as every data surface: the editor,
the results, and the Data grid share one text size per zoom step, never
per-pane fonts (ruled 2026-08-31). Each app theme carries a matching
highlight theme in ducktable.json — syntax colors drawn from the
theme's own families (keyword=primary, string=success, number=warning,
comment=muted italic).

The footer keeps the switcher on the left. The Data view's controls
(filter strip, Columns popover) are **absent, not disabled**. The
right-anchored status line follows the anti-jump ordering:
`12 ms · 1–500 rows · 9 columns · |< < 500 per > >|`, engine time
labeled, wall time alongside when they differ materially.

## Run semantics

**⌘Enter** — send it:

- **Selection present:** send exactly the selected text, trimmed,
  character-precise. You selected it, we send it — a fragment errors
  with the engine's message, a multi-statement selection runs like
  run-all. No silent expansion to statement boundaries.
- **No selection:** send the marked statement.
- **Empty buffer:** no-op, one quiet line in the status area.

**The marked statement** is defined by the grammar's statement spans: a
span runs from its first token through its terminating semicolon and
any same-line trailing comment. In the whitespace and comments
*between* statements, the statement **above** owns the caret — you just
finished typing it, and Enter-then-⌘Enter must send what you wrote, not
the next one down. Only before the first statement does the caret look
downward. The mark moves with the caret same-frame (content snaps).

**⌘⇧Enter** runs all statements top to bottom.

**Splitting** — *amendment to UI.md's "statements split server-side"*:
the client splits, because the wire shape already forces one exec call
per statement, and the tree-sitter grammar is derived from the engine's
own PEG grammar, so the split is the engine's split. A quote/comment/
dollar-aware token splitter backstops the tree when it is mid-keystroke
soup. The engine remains the sole judge of validity.

**Sessions:** the Query view opens one Harbor session lazily at first
run and holds it while the berth stays connected — every run shares it,
so temp tables and macros persist between ⌘Enter presses. This is the
notebook's shared-session ruling arrived early; it is what makes a
scratchpad a workbench. Released on disconnect; a dead session reopens
transparently on the next run, with a quiet "session reopened" note —
temp state is gone and the note says so.

**No implicit transaction.** Statements auto-commit individually, like
the duckdb CLI; write `BEGIN`/`COMMIT` yourself. A run-all stops at the
first error; whatever completed stays completed, and the status line
says how far it got. Runs containing non-SELECT statements refresh the
sidebar catalog afterward (fetch first, swap in one frame).

**One run in flight** per view. ⌘Enter during a run answers
`already running · ⌘. to cancel` — no queue. **⌘.** cancels through
Harbor; responses are fenced, so a stale result can never replace a
newer one. Closing the connection cancels the run.

**⌘S** flushes the scratch to disk immediately; the status line
flashes `saved`. It never runs anything. **⌘R** runs again: it repeats
the previous run's captured text exactly — refresh means "what I last
looked at," even if the scratch has moved on.

⌘Enter works from **either pane**: with focus in the results grid it
still sends the marked statement. Results have no staged powers, so
the grid's ⌘Enter meaning has nothing to claim; iterate from wherever
your hands are.

## Results

The results pane is the existing `Grid` entity built **without
identity** — read-only falls out of the capability gate, not a fork.
NULL tags, the value font, copy (cells, rows, with-headers) all
inherit. Row numbers are result ordinals.

**Multi-statement runs** keep one result per completed statement,
shown as a thin strip of chips above the grid:
`2 · SELECT · 500 rows · 12 ms` (ordinal, verb, count, time). The last
result is active; click switches; switching replaces in one frame.
When a run produced a single result, the strip does not render —
chrome for the uncommon case only.

**Statements without a result set** (DDL; DML returns its count row)
render a one-line acknowledgment card in place of the grid:
`ok · 3 rows affected · 2 ms` — never an empty grid pretending to be
data.

**Run feedback is three-phase** (ruled 2026-08-31): for the first
300ms of a run, nothing on screen changes — a fast query's verdict and
results land together in one atomic frame. A run still going at 300ms
earns a ticking `running… N ms` line and the prior results fade to
~45%, visibly stale but never blanked. Completion is always one atomic
swap. Law 5's commit-flicker discipline, applied to queries.

**Errors** land as a danger-colored message strip at the top of the
results pane: the engine's message verbatim, copyable, with statement
ordinal and position — clicking it moves the caret to the offense. The
failed statement's gutter bar turns danger until the statement is
edited. (Squiggles at the span join in v2 as idle-parse diagnostics;
the strip is the load-bearing surface.)

**Paging** (ruled 2026-08-31, ahead of the v2 forecast): a query
result pages like a table, because the grid's FROM target is simply
the user's statement parenthesized — "a Data window with a custom
query preceding it." `page_sql` emits `SELECT * FROM (statement)
LIMIT … OFFSET …` for the SELECT-shaped family (select / with / from /
values / table); the footer's pager, size cycling, and jump-to-last
all just work. The bare first run still materializes once — which is
how the exact total is known for free — and the grid keeps page 0 of
it. Unwrappable statements (PRAGMA, SHOW …) keep their whole result as
one inert page, pager hidden. *Wire seam still flagged:* capping the
FIRST run wants Harbor cooperation (cap-and-report); until then one
full materialization per send.

**Chrome taxonomy** (ruled 2026-08-31): display preferences are
GLOBAL and set once — row numbers, NULL tags, right-alignment are
about the reader, not the data, so the title strip's one lozenge
governs every grid and each grid self-heals to it on its next paint.
Stats and paging are PER-GRID state — facts about one grid's contents
— owned by that grid and *displayed* by the current view's chrome:
one grid per view today, so the app footer reads the active grid
(`FooterFacts`); if multi-result grids ever ship, each grid's stats
travel with it into a per-grid strip, and nothing re-architects.

## The editor

**The send mark.** Hairline accent bar in the gutter over the marked
statement's lines, plus a barely-there background tint. While running,
the running statement carries the accent; on error, danger until
edited. There are no gutter run buttons — the mark plus ⌘Enter *is*
the grammar; a play button would be mouse chrome duplicating it.

**Completion** is entirely local at keystroke time: keywords from
build-time tables (generated from DuckDB's own keyword lists),
functions fetched once at connect (`duckdb_functions()`), schema
objects and columns from the catalog. Triggers: as-you-type from the
first identifier character; `.` after an identifier or alias opens
member completion (columns); ⌃Space summons manually. Zero wire calls
at keystroke time means zero-latency popovers. The engine's own
`sql_auto_complete` (PEG-grammar-driven, catalog-aware) joins in v2 as
an additive re-ranker that never reorders or removes what is already
showing. Keywords complete in the case you started (`sel` → `select`,
`SEL` → `SELECT`); identifiers insert catalog-true case, quoted only
when necessary.

**Menu keys:** Tab accepts the highlighted completion; **Enter always
inserts a newline** — the menu appears as you type, and an Enter that
sometimes completes would steal newlines; one meaning per key. ↑/↓
navigate the menu; Esc dismisses it. If the stock popover fights this,
we own the widget (DESIGN.md's replacement rule). ⇧Enter and ⌥Enter
are newlines too — in a code editor the composer keys have nothing to
distinguish, so no reflex from the grid ever punishes.

**Tab without a menu:** multi-line selection indents, ⇧Tab outdents,
otherwise insert the indent unit — 4 spaces, matching the formatter's
`indent_size` so formatting never fights typing.

**Format** (v2): ⇧⌥F formats the selection if any, else the whole
buffer — via the engine's own `duckdb_format_sql()` over the wire,
grammar-exact and version-matched to the running engine (the bible
formats its own scripture; no third-party formatter). One undo entry,
caret staying in its statement, statements formatted independently so
one unparseable statement never blocks the rest. Engines too old to
have it answer `format unavailable on this engine` — no silent
fallback that formats differently than DuckDB would.

**⌘/** toggles `--` line comments on the line or selection — the
fastest way to mute a statement without deleting it.

**History:** every run appends to a per-berth NDJSON log (text,
timestamp, duration, rows, verdict) **from v1 day one** — capture
before UI, because history never captured is unrecoverable. The v2
popover (⌃R, the shell reflex) fuzzy-filters it; clicking inserts the
statement at the end of the scratch, never auto-runs. Pull-based
audit, the staged-changes popover's sibling. No up-arrow recall ever —
this is a multi-line editor and up-arrow moves the caret.

**EXPLAIN** (v2) — *amendment to UI.md's "EXPLAIN toggle"*: ⌘⇧E is a
one-shot that explains the marked statement, landing the plan as a
result chip of kind *plan*, rendered verbatim in the monospaced pane.
A sticky toggle that silently rewrites every ⌘Enter would lie about
what "send" sends.

**Snippets:** declined for v1. Completion plus history covers the
need; a snippet system is a second language to learn.

## The keymap

The Query view extends the decision ladder; nothing below contradicts
a rung:

1. Completion menu open → Tab accepts, Esc dismisses, ↑/↓ navigate;
   all else falls through to the editor.
2. Editor focused → the table below; everything unmatched falls to the
   text input, whose macOS answers we inherit whole (⌥←/→ word, ⌘←/→
   line, ⌥↑/↓ paragraph, ⌘↑/↓ document). This is why the carousel
   pauses while you type — the context stack already resolves it, and
   that is the ruling: correct, no new focus-escape chord.
3. Results grid focused → the grid's navigation grammar, read-only
   subset; ⌥←/→ carousel live again.
4. View-level exact chords, either pane focused.
5. The app ladder (rung 4 pass-through and all).

| Key | In the Query view |
|---|---|
| ⌘Enter | send: the selection if any, else the marked statement — from either pane |
| ⌘⇧Enter | run all, top to bottom, stop at first error |
| ⌘. | cancel the running query |
| ⌘R | run again — repeat the previous run exactly |
| ⌘S | flush the scratch to disk (`saved` flashes; it was already safe) |
| ⌘L | from anywhere in the window: switch to Query, focus the editor — the address-bar reflex |
| Enter / ⇧Enter / ⌥Enter | newline, always; Enter never accepts a completion |
| Tab / ⇧Tab | accept completion when the menu is open; else indent / outdent |
| ⌃Space | summon completion |
| ⌘/ | toggle `--` comment on line or selection |
| ⇧⌥F | format selection, else buffer (v2) |
| ⌘⇧E | explain the marked statement, one-shot (v2) |
| ⌃R | history popover (v2) |
| Esc | dismiss menu → collapse selection → nothing; never cancels a run (⌘. is the deliberate act), never blurs. In results: clear selection → focus the editor |
| ⌘⌥↓ / ⌘⌥↑ | focus results / focus editor |
| ⌘Z / ⌘⇧Z | text undo / redo (editor); inert in the read-only grid |
| ⌘C | copy — editor text, or grid cells/rows with the with-headers variant |
| ⌘F | find in buffer (v1 if the widget provides it, else early v2) |
| ⌥←/→ | word movement while the editor has focus; the segment carousel from the results grid or any non-text focus |
| ⌥↑/↓ | paragraph movement in the editor — free approximate statement-jumping; inert in v1 results |

## Persistence

- Scratch: `~/.config/ducktable/scratch/<berth>.sql`, beside
  `prefs.json`. Debounced autosave about a second after the last
  keystroke, plus on segment switch, blur, quit, and ⌘S. Caret, split
  position, and active result chip restore with the session.
- History: `~/.config/ducktable/history/<berth>.ndjson`, capped at 10k
  entries, oldest pruned at write.
- The quit dialog never mentions query text — it is always saved. The
  one dialog remains the grid's staged-edits dialog, unchanged.

## States

- Berth disconnected: the Query segment shows the full-pane connect
  state with the reason — never a blank pane.
- Empty scratch: one line — `Type SQL. ⌘Enter runs the statement under
  the caret; ⌘⇧Enter runs all.`
- Running: `running · 3.2 s · ⌘. to cancel`, elapsed ticking, the
  running statement's bar in accent.
- Capped result: `first 500 rows`.
- Session reopened after death: one quiet status note.

## Architecture: three clean rooms

The user constraint — editor code in its own space, nice and tidy —
holds structurally, not by convention. Two new workspace crates plus
one app module, each with one job and a narrow doorway:

### `crates/duckdb-lang` — the lens

A fresh **tree-sitter-duckdb** grammar — none exists anywhere; ours
will be the first — derived from DuckDB's PEG grammar, which serves as
coverage checklist, node-naming spec, and keyword source:

- **Node names are DuckDB's PEG rule names, snake_cased**
  (`select_statement`, `star_expression`, `qualify_clause`), so a diff
  in `select.gram` points at the grammar rule to touch and completion
  code speaks DuckDB's vocabulary. Not a DerekStride fork: its
  highlight assets are welded to 376 `keyword_*` nodes and a lexer
  token with `->` fused into ~40 Postgres JSON operators — the wrong
  philosophy to unweld. We lift only its dollar-quote scanner and file
  layout (MIT, attributed).
- **Keyword discipline mirrors DuckDB's own:** only the 75 reserved
  keywords are grammar tokens; unreserved/column/func/type keywords
  parse as identifiers (DuckDB's own `ColId` design) and highlight via
  generated predicate lists. The five `keywords/*.list` files compile
  to Rust tables and highlight predicates at build time.
- **`cargo xtask grammar-sync`** pins a DuckDB tag, regenerates the
  keyword layer, and CI-diffs all 40 `.gram` files per release — the
  maintenance contract that keeps "PEG as bible" true over time. A
  parse corpus sampled from DuckDB's own test suite gates ERROR-node
  rate in CI.
- **No runtime PEG interpretation.** The `.gram` files are
  deliberately approximate at the token level (native matcher fills in
  string escaping, keyword boundaries); executing them faithfully
  means reimplementing the matcher for a parser that cannot recover
  mid-keystroke. The engine over the wire is the validity oracle —
  version-exact for the attached database.
- Vendored `parser.c` compiled by `cc` in build.rs; `tree-sitter
  generate` runs only in xtask with a pinned CLI whose ABI matches
  gpui-component's tree-sitter runtime (verify the pairing first).
- Week-1 scope: the 16-level expression ladder (which collapses nearly
  1:1 into `prec.left` rules), FROM-first SELECT, GROUP/ORDER BY ALL,
  QUALIFY, star EXCLUDE/REPLACE/RENAME, trailing commas,
  struct/map/list literals, named args, params, `::`/slices — plus
  loose head rules for all 40 statements so nothing ERROR-poisons a
  buffer. Lambdas and PIVOT (the two known GLR conflict hot-spots) are
  phase 2. Publishing tree-sitter-duckdb upstream is the moonshot that
  pays for itself in outside grammar upkeep.

### `crates/sql-intel` — the brain

Pure data and logic: no I/O, no gpui, no harbor-client. The doorway:
`SchemaInfo` in (plain structs, field-aligned with harbor-client's
catalog so conversion is a mechanical map), an `EngineIntel` trait out
(the app implements it over Harbor on the background executor — every
method is SQL over the wire, never on the render path).

- **Context classifier:** tree walk by PEG-mirrored node names for
  clause/qualifier/scope (aliases, CTEs, subqueries, inner-shadows-
  outer), with a token-scan fallback for the ERROR-soup mid-keystroke
  case — paren-depth aware, reserved-keyword table killing the
  `FROM t WHERE` → "alias named WHERE" class of bug.
- **Ranking:** a hard context gate (only categories valid at the
  cursor are generated), then a lexicographic tuple: match class
  (exact prefix > ci prefix > word-boundary subsequence > substring >
  fuzzy), category priority per context, in-query scope boost, PK/NOT
  NULL nudge, session MRU, length, name. Capped at 64 items.
- **Diagnostics:** idle-debounced (500ms), changed statements only,
  parse-check via `json_serialize_sql` (pure, side-effect-free),
  engine positions mapped through statement span bases. Bind-level
  checks via EXPLAIN are later and opt-in.
- **Statement splitter:** lexer-aware top-level `;`, quote/comment/
  dollar-safe — the tree's backstop and the runner's unit of send.

### `crates/ducktable/src/query/` — the room

The view itself: segment, split, send mark, runner (session-holding,
fenced, cancelable), result chips, error strip, scratch/history
persistence, and the `CompletionProvider`/`EngineIntel`
implementations wiring sql-intel to gpui-component and Harbor. The
app's only new surface; everything below it is reusable outside
DuckTable.

## Progression

**v1 — one phase, shippable:** the Query segment with focus handoff;
per-berth scratch, autosave, restore; the split; statement spans and
the send mark; ⌘Enter / ⌘⇧Enter / ⌘. / ⌘R / ⌘S / ⌘L; the
session-holding runner; read-only results grid, chips, acknowledgment
cards, error strip with caret jump; status-line timing; local
completion (keywords, functions, schema, dot-members, ⌃Space,
Tab-accepts); ⌘/; history capture (log only); duckdb-lang week-1
grammar registered and highlighting. *Risks to spike first:* the
`.multi_line(true).rows(n)` one-row quirk in the code_editor path; the
result-row cap wire seam; the tree-sitter CLI/runtime ABI pairing.

**v2:** format (⇧⌥F, engine `duckdb_format_sql`), idle-parse
diagnostics with squiggles, history popover (⌃R), EXPLAIN (⌘⇧E),
`sql_auto_complete` additive re-ranking, find-in-buffer if not free in
v1, CSV/Parquet export on the result strip, true result paging via
server cursor, lambdas + PIVOT grammar fidelity.

**v3:** editable results — when a result proves identity (base table
plus key or rowid), the grid's staged powers light up through the
existing capability gate, and `SELECT * FROM t` becomes the Data view
with extra steps; then the tab strip (⌘T, ⌘1–9), then the notebook,
exactly as UI.md doored them.

## The one thing

**⌘Enter never surprises and never waits.** The send mark shows the
target before the keystroke; the result snaps into the pane in one
frame with the engine time in the corner; completion costs zero
round-trips; and the held session means the temp table from your last
run is still there for your next one. That loop — mark, send, snap,
again — is the whole product feel. Everything in v1 either serves it
or waits.
