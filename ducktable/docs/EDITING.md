# Editing

The editing grammar, as settled by comparative research (Sheets, Excel,
TablePlus, DataGrip, Postico, Sequel Ace, Airtable, Beekeeper, DBeaver)
and a three-way adversarial design panel (Sheets purist, data guardian,
ruthless minimalist). Where this document and older notes in UI.md
disagree, this document wins.

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
INSERT. It copies exact wire values rather than formatted cell text, includes
any staged updates already visible on the source row, and omits primary-key and
generated columns so DuckDB can supply the new identity and derived values. A
natural key without a default therefore remains `REQUIRED`. The entire copied
row is one undo step and is not written until ⌘S.

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
| Esc | clears the selection | cancels the edit, restores what was there, ring stays |
| Delete / ⌫ | clears the cell: text → `''`, everything else → NULL (NOT NULL columns refuse, with the reason in the status line) | deletes text |
| ⌃⇧N | stages NULL explicitly, any type | — |
| ⌘N | creates a new all-DEFAULT row and opens its first useful writable cell | — |
| ⌘D | duplicates the selected persisted row as one staged INSERT | — |
| ⌘⌫ | stages a row DELETE (ghost strikethrough; reversible until commit) | — |
| ⌘Z / ⌘⇧Z | un-stages / re-stages the most recent change | text undo / redo |
| ⌘S | commits all staged changes — one transaction, all or nothing | confirms the cell, then commits (⌘Enter is its equal) |
| ⌥Enter | — | newline (the Sheets-hand twin of ⇧Enter) |
| ⌘Enter | commits all staged changes | confirms the cell, then commits — "send it," the AI-composer universal (Steve's ruling, 2026: the world's ⌘Enter now means send, and ⇧Enter means newline; the older Sheets ⌘Enter-newline reflex keeps ⌥Enter) |

The replace-vs-kept-value arrow split is Sheets' own physics, unnamed:
the entry gesture *is* the state, your finger chose it a second ago. No
mode names, no status chip, no mid-edit toggle.

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
| ⌥← / ⌥→ | step the view switcher's segments left / right, rolling over at the ends (Data / Structure today; a carousel, ready for more segments) |
| ⌘⇧⌫ | discard all staged changes (TablePlus's chord; every discard stays undoable) |
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
  discard) — audit is pull-based, never pushed.
- Every staging operation — including a discard — is one entry on the
  ⌘Z stack. Nothing is ever more than one keystroke from recovery.

## Identity and capability

- Editing binds a row identity in the WHERE clause: the **original
  fetched values** of the primary-key columns when the catalog has a
  key — and DuckDB's implicit **rowid** when it doesn't. Every base
  table has a rowid, so keyless tables edit like any other: pages fetch
  `rowid, *`, the column stays hidden from every surface, and only the
  WHERE clauses see it. This beats the all-columns-WHERE fallback other
  tools use — duplicate rows each keep their own rowid (all-columns
  matching refuses to edit either copy), and NULL comparison never
  enters the picture. The physical-id caveat: a vacuum after heavy
  deletes can renumber rows; the affected-exactly-one check at commit
  is the backstop, and the fetch-to-⌘S window is seconds. (The original
  panel ruling — keyless means read-only — predates noticing the engine
  hands us an identity for free; Steve overruled it with rowid in
  hand. A read-only fallback remains for anything without one, views
  someday.)
- Primary-key cells are editable like any other — the WHERE holds the
  original, so `SET id = 7 WHERE id = 5` is just an update.
- Draft inserts need no fetched identity. Each carries a private local key
  until commit; `INSERT … RETURNING *` verifies that exactly one row landed,
  and the post-commit refetch acquires its real primary key or rowid.
- Statements are parameterized (`?` + bound params), never assembled
  from strings. Identifiers are quoted.

## Commit

⌘S opens a Harbor session (a pinned connection), then:
`BEGIN` → parameterized inserts, updates, and deletes, each verified to have
affected or returned **exactly one row** → `COMMIT` → release. Before opening
the session, DuckTable refuses a draft missing a `NOT NULL` column with no
default. Any failure — SQL error,
constraint, or a row that no longer matches its original values — rolls
the whole transaction back: nothing landed, every staged change is kept
and still visible, the offender is marked, and the status line says why,
ending with "edits kept."

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

- **Live mode** (write-per-edit): designed in UI.md, deferred until it
  re-clears an adversarial review. If it ships, type-to-edit turns off
  in it — the two are certified only as a pair with staging.
- Value popout editor for long/nested values, range selection and
  TSV paste-spread, crash-recovery journal for staged edits.
