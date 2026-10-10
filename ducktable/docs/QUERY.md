# The Query view

A berth-scoped SQL scratchpad above a read-only results grid. One ruling
governs the design: **DuckDB's PEG grammar is the bible.** DuckDB has its own
SQL philosophy, and the grammar files at `src/parser/peg/grammar/` are its law.
Everything here derives from them or from the engine itself; no third-party
SQL parser appears anywhere. This document extends EDITING.md and never
contradicts it. Everything here ships unless it sits under **Planned** at the
end.

## The five laws

1. **One scratchpad per database name, and it is never lost.** The Query view
   is the third segment of the view switcher, scoped to the berth, not the
   table. Its text saves to disk on every change and comes back across
   restarts. There is no unsaved state, so there is no save dialog and no dirty
   dot. The scratchpad and its history are files named for the database's name
   (Persistence, below), so two databases that carry one name share them: a
   file here and a remote called the same, or two files with the same stem.
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

**Each run stands alone, outside a transaction.** A run is one request on a
pooled connection, so it commits on its own, as it would in the duckdb CLI,
and nothing else carries to the next run: a temp table is gone by then. What
does carry is a transaction, below. A run that gets no answer (a timeout, a
dropped stream, Harbor's `cancelled` or `internal`) may have run and
committed, and the view says so under the error, so that a rerun of an
`INSERT` is a choice and not an accident.

**One run at a time.** ⌘Enter during a run answers `already running…` rather
than queueing, so no result is ever in flight behind another.

Every completed run refreshes the sidebar catalog and the open Data grid
afterward, fetch first and swapped in one frame: arbitrary SQL can change
tables in ways no client can classify. The Query result itself stays the
snapshot that run returned; DuckTable never reruns SQL on its own. ⌘R, Refresh
Tables, does the same refresh without running anything or replacing the
result.

## Transactions

One request carries one statement, and its connection goes back to Harbor's
pool when the request ends, so a `BEGIN` sent on its own would open a
transaction nothing could ever join: every statement after it would commit on
its own, and `ROLLBACK` would find nothing to roll back. The view holds a
Harbor session instead, a connection pinned to it, for exactly as long as a
transaction is open.

- **`BEGIN` or `START TRANSACTION` opens one.** The view opens a session,
  runs the statement there, and from then on sends every run through that
  session. The statement is never sent without one: if no session can be had,
  the run fails with the reason and nothing is sent.
- **`COMMIT`, `END`, `ROLLBACK` or `ABORT` ends it,** and the session goes
  back. With no transaction open they run on their own, and the engine answers
  that no transaction is active.
- **The keyword the engine acts on decides,** read by the one reader the
  view shares with the server and Harbor's own client (`wire::statement`),
  measured against the engine. It skips the comments and the spaces the engine
  skips and no others: a zero-width space pasted in front of `COMMIT` is
  skipped, so that `COMMIT` commits and the view knows it, and a character the
  engine does not skip makes the word a name. A word runs as far as an
  identifier does, so `COMMIT_X`, `COMMIT1` and a quoted `"COMMIT"` are names
  and end nothing: the engine looks for a table. `EXPLAIN ANALYZE` runs the
  statement it explains, a real `BEGIN`, `COMMIT` or `ROLLBACK`: `EXPLAIN
  ANALYZE COMMIT` commits. So the reader looks through `EXPLAIN` when
  `ANALYZE` (or `ANALYSE`) follows it or stands anywhere in its option list,
  `(ANALYZE false)` included, which the engine also runs. A plain `EXPLAIN
  COMMIT` only plans, and changes nothing.
- **It shows.** While a transaction is open the header band reads
  `transaction open · 4:32 left · COMMIT or ROLLBACK ends it`, and once the
  session has answered that an error aborted it, `transaction aborted by an
  error · 4:32 left · ROLLBACK ends it`. These statements have no result set
  and report `ok`, as any resultless statement does.
- **Only this view sees it.** The Data view, the sidebar's counts and every
  other client read outside the transaction and show what is committed; the
  refresh after each run shows none of its changes until `COMMIT`. Committing
  staged edits (⌘S in the Data view) to a row the transaction has written
  fails with the engine's `Conflict on update!`, edits kept.
- **The server bounds it.** A session lives five minutes from its `BEGIN`,
  whatever runs on it, and the band counts that down. Harbor also reclaims a
  session that sits thirty seconds between statements, which composing the
  next statement easily takes, so at a third of that interval the view
  renews the session, which runs nothing on it, waiting five seconds for the
  answer and no longer. A server that renews only backup sessions gets
  `SELECT 1` instead, sent while nothing else is running. At the five-minute
  deadline the server rolls the transaction back, and the view says so: `The
  transaction is gone: the server reclaimed its session and rolled back
  everything since BEGIN.` The view checks every second, so a loss a results
  page runs into shows within one. The keepalive is a timer in the app, and
  whether macOS delays it while the window is hidden (App Nap) is not
  measured: a transaction left open behind a hidden window may be found gone
  on return, and is then reported as any lost one is.
- **A statement typed for a lost transaction never runs on its own
  unannounced.** One sent to a session that is gone is not run outside it
  instead; it fails with that message and `This statement did not run.` When
  the view finds the loss between statements, the next ⌘Enter is refused once,
  whatever it would send: `This statement was not sent: outside a transaction
  it commits on its own. ⌘Enter again runs it that way.`
- **Errors are the engine's.** Measured, by sending a statement after each
  inside one session: a statement the parser refuses (`Parser Error`) leaves
  the transaction as it was, and the next statement still reads its writes.
  Every other class of engine error aborts it: a missing table or function
  (`Catalog Error`), an unknown column or mismatched types (`Binder Error`), a
  PRIMARY KEY, NOT NULL or CHECK violation (`Constraint Error`), a failed cast
  (`Conversion Error`), an overflow (`Out of Range Error`), `error()` and a
  missing parameter (`Invalid Input Error`), and a second `BEGIN`
  (`TransactionContext Error`). After any of them a statement that reads or
  writes answers `Current transaction is aborted (please ROLLBACK)` until
  `ROLLBACK` or `COMMIT` ends it. Harbor refuses a `COMMIT` of it, `400
  sql_error`, saying the transaction has been rolled back and nothing since
  `BEGIN` was kept.
- **The view knows whether the transaction is aborted by asking it.** Only
  one answer settles it: an aborted transaction answers `SELECT 1` with that
  error, and a sound one answers it. Other statements prove nothing, since
  some answer on an aborted transaction (measured: `PREPARE` does). So after
  any statement on the session that may have run and failed, the view asks
  `SELECT 1` at once, in the same turn on the session, and the band says what
  came back. While the band is unconfirmed or aborted the keepalive asks too,
  in place of the renew; a renew cannot change the band, since only a
  statement on the session can abort its transaction, and one another client
  sends there is found by the next statement or the `COMMIT`, which asks
  first. The band is therefore not a guess, and does not say aborted of a
  statement Harbor itself turned away, such as two statements sent as one.
  A protected `SET` is the engine's refusal (measured: the locked
  configuration answers `Invalid Input Error`), and aborts the transaction
  like any other. When the question cannot be answered, the band reads
  `transaction open, its state unconfirmed after an error`.
