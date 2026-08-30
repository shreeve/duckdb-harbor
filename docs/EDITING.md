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

## The grammar

One meaning per key. No contextual double-agents.

| Key | In the grid (navigating) | In the cell editor |
|---|---|---|
| typing | opens the editor **replacing** the value, seeded with the keystroke | inserts text |
| Enter | opens the editor **keeping** the value, caret at end | confirms the cell, ring moves down |
| ⇧Enter | (same as Enter) | confirms, ring moves up |
| Tab / ⇧Tab | moves the ring right / left | confirms, ring moves right / left |
| arrows | move the ring | *replace entry:* confirm + move the ring · *kept-value entry:* move the caret |
| double-click | opens the editor keeping the value, caret at the click | — |
| Esc | clears the selection | cancels the edit, restores what was there, ring stays |
| Delete / ⌫ | clears the cell: text → `''`, everything else → NULL (NOT NULL columns refuse, with the reason in the status line) | deletes text |
| ⌃⇧N | stages NULL explicitly, any type | — |
| ⌘⌫ | stages a row DELETE (ghost strikethrough; reversible until commit) | — |
| ⌘Z / ⌘⇧Z | un-stages / re-stages the most recent change | text undo / redo |
| ⌘S | commits all staged changes — one transaction, all or nothing | confirms the cell, then commits |
| ⌥Enter | — | newline (⌘Enter also, per Sheets muscle memory — ⌘Enter never commits) |

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
| PageUp / PageDown | previous / next page — the ring keeps its seat (same column, row clamped) |
| ⌥↑ / ⌥↓ | previous / next page (same as the Page keys, reachable without Fn) |
| ⇧ + arrows | deliberately inert — range selection's seat, reserved until ranges ship; a ring that moved when you expected a range to grow would lie |
| ⌃ + arrows | never bound — macOS owns them (Mission Control, Spaces) |

⌘-arrow edges are page-scoped on purpose, the same ruling as fit: the
keyboard operates on *what you are looking at*. Crossing pages is always
an explicit act (the Page keys, ⌥↑/⌥↓, or the pager).

## Staging

- A staged cell shows a soft accent tint. A staged delete shows the row
  ghosted with strikethrough. While an editor is open there is no tint —
  the editor surface is the state; staging happens when the editor
  confirms.
- One entry per cell, last wins. A cell edited back to its original
  value auto-cleans: "3 changes" always means three real diffs.
- The status line counts, verb-split when destruction is pending:
  `3 changes · ⌘S to commit`, or `2 updates · 1 delete · ⌘S to commit`
  with the delete in the danger color. Clicking the count opens a
  popover listing every staged change (`column: old → new`, per-change
  discard) — audit is pull-based, never pushed.
- Every staging operation — including a discard — is one entry on the
  ⌘Z stack. Nothing is ever more than one keystroke from recovery.

## Identity and capability

- Editing requires a primary key. The UPDATE/DELETE WHERE binds the
  **original fetched values** of the key columns; a table without a
  primary key is read-only, with the reason stated, never a mystery.
  (Panel ruling: every clever keyless workaround converts a visible
  refusal into an invisible lottery.)
- Primary-key cells are editable like any other — the WHERE holds the
  original, so `SET id = 7 WHERE id = 5` is just an update.
- Statements are parameterized (`?` + bound params), never assembled
  from strings. Identifiers are quoted.

## Commit

⌘S opens a Harbor session (a pinned connection), then:
`BEGIN` → each staged statement in order, each verified to have affected
**exactly one row** → `COMMIT` → release. Any failure — SQL error,
constraint, or a row that no longer matches its original values — rolls
the whole transaction back: nothing landed, every staged change is kept
and still visible, the offender is marked, and the status line says why,
ending with "edits kept."

After a successful commit the page refetches so the grid shows the
database's truth. NULL renders as the NULL tag, visually distinct from
empty, always.

## Dialogs

Exactly one: quitting with staged changes (default button = Cancel).
Deletes never confirm — they stage, visibly, reversibly, and execute
only at ⌘S. Reversibility replaces confirmation.

## Deferred, deliberately

- **Live mode** (write-per-edit): designed in UI.md, deferred until it
  re-clears an adversarial review. If it ships, type-to-edit turns off
  in it — the two are certified only as a pair with staging.
- Ghost insert row, duplicate-row (menu, blanking key/unique columns),
  value popout editor for long/nested values, range selection and
  TSV paste-spread, crash-recovery journal for staged edits.
