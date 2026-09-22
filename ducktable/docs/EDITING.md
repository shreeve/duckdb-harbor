# Editing

The editing grammar, drawn from the spreadsheets and database clients people
already know (Sheets, Excel, TablePlus, DataGrip, Postico, Sequel Ace,
Airtable, Beekeeper, DBeaver). On editing, this document is the authority over
DESIGN.md.

## The five laws

1. **The grid is a place you type into, not a form you unlock.** Type on
   any editable cell and the editor materializes around the keystroke —
   same frame, first character never lost.
2. **Nothing writes to the database until you say so.** ⌘S commits
   everything staged as one all-or-nothing transaction. There is no live
   mode in v1; staging is the load-bearing wall that makes every other
   liberty here safe.
3. **Esc is a panic key, so it is lossless.** It cancels what you are
   typing; it never touches a staged change. Single level, no exceptions.
4. **Staged changes are keyed by row identity, owned by the table,
   untouchable by the view.** Sort, filter, page, switch tables — nothing
   is lost, no dialog needed, and the count never lies.
5. **Content snaps, chrome fades.** Values change same-frame; decorations
   ease. After commit the grid refetches, so every row shows the
   database's truth — defaults filled, triggers applied.

## Inserting rows

The `+` button in the Data footer creates a draft row at the top of the
current grid and opens its first required writable cell. The `+` in its row
number rail is its identity: it is staged, not yet a database row. Additional
clicks create additional drafts. Drafts stay above the fetched page through
filter and page changes because hiding an uncommitted insert would be data
loss disguised as navigation.

An untouched draft cell means SQL `DEFAULT`, not NULL. DuckTable omits it from
the INSERT so DuckDB can apply declared defaults, sequences, and generated
expressions. The placeholder says what will happen: `REQUIRED`, `DEFAULT`,
`NULL`, or `GENERATED`. Generated cells are read-only. Delete/Backspace keeps
the grid's type-honest meaning — empty string for text, explicit NULL for
other nullable types — and ⌃⇧N is always explicit NULL.

Ordinary viewing keeps columns at their compact content-fit widths. While a
cell editor or draft row is present, columns expand only as needed to fit these
placeholder pills; they return to their compact widths when editing ends and no
draft remains.

Moving out of a draft never writes it. The row joins the same staged set as
updates and deletes immediately, including undo, review, discard, table
switches, and the all-or-nothing commit. Discarding/deleting a draft removes
the pending INSERT; it never emits a DELETE.

**Duplicate Row** (⌘D) copies the selected persisted row into a new staged
INSERT. The engine makes the copy, not the wire: the INSERT selects each copied
column from the source row, named by its original key or its rowid, so every
value arrives as the value it was — a DATE or a DECIMAL inside a `VARIANT`, an
integer past 64 bits, the bytes in a `BLOB[]`, an INTERVAL, a MAP, a UNION, none
of which survive a trip out as JSON and back. The draft shows the source row's
text. A cell with a staged update on the source row is copied as staged, and a
cell typed over in the draft is an ordinary typed cell; both are bound the way
their column's type asks (below). A copied cell typed back to the text it was
copied with is read from the source row again, so a DATE inside a `VARIANT`
that was typed over and restored is still a DATE. Primary-key and generated
columns are omitted so DuckDB can supply the new identity and derived values. A
natural key without a default therefore remains `REQUIRED`. The entire copied
row is one undo step and is not written until ⌘S. Inserts run first in the
transaction, so the source row is read as the database holds it at ⌘S, even
when the same commit updates or deletes it; a source row that is gone by then
fails the commit, and nothing lands. A refresh does not help, because the draft
still names that row: discard the duplicate (⌘Z, or the review popover) and the
rest commits.

⌘D copies a row the database holds. On a draft it copies nothing and the status
line says why: a new row is not in the database yet, so the INSERT has nothing
to read; commit it, then duplicate it. On a row staged for deletion it says to
discard the delete first, and with no row selected it says to select one.

## The grammar

One meaning per key. No contextual double-agents.