- **A `COMMIT` that rolled back says so.** Before a `COMMIT` the view asks
  once more, and sends the `COMMIT` in the same turn, so no results page or
  keepalive can come between the two and abort what was just found sound. A
  `COMMIT` of an aborted transaction then reports `COMMIT rolled back: an
  earlier error aborted the transaction, and nothing since BEGIN was kept.`
  and never `ok`. If the question gets no answer, because the session is busy
  or silent, the `COMMIT` is not sent: `could not confirm the transaction's
  state (…), so the COMMIT was not sent: try again`, with the transaction as
  it was. This covers the errors the user did not see as a statement's
  verdict: inside a transaction a count of a paged result that ran and failed
  is reported as the run's error, where outside one it only leaves the total
  unknown, and a results page that fails to read turns the band.
- **Only the engine's answer ends it.** A `COMMIT` the engine refuses (two
  transactions inserting the same key) ends the transaction rolled back, and
  the view adds `The transaction is over: its changes were rolled back.` A
  `COMMIT` or `ROLLBACK` that never ran leaves the transaction open and the
  session held, under the message it got: Harbor refused it before the engine
  saw it, because the session is still running the statement before it
  (`session_busy`, which a statement that outlived the view's two-minute wait
  can cause) or the server is not serving, or it could not be sent at all.