| Key | In the grid (navigating) | In the cell editor |
|---|---|---|
| typing | opens the editor **replacing** the value, seeded with the keystroke | inserts text |
| Enter | opens the editor **keeping** the value, caret at end — but during a Tab run, sweeps to the run's anchor column one row down (the carriage return) | confirms the cell, ring moves down — or sweeps, if a Tab run is going |
| ⇧Enter | (same as Enter, sweeping/moving up) | inserts a line break — the chat-composer convention (Slack, every message box); confirm-and-move-up retired in its favor |
| Tab / ⇧Tab | moves the ring right / left with row-local wraparound, arming the typewriter anchor | confirms, moves right / left with row-local wraparound, and immediately edits the destination cell; anchor kept |
| arrows | move the ring | *replace entry:* confirm + move the ring · *kept-value entry:* move the caret |
| double-click | opens the editor keeping the value, caret at the click | — |
| ⌘-click | adds the row to the selection, or removes it | — |
| ⇧-click | selects every row from the anchor through this one, replacing the selection | — |
| Esc | clears the selection | cancels the edit, restores what was there, ring stays |
| Delete / ⌫ | clears the cell: text → `''`, everything else → NULL (NOT NULL columns refuse, with the reason in the status line) | deletes text |
| ⌃⇧N | stages NULL explicitly, any type | — |
| ⌘N | creates a new all-DEFAULT row and opens its first useful writable cell | — |
| ⌘D | duplicates the lead selected persisted row as one staged INSERT | — |
| ⌘⌫ | stages a DELETE for every selected row (ghost strikethrough; one undo step, each row its own entry for review; reversible until commit) | — |
| ⌘Z / ⌘⇧Z | un-stages / re-stages the most recent change | text undo / redo |
| ⌘S | commits all staged changes — one transaction, all or nothing | confirms the cell, then commits (⌘Enter is its equal) |
| ⌥Enter | — | newline (the Sheets-hand twin of ⇧Enter) |
| ⌘Enter | commits all staged changes | confirms the cell, then commits: ⌘Enter means send, as it does in a chat composer, and ⇧Enter or ⌥Enter mean newline |

The replace-vs-kept-value arrow split is Sheets' own physics, unnamed:
the entry gesture *is* the state, your finger chose it a second ago. No
mode names, no status chip, no mid-edit toggle.

## Selecting rows

A click selects one row (and, in the body, seats the ring on the clicked cell). The macOS list grammar builds on it: ⌘-click toggles a row in or out, ⇧-click selects the span from the anchor through the clicked row, replacing whatever was selected. The anchor is the last row clicked without ⇧, so a second ⇧-click re-spans from the same place. The gutter and the body cells select the same way.

The selection has a lead: the row the ring, the inspector, and ⌘D act on. It is the row last clicked into the selection; ⌘-clicking the lead away hands the role to the last remaining row. ⌘⌫ acts on every selected row as one gesture: one ⌘Z brings them all back, while the review popover still lists and discards them one by one. Esc, a page change, and a commit clear the selection whole.

⇧-click in the body takes a seat that cell ranges would want (TablePro spans cells there and keeps row spans on its gutter); if cell ranges ship, body ⇧-click moves to them and the gutter keeps the row span.

## Navigation

With a cell selected and no editor open, every modifier + arrow
combination has a deliberate answer:

| Keys | Meaning |
|---|---|
| arrows | move the ring one cell |
| ⌘↑ / ⌘↓ | first / last row of the page |
| ⌘← / ⌘→ | first / last visible column |
| Home / End | first / last visible column |
| ⌘Home / ⌘End | first / last cell of the page (Sheets' A1 / end-of-data, page-scoped) |
| F2 | opens the kept-value editor (the third door, with Enter and double-click — and the one that works mid-Tab-run) |
| PageUp / PageDown | one screenful up / down within the loaded page (Sheets' meaning), a row of overlap, clamped at the page edge |
| ⌥↑ / ⌥↓ | previous / next DATABASE page (the pager) — the ring keeps its seat (same column, row clamped); when multiple grid tabs exist someday, these migrate to tab switching (Sheets' worksheet keys) |
| ⌥← / ⌥→ | step the view switcher's segments left / right, rolling over at the ends (Structure / Data / Query) |
| ⌘⇧⌫ | discard all staged changes (TablePlus's chord; one undo step, so even this is reversible) |
| ⇧ + arrows | deliberately inert — range selection's seat, reserved until ranges ship; a ring that moved when you expected a range to grow would lie |
| ⌃ + arrows | never bound — macOS owns them (Mission Control, Spaces) |

⌘-arrow edges are page-scoped on purpose, the same ruling as fit: the
keyboard operates on *what you are looking at*. Crossing pages is always
an explicit act (the Page keys, ⌥↑/⌥↓, or the pager).

**The typewriter sweep** (Sheets' own physics, and the reference rules
from Univer — the only open-source implementation that has it): the
first **forward** Tab of a run remembers its column (⇧Tab retreats
within a run but never begins one). Enter during the run — whether confirming an edit
or just navigating — returns to that column one row down, like a
carriage return; ⇧Enter sweeps up. Any arrow, click, Esc, or page
change ends the run. Tab a row's cells, edit some, press Enter, and you
are at the start of the next row.

### How the whole space is defined

Five modifiers times four arrows times their combinations is hundreds of
chords; nobody enumerates that, and we don't either. Every keystroke
falls through a decision ladder to exactly one rung, so every
combination has a defined outcome without a defined row:

1. Editor open → the editor grammar; everything unmatched falls through
   to the text input, whose answers we inherit whole.
2. Focus outside the table → not ours; each input owns its keys.
3. An exact chord we bound → it means what the tables above say.
4. Any other chord containing ⌘, ⌃, or Fn → passes through untouched;
   menus and the OS own that space. This one rung defines most of the
   hundreds.
5. ⇧ + arrows → inert, range selection's reserved seat.
6. A bare printable character → type-to-edit.
7. Anything left → nothing, on purpose.

"What does ⌘⌥⇧↑ do?" is answered by rung 4, not by a missing row.

Porting note: gpui's `platform` modifier is ⌘ on macOS, the Win key on
Windows, Super on Linux. A Windows/Linux build swaps the primary chord
modifier to Ctrl in one helper at rungs 3–4 — the ladder itself does
not change, and printable exotica (AltGr, IME) already land on rung 6.

## Staging

- A staged cell shows a soft accent tint. A draft insert shows the same tint
  across its synthetic row; a staged delete shows the row ghosted with
  strikethrough. While an editor is open there is no tint — the editor surface
  is the state; staging happens when the editor confirms.
- One entry per cell, last wins. A cell edited back to its original
  value auto-cleans: "3 changes" always means three real diffs.
- The status line counts, verb-split when destruction is pending:
  `3 changes · ⌘S to commit`, or
  `1 insert · 2 updates · 1 delete · ⌘S to commit`
  with the delete in the danger color. Clicking the count opens a
  popover listing every staged change (`column: old → new`, per-change
  discard) — audit is pull-based, never pushed. A duplicate is listed as a
  new row with the row it copies, every key column named: `new row · copy of
  id = 5`, `copy of a = 1, b = 2` for a composite key, `copy of rowid = 3`
  for a table without one.
- Every staging operation — including a discard — is one entry on the
  ⌘Z stack. Nothing is ever more than one keystroke from recovery.
- Staged changes parked by a table switch wait for a grid that can take them.
  Returning to the table while its first page fails to load leaves them
  parked: the grid has no columns to check them against, keeps them until a
  refresh brings the page, and hands them back if another table is chosen
  first. A failed fetch never costs a staged change.

## A table altered elsewhere

A grid takes its columns — names, types, key — from its first page, and every
staged value is bound the way its column's type asks. Another client can
`ALTER TABLE` while the grid is open, so every fetched page is checked: its
column names and DuckDB types, in order, against the grid's.

- With nothing staged, the grid adopts the table as it is — columns, types,
  key, placeholders, a fresh staging set — exactly as a first page does, and
  asks for the catalog again. Hidden columns and dragged widths belong to the
  old columns and are reset. The Structure view still shows the catalog the
  grid was opened with; selecting the table again shows the present one.
- With edits staged, they are not rebound: they were typed and keyed against
  the old columns. The page on screen stays, the fetched page is dropped, and
  the status line says the table's columns changed. Until the staged set is
  empty nothing more can be staged and ⌘S is refused with the same message;
  the review popover, ⌘Z and discard work as always. When the last staged
  change is discarded or undone, the grid fetches the table as it is and
  adopts it.
- Staged changes parked for a table that changed shape while it was off-screen
  cannot be shown against the old columns. They are dropped when the table is
  opened again, and the status line says how many.

A grid learns of the change only from a page. Between the `ALTER` and the next
fetch, a commit binds through the old types; the engine's casts and the
affected-exactly-one check are the backstop there, as everywhere.

## Identity and capability

- Editing binds a row identity in the WHERE clause: the **original
  fetched values** of the primary-key columns when the catalog has a
  key, and DuckDB's implicit **rowid** paired with a **hash of the whole
  row** when it doesn't. Every base table has a rowid, so keyless tables edit
  like any other: pages fetch the pair as one hidden column, and only the
  WHERE clauses see it, as `rowid = ? AND hash(row) = ?`. A rowid alone
  names a position, not a row: a checkpoint that compacts deleted rows
  renumbers the rest, and a rowid then names another row that the
  exactly-one check would pass. With the hash, a row that moved or changed
  since the fetch matches nothing, and the commit refuses. This beats the
  all-columns WHERE other tools use: duplicate rows each keep their own
  rowid, where all-columns matching refuses to edit either copy, and NULL
  comparison never enters the picture. Anything without an identity is
  read-only: a view, and a keyless table with a column of its own called
  `rowid`, which shadows DuckDB's in every query.
- Primary-key cells are editable like any other — the WHERE holds the
  original, so `SET id = 7 WHERE id = 5` is just an update.
- A draft insert has no identity of its own. Each carries a private local key
  until commit; `INSERT … RETURNING *` verifies that exactly one row landed,
  and the post-commit refetch acquires its real primary key or rowid. A
  duplicate also carries the identity of the row it copies, which its INSERT
  names in a WHERE like any other.
- Statements are parameterized (`?` + bound params), never assembled
  from strings. Identifiers are quoted.
- A value is bound as what it is. Harbor binds text as VARCHAR, and for most
  types the engine's cast from there is the right one. Two are not, and their
  placeholder says so, in the SET list, the VALUES list and the WHERE alike:
  - A `VARIANT` or `JSON` cell is JSON text both ways, and binds through
    `?::JSON`. JSON text bound bare into a `VARIANT` is stored as a string —
    every path into it NULL, nothing said. So the cell takes JSON: `{"a": 1}`,
    `[1, 2]`, `42`, `true`, and a string in its quotes, `"Morel"`, as the cell
    shows it. Text that is not JSON is refused in the editor with the reason.
    `null` is SQL NULL in a `VARIANT`, where the engine knows no other, and a
    JSON value in a `JSON` column. The text is bound as typed. A `VARIANT`
    stores the values, so whitespace is not kept, the last of a repeated key
    wins, `100.00` is `100.0`, and the cell reads back compact after the
    refetch. A `JSON` column stores the text, character for character.
  - The JSON is strict: `NaN`, `Infinity` and `-Infinity` are refused. The
    engine reads them in a document and a `VARIANT` stores them, but Harbor
    then sends `{"x":NaN}`, which is not JSON, and a client reads the whole
    document back as a string. A cell that already holds one can be opened
    and left, since unchanged text is never validated.
  - A document nests at most 100 levels, the limit every first-party client
    keeps; deeper text is refused in the editor, which says so.
  - A typed edit of a `VARIANT` retypes it through JSON. Values written from
    SQL that JSON cannot name — a DATE, a DECIMAL, a HUGEINT inside the
    document — come back as a string, a DOUBLE, a DOUBLE: the cell shows JSON,
    and the JSON shown is what gets bound. To change one path and keep the
    rest as typed, use the Query tab.
  - A `BLOB` cell is base64 both ways, and binds through
    `from_base64(?::VARCHAR)`. Bound bare, the base64 characters themselves
    become the bytes. A `BLOB` key in the WHERE is decoded the same way, so
    the row named is the row changed. Text typed into a `BLOB` cell is always
    base64, `null` included: `null`, `NULL` and `Null` are four base64
    characters each, three bytes (`9EE965`, `3542CB`, `36E965`) that a cell
    can hold and show. A `BLOB`'s NULL is entered with ⌃⇧N, with Delete, or by
    emptying the cell.
- A `FLOAT` key binds through `?::FLOAT` in the WHERE. A FLOAT crosses the
  wire as the shortest decimal that names it and a JSON number binds as a
  DOUBLE; compared bare, the column is widened and 1.1 the FLOAT is not 1.1
  the DOUBLE, so the statement names no row. A FLOAT value in a SET or VALUES
  list needs no cast: assignment rounds it to the same FLOAT.
- A number is bound as a JSON number where JSON can carry it and as text where
  it cannot. An integer past 64 bits — a `UBIGINT`, a `HUGEINT`, a `UHUGEINT` —
  goes as its digits, and `nan`, `inf`, `-inf`, `Infinity` into a `DOUBLE` or a
  `FLOAT` go by name; the engine casts both exactly, and a typed number is never
  staged as NULL. An integer outside its type's range, and digits too large for
  a `DOUBLE`, are refused in the editor with the reason. So is a number a
  `FLOAT` cannot hold: the engine rounds the DOUBLE it is handed to the nearest
  FLOAT and refuses one that rounds past the largest, so `3.40282356e38` is
  taken (it is the largest FLOAT, 3.4028235e38) and `3.4028236e38`, `3.5e38` and
  `1e39` are refused, where a `DOUBLE` cell takes all three. The names have no
  range.
- The literal `null`, in any case, typed into a cell that is not text is SQL
  NULL — it was never a number or a date. In a text cell it is those four
  characters, in a `JSON` cell the JSON value, and in a `BLOB` cell base64.
- The editor judges a scalar by its own type name. A nested type (`INTEGER[]`,
  `STRUCT(a INTEGER)`, a `MAP`), an `INTERVAL` and an `ENUM` are bound as their
  text for the engine to cast. They clear to NULL, as a `UUID` does: the engine
  takes `''` for none of them. An `INTERVAL` cell shows its months, days and
  microseconds as JSON and a `MAP` its key/value pairs, and neither is text the
  engine reads back, so type the value in DuckDB's own form instead:
  `3 days`, `{a=1}`.
- Confirming a cell with the text it already holds stages nothing and is never
  validated, so a value the engine accepted is never one the editor refuses to
  leave: a `VARIANT` holding NaN, a DOUBLE that is NaN, an integer wider
  than 64 bits. NULL and `''` both open an empty editor, so an empty editor
  confirmed over a cell that holds NULL — fetched, staged with ⌃⇧N, or copied
  by ⌘D — leaves it NULL, and over a draft's untouched cell leaves it
  `DEFAULT`. `''` is entered by emptying a text cell that held something, or
  with Delete.
- Text typed back to what the cell had is not validated either: on a fetched
  row the staged edit is dropped, and on a duplicate the cell is read from its
  source row again.
- Whitespace around a value that is not text is not a change. ` 5` or `5 ` over
  an INTEGER that holds `5` stages nothing, and over a staged `7` it is the
  fetched `5` again: the engine reads a padded number, date, time, interval,
  list, struct or `VARIANT` as it reads the bare one, and refuses a padded
  `UUID`, `BIT` or base64 outright, so padding never names another value.
  Where it does, the comparison is exact: text (` 5` is another string), a
  `JSON` column, which stores its text character for character, and an `ENUM`,
  whose values are strings. Text that differs by more than its padding is
  staged as typed.
- A container that holds a `VARIANT`, a `JSON` or a `BLOB` — `BLOB[]`,
  `VARIANT[]`, `JSON[]`, `STRUCT(v VARIANT, …)`, `MAP(VARCHAR, BLOB)` — takes
  no typed text. It would be bound as the container's text, which the engine
  casts without reaching the inner value: a `BLOB[]` stores the base64
  characters as the bytes, and the elements of a `VARIANT[]` or a `JSON[]`
  become strings, with no error. The editor refuses the text and names the
  Query tab. Such a cell can still be opened and left, cleared to NULL, and
  copied by ⌘D, which reads it in SQL.

## Commit

⌘S opens a Harbor session (a pinned connection), then:
`BEGIN` → parameterized inserts, updates, and deletes, each verified to have
affected or returned **exactly one row** → `COMMIT` → release. Before opening
the session, DuckTable refuses a draft missing a `NOT NULL` column with no
default. Any failure rolls the whole transaction back: an SQL error, a
constraint, or a row its identity no longer names, because the row is gone,
its key changed, or on a keyless table it moved or changed. Nothing lands,
every staged change is kept and still visible, and the status line says why,
ending with "edits kept." The status line is the whole report: no row or cell
is marked as the one that failed.

The WHERE compares the identity, not every value. On a keyed table, a cell
another client changed since the fetch is overwritten by the staged value
without a conflict; only a keyless row's hash notices other columns.

While a commit is in flight, nothing stages, undoes or discards: its
statements were built at ⌘S, and a change made now would be shown undone while
the commit writes it anyway, or erased when the commit succeeds. A table
switch waits for the commit to settle, then runs.

After a successful commit the page refetches so the grid shows the
database's truth, and Refresh Tables refetches `/catalog` so every sidebar
row count reflects the committed transaction. Manual Refresh Tables and a
completed Query run also refetch the currently open Data page. NULL renders as
the NULL tag, visually distinct from empty, always.

## Dialogs

Exactly one: quitting with staged changes (default button = Cancel).
Deletes never confirm — they stage, visibly, reversibly, and execute
only at ⌘S. Reversibility replaces confirmation.

## Deferred, deliberately

- **Live mode** (write-per-edit, DESIGN.md's Planned): deferred until it
  clears an adversarial review. If it ships, type-to-edit turns off
  in it — the two are certified only as a pair with staging.
- Value popout editor for long/nested values, range selection and
  TSV paste-spread, crash-recovery journal for staged edits.