- **An ending with no verdict is not shown as open.** That is a `COMMIT` or
  `ROLLBACK` that got no answer, or that Harbor answered `internal`, or a
  `ROLLBACK` it answered `cancelled`. The view releases the session, which
  rolls back anything still open, and says the transaction may have ended
  either way. A `COMMIT` answered `cancelled` is a verdict: Harbor runs a
  session's `COMMIT` to its answer, so a `499` means it never started, and
  the view says `The COMMIT did not run, and nothing since BEGIN was kept.`
  and releases the session. Two cases leave no doubt and say so: a
  `ROLLBACK` is rolled back either way, by the statement or by that release,
  and so is a `COMMIT` of a transaction already found aborted. If the engine
  answers that no transaction is active, the view says the session held none
  and that the statement changed nothing; it does not claim a rollback.
- **Leaving ends it.** The session is released, and the transaction rolled
  back, when the view goes: the connection drops, another database is chosen,
  the server is stopped, or the app quits. ⌘Q, the close button, choosing
  another database, Stop and Remove Database ask first (EDITING.md,
  "Dialogs"); every quit releases the session, the ones that ask nothing
  included, and so does one while the `BEGIN` itself is still in flight.

## Results

The results pane is the ordinary `Grid`, built without identity, so read-only
falls out of the capability gate rather than a fork. NULL tags, the value font
and copy all carry over. Row numbers are result ordinals.

**Paging.** A result pages like a table, because the grid's FROM target is the
statement itself in parentheses: `SELECT * FROM (statement) LIMIT … OFFSET …`,
for the SELECT-shaped family (select, with, from, values, table, or a
parenthesized statement) and for the statements that answer with a table of
their own (describe, summarize, show, pivot, unpivot), behind any leading
comments. The pager, size cycling and jump-to-last all work.

A run costs at most the two queries the Data view pays for a table, and usually
one. Page 0 fetches `size + 1` rows, and a result that fits the page is its own
exact count; only the extra row's arrival proves there is more, and only then
does `count(*)` run for the total. The page query doubles as the wrap probe: if
the engine refuses it, because the statement is not really SELECT-shaped or
does not parse, the statement runs bare, so an engine error always quotes the
user's own SQL, never the wrapper's. A probe that got no answer, or that
Harbor cut short, may have run, and is the run's verdict: a SELECT that timed
out does not run a second time, nor a `nextval` twice. A statement that cannot
be wrapped keeps its whole result as one page, with the pager hidden. The
costs are named: a big result runs its plan twice, and deep OFFSET pages
re-skip rows, as table paging does.

Inside a transaction every page of a result is read on the transaction's
session, so later pages see what it has written, for as long as it is open.
There the probe's failure is the verdict unless it failed to parse: any other
error has aborted the transaction, and running the statement bare would only
report that. Such an error quotes the wrapped statement, whose line numbers
are one higher than the statement's own.

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
- **A session for the whole visit.** One Harbor session held while the berth
  stays connected, so temp tables and macros carry between runs outside a
  transaction too. It needs a session without the five-minute deadline. A dead
  session would reopen on the next run with a note that its temporary state is
  gone.
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
